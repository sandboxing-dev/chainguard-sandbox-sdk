//! The gRPC client for the Sessions API.

use std::time::Duration;

use tonic::metadata::MetadataValue;
use tonic::transport::{Channel, ClientTlsConfig, Endpoint};

use super::auth::{SharedTokenSource, default_token_source, env_nonempty};
use super::orgs::{detect_instance, platform_token_source};
use super::pb::sessions_client::SessionsClient;
use super::{DEFAULT_URL, ENV_AUTH_TOKEN, ENV_INSTANCE, ENV_NO_DETECT_INSTANCE, ENV_SESSIONS_URL};
use crate::{Error, Result};

crate::opaque_debug!(Options);

/// The user agent sent with every call unless [`Options::user_agent`] says otherwise.
pub const DEFAULT_USER_AGENT: &str = "chainguard-sandbox-sdk-rs";

/// Configures a [`Client`].
#[derive(Clone, Default)]
pub struct Options {
    /// The sandbox plane's URL, e.g. `https://sessions.chainbox.dev` or `http://localhost:8080`
    /// (plaintext). Defaults to `$CHAINCTL_SESSIONS_URL`, then [`DEFAULT_URL`].
    pub url: String,
    /// The organization ID, used as "instances/{instance}". If empty, `$CHAINCTL_SANDBOX_INSTANCE`
    /// is used; if that is empty too, the organization is auto-detected (see
    /// [`detect_instance`]) unless `no_detect_instance` is set.
    pub instance: String,
    /// Turns off organization auto-detection (also `$CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1`).
    pub no_detect_instance: bool,
    /// Mints account-API tokens for auto-detection. Defaults to [`platform_token_source`] when
    /// `token_source` is unset and `$CHAINCTL_AUTH_TOKEN` is empty; otherwise detection needs it.
    pub platform_token_source: Option<SharedTokenSource>,
    /// Supplies bearer tokens for [`DEFAULT_AUDIENCE`](super::DEFAULT_AUDIENCE). Defaults to
    /// [`default_token_source`].
    pub token_source: Option<SharedTokenSource>,
    /// Sent with every call (default [`DEFAULT_USER_AGENT`]).
    pub user_agent: String,
}

/// A connection to the Sessions service. Cloning is cheap and shares the connection.
#[derive(Clone)]
pub struct Client {
    raw: SessionsClient<Channel>,
    ts: SharedTokenSource,
    instance: String,
    detect_err: Option<String>,
}

impl std::fmt::Debug for Client {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Client")
            .field("instance", &self.instance)
            .finish_non_exhaustive()
    }
}

impl Client {
    /// Connects to the sandbox plane (lazily: the connection is made on the first call).
    pub async fn new(mut o: Options) -> Result<Client> {
        if o.url.is_empty() {
            o.url = env_nonempty(ENV_SESSIONS_URL).unwrap_or_else(|| DEFAULT_URL.into());
        }
        if o.instance.is_empty() {
            o.instance = std::env::var(ENV_INSTANCE).unwrap_or_default();
        }
        let mut detect_err = None;
        if o.instance.is_empty()
            && !o.no_detect_instance
            && std::env::var(ENV_NO_DETECT_INSTANCE).as_deref() != Ok("1")
        {
            let mut pts = o.platform_token_source.clone();
            if pts.is_none() && o.token_source.is_none() && env_nonempty(ENV_AUTH_TOKEN).is_none() {
                pts = Some(platform_token_source());
            }
            match pts {
                None => {
                    detect_err = Some(format!(
                        "needs Options::platform_token_source when a custom token source or ${ENV_AUTH_TOKEN} is used"
                    ))
                }
                Some(pts) => {
                    // Room for a browser login.
                    match tokio::time::timeout(
                        Duration::from_secs(11 * 60),
                        detect_instance(Some(pts)),
                    )
                    .await
                    {
                        Ok(Ok(id)) => o.instance = id,
                        Ok(Err(e)) => detect_err = Some(e.to_string()),
                        Err(_) => detect_err = Some("timed out".into()),
                    }
                }
            }
        } else if o.instance.is_empty() {
            detect_err = Some("turned off".into());
        }
        let ts = o.token_source.clone().unwrap_or_else(default_token_source);
        let ua = if o.user_agent.is_empty() {
            DEFAULT_USER_AGENT.to_string()
        } else {
            o.user_agent.clone()
        };
        let channel = channel(&o.url, &ua)?;
        Ok(Client {
            raw: SessionsClient::new(channel),
            ts,
            instance: o.instance,
            detect_err,
        })
    }

    /// Builds a client on an existing channel. `instance` may be empty if only full resource
    /// names are used.
    pub fn from_channel(
        channel: Channel,
        ts: SharedTokenSource,
        instance: impl Into<String>,
    ) -> Client {
        Client {
            raw: SessionsClient::new(channel),
            ts,
            instance: instance.into(),
            detect_err: None,
        }
    }

    /// The generated gRPC client, for calls this crate doesn't wrap. It covers only the SDK's
    /// subset of the Sessions API. Authorize each request with [`Client::request`].
    pub fn raw(&self) -> SessionsClient<Channel> {
        self.raw.clone()
    }

    /// The resource name "instances/{id}" used as the parent of sessions.
    pub fn instance(&self) -> String {
        super::instance_name(&self.instance)
    }

    /// The token source the client authorizes calls with.
    pub fn token_source(&self) -> SharedTokenSource {
        self.ts.clone()
    }

    pub(crate) fn parent(&self) -> Result<String> {
        if self.instance.is_empty() {
            return Err(Error::NoInstance {
                detection: self.detect_err.clone(),
            });
        }
        Ok(super::instance_name(&self.instance))
    }

    /// Wraps a message in a request carrying the bearer token.
    pub async fn request<T>(&self, msg: T) -> Result<tonic::Request<T>> {
        let tok = self.ts.token().await?;
        let mut r = tonic::Request::new(msg);
        let v: MetadataValue<_> = format!("Bearer {tok}")
            .parse()
            .map_err(|_| Error::other("workspaces: the token is not a valid header value"))?;
        r.metadata_mut().insert("authorization", v);
        Ok(r)
    }

    /// Converts a call's result, dropping the cached token when the plane rejects it.
    pub(crate) fn check<T>(&self, r: std::result::Result<T, tonic::Status>) -> Result<T> {
        r.map_err(|s| self.status(s))
    }

    pub(crate) fn status(&self, s: tonic::Status) -> Error {
        if s.code() == tonic::Code::Unauthenticated {
            self.ts.invalidate();
        }
        Error::Rpc(s)
    }
}

/// Builds the channel for a URL: `https://` gets TLS with the system's roots, `http://` is
/// plaintext, and a URL without a scheme is taken as `https://`.
fn channel(raw: &str, ua: &str) -> Result<Channel> {
    let raw = if raw.contains("://") {
        raw.to_string()
    } else {
        format!("https://{raw}")
    };
    let u = url::Url::parse(&raw)
        .map_err(|e| Error::other(format!("parsing sessions URL {raw:?}: {e}")))?;
    let secure = match u.scheme() {
        "https" => true,
        "http" => false,
        s => {
            return Err(Error::other(format!(
                "sessions URL {raw:?}: unsupported scheme {s:?}"
            )));
        }
    };
    let host = u
        .host_str()
        .ok_or_else(|| Error::other(format!("sessions URL {raw:?}: no host")))?;
    let port = u.port().unwrap_or(if secure { 443 } else { 80 });
    let target = format!("{}://{}:{port}", u.scheme(), host);
    let mut ep = Endpoint::from_shared(target)
        .map_err(|e| Error::transport(format!("dialing {raw}"), e))?
        .user_agent(ua)
        .map_err(|e| Error::transport(format!("dialing {raw}"), e))?
        .connect_timeout(Duration::from_secs(30))
        .http2_keep_alive_interval(Duration::from_secs(30))
        .keep_alive_while_idle(true);
    if secure {
        let tls = ClientTlsConfig::new()
            .with_native_roots()
            .domain_name(host.trim_start_matches('[').trim_end_matches(']'));
        ep = ep
            .tls_config(tls)
            .map_err(|e| Error::transport(format!("dialing {raw}"), e))?;
    }
    Ok(ep.connect_lazy())
}
