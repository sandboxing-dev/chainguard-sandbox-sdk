//! Browser login, without chainctl.
//!
//! The login flow is adapted from chainguard.dev/sdk/auth/login (v0.1.50, Copyright 2023
//! Chainguard, Inc., Apache-2.0): a callback server on a random localhost port, and the issuer's
//! /oauth page opened in the browser with `redirect=<callback>`. The issuer redirects back with
//! ?token=…&refresh_token=…. The request parameters match what chainctl 0.2.398 sends.
//!
//! The saved login is shared with the Go, Python and TypeScript SDKs: same file, same format,
//! same lock.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::oneshot;

use super::auth::{BoxFuture, TokenSource, jwt_claims_json};
use super::http;
use super::loopback::{LoopbackReply, LoopbackServer};
use super::{DEFAULT_AUDIENCE, DEFAULT_ISSUER, PLATFORM_AUDIENCE};
use crate::{Error, Result};

crate::opaque_debug!(LoginOptions, LoginLock, BrowserLoginToken);

/// Receives progress messages ("open this URL ..."). The default writes to stderr.
pub type MessageSink = Arc<dyn Fn(&str) + Send + Sync>;

/// Opens a URL in a browser; returns false if it couldn't (the URL is then printed instead).
pub type UrlOpener = Arc<dyn Fn(&str) -> bool + Send + Sync>;

pub(crate) fn say(out: &Option<MessageSink>, msg: &str) {
    match out {
        Some(f) => f(msg),
        None => eprintln!("{msg}"),
    }
}

/// Configures [`browser_login`].
#[derive(Clone, Default)]
pub struct LoginOptions {
    /// Defaults to [`DEFAULT_ISSUER`].
    pub issuer: String,
    /// What the login's tokens may be minted for. Empty means [`default_login_audiences`]: the
    /// sessions service plus Chainguard's account API.
    pub audience: Vec<String>,
    /// Preselects a social login: "github", "google-oauth2" or "gitlab". Empty lets the page ask.
    pub connection: String,
    /// Defaults to "auth0", as chainctl sends. Ignored with `identity_provider` or `org_name`.
    pub client_id: String,
    /// The ID of an organization's custom identity provider.
    pub identity_provider: String,
    /// The name of an organization with a custom identity provider (verified). At most one of
    /// `identity_provider` and `org_name`.
    pub org_name: String,
    /// An assumable identity to log in as (optional).
    pub identity: String,
    /// Skips asking for a refresh token. Without one, every token after the first needs another
    /// browser login.
    pub no_refresh_token: bool,
    /// Opens the login page. Defaults to [`open_browser`].
    pub open_url: Option<UrlOpener>,
    /// Receives the "open this URL" message. Defaults to stderr.
    pub out: Option<MessageSink>,
}

/// What the issuer hands back.
#[derive(Clone, Default)]
pub struct LoginResult {
    /// A Chainguard token for the requested audiences.
    pub token: String,
    /// Mints further tokens (see [`refresh_login`]); empty with `no_refresh_token`.
    pub refresh_token: String,
}

impl std::fmt::Debug for LoginResult {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("LoginResult { .. }")
    }
}

/// Runs the interactive login: opens the issuer's login page in a browser and waits for the
/// redirect back to a localhost callback. Bound it with `tokio::time::timeout`.
pub async fn browser_login(o: LoginOptions) -> Result<LoginResult> {
    let issuer = if o.issuer.is_empty() {
        DEFAULT_ISSUER.to_string()
    } else {
        o.issuer.clone()
    };
    let aud = if o.audience.is_empty() {
        default_login_audiences()
    } else {
        o.audience.clone()
    };
    if !o.identity_provider.is_empty() && !o.org_name.is_empty() {
        return Err(Error::other(
            "workspaces: login: set at most one of identity_provider and org_name",
        ));
    }
    let srv = LoginServer::start().await.map_err(|e| {
        Error::io(
            "workspaces: login: starting the localhost callback server",
            e,
        )
    })?;

    let mut p: Vec<(&str, &str)> = Vec::new();
    let cid;
    if !o.identity_provider.is_empty() {
        p.push(("idp_id", &o.identity_provider));
    } else if !o.org_name.is_empty() {
        p.push(("idp_id", &o.org_name)); // the issuer takes a verified org name here too
    } else {
        cid = if o.client_id.is_empty() {
            "auth0".to_string()
        } else {
            o.client_id.clone()
        };
        p.push(("client_id", &cid));
        if !o.connection.is_empty() {
            p.push(("connection", &o.connection));
        }
    }
    let aud_joined = aud.join(",");
    p.push(("audience", &aud_joined));
    if !o.no_refresh_token {
        p.push(("create_refresh_token", "true"));
        p.push(("refresh_token_rotation", "true"));
    }
    if !o.identity.is_empty() {
        p.push(("identity", &o.identity));
    }
    p.push(("skip_registration", "true"));
    p.push(("exit", "redirect"));
    p.push(("redirect", &srv.callback_url));
    let u = format!("{}/oauth?{}", issuer.trim_end_matches('/'), http::query(&p));

    let opened = match &o.open_url {
        Some(f) => f(&u),
        None => open_browser(&u),
    };
    if opened {
        say(
            &o.out,
            &format!("Opened a browser to log in to Chainguard. If it didn't open, visit:\n  {u}"),
        );
    } else {
        say(
            &o.out,
            &format!("Open this URL in a browser to log in to Chainguard:\n  {u}"),
        );
    }
    let LoginServer {
        result,
        page_served,
        server,
        ..
    } = srv;
    let outcome = result.await.unwrap_or_else(|_| {
        Err(Error::other(
            "workspaces: login: the callback server stopped",
        ))
    });
    if outcome.is_ok() {
        // Keep serving until the browser has loaded the success page (the callback redirects
        // there), so it doesn't show an error.
        let _ = tokio::time::timeout(Duration::from_secs(5), page_served).await;
    }
    server.stop();
    outcome
}

struct LoginServer {
    server: LoopbackServer,
    callback_url: String,
    result: oneshot::Receiver<Result<LoginResult>>,
    page_served: oneshot::Receiver<()>,
}

#[derive(Default)]
struct LoginServerState {
    result: Option<oneshot::Sender<Result<LoginResult>>>,
    page: Option<oneshot::Sender<()>>,
    got: bool,
}

impl LoginServer {
    async fn start() -> std::io::Result<LoginServer> {
        let (rtx, rrx) = oneshot::channel();
        let (ptx, prx) = oneshot::channel();
        let st = Arc::new(Mutex::new(LoginServerState {
            result: Some(rtx),
            page: Some(ptx),
            got: false,
        }));
        let server = LoopbackServer::bind(0, move |req| login_handler(&st, req)).await?;
        // The issuer only redirects to localhost; the host name is what chainctl sends.
        let callback_url = format!(
            "http://localhost:{}/callback?token=true&error=true",
            server.port
        );
        Ok(LoginServer {
            server,
            callback_url,
            result: rrx,
            page_served: prx,
        })
    }
}

/// The callback server's handler, exported for tests through [`login_callback_server`].
fn login_handler(
    st: &Mutex<LoginServerState>,
    req: super::loopback::LoopbackRequest,
) -> LoopbackReply {
    let headers = [
        ("Cache-Control", "no-store"),
        ("Referrer-Policy", "no-referrer"),
    ];
    if req.method != "GET" {
        return LoopbackReply::text(405, &headers, "method not allowed");
    }
    let finish = |r: Result<LoginResult>| {
        let mut s = st.lock().unwrap();
        if let Some(tx) = s.result.take() {
            s.got = true;
            let _ = tx.send(r);
        }
    };
    match req.path.as_str() {
        "/callback" => {
            let tok = req.query.get("token").cloned().unwrap_or_default();
            if tok.is_empty() {
                let code: i64 = req
                    .query
                    .get("status_code")
                    .and_then(|c| c.parse().ok())
                    .unwrap_or(0);
                if code == 404 {
                    finish(Err(Error::AccountNotFound));
                    return LoopbackReply::text(
                        404,
                        &headers,
                        "No Chainguard account was found for this login.",
                    );
                }
                let msg = req
                    .query
                    .get("error")
                    .filter(|m| !m.is_empty())
                    .cloned()
                    .unwrap_or_else(|| "login failed".into());
                finish(Err(Error::other(format!(
                    "workspaces: login: the issuer reported an error (status {code}): {msg}"
                ))));
                return LoopbackReply::text(400, &headers, format!("Login failed: {msg}"));
            }
            finish(Ok(LoginResult {
                token: tok,
                refresh_token: req.query.get("refresh_token").cloned().unwrap_or_default(),
            }));
            // Redirect so the tokens leave the address bar and history entry.
            let mut r = LoopbackReply::text(303, &headers, "");
            r.headers.push(("Location", "/".into()));
            r
        }
        "/" => {
            let mut s = st.lock().unwrap();
            if !s.got {
                return LoopbackReply::text(404, &headers, "404 page not found");
            }
            if let Some(tx) = s.page.take() {
                let _ = tx.send(());
            }
            let mut h: Vec<(&'static str, &str)> = headers.to_vec();
            h.push(("Content-Type", "text/html; charset=utf-8"));
            LoopbackReply::text(200, &h, LOGIN_DONE_PAGE)
        }
        _ => LoopbackReply::text(404, &headers, "404 page not found"),
    }
}

const LOGIN_DONE_PAGE: &str = r#"<!doctype html><html><head><meta charset="utf-8"><title>Login complete</title>
<style>:root{color-scheme:light dark;font-family:system-ui,sans-serif}body{display:grid;place-items:center;height:100vh;margin:0}</style>
</head><body><div><h1>Chainguard login complete</h1><p>You can close this tab and return to your program.</p></div></body></html>"#;

/// Opens `u` in the default browser (open on macOS, xdg-open on Linux and BSDs, the URL handler
/// on Windows). No shell is involved. Returns whether it worked.
pub fn open_browser(u: &str) -> bool {
    let mut cmd = if cfg!(target_os = "macos") {
        std::process::Command::new("open")
    } else if cfg!(windows) {
        let mut c = std::process::Command::new("rundll32");
        c.arg("url.dll,FileProtocolHandler");
        c
    } else {
        std::process::Command::new("xdg-open")
    };
    cmd.arg(u)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

// ---- refresh tokens -----------------------------------------------------------------------

/// Trades a refresh token for a token for `aud` ([`DEFAULT_AUDIENCE`] if empty). With rotation the
/// issuer returns a new refresh token, which replaces the old one (empty if it didn't rotate).
/// Returns `(token, new_refresh_token)`. A refused refresh token is [`Error::LoginExpired`].
pub async fn refresh_login(
    issuer: &str,
    refresh_token: &str,
    aud: &str,
) -> Result<(String, String)> {
    let issuer = if issuer.is_empty() {
        DEFAULT_ISSUER
    } else {
        issuer
    };
    let aud = if aud.is_empty() {
        DEFAULT_AUDIENCE
    } else {
        aud
    };
    // The HTTP binding takes the request fields as query parameters (a JSON body is ignored).
    let url = format!(
        "{}/sts/exchange_refresh_token?{}",
        issuer.trim_end_matches('/'),
        http::query(&[("aud", aud)])
    );
    let req = http::client()?
        .post(url)
        // The issuer refuses a POST without Content-Length (HTTP 411).
        .header("Content-Length", "0")
        .header("Authorization", format!("Bearer {refresh_token}"))
        .header("Accept", "application/json");
    let a = http::send(req, 1 << 20, "refreshing the login").await?;
    if a.status != 200 {
        let msg = http::error_message(&a.body);
        if refresh_refused(a.status, &msg) {
            return Err(Error::LoginExpired {
                detail: format!("refreshing the login: HTTP {}: {msg}", a.status),
            });
        }
        return Err(Error::Http {
            context: "refreshing the login".into(),
            status: a.status,
            message: msg,
        });
    }
    #[derive(serde::Deserialize, Default)]
    struct Raw {
        #[serde(default)]
        token: String,
    }
    #[derive(serde::Deserialize)]
    struct Pair {
        #[serde(default)]
        token: Raw,
        #[serde(default, rename = "refreshToken")]
        refresh_token: Raw,
        #[serde(default, rename = "refresh_token")]
        refresh_alt: Raw,
    }
    let pair: Pair = serde_json::from_slice(&a.body)
        .map_err(|_| Error::other("refreshing the login: response has no token"))?;
    if pair.token.token.is_empty() {
        return Err(Error::other("refreshing the login: response has no token"));
    }
    let nr = if pair.refresh_token.token.is_empty() {
        pair.refresh_alt.token
    } else {
        pair.refresh_token.token
    };
    Ok((pair.token.token, nr))
}

/// Whether the issuer refused the refresh token itself (expired, revoked, already spent), as
/// opposed to a malformed request or an outage: 401/403, or 400 mentioning "refresh token".
fn refresh_refused(status: u16, msg: &str) -> bool {
    match status {
        401 | 403 => true,
        400 => msg.to_lowercase().contains("refresh token"),
        _ => false,
    }
}

/// What a browser login asks for: the sessions service and the account API.
/// `$CHAINGUARD_SANDBOX_LOGIN_AUDIENCES` (comma-separated) overrides it.
pub fn default_login_audiences() -> Vec<String> {
    if let Ok(v) = std::env::var("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES") {
        let v = v.trim();
        if !v.is_empty() {
            return v
                .split(',')
                .map(str::trim)
                .filter(|a| !a.is_empty())
                .map(String::from)
                .collect();
        }
    }
    vec![DEFAULT_AUDIENCE.into(), PLATFORM_AUDIENCE.into()]
}

/// A refresh token's (unverified) expiry and audiences. Chainguard refresh tokens are
/// base64-encoded JSON.
fn refresh_claims(rt: &str) -> Option<(SystemTime, Vec<String>)> {
    let b = base64::engine::general_purpose::STANDARD
        .decode(rt)
        .or_else(|_| base64::engine::general_purpose::STANDARD_NO_PAD.decode(rt))
        .ok()?;
    let v: serde_json::Value = serde_json::from_slice(&b).ok()?;
    let exp = v.get("exp")?.as_i64().filter(|e| *e != 0)?;
    Some((
        UNIX_EPOCH + Duration::from_secs(exp.max(0) as u64),
        audiences(v.get("aud")),
    ))
}

fn audiences(v: Option<&serde_json::Value>) -> Vec<String> {
    match v {
        Some(serde_json::Value::String(s)) => vec![s.clone()],
        Some(serde_json::Value::Array(a)) => a
            .iter()
            .filter_map(|x| x.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

// ---- the saved login ----------------------------------------------------------------------

/// Releases a [`LoginStore`] lock when dropped.
pub struct LoginLock(Option<Box<dyn FnOnce() + Send + Sync>>);

impl LoginLock {
    /// A lock released by calling `unlock`.
    pub fn new(unlock: impl FnOnce() + Send + Sync + 'static) -> Self {
        LoginLock(Some(Box::new(unlock)))
    }
}

impl Drop for LoginLock {
    fn drop(&mut self) {
        if let Some(f) = self.0.take() {
            f();
        }
    }
}

/// Keeps refresh tokens between runs, per issuer.
pub trait LoginStore: Send + Sync {
    /// The saved refresh token, or "" when there is none.
    fn load(&self, issuer: &str) -> Result<String>;
    /// Saves a refresh token; "" deletes it.
    fn save(&self, issuer: &str, refresh_token: &str) -> Result<()>;
    /// Serializes refresh-and-save across processes (refresh tokens rotate, so two processes
    /// must not spend the same one).
    fn lock(&self) -> BoxFuture<'_, Result<LoginLock>>;
}

/// Stores logins in a JSON file (0600, in a 0700 directory). The Go, Python, TypeScript and Rust
/// libraries share [`default_login_file`], so one login serves all four.
#[derive(Debug, Clone, Default)]
pub struct FileLoginStore {
    /// Empty means [`default_login_file`].
    pub path: PathBuf,
}

/// `<user config dir>/chainguard-sandbox/login.json`: on macOS
/// `~/Library/Application Support/chainguard-sandbox/login.json`, on Linux `$XDG_CONFIG_HOME` (or
/// `~/.config`)`/chainguard-sandbox/login.json`, on Windows `%AppData%`. Overridden by
/// `$CHAINGUARD_SANDBOX_LOGIN_FILE`.
pub fn default_login_file() -> PathBuf {
    if let Some(p) = std::env::var_os("CHAINGUARD_SANDBOX_LOGIN_FILE").filter(|p| !p.is_empty()) {
        return PathBuf::from(p);
    }
    user_config_dir()
        .unwrap_or_else(std::env::temp_dir)
        .join("chainguard-sandbox")
        .join("login.json")
}

/// Go's `os.UserConfigDir`, which the other SDKs reproduce too.
fn user_config_dir() -> Option<PathBuf> {
    let nonempty = |k: &str| {
        std::env::var_os(k)
            .filter(|v| !v.is_empty())
            .map(PathBuf::from)
    };
    if cfg!(windows) {
        return nonempty("AppData");
    }
    if cfg!(target_os = "macos") || cfg!(target_os = "ios") {
        return nonempty("HOME").map(|h| h.join("Library/Application Support"));
    }
    if let Some(x) = nonempty("XDG_CONFIG_HOME") {
        return x.is_absolute().then_some(x);
    }
    nonempty("HOME").map(|h| h.join(".config"))
}

#[derive(serde::Serialize, serde::Deserialize)]
struct LoginFile {
    #[serde(default)]
    version: i32,
    #[serde(default)]
    logins: BTreeMap<String, SavedLogin>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct SavedLogin {
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    updated: String,
}

impl FileLoginStore {
    fn file(&self) -> PathBuf {
        if self.path.as_os_str().is_empty() {
            default_login_file()
        } else {
            self.path.clone()
        }
    }

    fn read(&self) -> Result<LoginFile> {
        let p = self.file();
        match std::fs::read(&p) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(LoginFile {
                version: 1,
                logins: BTreeMap::new(),
            }),
            Err(e) => Err(Error::io(p.display().to_string(), e)),
            Ok(b) => serde_json::from_slice(&b)
                .map_err(|e| Error::other(format!("{}: {e}", p.display()))),
        }
    }
}

impl LoginStore for FileLoginStore {
    fn load(&self, issuer: &str) -> Result<String> {
        Ok(self
            .read()?
            .logins
            .get(&norm_issuer(issuer))
            .map(|l| l.refresh_token.clone())
            .unwrap_or_default())
    }

    fn save(&self, issuer: &str, refresh_token: &str) -> Result<()> {
        // A corrupt file is replaced.
        let mut lf = self.read().unwrap_or(LoginFile {
            version: 1,
            logins: BTreeMap::new(),
        });
        lf.version = 1;
        if refresh_token.is_empty() {
            lf.logins.remove(&norm_issuer(issuer));
        } else {
            let updated = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true);
            lf.logins.insert(
                norm_issuer(issuer),
                SavedLogin {
                    refresh_token: refresh_token.into(),
                    updated,
                },
            );
        }
        let mut b = serde_json::to_vec_pretty(&lf).map_err(|e| Error::other(e.to_string()))?;
        b.push(b'\n');
        let p = self.file();
        let dir = p.parent().unwrap_or(Path::new(".")).to_path_buf();
        create_private_dir(&dir)?;
        let tmp = dir.join(format!(".login-{}.json", random_hex(6)));
        let write = || -> std::io::Result<()> {
            let mut f = private_file(&tmp)?;
            f.write_all(&b)?;
            f.sync_all()?;
            drop(f);
            std::fs::rename(&tmp, &p)
        };
        let r = write();
        if r.is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
        r.map_err(|e| Error::io(p.display().to_string(), e))
    }

    /// Takes `<file>.lock`, created exclusively; a lock older than two minutes is taken to be left
    /// over from a crashed process. The same scheme as the Go, Python and TypeScript libraries.
    fn lock(&self) -> BoxFuture<'_, Result<LoginLock>> {
        Box::pin(async move {
            let mut p = self.file().into_os_string();
            p.push(".lock");
            let p = PathBuf::from(p);
            if let Some(d) = p.parent() {
                create_private_dir(d)?;
            }
            let deadline = std::time::Instant::now() + Duration::from_secs(150);
            loop {
                match private_file(&p) {
                    Ok(mut f) => {
                        let _ = writeln!(f, "{}", std::process::id());
                        let held = p.clone();
                        return Ok(LoginLock::new(move || {
                            let _ = std::fs::remove_file(held);
                        }));
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {}
                    Err(e) => return Err(Error::io(p.display().to_string(), e)),
                }
                if let Ok(md) = std::fs::metadata(&p)
                    && md
                        .modified()
                        .ok()
                        .and_then(|m| m.elapsed().ok())
                        .is_some_and(|age| age > Duration::from_secs(120))
                {
                    let _ = std::fs::remove_file(&p);
                    continue;
                }
                if std::time::Instant::now() > deadline {
                    return Err(Error::other(format!(
                        "waiting for {}: timed out",
                        p.display()
                    )));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        })
    }
}

fn create_private_dir(d: &Path) -> Result<()> {
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        b.mode(0o700);
    }
    b.create(d)
        .map_err(|e| Error::io(d.display().to_string(), e))
}

/// Creates p exclusively, mode 0600.
fn private_file(p: &Path) -> std::io::Result<std::fs::File> {
    let mut o = std::fs::OpenOptions::new();
    o.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        o.mode(0o600);
    }
    o.open(p)
}

pub(crate) fn random_hex(n: usize) -> String {
    let mut b = vec![0u8; n];
    getrandom::fill(&mut b).expect("the system random number generator failed");
    b.iter().map(|x| format!("{x:02x}")).collect()
}

fn norm_issuer(s: &str) -> String {
    let s = if s.is_empty() { DEFAULT_ISSUER } else { s };
    s.strip_suffix('/').unwrap_or(s).to_string()
}

// ---- the token source ---------------------------------------------------------------------

/// A [`TokenSource`] backed by the library's own login: it mints tokens from the saved refresh
/// token, and when there is none (or it has expired) runs [`browser_login`], unless `no_browser` is
/// set. Wrap it in [`cached`](super::cached). Concurrent processes share the saved login safely.
#[derive(Clone, Default)]
pub struct BrowserLoginToken {
    pub login: LoginOptions,
    /// Defaults to [`FileLoginStore`] at [`default_login_file`].
    pub store: Option<Arc<dyn LoginStore>>,
    /// The audience of the returned tokens; defaults to [`DEFAULT_AUDIENCE`].
    pub audience: String,
    /// Fail with [`Error::LoginRequired`] instead of opening a browser.
    pub no_browser: bool,
    /// Bounds a browser login (default 10 minutes).
    pub login_timeout: Option<Duration>,
}

impl BrowserLoginToken {
    fn store(&self) -> Arc<dyn LoginStore> {
        self.store
            .clone()
            .unwrap_or_else(|| Arc::new(FileLoginStore::default()))
    }

    async fn get(&self) -> Result<String> {
        let aud = if self.audience.is_empty() {
            DEFAULT_AUDIENCE.to_string()
        } else {
            self.audience.clone()
        };
        if let Some(tok) = self.saved_token(&aud).await? {
            return Ok(tok);
        }
        if self.no_browser {
            return Err(Error::LoginRequired { detail: None });
        }
        let mut o = self.login.clone();
        if o.audience.is_empty() {
            o.audience = default_login_audiences();
        }
        if !o.audience.contains(&aud) {
            o.audience.push(aud.clone());
        }
        // No lock while the person logs in (that can take minutes).
        let to = self.login_timeout.unwrap_or(Duration::from_secs(600));
        let res = tokio::time::timeout(to, browser_login(o))
            .await
            .map_err(|_| {
                Error::other(format!(
                    "workspaces: login: no answer from the browser within {}s",
                    to.as_secs()
                ))
            })??;
        self.finish_login(res, &aud).await
    }

    /// Mints a token from the saved login, under the store's lock. `None` when there is no usable
    /// saved login.
    async fn saved_token(&self, aud: &str) -> Result<Option<String>> {
        let st = self.store();
        let _lock = st.lock().await?;
        let rt = st.load(&self.login.issuer)?;
        if rt.is_empty() {
            return Ok(None);
        }
        match refresh_login(&self.login.issuer, &rt, aud).await {
            Err(Error::LoginExpired { .. }) => {
                // Forget the login only if it really is unusable: expired, or refused for an
                // audience it covers. A login made without this audience is still good for its
                // own audiences, so it's kept.
                if let Some((exp, auds)) = refresh_claims(&rt)
                    && SystemTime::now() < exp
                    && !auds.iter().any(|a| a == aud)
                {
                    if self.no_browser {
                        return Err(Error::LoginRequired {
                            detail: Some(format!(
                                "the saved login doesn't cover {aud}; log in again (chainguard-sandbox login) to add it"
                            )),
                        });
                    }
                    return Ok(None); // a browser login asks for it, then replaces the saved one
                }
                st.save(&self.login.issuer, "")?;
                Ok(None)
            }
            Err(e) => Err(e),
            Ok((tok, nrt)) => {
                if !nrt.is_empty() && nrt != rt {
                    st.save(&self.login.issuer, &nrt)
                        .map_err(|e| Error::other(format!("saving the rotated login: {e}")))?;
                }
                Ok(Some(tok))
            }
        }
    }

    /// Saves a fresh login and returns a token for `aud`.
    async fn finish_login(&self, res: LoginResult, aud: &str) -> Result<String> {
        let st = self.store();
        let _lock = st.lock().await?;
        if !res.refresh_token.is_empty() {
            st.save(&self.login.issuer, &res.refresh_token)
                .map_err(|e| Error::other(format!("saving the login: {e}")))?;
        }
        if token_has_audience(&res.token, aud) {
            return Ok(res.token);
        }
        if res.refresh_token.is_empty() {
            return Err(Error::other(format!(
                "workspaces: login: the token isn't for {aud} and there's no refresh token to mint one"
            )));
        }
        let (tok, nrt) = refresh_login(&self.login.issuer, &res.refresh_token, aud).await?;
        if !nrt.is_empty() {
            st.save(&self.login.issuer, &nrt)
                .map_err(|e| Error::other(format!("saving the rotated login: {e}")))?;
        }
        Ok(tok)
    }
}

impl TokenSource for BrowserLoginToken {
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(self.get())
    }
}

/// Forgets the saved login for `issuer` ("" = [`DEFAULT_ISSUER`]). `None` uses the default file.
pub fn logout(store: Option<&dyn LoginStore>, issuer: &str) -> Result<()> {
    match store {
        Some(s) => s.save(issuer, ""),
        None => FileLoginStore::default().save(issuer, ""),
    }
}

/// A token's unverified identity claims (for display).
#[derive(Debug, Clone, Default)]
pub struct Claims {
    pub subject: String,
    pub issuer: String,
    pub audience: Vec<String>,
    pub expiry: Option<SystemTime>,
}

/// Decodes a JWT's claims without verifying it.
pub fn token_claims(tok: &str) -> Result<Claims> {
    let v = jwt_claims_json(tok).ok_or_else(|| Error::other("not a JWT"))?;
    let s = |k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let exp = v
        .get("exp")
        .and_then(|x| x.as_i64())
        .map(|e| UNIX_EPOCH + Duration::from_secs(e.max(0) as u64));
    Ok(Claims {
        subject: s("sub"),
        issuer: s("iss"),
        audience: audiences(v.get("aud")),
        expiry: exp,
    })
}

fn token_has_audience(tok: &str, aud: &str) -> bool {
    token_claims(tok)
        .map(|c| c.audience.iter().any(|a| a == aud))
        .unwrap_or(false)
}

/// Starts the login callback server alone and returns its callback URL plus a receiver for the
/// result. For tests that play the browser.
#[doc(hidden)]
pub async fn login_callback_server() -> std::io::Result<(
    String,
    impl std::future::Future<Output = Result<LoginResult>>,
    impl Drop,
)> {
    let srv = LoginServer::start().await?;
    let LoginServer {
        server,
        callback_url,
        result,
        ..
    } = srv;
    let fut = async move {
        result
            .await
            .unwrap_or_else(|_| Err(Error::other("stopped")))
    };
    Ok((callback_url, fut, server))
}
