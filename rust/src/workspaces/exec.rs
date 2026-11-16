//! Running commands in a session over the Exec stream.

use std::time::Duration;

use super::pb::{ExecRequest, ExecResponse, exec_response};
use super::{Client, DEFAULT_WORK_DIR, MAX_EXEC_STDIN};
use crate::{Error, Result};

crate::opaque_debug!(ExecOptions<'_>);

/// Receives a stream's output as it arrives.
pub type OutputSink<'a> = &'a mut (dyn FnMut(&[u8]) + Send);

/// Configures [`Client::exec`].
#[derive(Default)]
pub struct ExecOptions<'a> {
    /// The working directory, absolute or relative to /work. Empty means /work, as with
    /// `chainctl develop exec` (the plane itself starts commands in /). Set `no_dir` to pass argv
    /// through unwrapped.
    pub dir: String,
    pub no_dir: bool,
    /// Passed to the command (at most [`MAX_EXEC_STDIN`] bytes).
    pub stdin: Vec<u8>,
    /// Stops the command; the result then has `timed_out` set.
    pub timeout: Option<Duration>,
    /// Receive output as it streams; `None` discards it.
    pub stdout: Option<OutputSink<'a>>,
    pub stderr: Option<OutputSink<'a>>,
}

/// A finished command's status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct ExecResult {
    pub exit_code: i32,
    pub timed_out: bool,
}

/// Whether `e` is the plane refusing a call because another controller holds the guest (an
/// attached shell that is still heartbeating), including the FailedPrecondition "... held by
/// another controller ... Channel already in use" it returns for a few seconds after another
/// client's attach or exec ends.
pub fn is_guest_held(e: &Error) -> bool {
    match e {
        Error::Rpc(s) => match s.code() {
            tonic::Code::Aborted => s.message().contains("held"),
            tonic::Code::FailedPrecondition => controller_busy(s.message()),
            _ => false,
        },
        _ => false,
    }
}

fn controller_busy(msg: &str) -> bool {
    msg.contains("held by another controller") || msg.contains("Channel already in use")
}

/// Classifies an error from the exec stream. Once the stream has delivered an event the command
/// was running, so any failure leaves its outcome unknown. Before that, a failure of the
/// connection itself does too (the request may have arrived); an answer from the plane (not
/// found, held, invalid, over the limit, ...) means the command did not run.
fn interrupted(e: Error, started: bool) -> Error {
    if matches!(e, Error::OutcomeUnknown { .. }) {
        return e;
    }
    let transport = matches!(
        e.grpc_code(),
        Some(
            tonic::Code::Unavailable
                | tonic::Code::Internal
                | tonic::Code::Unknown
                | tonic::Code::DataLoss
                | tonic::Code::DeadlineExceeded
                | tonic::Code::Cancelled
        )
    );
    if started || transport {
        Error::OutcomeUnknown {
            cause: Some(Box::new(e)),
            result: None,
        }
    } else {
        e
    }
}

impl Client {
    /// Runs argv in the session (no shell; use [`Client::shell`] for that). A parked session is
    /// resumed by the plane first. Exec is refused while a shell attach holds the guest (see
    /// [`is_guest_held`]). A non-zero exit is reported in [`ExecResult`], not as an error. If the
    /// connection fails once the command may have started, the error is
    /// [`Error::OutcomeUnknown`] with the transport error as its cause.
    pub async fn exec(
        &self,
        name: &str,
        argv: &[String],
        o: ExecOptions<'_>,
    ) -> Result<ExecResult> {
        if argv.is_empty() {
            return Err(Error::other("workspaces: empty argv"));
        }
        if o.stdin.len() > MAX_EXEC_STDIN {
            return Err(Error::other(format!(
                "workspaces: stdin is {} bytes; the limit is {MAX_EXEC_STDIN}",
                o.stdin.len()
            )));
        }
        let argv = if o.no_dir {
            argv.to_vec()
        } else {
            let dir = if o.dir.is_empty() {
                DEFAULT_WORK_DIR
            } else {
                &o.dir
            };
            with_dir(dir, argv)
        };
        let req = ExecRequest {
            name: name.to_string(),
            argv,
            stdin: o.stdin,
            deadline: o.timeout.filter(|t| !t.is_zero()).map(duration),
        };
        let (mut stream, first) = self
            .start_exec(req)
            .await
            .map_err(|e| interrupted(e, false))?;
        let (mut stdout, mut stderr) = (o.stdout, o.stderr);
        let mut next = Some(first);
        loop {
            let ev = match next.take() {
                Some(ev) => ev,
                None => match stream.message().await {
                    Ok(Some(ev)) => ev,
                    Ok(None) => {
                        return Err(Error::OutcomeUnknown {
                            cause: None,
                            result: None,
                        });
                    }
                    // The stream delivered at least one event: the command was running.
                    Err(s) => return Err(interrupted(self.status(s), true)),
                },
            };
            match ev.event {
                Some(exec_response::Event::Stdout(b)) => {
                    if let Some(f) = stdout.as_mut() {
                        f(&b);
                    }
                }
                Some(exec_response::Event::Stderr(b)) => {
                    if let Some(f) = stderr.as_mut() {
                        f(&b);
                    }
                }
                Some(exec_response::Event::Exited(x)) => {
                    return Ok(ExecResult {
                        exit_code: x.code,
                        timed_out: x.timed_out,
                    });
                }
                None => {}
            }
        }
    }

    /// Opens the Exec stream and reads its first event. While the plane answers
    /// FailedPrecondition "instance held by another controller" (briefly, right after another
    /// client's attach or exec; nothing ran), it retries with backoff for up to ~30s.
    async fn start_exec(
        &self,
        req: ExecRequest,
    ) -> Result<(tonic::Streaming<ExecResponse>, ExecResponse)> {
        let delays = [2u64, 4, 8, 8, 8];
        let mut i = 0;
        loop {
            let r = self.request(req.clone()).await?;
            let mut stream = self.check(self.raw().exec(r).await)?.into_inner();
            match stream.message().await {
                Ok(Some(first)) => return Ok((stream, first)),
                Ok(None) => {
                    return Err(Error::OutcomeUnknown {
                        cause: None,
                        result: None,
                    });
                }
                Err(s)
                    if s.code() == tonic::Code::FailedPrecondition
                        && controller_busy(s.message())
                        && i < delays.len() =>
                {
                    tokio::time::sleep(Duration::from_secs(delays[i])).await;
                    i += 1;
                }
                Err(s) => return Err(self.status(s)),
            }
        }
    }

    /// Runs argv and returns its stdout, or [`Error::Exit`] on a non-zero exit or a timeout.
    pub async fn output(&self, name: &str, argv: &[String], o: ExecOptions<'_>) -> Result<Vec<u8>> {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let res = {
            let mut fo = |b: &[u8]| out.extend_from_slice(b);
            let mut fe = |b: &[u8]| err.extend_from_slice(b);
            let o = ExecOptions {
                dir: o.dir,
                no_dir: o.no_dir,
                stdin: o.stdin,
                timeout: o.timeout,
                stdout: Some(&mut fo),
                stderr: Some(&mut fe),
            };
            self.exec(name, argv, o).await?
        };
        if res.exit_code != 0 || res.timed_out {
            return Err(Error::Exit {
                exit_code: res.exit_code,
                timed_out: res.timed_out,
                stdout: out,
                stderr: err,
            });
        }
        Ok(out)
    }

    /// Runs `script` with `sh -c`; `args` become $1, $2, ...
    pub async fn shell(
        &self,
        name: &str,
        script: &str,
        o: ExecOptions<'_>,
        args: &[String],
    ) -> Result<Vec<u8>> {
        let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "_".into()];
        argv.extend_from_slice(args);
        self.output(name, &argv, o).await
    }
}

pub(crate) fn duration(d: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: d.as_secs() as i64,
        nanos: d.subsec_nanos() as i32,
    }
}

/// How chainctl implements `exec --cwd`.
const CD_SCRIPT: &str = r#"cd "$1" && shift && exec "$@""#;

fn with_dir(dir: &str, argv: &[String]) -> Vec<String> {
    let mut out = vec![
        "sh".to_string(),
        "-c".into(),
        CD_SCRIPT.into(),
        "_".into(),
        guest_path(dir),
    ];
    out.extend_from_slice(argv);
    out
}

/// Resolves p against /work, cleaning it like Go's `path.Clean`.
pub fn guest_path(p: &str) -> String {
    if p.starts_with('/') {
        clean_path(p)
    } else {
        clean_path(&format!("{DEFAULT_WORK_DIR}/{p}"))
    }
}

/// Go's `path.Clean` for slash-separated paths.
pub(crate) fn clean_path(p: &str) -> String {
    if p.is_empty() {
        return ".".into();
    }
    let rooted = p.starts_with('/');
    let mut out: Vec<&str> = Vec::new();
    for part in p.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                if out.last().is_some_and(|l| *l != "..") {
                    out.pop();
                } else if !rooted {
                    out.push("..");
                }
            }
            _ => out.push(part),
        }
    }
    let joined = out.join("/");
    match (rooted, joined.is_empty()) {
        (true, _) => format!("/{joined}"),
        (false, true) => ".".into(),
        (false, false) => joined,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clean_path_matches_go() {
        for (i, o) in [
            ("/a/b/../c", "/a/c"),
            ("/../x", "/x"),
            ("a/./b/", "a/b"),
            ("", "."),
            ("../a", "../a"),
            ("/", "/"),
            ("a/..", "."),
        ] {
            assert_eq!(clean_path(i), o, "{i}");
        }
        assert_eq!(guest_path("x/y"), "/work/x/y");
        assert_eq!(guest_path("/tmp/../etc"), "/etc");
    }
}
