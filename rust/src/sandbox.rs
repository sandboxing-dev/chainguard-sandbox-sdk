//! Sandboxes: create, connect, list, delete, pause.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::commands::Commands;
use crate::files::Files;
use crate::workspaces::{self, Client, ListOptions, Options, Session, pb::Phase};
use crate::{Error, Result};

/// The toolchain a sandbox gets when [`CreateOptions::packages`] is `None`: a shell, the core
/// tools, git, curl, jq, Python and Node.js. Package names are Chainguard (Wolfi) package names,
/// e.g. "python-3.12", "go", "nodejs-22", "bun", "rust".
pub const DEFAULT_PACKAGES: &[&str] = &[
    "bash",
    "coreutils",
    "curl",
    "git",
    "jq",
    "python-3.13",
    "nodejs-22",
];

/// Where commands run and relative paths resolve.
pub const DEFAULT_WORKDIR: &str = "/work";

/// Marks sessions created by this package (the workspace label).
const LABEL_PREFIX: &str = "/chainguard-sandbox/";

/// Starts the sandbox from a GitHub repository instead of an empty tree: the plane clones it into
/// /work (no upload).
#[derive(Debug, Clone, Default)]
pub struct GitSource {
    /// A github.com repository (`github.com/o/r`, `https://github.com/o/r`, `git@github.com:o/r.git`).
    pub url: String,
    /// The revision to check out; empty means the default branch.
    pub revision: String,
}

/// Configures [`Sandbox::create`]. The default is a sandbox with [`DEFAULT_PACKAGES`] and open
/// egress.
#[derive(Clone, Default)]
pub struct CreateOptions {
    /// Packages installed in the sandbox (Chainguard package names). `None` means
    /// [`DEFAULT_PACKAGES`]; `Some(vec![])` means none beyond the base system. They are resolved
    /// when the sandbox is created; an unknown package fails `create`.
    pub packages: Option<Vec<String>>,
    /// Environment variables for every command (per-command ones override them). They are kept
    /// client-side, in this `Sandbox` value.
    pub envs: HashMap<String, String>,
    /// Labels the sandbox. Its display name becomes `<name>/<word-word>`. Defaults to "sandbox".
    pub name: String,
    /// The outbound allowlist (hostnames; "*" allows all). `None` means `["*"]`. The platform
    /// always blocks cloud metadata, loopback and private ranges.
    pub egress: Option<Vec<String>>,
    /// What happens to a connection outside `egress`: "deny" (refuse it; the default) or
    /// "destroy" (tear the sandbox down).
    pub egress_policy: String,
    /// Replaces the generated environment document entirely (a .chainguard/ci.yaml-style
    /// document). `packages`, `egress` and `egress_policy` are then ignored.
    pub document: Option<Vec<u8>>,
    /// Starts from a GitHub repository instead of an empty tree.
    pub source: Option<GitSource>,
    /// Bounds waiting for the guest to start (default 3 minutes).
    pub start_timeout: Option<Duration>,
    /// The Workspaces client to use. `None` creates one with the default login and the
    /// organization from `instance`.
    pub client: Option<Client>,
    /// The organization ID (used when `client` is `None`). If empty, `$CHAINCTL_SANDBOX_INSTANCE`
    /// is used; if that's empty too, the organization is auto-detected when you belong to exactly
    /// one (turn off with `$CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1`). List IDs with
    /// [`list_organizations`], `chainguard-sandbox orgs` or `chainctl iam organizations list`.
    pub instance: String,
}

/// Configures [`Sandbox::connect`], [`list`] and [`kill`].
#[derive(Clone, Default)]
pub struct ConnectOptions {
    /// Environment variables for every command on the returned sandbox (not stored remotely).
    pub envs: HashMap<String, String>,
    pub client: Option<Client>,
    pub instance: String,
}

pub(crate) struct Inner {
    pub client: Client,
    pub id: String,
    pub name: String,
    pub envs: HashMap<String, String>,
}

/// A handle to one sandbox. Cloning is cheap; clones refer to the same sandbox. The sandbox runs
/// one command at a time: concurrent calls queue in the plane.
///
/// Deleting is explicit: call [`Sandbox::kill`], or use [`Sandbox::with`], which deletes the
/// sandbox when its closure returns. Dropping a `Sandbox` leaves the sandbox running (it parks
/// when idle and is discarded after 5 days).
#[derive(Clone)]
pub struct Sandbox {
    /// The short ID (8 hex characters), usable with [`Sandbox::connect`].
    pub id: String,
    /// The full resource name (`instances/<org>/sessions/<id>`).
    pub name: String,
    /// Runs commands in the sandbox.
    pub commands: Commands,
    /// Reads and writes the sandbox's filesystem.
    pub files: Files,
    inner: Arc<Inner>,
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

/// Describes a sandbox.
#[derive(Debug, Clone, Default)]
pub struct Info {
    pub id: String,
    pub name: String,
    pub display_name: String,
    /// "creating", "running", "paused" or "stopped".
    pub state: String,
    pub region: String,
    pub created_at: Option<SystemTime>,
    /// When a paused sandbox is discarded.
    pub expires_at: Option<SystemTime>,
    pub cpus: i32,
    pub memory_mib: i64,
}

async fn client_for(c: &Option<Client>, instance: &str) -> Result<Client> {
    match c {
        Some(c) => Ok(c.clone()),
        None => {
            Client::new(Options {
                instance: instance.into(),
                ..Default::default()
            })
            .await
        }
    }
}

/// The environment document [`Sandbox::create`] would send for `o`.
pub fn document(o: &CreateOptions) -> Vec<u8> {
    if let Some(d) = &o.document {
        return d.clone();
    }
    let defaults: Vec<String> = DEFAULT_PACKAGES.iter().map(|p| p.to_string()).collect();
    let pkgs = o.packages.as_ref().unwrap_or(&defaults);
    let all = vec!["*".to_string()];
    let egress = o.egress.as_ref().unwrap_or(&all);
    let policy = if o.egress_policy.is_empty() {
        "deny"
    } else {
        &o.egress_policy
    };
    let mut b = String::from(
        "# Generated by chainguard-sandbox-sdk.\nversion: 1\nenvironment:\n  packages:\n",
    );
    for p in pkgs {
        b.push_str(&format!("    - {}\n", yaml_string(p)));
    }
    b.push_str("checks:\n  develop:\n    network:\n      egress:");
    if egress.is_empty() {
        b.push_str(" []\n");
    } else {
        b.push('\n');
        for h in egress {
            b.push_str(&format!("        - {}\n", yaml_string(h)));
        }
    }
    b.push_str(&format!("      policy: {}\n", yaml_string(policy)));
    b.into_bytes()
}

/// Quotes s as a YAML double-quoted scalar.
fn yaml_string(s: &str) -> String {
    format!(
        "\"{}\"",
        s.replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('\n', "\\n")
    )
}

/// Timings, adjustable by tests.
pub(crate) mod timings {
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Duration;

    static PLACEMENT_GRACE_MS: AtomicU64 = AtomicU64::new(30_000);
    static START_POLL_MS: AtomicU64 = AtomicU64::new(1_000);
    static KILL_WAIT_MS: AtomicU64 = AtomicU64::new(30_000);
    static KILL_POLL_MS: AtomicU64 = AtomicU64::new(500);

    /// How long "starting guest failed" may show before giving up on a host.
    pub fn placement_grace() -> Duration {
        Duration::from_millis(PLACEMENT_GRACE_MS.load(Ordering::Relaxed))
    }
    /// Between GetSession calls while starting.
    pub fn start_poll() -> Duration {
        Duration::from_millis(START_POLL_MS.load(Ordering::Relaxed))
    }
    /// How long kill waits for the session to disappear.
    pub fn kill_wait() -> Duration {
        Duration::from_millis(KILL_WAIT_MS.load(Ordering::Relaxed))
    }
    /// Between GetSession calls while deleting.
    pub fn kill_poll() -> Duration {
        Duration::from_millis(KILL_POLL_MS.load(Ordering::Relaxed))
    }

    /// Shortens every timing (tests only).
    #[doc(hidden)]
    pub fn shorten(placement_grace: u64, start_poll: u64, kill_wait: u64, kill_poll: u64) {
        PLACEMENT_GRACE_MS.store(placement_grace, Ordering::Relaxed);
        START_POLL_MS.store(start_poll, Ordering::Relaxed);
        KILL_WAIT_MS.store(kill_wait, Ordering::Relaxed);
        KILL_POLL_MS.store(kill_poll, Ordering::Relaxed);
    }
}

#[doc(hidden)]
pub use timings::shorten as shorten_timings_for_tests;

/// Maps lower-level errors to the sandbox API's (like Go's `wrapErr`).
pub(crate) fn wrap(e: Error) -> Error {
    if matches!(e, Error::OutcomeUnknown { .. }) {
        return e;
    }
    if workspaces::is_guest_held(&e) {
        return Error::Busy { cause: Box::new(e) };
    }
    if let Error::Rpc(s) = &e {
        if s.code() == tonic::Code::PermissionDenied
            && s.message()
                .contains("not entitled to Chainguard Workspaces")
        {
            return Error::NotEnabled { cause: Box::new(e) };
        }
        if s.code() == tonic::Code::ResourceExhausted {
            return Error::LimitReached {
                message: s.message().to_string(),
            };
        }
    }
    e
}

impl Sandbox {
    fn from_session(client: Client, s: &Session, envs: HashMap<String, String>) -> Sandbox {
        let inner = Arc::new(Inner {
            client,
            id: s.short_id.clone(),
            name: s.name.clone(),
            envs,
        });
        Sandbox {
            id: inner.id.clone(),
            name: inner.name.clone(),
            commands: Commands::new(inner.clone()),
            files: Files::new(inner.clone()),
            inner,
        }
    }

    /// Starts a new sandbox and returns when it can run commands (about 8–20 seconds). On failure
    /// it deletes anything it created. A session the plane places on a host that never starts is
    /// deleted and recreated (up to three attempts).
    pub async fn create(o: CreateOptions) -> Result<Sandbox> {
        let c = client_for(&o.client, &o.instance).await?;
        let doc = document(&o);
        let name = if o.name.is_empty() {
            "sandbox"
        } else {
            &o.name
        };
        if name.contains('/') || name.contains('\n') {
            return Err(Error::other(format!(
                "sandbox: name {name:?} must not contain '/' or newlines"
            )));
        }
        let start_timeout = o
            .start_timeout
            .filter(|d| !d.is_zero())
            .unwrap_or(Duration::from_secs(180));
        let mut last_err = String::new();
        for _attempt in 1..=3 {
            let req = match &o.source {
                Some(src) => {
                    let mut s = workspaces::repository_session(&src.url, &src.revision);
                    if let Some(workspaces::pb::session::Environment::Source(so)) =
                        &mut s.environment
                    {
                        so.fallback_document = doc.clone();
                    }
                    s
                }
                None => workspaces::workspace_session(
                    &format!("{LABEL_PREFIX}{name}"),
                    "",
                    "",
                    Some(doc.clone()),
                ),
            };
            let s = c.create(req).await.map_err(wrap)?;
            let created = s.name.clone();
            let cleanup = || async {
                let _ = tokio::time::timeout(Duration::from_secs(30), c.delete(&created)).await;
            };
            let s = match wait_started(&c, &created, start_timeout).await {
                Err(Placement(msg)) => {
                    cleanup().await;
                    last_err = msg;
                    continue;
                }
                Ok(Err(e)) => {
                    cleanup().await;
                    return Err(e);
                }
                Ok(Ok(s)) => s,
            };
            if s.workspace_pending
                && let Err(e) = empty_upload(&c, &s.name).await
            {
                cleanup().await;
                return Err(e);
            }
            return Ok(Sandbox::from_session(c, &s, o.envs.clone()));
        }
        Err(Error::other(format!(
            "sandbox: the plane could not start a guest in 3 attempts: the guest failed to start: {last_err}"
        )))
    }

    /// Creates a sandbox, runs `f` with it, and deletes the sandbox afterwards, whether `f`
    /// succeeds or fails. If the returned future is dropped early or `f` panics, the delete is
    /// started in the background instead.
    ///
    /// ```no_run
    /// # async fn g() -> chainguard_sandbox::Result<()> {
    /// use chainguard_sandbox::{CreateOptions, Sandbox};
    /// let out = Sandbox::with(CreateOptions::default(), async |sbx| {
    ///     Ok(sbx.commands.run("echo hi").await?.stdout)
    /// })
    /// .await?;
    /// # Ok(()) }
    /// ```
    pub async fn with<T, F>(o: CreateOptions, f: F) -> Result<T>
    where
        F: AsyncFnOnce(&Sandbox) -> Result<T>,
    {
        let sbx = Sandbox::create(o).await?;
        let mut guard = KillOnDrop(Some(sbx.clone()));
        let r = f(&sbx).await;
        guard.0 = None;
        let k = sbx.kill().await;
        let v = r?;
        k?;
        Ok(v)
    }

    /// Returns a handle to an existing sandbox by short ID, display name or resource name. A
    /// paused sandbox resumes on its first command.
    pub async fn connect(id: &str, o: ConnectOptions) -> Result<Sandbox> {
        let c = client_for(&o.client, &o.instance).await?;
        let s = match c.resolve(id).await {
            Err(Error::NotFound { .. }) => {
                return Err(Error::NotFound {
                    what: "sandbox".into(),
                    name: id.into(),
                });
            }
            Err(e) => return Err(wrap(e)),
            Ok(s) => s,
        };
        if s.workspace_pending {
            empty_upload(&c, &s.name).await?;
        }
        Ok(Sandbox::from_session(c, &s, o.envs))
    }

    /// Deletes the sandbox and everything in it, and returns once it is gone, so the session slot
    /// is free for a new sandbox. Returns false if the sandbox was already gone. If it still
    /// exists 30 seconds after the delete request (or its disappearance can't be confirmed), the
    /// error is [`Error::Timeout`] with op "kill": the deletion was requested but the slot may not
    /// be free yet.
    pub async fn kill(&self) -> Result<bool> {
        delete_and_wait(&self.inner.client, &self.inner.name).await
    }

    /// Snapshots the sandbox (memory and disk) and stops it. Running processes, including
    /// background commands, resume where they were on the next command or file operation.
    pub async fn pause(&self) -> Result<()> {
        self.inner
            .client
            .park(&self.inner.name)
            .await
            .map_err(wrap)?;
        Ok(())
    }

    /// Asks the plane to hold the sandbox running (not park it when idle) for `d`. An idle
    /// sandbox parks after about 10 minutes, and a parked one is discarded after 5 days.
    pub async fn keep_alive(&self, d: Duration) -> Result<()> {
        self.inner
            .client
            .heartbeat(&self.inner.name, d)
            .await
            .map_err(wrap)?;
        Ok(())
    }

    /// Fetches the sandbox's current state.
    pub async fn get_info(&self) -> Result<Info> {
        match self.inner.client.get(&self.inner.name).await {
            Ok(s) => Ok(info(&s)),
            Err(e) if e.grpc_code() == Some(tonic::Code::NotFound) => Err(Error::NotFound {
                what: "sandbox".into(),
                name: self.id.clone(),
            }),
            Err(e) => Err(wrap(e)),
        }
    }

    /// Whether the sandbox is running (not paused, stopped or still starting).
    pub async fn is_running(&self) -> Result<bool> {
        match self.get_info().await {
            Ok(i) => Ok(i.state == "running"),
            Err(Error::NotFound { .. }) => Ok(false),
            Err(e) => Err(e),
        }
    }

    /// The underlying Workspaces client, for anything this API doesn't wrap.
    pub fn client(&self) -> &Client {
        &self.inner.client
    }
}

/// Deletes the sandbox in the background if the scope it guards is left abnormally.
struct KillOnDrop(Option<Sandbox>);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        if let Some(sbx) = self.0.take()
            && let Ok(h) = tokio::runtime::Handle::try_current()
        {
            h.spawn(async move {
                let _ = sbx.kill().await;
            });
        }
    }
}

/// The session landed on a host that keeps failing to start it.
struct Placement(String);

/// Polls until the guest has started. `limit` bounds the whole wait, including each GetSession
/// call: a call that stalls is cut off at the deadline, so a late answer can never be reported as
/// a start in time.
async fn wait_started(
    c: &Client,
    name: &str,
    limit: Duration,
) -> std::result::Result<Result<Session>, Placement> {
    let t0 = Instant::now();
    let deadline = t0 + limit;
    let mut last: Option<Session> = None;
    let expired = |last: &Option<Session>| {
        let mut detail = format!("the guest did not start within {}", go_duration(limit));
        if let Some(s) = last {
            let phase = Phase::try_from(s.phase)
                .map(|p| p.as_str_name())
                .unwrap_or("PHASE_UNSPECIFIED");
            detail.push_str(&format!(" (phase {phase}: {})", s.message));
        }
        Err(Error::Timeout {
            op: "start".into(),
            detail,
            result: None,
        })
    };
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let s = match tokio::time::timeout(remaining, c.get(name)).await {
            Err(_) => return Ok(expired(&last)),
            Ok(Err(e)) => return Ok(Err(wrap(e))),
            Ok(Ok(s)) => s,
        };
        if s.guest_started_time.is_some() {
            return Ok(Ok(s));
        }
        if s.message.contains("starting guest failed") && t0.elapsed() > timings::placement_grace()
        {
            return Err(Placement(s.message));
        }
        last = Some(s);
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Ok(expired(&last));
        }
        tokio::time::sleep(timings::start_poll().min(remaining)).await;
        if Instant::now() >= deadline {
            return Ok(expired(&last));
        }
    }
}

/// Go's `time.Duration` formatting for the durations used in messages ("3m0s", "30s", "1.5s").
pub(crate) fn go_duration(d: Duration) -> String {
    let ms = d.as_millis();
    if ms % 1000 != 0 {
        return format!("{}s", d.as_secs_f64());
    }
    let s = d.as_secs();
    match (s / 3600, (s / 60) % 60, s % 60) {
        (0, 0, sec) => format!("{sec}s"),
        (0, m, sec) => format!("{m}m{sec}s"),
        (h, m, sec) => format!("{h}h{m}m{sec}s"),
    }
}

/// Completes the first attach with an empty tree. The plane refuses Exec until a shell attach has
/// uploaded the workspace.
async fn empty_upload(c: &Client, name: &str) -> Result<()> {
    match c.complete_first_attach(name).await {
        Ok(()) => Ok(()),
        Err(e @ Error::Timeout { .. }) => Err(e),
        Err(e) => Err(Error::other(format!(
            "sandbox: completing the first attach: {}",
            wrap(e)
        ))),
    }
}

/// Returns the sandboxes this API created (sessions with a sandbox label) in the instance.
pub async fn list(o: ConnectOptions) -> Result<Vec<Info>> {
    let c = client_for(&o.client, &o.instance).await?;
    let ss = c.list(&ListOptions::default()).await.map_err(wrap)?;
    Ok(ss
        .iter()
        .filter(|s| match &s.environment {
            Some(workspaces::pb::session::Environment::Workspace(w)) => {
                w.label.starts_with(LABEL_PREFIX)
            }
            _ => false,
        })
        .map(info)
        .collect())
}

/// Deletes the sandbox with the given ID (short ID, display name or resource name). Returns false
/// if there was none. Unlike [`Sandbox::connect`] it does not attach to the sandbox first, so it
/// also deletes one that never started or cannot be attached to. Errors as for
/// [`Sandbox::kill`].
pub async fn kill(id: &str, o: ConnectOptions) -> Result<bool> {
    let c = client_for(&o.client, &o.instance).await?;
    match c.resolve(id).await {
        Err(Error::NotFound { .. }) => Ok(false),
        Err(e) if e.grpc_code() == Some(tonic::Code::NotFound) => Ok(false),
        Err(e) => Err(wrap(e)),
        Ok(s) => delete_and_wait(&c, &s.name).await,
    }
}

/// Sends the delete request and waits until the session is gone.
async fn delete_and_wait(c: &Client, name: &str) -> Result<bool> {
    match c.delete(name).await {
        Err(e) if e.grpc_code() == Some(tonic::Code::NotFound) => return Ok(false),
        Err(e) => return Err(wrap(e)),
        Ok(()) => {}
    }
    // Deletion is asynchronous (the session disappears 1-8 s later). Each check runs under the
    // deadline, so a stalled call can't stretch the wait.
    let wait = timings::kill_wait();
    let deadline = Instant::now() + wait;
    let mut last_err: Option<Error> = None;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match tokio::time::timeout(remaining, c.get(name)).await {
            Ok(Err(e)) if e.grpc_code() == Some(tonic::Code::NotFound) => return Ok(true),
            Ok(Err(e)) => last_err = Some(wrap(e)),
            Ok(Ok(_)) => last_err = None,
            Err(_) => {}
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            let detail = match &last_err {
                Some(e) if e.grpc_code() != Some(tonic::Code::DeadlineExceeded) => {
                    format!(
                        "could not confirm the sandbox is gone within {} (last check: {e})",
                        go_duration(wait)
                    )
                }
                _ => format!(
                    "the sandbox still exists {} after the delete request",
                    go_duration(wait)
                ),
            };
            return Err(Error::Timeout {
                op: "kill".into(),
                detail,
                result: None,
            });
        }
        tokio::time::sleep(timings::kill_poll().min(remaining)).await;
    }
}

fn timestamp(t: &Option<prost_types::Timestamp>) -> Option<SystemTime> {
    t.as_ref()
        .map(|t| UNIX_EPOCH + Duration::new(t.seconds.max(0) as u64, t.nanos.max(0) as u32))
}

pub(crate) fn info(s: &Session) -> Info {
    let rc = s.max_resource_class.unwrap_or_default();
    let state = match Phase::try_from(s.phase) {
        Ok(Phase::Creating) => "creating",
        Ok(Phase::Attached) | Ok(Phase::Idle) => "running",
        Ok(Phase::Parked) => "paused",
        _ => "stopped",
    };
    Info {
        id: s.short_id.clone(),
        name: s.name.clone(),
        display_name: s.display_name.clone(),
        state: state.into(),
        region: s.region.clone(),
        created_at: timestamp(&s.create_time),
        expires_at: timestamp(&s.expire_time),
        cpus: rc.cpus,
        memory_mib: rc.memory_mib,
    }
}

/// Resolves p against [`DEFAULT_WORKDIR`].
pub(crate) fn guest_path(p: &str) -> String {
    if p.is_empty() {
        DEFAULT_WORKDIR.into()
    } else {
        workspaces::guest_path(p)
    }
}

/// A Chainguard organization a sandbox can live in.
pub use crate::workspaces::Organization;

crate::opaque_debug!(CreateOptions, ConnectOptions);

/// The organizations you belong to (their IDs are what [`CreateOptions::instance`] and
/// `$CHAINCTL_SANDBOX_INSTANCE` take). With exactly one, [`Sandbox::create`] uses it automatically
/// when no organization is set. It uses the saved login's account-API token.
pub async fn list_organizations() -> Result<Vec<Organization>> {
    workspaces::list_organizations(None).await
}
