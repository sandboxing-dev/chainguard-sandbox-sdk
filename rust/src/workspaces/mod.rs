//! The lower-level client under the sandbox API: the parts of the Chainguard Workspaces Sessions
//! API (`chainguard.sandbox.runtime.session.v1.Sessions`, the service `chainctl develop` uses) that
//! the sandbox API needs, plus login and token handling. It is compatible with chainctl 0.2.398.
//!
//! It is unofficial: not published or supported by Chainguard. Most programs should use the
//! [`Sandbox`](crate::Sandbox) API instead.
//!
//! ```no_run
//! # async fn f() -> chainguard_sandbox::Result<()> {
//! use chainguard_sandbox::workspaces::{Client, ExecOptions, ListOptions, Options};
//! let c = Client::new(Options { instance: "XXXXXX".into(), ..Default::default() }).await?;
//! let sessions = c.list(&ListOptions::default()).await?;
//! let argv = ["go", "test", "./..."].map(String::from);
//! let res = c.exec(&sessions[0].name, &argv, ExecOptions::default()).await?;
//! println!("exit {}", res.exit_code);
//! # Ok(()) }
//! ```

mod auth;
mod client;
mod exchange;
mod exec;
mod files;
mod firstattach;
mod http;
mod login;
mod loopback;
mod orgs;
mod repourl;
mod sessions;
mod setup;

/// The generated protobuf messages and gRPC client and server for the SDK's subset of the
/// Sessions API.
#[allow(clippy::all, missing_docs)]
pub mod pb {
    include!("gen/chainguard.sandbox.runtime.session.v1.rs");
}

pub use auth::{
    BoxFuture, ChainctlToken, SharedTokenSource, StaticToken, TokenSource, cached,
    default_token_source, token_fn,
};
pub use client::{Client, DEFAULT_USER_AGENT, Options};
pub use exchange::{
    IdTokenFn, IdentityExchange, env_id_token, file_id_token, github_actions_id_token,
};
pub use exec::{ExecOptions, ExecResult, OutputSink, guest_path, is_guest_held};
pub use files::{SkipFn, tar_dir, tar_gz_dir};
pub use firstattach::FIRST_ATTACH_TIMEOUT;
pub use login::{
    BrowserLoginToken, Claims, FileLoginStore, LoginLock, LoginOptions, LoginResult, LoginStore,
    MessageSink, UrlOpener, browser_login, default_login_audiences, default_login_file,
    login_callback_server, logout, open_browser, refresh_login, token_claims,
};
pub use orgs::{Organization, detect_instance, list_organizations, platform_token_source};
pub use repourl::normalize_github_url;
pub use sessions::{
    DEFAULT_ENVIRONMENT, ListOptions, instance_name, is_session_name, repository_session,
    repository_url, workspace_session,
};
pub use setup::{
    DEFAULT_GITHUB_OAUTH_PORT, ENABLE_TERMS, Entitlement, GITHUB_APP_INSTALL_URL,
    GitHubAuthorization, GitHubAuthorizeOptions, GitHubOAuthStart, LinkedGitHub, PlatformError,
    TermsDocument, WORKSPACES_TERMS, accept_terms, accepted_terms, authorize_github,
    begin_github_oauth, enable_sandbox, link_github_organization, linked_github_organizations,
    missing_workspaces_terms, terms_document_info, workspaces_entitlement,
};

pub(crate) use exec::clean_path;
pub(crate) use login::random_hex;

/// The plane's session resource.
pub type Session = pb::Session;

/// The sandbox plane chainctl dials (overridable with `$CHAINCTL_SESSIONS_URL`).
pub const DEFAULT_URL: &str = "https://sessions.chainbox.dev";
/// The Chainguard token audience the plane accepts.
pub const DEFAULT_AUDIENCE: &str = "https://sessions.chainbox.dev";
/// Chainguard's security token service (STS).
pub const DEFAULT_ISSUER: &str = "https://issuer.enforce.dev";
/// Chainguard's account (IAM) API.
pub const DEFAULT_PLATFORM_URL: &str = "https://console-api.enforce.dev";
/// The audience of tokens for the account API (used to list your organizations).
pub const PLATFORM_AUDIENCE: &str = "https://console-api.enforce.dev";
/// Where the repository lives inside the guest.
pub const DEFAULT_WORK_DIR: &str = "/work";
/// The stdin size limit chainctl documents for exec.
pub const MAX_EXEC_STDIN: usize = 16 << 20;
/// The plane's URL (chainctl's override).
pub const ENV_SESSIONS_URL: &str = "CHAINCTL_SESSIONS_URL";
/// The organization ID (chainctl's override).
pub const ENV_INSTANCE: &str = "CHAINCTL_SANDBOX_INSTANCE";
/// A static sessions token (chainctl's override).
pub const ENV_AUTH_TOKEN: &str = "CHAINCTL_AUTH_TOKEN";
/// Turns off organization auto-detection when set to "1".
pub const ENV_NO_DETECT_INSTANCE: &str = "CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE";
