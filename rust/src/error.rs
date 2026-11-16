//! The crate's error type.
//!
//! Every fallible call returns [`Error`]. Match on its variants the way the Go SDK's callers use
//! `errors.As` and `errors.Is`:
//!
//! ```no_run
//! # async fn f(sbx: &chainguard_sandbox::Sandbox) {
//! use chainguard_sandbox::Error;
//! match sbx.commands.run("false").await {
//!     Ok(r) => println!("{}", r.stdout),
//!     Err(Error::CommandExit(r)) => println!("exited {}: {}", r.exit_code, r.stderr),
//!     Err(Error::Busy { .. }) => println!("another client is attached"),
//!     Err(e) => println!("{e}"),
//! }
//! # }
//! ```

use std::fmt;
use std::sync::Arc;

use crate::commands::CommandResult;
use crate::workspaces::{Organization, PlatformError, TermsDocument};

/// `Result` with this crate's [`Error`].
pub type Result<T, E = Error> = std::result::Result<T, E>;

/// Everything that can go wrong. The variants mirror the typed errors of the Go, Python and
/// TypeScript SDKs (`CommandExitError`, `NotFoundError`, `ErrBusy`, ...).
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    // ---- the sandbox API -------------------------------------------------------------------
    /// A command exited with a non-zero code. Holds the full result.
    #[error("{}", command_exit_message(.0))]
    CommandExit(CommandResult),

    /// A file, directory, path, background command or sandbox does not exist. `what` is
    /// "file", "directory", "path", "command", "sandbox" or "session".
    #[error("sandbox: {what} not found: {name}")]
    NotFound { what: String, name: String },

    /// The path exists but is the wrong kind; `reason` is "is a directory" or "not a directory".
    #[error("sandbox: {path}: {reason}")]
    InvalidPath { path: String, reason: String },

    /// A deadline passed. `op` is "command", "start" or "kill". For a command, `result` holds the
    /// output received before it was stopped.
    #[error("{}", timeout_message(.op, .detail))]
    Timeout {
        op: String,
        detail: String,
        result: Option<CommandResult>,
    },

    /// The sandbox is held by another controller, typically an interactive shell attached with
    /// chainctl. Commands can't run until it detaches.
    #[error(
        "sandbox: busy: another client is attached to this sandbox (an interactive shell); commands run once it detaches ({cause})"
    )]
    Busy { cause: Box<Error> },

    /// The organization is at its session limit. Kill a sandbox first.
    #[error(
        "sandbox: session limit reached (one sandbox per user during the Workspaces beta); kill an existing sandbox first ({message})"
    )]
    LimitReached { message: String },

    /// The connection ended, or failed, before the command reported an exit status. It may or
    /// may not have run, in part or in full. `cause` is the transport error, when there was one;
    /// `result` holds the output received before the stream ended.
    #[error("sandbox: the command's outcome is unknown (the connection ended before it reported an exit status){}", cause_suffix(.cause))]
    OutcomeUnknown {
        cause: Option<Box<Error>>,
        result: Option<CommandResult>,
    },

    /// Workspaces is not turned on for the organization. An owner can turn it on with
    /// [`enable_workspaces`](crate::enable_workspaces) or `chainguard-sandbox enable`.
    #[error(
        "sandbox: Workspaces is not turned on for this organization; an owner can turn it on with enable_workspaces or `chainguard-sandbox enable`: {cause}"
    )]
    NotEnabled { cause: Box<Error> },

    /// The organization has not accepted the legal documents Workspaces is offered under.
    /// [`enable_workspaces`](crate::enable_workspaces) accepts them when `accept_terms` is set.
    #[error("sandbox: the organization has not accepted the terms Workspaces is offered under: {}", terms_list(.missing))]
    TermsNotAccepted { missing: Vec<TermsDocument> },

    /// The organization has no GitHub account linked, which the service requires before it turns
    /// Workspaces on, and enabling was not allowed to open a browser to link one.
    #[error(
        "sandbox: organization {organization} has to link a GitHub account before Workspaces can be turned on; run enable_workspaces where it can open a browser, or `chainguard-sandbox enable`"
    )]
    GitHubLinkRequired { organization: String },

    /// The Chainguard GitHub App is not installed on the GitHub account being linked. Install it
    /// at `install_url`, then enable again.
    #[error(
        "sandbox: the Chainguard GitHub App is not installed on that GitHub account; install it at {install_url} and try again ({cause})"
    )]
    GitHubAppNotInstalled {
        install_url: String,
        cause: Box<Error>,
    },

    // ---- the lower layer -------------------------------------------------------------------
    /// The sandbox plane answered a call with a gRPC error.
    #[error("rpc error: code = {} desc = {}", code_name(.0.code()), .0.message())]
    Rpc(tonic::Status),

    /// The platform (account) API answered with an error. `context` says which call.
    #[error("{context}: {error}")]
    Platform {
        context: String,
        error: PlatformError,
    },

    /// An HTTP call answered with an error status.
    #[error("{context}: HTTP {status}: {message}")]
    Http {
        context: String,
        status: u16,
        message: String,
    },

    /// An HTTP or gRPC connection could not be made or broke off.
    #[error("{context}: {message}")]
    Transport { context: String, message: String },

    /// There is no saved login and the token source may not open a browser.
    #[error("workspaces: not logged in (run `chainguard-sandbox login`, or use a BrowserLoginToken that may open a browser){}", detail_suffix(.detail))]
    LoginRequired { detail: Option<String> },

    /// The saved login was refused (expired, revoked or already spent); log in again.
    #[error("workspaces: the saved login has expired or was revoked: {detail}")]
    LoginExpired { detail: String },

    /// The issuer knows no Chainguard account for the person who logged in.
    #[error(
        "workspaces: login: no Chainguard account for this login (sign up for Workspaces at https://console.chainguard.dev/workspaces/signup, or register with `chainctl auth login`)"
    )]
    AccountNotFound,

    /// The identity belongs to no Chainguard organization.
    #[error("workspaces: your identity belongs to no Chainguard organization")]
    NoOrganization,

    /// The identity belongs to several organizations; choose one explicitly.
    #[error("{}", ambiguous_orgs(.organizations))]
    AmbiguousOrganization { organizations: Vec<Organization> },

    /// No organization was set and none could be detected. `detection` says why detection
    /// didn't pick one.
    #[error("{}", no_instance(.detection))]
    NoInstance { detection: Option<String> },

    /// A session reference matched more than one session.
    #[error("workspaces: reference matches more than one session: {reference:?}")]
    Ambiguous { reference: String },

    /// A lower-level helper (`output`, `shell`, `write_file`) ran a command that exited non-zero
    /// or timed out.
    #[error("{}", exit_message(*.exit_code, *.timed_out, .stderr))]
    Exit {
        exit_code: i32,
        timed_out: bool,
        stdout: Vec<u8>,
        stderr: Vec<u8>,
    },

    /// Another client attached to the session and took it over during the first attach.
    #[error("workspaces: another client took over the session")]
    Detached,

    /// A local file operation failed.
    #[error("{context}: {source}")]
    Io {
        context: String,
        source: Arc<std::io::Error>,
    },

    /// Anything else, described by its message.
    #[error("{0}")]
    Other(String),
}

impl Error {
    /// The gRPC status code behind the error, looking through `Busy`, `NotEnabled`,
    /// `OutcomeUnknown` and `GitHubAppNotInstalled` (like Go's `status.Code(err)`).
    pub fn grpc_code(&self) -> Option<tonic::Code> {
        match self {
            Error::Rpc(s) => Some(s.code()),
            Error::Busy { cause }
            | Error::NotEnabled { cause }
            | Error::GitHubAppNotInstalled { cause, .. } => cause.grpc_code(),
            Error::OutcomeUnknown { cause: Some(c), .. } => c.grpc_code(),
            _ => None,
        }
    }

    /// The platform API's error, if this is (or wraps) one.
    pub fn platform_error(&self) -> Option<&PlatformError> {
        match self {
            Error::Platform { error, .. } => Some(error),
            Error::GitHubAppNotInstalled { cause, .. } | Error::NotEnabled { cause } => {
                cause.platform_error()
            }
            _ => None,
        }
    }

    /// The command result an error carries: a non-zero exit, a timeout, or the partial output
    /// of a command whose outcome is unknown.
    pub fn command_result(&self) -> Option<&CommandResult> {
        match self {
            Error::CommandExit(r) => Some(r),
            Error::Timeout { result, .. } | Error::OutcomeUnknown { result, .. } => result.as_ref(),
            _ => None,
        }
    }

    pub(crate) fn other(msg: impl Into<String>) -> Error {
        Error::Other(msg.into())
    }

    pub(crate) fn io(context: impl Into<String>, e: std::io::Error) -> Error {
        Error::Io {
            context: context.into(),
            source: Arc::new(e),
        }
    }

    pub(crate) fn transport(context: impl Into<String>, e: impl fmt::Display) -> Error {
        Error::Transport {
            context: context.into(),
            message: e.to_string(),
        }
    }
}

impl From<tonic::Status> for Error {
    fn from(s: tonic::Status) -> Self {
        Error::Rpc(s)
    }
}

fn command_exit_message(r: &CommandResult) -> String {
    let msg = r.stderr.trim();
    let msg = last_bytes(msg, 500);
    if msg.is_empty() {
        format!("sandbox: command exited with code {}", r.exit_code)
    } else {
        format!("sandbox: command exited with code {}: {msg}", r.exit_code)
    }
}

/// The last `n` bytes of s, moved forward to a character boundary.
fn last_bytes(s: &str, n: usize) -> &str {
    if s.len() <= n {
        return s;
    }
    let mut i = s.len() - n;
    while !s.is_char_boundary(i) {
        i += 1;
    }
    &s[i..]
}

fn timeout_message(op: &str, detail: &str) -> String {
    if detail.is_empty() {
        format!("sandbox: {op} timed out")
    } else {
        format!("sandbox: timeout: {detail}")
    }
}

fn cause_suffix(c: &Option<Box<Error>>) -> String {
    c.as_ref().map(|c| format!(": {c}")).unwrap_or_default()
}

fn detail_suffix(d: &Option<String>) -> String {
    d.as_ref().map(|d| format!(": {d}")).unwrap_or_default()
}

fn terms_list(docs: &[TermsDocument]) -> String {
    docs.iter()
        .map(|d| format!("{} ({})", d.label, d.url))
        .collect::<Vec<_>>()
        .join("; ")
}

fn ambiguous_orgs(orgs: &[Organization]) -> String {
    let mut s = format!(
        "workspaces: you belong to several organizations; choose one with the instance option or ${}:",
        crate::workspaces::ENV_INSTANCE
    );
    for o in orgs {
        s.push_str(&format!("\n  {}  {}", o.id, o.name));
    }
    s
}

fn no_instance(detection: &Option<String>) -> String {
    let mut msg = format!(
        "workspaces: no organization configured. Set one of:\n  \
         - the instance option (Go: Options.Instance, Python: instance=, TypeScript: instance) to the organization ID\n  \
         - ${}=<organization ID>\n  \
         - or let it be auto-detected when you belong to exactly one organization\n\
         List your organization IDs with `chainguard-sandbox orgs` or `chainctl iam organizations list`.",
        crate::workspaces::ENV_INSTANCE
    );
    if let Some(d) = detection {
        msg.push_str("\nAuto-detection: ");
        msg.push_str(d);
    }
    msg
}

fn exit_message(code: i32, timed_out: bool, stderr: &[u8]) -> String {
    let msg = String::from_utf8_lossy(stderr);
    let msg = msg.trim();
    if timed_out {
        format!("command timed out: {msg}")
    } else {
        format!("command exited {code}: {msg}")
    }
}

/// The gRPC code's name as Go prints it ("Unavailable", "NotFound", ...).
pub(crate) fn code_name(c: tonic::Code) -> &'static str {
    use tonic::Code::*;
    match c {
        Ok => "OK",
        Cancelled => "Canceled",
        Unknown => "Unknown",
        InvalidArgument => "InvalidArgument",
        DeadlineExceeded => "DeadlineExceeded",
        NotFound => "NotFound",
        AlreadyExists => "AlreadyExists",
        PermissionDenied => "PermissionDenied",
        ResourceExhausted => "ResourceExhausted",
        FailedPrecondition => "FailedPrecondition",
        Aborted => "Aborted",
        OutOfRange => "OutOfRange",
        Unimplemented => "Unimplemented",
        Internal => "Internal",
        Unavailable => "Unavailable",
        DataLoss => "DataLoss",
        Unauthenticated => "Unauthenticated",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn command_exit_message_keeps_the_last_500_bytes_on_a_boundary() {
        let long = format!("{}€end", "x".repeat(600));
        let e = Error::CommandExit(CommandResult {
            exit_code: 2,
            stdout: String::new(),
            stderr: long,
        });
        let m = e.to_string();
        assert!(m.starts_with("sandbox: command exited with code 2: "));
        assert!(m.ends_with("€end"));
        let e = Error::CommandExit(CommandResult {
            exit_code: 3,
            stdout: String::new(),
            stderr: " \n".into(),
        });
        assert_eq!(e.to_string(), "sandbox: command exited with code 3");
    }

    #[test]
    fn grpc_code_looks_through_wrappers() {
        let e = Error::OutcomeUnknown {
            cause: Some(Box::new(Error::Rpc(tonic::Status::unavailable("x")))),
            result: None,
        };
        assert_eq!(e.grpc_code(), Some(tonic::Code::Unavailable));
        assert!(
            e.to_string()
                .contains("rpc error: code = Unavailable desc = x")
        );
    }
}
