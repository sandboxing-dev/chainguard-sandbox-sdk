//! Identity exchange for machines without a browser: CI jobs, servers, Kubernetes pods.

use std::sync::Arc;

use super::auth::{BoxFuture, TokenSource};
use super::{DEFAULT_AUDIENCE, DEFAULT_ISSUER, http};
use crate::{Error, Result};

crate::opaque_debug!(IdentityExchange);

/// Returns the external OIDC token presented to the STS.
pub type IdTokenFn = Arc<dyn Fn() -> BoxFuture<'static, Result<String>> + Send + Sync>;

/// A [`TokenSource`] that trades a token from an external OIDC issuer (GitHub Actions, GitLab, a
/// Kubernetes service account, ...) for a Chainguard token, acting as a Chainguard *assumable
/// identity* that trusts that issuer and subject.
///
/// One-time setup, by an organization owner:
///
/// ```text
/// chainctl iam identities create <name> --parent <org> \
///   --identity-issuer <issuer URL> --subject <subject> --role sandbox.user
/// ```
///
/// It calls the same SecurityTokenService/Exchange API chainctl uses, over its HTTP binding
/// (`POST /sts/exchange`). Wrap it in [`cached`](super::cached).
#[derive(Clone)]
pub struct IdentityExchange {
    /// The assumable identity's ID (from `chainctl iam identities list`).
    pub identity: String,
    /// See [`github_actions_id_token`], [`env_id_token`] and [`file_id_token`].
    pub id_token: IdTokenFn,
    /// Defaults to [`DEFAULT_ISSUER`].
    pub issuer: String,
    /// Defaults to [`DEFAULT_AUDIENCE`].
    pub audience: String,
}

impl TokenSource for IdentityExchange {
    fn token(&self) -> BoxFuture<'_, Result<String>> {
        Box::pin(async move {
            if self.identity.is_empty() {
                return Err(Error::other(
                    "workspaces: IdentityExchange needs identity and id_token",
                ));
            }
            let issuer = if self.issuer.is_empty() {
                DEFAULT_ISSUER
            } else {
                &self.issuer
            };
            let aud = if self.audience.is_empty() {
                DEFAULT_AUDIENCE
            } else {
                &self.audience
            };
            let idt = (self.id_token)()
                .await
                .map_err(|e| Error::other(format!("getting the OIDC token to exchange: {e}")))?;
            let url = format!(
                "{}/sts/exchange?{}",
                issuer.trim_end_matches('/'),
                http::query(&[("aud", aud), ("identity", &self.identity)])
            );
            let req = http::client()?
                .post(url)
                .header("Content-Length", "0")
                .header("Authorization", format!("Bearer {idt}"))
                .header("Accept", "application/json");
            let a = http::send(req, 1 << 20, "STS exchange").await?;
            if a.status != 200 {
                return Err(Error::Http {
                    context: format!("STS exchange for identity {}", self.identity),
                    status: a.status,
                    message: http::error_message(&a.body),
                });
            }
            #[derive(serde::Deserialize)]
            struct Tok {
                #[serde(default)]
                token: String,
            }
            match serde_json::from_slice::<Tok>(&a.body) {
                Ok(t) if !t.token.is_empty() => Ok(t.token),
                _ => Err(Error::other("STS exchange: response has no token")),
            }
        })
    }
}

/// Fetches a GitHub Actions OIDC token whose audience is the Chainguard issuer (`""` =
/// [`DEFAULT_ISSUER`]). The workflow needs `permissions: id-token: write`.
pub fn github_actions_id_token(issuer: &str) -> IdTokenFn {
    let issuer = if issuer.is_empty() {
        DEFAULT_ISSUER.to_string()
    } else {
        issuer.to_string()
    };
    Arc::new(move || {
        let issuer = issuer.clone();
        Box::pin(async move {
            let (Ok(req_url), Ok(req_tok)) = (
                std::env::var("ACTIONS_ID_TOKEN_REQUEST_URL"),
                std::env::var("ACTIONS_ID_TOKEN_REQUEST_TOKEN"),
            ) else {
                return Err(Error::other(
                    "not in a GitHub Actions job with `permissions: id-token: write`",
                ));
            };
            if req_url.is_empty() || req_tok.is_empty() {
                return Err(Error::other(
                    "not in a GitHub Actions job with `permissions: id-token: write`",
                ));
            }
            let mut u = url::Url::parse(&req_url).map_err(|e| Error::other(e.to_string()))?;
            let kept: Vec<(String, String)> = u
                .query_pairs()
                .into_owned()
                .filter(|(k, _)| k != "audience")
                .collect();
            u.query_pairs_mut()
                .clear()
                .extend_pairs(kept)
                .append_pair("audience", &issuer);
            let req = http::client()?
                .get(u)
                .header("Authorization", format!("Bearer {req_tok}"));
            let a = http::send(req, 1 << 20, "GitHub Actions OIDC token request").await?;
            #[derive(serde::Deserialize)]
            struct V {
                #[serde(default)]
                value: String,
            }
            match serde_json::from_slice::<V>(&a.body) {
                Ok(v) if a.status == 200 && !v.value.is_empty() => Ok(v.value),
                _ => Err(Error::other(format!(
                    "GitHub Actions OIDC token request failed: HTTP {}",
                    a.status
                ))),
            }
        })
    })
}

/// Reads the OIDC token from an environment variable (e.g. a GitLab CI `id_tokens:` variable).
pub fn env_id_token(name: &str) -> IdTokenFn {
    let name = name.to_string();
    Arc::new(move || {
        let name = name.clone();
        Box::pin(async move {
            match std::env::var(&name) {
                Ok(v) if !v.trim().is_empty() => Ok(v.trim().to_string()),
                _ => Err(Error::other(format!("${name} is empty"))),
            }
        })
    })
}

/// Reads the OIDC token from a file on every call (e.g. a Kubernetes projected service-account
/// token, which is rotated in place).
pub fn file_id_token(path: impl Into<std::path::PathBuf>) -> IdTokenFn {
    let path = path.into();
    Arc::new(move || {
        let path = path.clone();
        Box::pin(async move {
            let b = tokio::fs::read(&path)
                .await
                .map_err(|e| Error::io(path.display().to_string(), e))?;
            let v = String::from_utf8_lossy(&b).trim().to_string();
            if v.is_empty() {
                return Err(Error::other(format!("{} is empty", path.display())));
            }
            Ok(v)
        })
    })
}
