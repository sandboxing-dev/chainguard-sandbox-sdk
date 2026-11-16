//! Token sources for the sandbox plane: static tokens, chainctl, the cache.

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use tokio::sync::watch;

use super::login::{BrowserLoginToken, LoginOptions};
use super::{DEFAULT_AUDIENCE, ENV_AUTH_TOKEN};
use crate::{Error, Result};

/// A boxed, sendable future, as returned by [`TokenSource::token`].
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Returns a Chainguard bearer token whose audience includes [`DEFAULT_AUDIENCE`] (or, for the
/// platform API, [`PLATFORM_AUDIENCE`](super::PLATFORM_AUDIENCE)).
///
/// Implement it with a function through [`token_fn`]. Caching sources also implement
/// [`invalidate`](TokenSource::invalidate): the client calls it when the plane answers
/// `Unauthenticated`, so the next call fetches a fresh token.
pub trait TokenSource: Send + Sync {
    /// Returns a token.
    fn token(&self) -> BoxFuture<'_, Result<String>>;
    /// Forgets a cached token. The default does nothing.
    fn invalidate(&self) {}
}

/// A shared token source.
pub type SharedTokenSource = Arc<dyn TokenSource>;

/// Always returns the same token.
#[derive(Clone)]
pub struct StaticToken(pub String);

impl std::fmt::Debug for StaticToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StaticToken(..)") // never print the token
    }
}

impl TokenSource for StaticToken {
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            if self.0.is_empty() {
                return Err(Error::other("workspaces: empty token"));
            }
            Ok(self.0.clone())
        })
    }
}

struct FnToken<F>(F);

impl<F, Fut> TokenSource for FnToken<F>
where
    F: Fn() -> Fut + Send + Sync,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin((self.0)())
    }
}

/// Adapts an async function to a [`TokenSource`].
pub fn token_fn<F, Fut>(f: F) -> SharedTokenSource
where
    F: Fn() -> Fut + Send + Sync + 'static,
    Fut: Future<Output = Result<String>> + Send + 'static,
{
    Arc::new(FnToken(f))
}

/// Mints tokens by running `chainctl auth token --audience <audience>`. It needs a prior
/// `chainctl auth login`.
#[derive(Debug, Clone, Default)]
pub struct ChainctlToken {
    /// Path to chainctl; defaults to "chainctl" on `$PATH`.
    pub path: String,
    /// Defaults to [`DEFAULT_AUDIENCE`].
    pub audience: String,
}

impl TokenSource for ChainctlToken {
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let path = if self.path.is_empty() {
                "chainctl"
            } else {
                &self.path
            };
            let aud = if self.audience.is_empty() {
                DEFAULT_AUDIENCE
            } else {
                &self.audience
            };
            let out = tokio::process::Command::new(path)
                .args(["auth", "token", "--audience", aud])
                .kill_on_drop(true)
                .output()
                .await
                .map_err(|e| Error::io(format!("chainctl auth token --audience {aud}"), e))?;
            if !out.status.success() {
                return Err(Error::other(format!(
                    "chainctl auth token --audience {aud}: {}: {}",
                    out.status,
                    String::from_utf8_lossy(&out.stderr).trim()
                )));
            }
            let tok = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if tok.is_empty() {
                return Err(Error::other("chainctl auth token returned no token"));
            }
            Ok(tok)
        })
    }
}

/// `$CHAINCTL_AUTH_TOKEN` when set, otherwise the library's own login (a cached
/// [`BrowserLoginToken`]): tokens are minted from the saved login, and a browser login runs when
/// there is none. chainctl isn't used; set `$CHAINGUARD_SANDBOX_AUTH=chainctl` to mint tokens with
/// it instead. `$CHAINGUARD_SANDBOX_SOCIAL_LOGIN` preselects the login's social provider (github,
/// google-oauth2, gitlab), and `$CHAINGUARD_SANDBOX_NO_BROWSER=1` makes a missing login an error
/// ([`Error::LoginRequired`]) instead of opening a browser.
pub fn default_token_source() -> SharedTokenSource {
    if let Some(t) = env_nonempty(ENV_AUTH_TOKEN) {
        return Arc::new(StaticToken(t));
    }
    if std::env::var("CHAINGUARD_SANDBOX_AUTH").as_deref() == Ok("chainctl") {
        return cached(Arc::new(ChainctlToken::default()));
    }
    cached(Arc::new(BrowserLoginToken {
        login: LoginOptions {
            connection: std::env::var("CHAINGUARD_SANDBOX_SOCIAL_LOGIN").unwrap_or_default(),
            ..Default::default()
        },
        no_browser: std::env::var("CHAINGUARD_SANDBOX_NO_BROWSER").as_deref() == Ok("1"),
        ..Default::default()
    }))
}

pub(crate) fn env_nonempty(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// How long a shared fetch may take: room for a browser login (10 minutes) plus the refresh.
const FETCH_TIMEOUT: Duration = Duration::from_secs(11 * 60);

/// Reuses a token until a minute before the `exp` claim of the JWT. Tokens whose expiry can't be
/// read are cached for five minutes. Concurrent callers share one fetch, which runs on its own
/// task, so one caller giving up (dropping its future) doesn't fail the others. The client drops
/// the cached token when the plane answers `Unauthenticated` (see [`TokenSource::invalidate`]).
pub fn cached(src: SharedTokenSource) -> SharedTokenSource {
    Arc::new(Cached(Arc::new(CacheInner {
        src,
        state: Mutex::new(CacheState::default()),
    })))
}

struct Cached(Arc<CacheInner>);

struct CacheInner {
    src: SharedTokenSource,
    state: Mutex<CacheState>,
}

#[derive(Default)]
struct CacheState {
    tok: String,
    exp: Option<SystemTime>,
    inflight: Option<watch::Receiver<Option<Result<String>>>>,
}

impl TokenSource for Cached {
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            let mut rx = {
                let mut st = self.0.state.lock().unwrap();
                if !st.tok.is_empty()
                    && st
                        .exp
                        .is_some_and(|e| e > SystemTime::now() + Duration::from_secs(60))
                {
                    return Ok(st.tok.clone());
                }
                match &st.inflight {
                    Some(rx) => rx.clone(),
                    None => {
                        let (tx, rx) = watch::channel(None);
                        st.inflight = Some(rx.clone());
                        let inner = self.0.clone();
                        tokio::spawn(async move {
                            let r = match tokio::time::timeout(FETCH_TIMEOUT, inner.src.token())
                                .await
                            {
                                Ok(r) => r,
                                Err(_) => Err(Error::other(
                                    "workspaces: getting a token took longer than 11 minutes",
                                )),
                            };
                            {
                                let mut st = inner.state.lock().unwrap();
                                if let Ok(t) = &r {
                                    st.tok = t.clone();
                                    st.exp = Some(jwt_expiry(t).unwrap_or_else(|| {
                                        SystemTime::now() + Duration::from_secs(300)
                                    }));
                                }
                                st.inflight = None;
                            }
                            let _ = tx.send(Some(r));
                        });
                        rx
                    }
                }
            };
            loop {
                if let Some(r) = rx.borrow_and_update().clone() {
                    return r;
                }
                if rx.changed().await.is_err() {
                    return Err(Error::other(
                        "workspaces: the token fetch ended without a result",
                    ));
                }
            }
        })
    }

    fn invalidate(&self) {
        let mut st = self.0.state.lock().unwrap();
        st.tok.clear();
        st.exp = None;
    }
}

/// Reads the unverified `exp` claim of a JWT.
pub(crate) fn jwt_expiry(tok: &str) -> Option<SystemTime> {
    let claims = jwt_claims_json(tok)?;
    let exp = claims.get("exp")?.as_i64()?;
    if exp == 0 {
        return None;
    }
    Some(UNIX_EPOCH + Duration::from_secs(exp.max(0) as u64))
}

/// Decodes a JWT's payload without verifying it.
pub(crate) fn jwt_claims_json(tok: &str) -> Option<serde_json::Value> {
    let parts: Vec<&str> = tok.split('.').collect();
    if parts.len() != 3 {
        return None;
    }
    let b = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(parts[1])
        .ok()?;
    serde_json::from_slice(&b).ok()
}
