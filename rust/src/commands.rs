//! Running commands in a sandbox: `sbx.commands`.

use std::collections::{BTreeMap, HashMap};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::sandbox::{Inner, guest_path, wrap};
use crate::scripts::*;
use crate::workspaces::ExecOptions;
use crate::{Error, Result};

crate::opaque_debug!(RunOptions, Commands, WaitOptions);

/// Bounds a foreground command when [`RunOptions::timeout`] is `None`.
pub const DEFAULT_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);

/// Receives a command's output as text, as it arrives.
pub type TextCallback = Box<dyn FnMut(&str) + Send>;

/// Configures [`Commands::run_with`] and [`Commands::run_args_with`].
#[derive(Default)]
pub struct RunOptions {
    /// The working directory (default /work; relative paths resolve against /work).
    pub cwd: String,
    /// Added to the sandbox-level environment (and override it).
    pub envs: HashMap<String, String>,
    /// Kills the command after this long. `None` means [`DEFAULT_COMMAND_TIMEOUT`];
    /// `Some(Duration::ZERO)` means no limit. The plane enforces it.
    pub timeout: Option<Duration>,
    /// Sent to the command (at most 16 MiB), then closed.
    pub stdin: Vec<u8>,
    /// Receive output as it arrives. A character whose bytes arrive in two pieces is delivered
    /// whole, with the later piece.
    pub on_stdout: Option<TextCallback>,
    pub on_stderr: Option<TextCallback>,
}

/// Configures a background command ([`Commands::start_with`]).
#[derive(Debug, Clone, Default)]
pub struct StartOptions {
    pub cwd: String,
    pub envs: HashMap<String, String>,
}

/// A finished command's output and exit code.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct CommandResult {
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
}

/// Describes a background command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcessInfo {
    pub pid: i64,
    pub id: String,
    pub cmd: String,
    pub running: bool,
    /// Set once the command has exited.
    pub exit_code: Option<i32>,
}

/// Runs commands in a sandbox.
#[derive(Clone)]
pub struct Commands {
    sb: Arc<Inner>,
}

fn strings<I>(argv: I) -> Vec<String>
where
    I: IntoIterator,
    I::Item: Into<String>,
{
    argv.into_iter().map(Into::into).collect()
}

fn sh(script: &str, args: &[&str]) -> Vec<String> {
    let mut v = vec!["sh".to_string(), "-c".into(), script.into(), "_".into()];
    v.extend(args.iter().map(|a| a.to_string()));
    v
}

fn quick() -> ExecOptions<'static> {
    ExecOptions {
        no_dir: true,
        timeout: Some(Duration::from_secs(30)),
        ..Default::default()
    }
}

impl Commands {
    pub(crate) fn new(sb: Arc<Inner>) -> Self {
        Commands { sb }
    }

    /// Runs `cmd` with bash (a login shell, so shell syntax works) and waits for it. A non-zero
    /// exit is [`Error::CommandExit`] holding the result; a timeout is [`Error::Timeout`].
    pub async fn run(&self, cmd: &str) -> Result<CommandResult> {
        self.run_with(cmd, RunOptions::default()).await
    }

    /// [`run`](Self::run) with options.
    pub async fn run_with(&self, cmd: &str, o: RunOptions) -> Result<CommandResult> {
        self.run_args_with(["bash", "-lc", cmd], o).await
    }

    /// Runs argv directly, without a shell (like Vercel's runCommand and Modal's exec). Errors as
    /// for [`run`](Self::run).
    pub async fn run_args<I>(&self, argv: I) -> Result<CommandResult>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        self.run_args_with(argv, RunOptions::default()).await
    }

    /// [`run_args`](Self::run_args) with options.
    pub async fn run_args_with<I>(&self, argv: I, mut o: RunOptions) -> Result<CommandResult>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let argv = strings(argv);
        if argv.is_empty() {
            return Err(Error::other("sandbox: empty command"));
        }
        if o.stdin.len() > 16 << 20 {
            return Err(Error::other(format!(
                "sandbox: stdin is {} bytes; the limit is 16 MiB (write a file instead)",
                o.stdin.len()
            )));
        }
        let timeout = o.timeout.unwrap_or(DEFAULT_COMMAND_TIMEOUT);
        let cwd = guest_path(&o.cwd);
        let mut full = sh(RUN_SCRIPT, &[&cwd]);
        full.extend(env_argv(&self.sb.envs, &o.envs, argv));
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut text_out = TextStream::new(o.on_stdout.take());
        let mut text_err = TextStream::new(o.on_stderr.take());
        let res = {
            let mut fo = |b: &[u8]| {
                out.extend_from_slice(b);
                text_out.write(b);
            };
            let mut fe = |b: &[u8]| {
                err.extend_from_slice(b);
                text_err.write(b);
            };
            let eo = ExecOptions {
                no_dir: true,
                stdin: std::mem::take(&mut o.stdin),
                timeout: (!timeout.is_zero()).then_some(timeout),
                stdout: Some(&mut fo),
                stderr: Some(&mut fe),
                ..Default::default()
            };
            self.sb.client.exec(&self.sb.name, &full, eo).await
        };
        text_out.flush();
        text_err.flush();
        let mut r = CommandResult {
            exit_code: 255,
            stdout: lossy(out),
            stderr: lossy(err),
        };
        let res = match res {
            Ok(res) => res,
            Err(Error::OutcomeUnknown { cause, .. }) => {
                return Err(Error::OutcomeUnknown {
                    cause,
                    result: Some(r),
                });
            }
            Err(e) => return Err(wrap(e)),
        };
        r.exit_code = res.exit_code;
        if res.timed_out {
            let detail = format!(
                "command did not finish within {}",
                crate::sandbox::go_duration(timeout)
            );
            return Err(Error::Timeout {
                op: "command".into(),
                detail,
                result: Some(r),
            });
        }
        if res.exit_code == EXIT_CANT_CHDIR && r.stderr.starts_with("sandbox: no such directory:") {
            return Err(Error::NotFound {
                what: "directory".into(),
                name: cwd,
            });
        }
        if res.exit_code != 0 {
            return Err(Error::CommandExit(r));
        }
        Ok(r)
    }

    /// Runs `cmd` with bash in the background and returns at once. The command keeps running after
    /// this call (and across a pause); its output goes to files in the sandbox, read with the
    /// returned handle.
    pub async fn start(&self, cmd: &str) -> Result<CommandHandle> {
        self.start_args_with(["bash", "-lc", cmd], StartOptions::default())
            .await
    }

    /// [`start`](Self::start) with options.
    pub async fn start_with(&self, cmd: &str, o: StartOptions) -> Result<CommandHandle> {
        self.start_args_with(["bash", "-lc", cmd], o).await
    }

    /// [`start`](Self::start) without a shell.
    pub async fn start_args_with<I>(&self, argv: I, o: StartOptions) -> Result<CommandHandle>
    where
        I: IntoIterator,
        I::Item: Into<String>,
    {
        let argv = strings(argv);
        if argv.is_empty() {
            return Err(Error::other("sandbox: empty command"));
        }
        let id = crate::workspaces_random_id();
        let dir = format!("{STATE_DIR}/cmd/{id}");
        let mut full = sh(START_SCRIPT, &[&dir, &guest_path(&o.cwd)]);
        full.extend(env_argv(&self.sb.envs, &o.envs, argv));
        let out = self
            .sb
            .client
            .output(&self.sb.name, &full, quick())
            .await
            .map_err(|e| Error::other(format!("sandbox: starting the command: {}", wrap(e))))?;
        let text = String::from_utf8_lossy(&out);
        let pid: i64 = text.trim().parse().map_err(|_| {
            Error::other(format!(
                "sandbox: starting the command: unexpected output {:?}",
                text
            ))
        })?;
        Ok(CommandHandle::new(self.sb.clone(), pid, id, dir))
    }

    /// The background commands started in this sandbox (running and finished).
    pub async fn list(&self) -> Result<Vec<ProcessInfo>> {
        let out = self
            .sb
            .client
            .output(&self.sb.name, &sh(LIST_SCRIPT, &[STATE_DIR]), quick())
            .await
            .map_err(wrap)?;
        let text = String::from_utf8_lossy(&out);
        let mut ps = Vec::new();
        for line in text.trim_end_matches('\n').split('\n') {
            let f: Vec<&str> = line.splitn(4, '\t').collect();
            if f.len() != 4 {
                continue;
            }
            ps.push(ProcessInfo {
                pid: f[1].parse().unwrap_or(0),
                id: f[0].into(),
                cmd: f[3].into(),
                running: f[2] == "-",
                exit_code: f[2].parse().ok(),
            });
        }
        Ok(ps)
    }

    /// A handle to a background command by pid.
    pub async fn connect(&self, pid: i64) -> Result<CommandHandle> {
        let args = sh(RESOLVE_PID_SCRIPT, &[STATE_DIR, &pid.to_string()]);
        let out = match self.sb.client.output(&self.sb.name, &args, quick()).await {
            Err(Error::Exit { exit_code, .. }) if exit_code == EXIT_NOT_FOUND => {
                return Err(Error::NotFound {
                    what: "command".into(),
                    name: pid.to_string(),
                });
            }
            Err(e) => return Err(wrap(e)),
            Ok(o) => o,
        };
        let dir = String::from_utf8_lossy(&out).trim().to_string();
        let id = dir.rsplit('/').next().unwrap_or_default().to_string();
        Ok(CommandHandle::new(self.sb.clone(), pid, id, dir))
    }

    /// Sends SIGKILL to a background command (and its process group). Returns false if the
    /// command had already exited.
    pub async fn kill(&self, pid: i64) -> Result<bool> {
        self.connect(pid).await?.kill().await
    }
}

/// Prefixes argv with `env K=V ...` for the merged environment (keys sorted).
fn env_argv(
    base: &HashMap<String, String>,
    extra: &HashMap<String, String>,
    argv: Vec<String>,
) -> Vec<String> {
    let mut merged: BTreeMap<&String, &String> = base.iter().collect();
    merged.extend(extra.iter());
    if merged.is_empty() {
        return argv;
    }
    let mut out = vec!["env".to_string()];
    out.extend(merged.into_iter().map(|(k, v)| format!("{k}={v}")));
    out.extend(argv);
    out
}

fn lossy(b: Vec<u8>) -> String {
    String::from_utf8(b).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

/// Passes a byte stream to a callback as strings. Output arrives in arbitrary chunks, so a
/// multi-byte UTF-8 character can be split across two of them; the incomplete tail is held back
/// and delivered with the next chunk. `flush` delivers whatever is still held when the stream
/// ends.
pub(crate) struct TextStream {
    f: Option<TextCallback>,
    pending: Vec<u8>,
}

impl TextStream {
    pub(crate) fn new(f: Option<TextCallback>) -> Self {
        TextStream {
            f,
            pending: Vec::new(),
        }
    }

    pub(crate) fn write(&mut self, p: &[u8]) {
        let Some(f) = self.f.as_mut() else { return };
        if p.is_empty() {
            return;
        }
        let mut b = std::mem::take(&mut self.pending);
        b.extend_from_slice(p);
        let n = complete_utf8(&b);
        if n < b.len() {
            self.pending = b[n..].to_vec();
        }
        if n > 0 {
            f(&String::from_utf8_lossy(&b[..n]));
        }
    }

    pub(crate) fn flush(&mut self) {
        if let Some(f) = self.f.as_mut()
            && !self.pending.is_empty()
        {
            f(&String::from_utf8_lossy(&self.pending));
        }
        self.pending.clear();
    }
}

/// The length of b without a trailing incomplete UTF-8 sequence (a lead byte still waiting for
/// some of its continuation bytes).
fn complete_utf8(b: &[u8]) -> usize {
    for i in 1..=b.len().min(3) {
        let lead = b[b.len() - i];
        if lead & 0xC0 != 0x80 {
            let need = match lead {
                0xC2..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF4 => 4,
                _ => 1,
            };
            if i < need {
                return b.len() - i;
            }
            break;
        }
    }
    b.len()
}

#[derive(Default)]
struct HandleState {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    out_off: i64,
    err_off: i64,
    exit_code: Option<i32>,
}

/// A background command.
#[derive(Clone)]
pub struct CommandHandle {
    /// The pid of the command's process group leader in the sandbox.
    pub pid: i64,
    /// Names the command's state directory.
    pub id: String,
    sb: Arc<Inner>,
    dir: String,
    // Polls take turns: each one reads from the offsets the one before it left, so overlapping
    // poll and wait calls never fetch (and append) the same bytes twice.
    poll_lock: Arc<tokio::sync::Mutex<()>>,
    state: Arc<Mutex<HandleState>>,
}

impl std::fmt::Debug for CommandHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CommandHandle")
            .field("pid", &self.pid)
            .field("id", &self.id)
            .finish_non_exhaustive()
    }
}

/// Configures [`CommandHandle::wait_with`].
#[derive(Default)]
pub struct WaitOptions {
    /// Receive the output as wait polls for it (about every 2 seconds while the command runs),
    /// starting with any output an earlier poll already fetched. Output a concurrent poll fetches
    /// is delivered too, once and in order.
    pub on_stdout: Option<TextCallback>,
    pub on_stderr: Option<TextCallback>,
}

impl CommandHandle {
    fn new(sb: Arc<Inner>, pid: i64, id: String, dir: String) -> Self {
        CommandHandle {
            pid,
            id,
            sb,
            dir,
            poll_lock: Default::default(),
            state: Default::default(),
        }
    }

    /// The standard output received so far.
    pub fn stdout(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().unwrap().stdout).into_owned()
    }

    /// The standard error received so far.
    pub fn stderr(&self) -> String {
        String::from_utf8_lossy(&self.state.lock().unwrap().stderr).into_owned()
    }

    /// The exit code once known (after wait or a poll that saw the exit).
    pub fn exit_code(&self) -> Option<i32> {
        self.state.lock().unwrap().exit_code
    }

    /// Fetches new output and returns the exit code, or `None` while the command runs.
    pub async fn poll(&self) -> Result<Option<i32>> {
        self.poll_for(0).await
    }

    /// Waits for the command to exit. A non-zero exit is [`Error::CommandExit`] with the result.
    pub async fn wait(&self) -> Result<CommandResult> {
        self.wait_with(WaitOptions::default()).await
    }

    /// [`wait`](Self::wait), delivering output to callbacks as it arrives.
    pub async fn wait_with(&self, o: WaitOptions) -> Result<CommandResult> {
        // The callbacks are fed from the handle's accumulated output, from where this wait last
        // left off: first whatever arrived before (via poll), then everything new, whichever call
        // fetched it.
        let mut text_out = TextStream::new(o.on_stdout);
        let mut text_err = TextStream::new(o.on_stderr);
        let (mut sent_out, mut sent_err) = (0usize, 0usize);
        let mut deliver = |text_out: &mut TextStream, text_err: &mut TextStream| {
            let (new_out, new_err) = {
                let st = self.state.lock().unwrap();
                let r = (
                    st.stdout[sent_out..].to_vec(),
                    st.stderr[sent_err..].to_vec(),
                );
                sent_out = st.stdout.len();
                sent_err = st.stderr.len();
                r
            };
            text_out.write(&new_out);
            text_err.write(&new_err);
        };
        deliver(&mut text_out, &mut text_err);
        loop {
            let code = self.poll_for(20).await;
            deliver(&mut text_out, &mut text_err);
            if let Some(code) = code? {
                text_out.flush();
                text_err.flush();
                let r = CommandResult {
                    exit_code: code,
                    stdout: self.stdout(),
                    stderr: self.stderr(),
                };
                if code != 0 {
                    return Err(Error::CommandExit(r));
                }
                return Ok(r);
            }
        }
    }

    /// Sends SIGKILL to the command's process group. Returns false if the command had already
    /// exited. The exit code becomes 137.
    pub async fn kill(&self) -> Result<bool> {
        let args = sh(KILL_SCRIPT, &[&self.dir, "KILL", "137"]);
        match self.sb.client.output(&self.sb.name, &args, quick()).await {
            Err(Error::Exit { exit_code, .. }) if exit_code == EXIT_NOT_FOUND => {
                Err(Error::NotFound {
                    what: "command".into(),
                    name: self.pid.to_string(),
                })
            }
            Err(e) => Err(wrap(e)),
            Ok(out) => Ok(String::from_utf8_lossy(&out).trim() == "1"),
        }
    }

    async fn poll_for(&self, wait_tenths: u32) -> Result<Option<i32>> {
        let _turn = self.poll_lock.lock().await;
        let (out_off, err_off) = {
            let st = self.state.lock().unwrap();
            if st.exit_code.is_some() {
                return Ok(st.exit_code);
            }
            (st.out_off, st.err_off)
        };
        let args = sh(
            POLL_SCRIPT,
            &[
                &self.dir,
                &out_off.to_string(),
                &err_off.to_string(),
                &wait_tenths.to_string(),
                &MAX_POLL_CHUNK.to_string(),
            ],
        );
        let o = ExecOptions {
            no_dir: true,
            timeout: Some(Duration::from_secs(60)),
            ..Default::default()
        };
        let out = match self.sb.client.output(&self.sb.name, &args, o).await {
            Err(Error::Exit { exit_code, .. }) if exit_code == EXIT_NOT_FOUND => {
                return Err(Error::NotFound {
                    what: "command".into(),
                    name: self.pid.to_string(),
                });
            }
            Err(e) => return Err(wrap(e)),
            Ok(o) => o,
        };
        let p = parse_poll(&out, out_off, err_off)?;
        let mut st = self.state.lock().unwrap();
        st.stdout.extend_from_slice(&p.stdout);
        st.stderr.extend_from_slice(&p.stderr);
        st.out_off = p.out_end;
        st.err_off = p.err_end;
        // The exit code counts only once all output is in (a capped poll may leave more).
        if p.exit.is_some() && !p.truncated {
            st.exit_code = p.exit;
        }
        Ok(st.exit_code)
    }
}

#[derive(Debug, PartialEq)]
pub(crate) struct PollResult {
    pub exit: Option<i32>,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    pub out_end: i64,
    pub err_end: i64,
    pub truncated: bool,
}

/// Decodes the poll script's output: "<exit|-> <outEnd> <errEnd>\n" + stdout bytes + stderr bytes.
pub(crate) fn parse_poll(out: &[u8], out_off: i64, err_off: i64) -> Result<PollResult> {
    let Some(nl) = out.iter().position(|b| *b == b'\n') else {
        return Err(Error::other(format!(
            "sandbox: malformed poll output {:?}",
            String::from_utf8_lossy(out)
        )));
    };
    let head = String::from_utf8_lossy(&out[..nl]).into_owned();
    let malformed = || Error::other(format!("sandbox: malformed poll header {head:?}"));
    let f: Vec<&str> = head.split_whitespace().collect();
    if f.len() != 3 {
        return Err(malformed());
    }
    let exit = if f[0] == "-" {
        None
    } else {
        Some(
            f[0].parse::<i32>()
                .map_err(|_| Error::other(format!("sandbox: malformed exit code {:?}", f[0])))?,
        )
    };
    let (Ok(so), Ok(se)) = (f[1].parse::<i64>(), f[2].parse::<i64>()) else {
        return Err(malformed());
    };
    let body = &out[nl + 1..];
    let (mut n_out, mut n_err) = ((so - out_off).max(0), (se - err_off).max(0));
    let mut truncated = false;
    if (body.len() as i64) < n_out + n_err {
        // Output shrank or was cut short; take what arrived.
        n_out = n_out.min(body.len() as i64);
        n_err = body.len() as i64 - n_out;
        truncated = true;
    }
    let (no, ne) = (n_out as usize, n_err as usize);
    if n_out >= MAX_POLL_CHUNK as i64 || n_err >= MAX_POLL_CHUNK as i64 {
        truncated = true;
    }
    Ok(PollResult {
        exit,
        stdout: body[..no].to_vec(),
        stderr: body[no..no + ne].to_vec(),
        out_end: out_off + n_out,
        err_end: err_off + n_err,
        truncated,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_poll_vectors() {
        let p = parse_poll(b"- 5 3\nhelloerr", 0, 0).unwrap();
        assert_eq!(
            (
                p.exit,
                p.stdout.as_slice(),
                p.stderr.as_slice(),
                p.out_end,
                p.err_end,
                p.truncated
            ),
            (None, &b"hello"[..], &b"err"[..], 5, 3, false)
        );
        let p = parse_poll(b"7 8 3\nxyz", 5, 3).unwrap();
        assert_eq!(
            (
                p.exit,
                p.stdout.as_slice(),
                p.stderr.as_slice(),
                p.out_end,
                p.err_end
            ),
            (Some(7), &b"xyz"[..], &b""[..], 8, 3)
        );
        let p = parse_poll(b"0 10 0\nabc", 0, 0).unwrap();
        assert!(p.truncated);
        assert_eq!((p.stdout.as_slice(), p.out_end), (&b"abc"[..], 3));
        assert!(parse_poll(b"nonsense", 0, 0).is_err());
        assert!(parse_poll(b"x y\n", 0, 0).is_err());
    }

    #[test]
    fn text_stream_holds_back_a_split_character() {
        let got = Arc::new(Mutex::new(Vec::<String>::new()));
        let g = got.clone();
        let mut t = TextStream::new(Some(Box::new(move |s: &str| {
            g.lock().unwrap().push(s.to_string())
        })));
        let euro = "€".as_bytes();
        t.write(&[b'a', euro[0]]);
        t.write(&euro[1..]);
        t.write(&[0xE2]);
        t.flush();
        let got = got.lock().unwrap();
        assert_eq!(got[0], "a");
        assert_eq!(got[1], "€");
        assert_eq!(got.len(), 3);
    }

    #[test]
    fn env_argv_sorts_and_overrides() {
        let base = HashMap::from([
            ("B".to_string(), "1".to_string()),
            ("A".to_string(), "x".to_string()),
        ]);
        let extra = HashMap::from([("A".to_string(), "y".to_string())]);
        assert_eq!(
            env_argv(&base, &extra, vec!["ls".into()]),
            ["env", "A=y", "B=1", "ls"]
        );
        assert_eq!(
            env_argv(&HashMap::new(), &HashMap::new(), vec!["ls".into()]),
            ["ls"]
        );
    }
}
