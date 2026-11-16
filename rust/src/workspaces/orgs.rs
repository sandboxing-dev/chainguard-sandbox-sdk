//! Organizations: listing them and choosing the one a client works in.
//!
//! The organization ("instance") can be given three ways, in this order:
//!
//! 1. explicitly: `Options::instance` (the 40-hex organization ID);
//! 2. the environment: `$CHAINCTL_SANDBOX_INSTANCE`;
//! 3. auto-detection: if neither is set, the client lists the organizations your identity belongs
//!    to and uses the only one. With several, it reports them. Turn it off with
//!    `Options::no_detect_instance` or `$CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1`.

use std::sync::Arc;

use super::auth::{SharedTokenSource, cached};
use super::login::{BrowserLoginToken, LoginOptions};
use super::{ChainctlToken, DEFAULT_PLATFORM_URL, PLATFORM_AUDIENCE, http};
use crate::{Error, Result};

/// A Chainguard organization (a root IAM group).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    /// The organization's 40-hex ID, used as the sessions instance.
    pub id: String,
    pub name: String,
}

pub(crate) fn platform_url() -> String {
    std::env::var("CHAINGUARD_SANDBOX_PLATFORM_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or_else(|| DEFAULT_PLATFORM_URL.into())
}

/// Returns the organizations visible to the token source's identity, sorted by name. `ts` must
/// mint tokens for [`PLATFORM_AUDIENCE`]; `None` uses [`platform_token_source`]. It calls the IAM
/// Groups API: `GET /iam/v1/groups?uidp.in_root=true`.
pub async fn list_organizations(ts: Option<SharedTokenSource>) -> Result<Vec<Organization>> {
    let ts = ts.unwrap_or_else(platform_token_source);
    let tok = ts.token().await?;
    let url = format!(
        "{}/iam/v1/groups?uidp.in_root=true",
        platform_url().trim_end_matches('/')
    );
    let req = http::client()?
        .get(url)
        .header("Authorization", format!("Bearer {tok}"))
        .header("Accept", "application/json");
    let a = http::send(req, 4 << 20, "listing organizations").await?;
    if a.status == 401 {
        ts.invalidate();
    }
    if a.status != 200 {
        return Err(Error::Http {
            context: "listing organizations".into(),
            status: a.status,
            message: http::error_message(&a.body),
        });
    }
    #[derive(serde::Deserialize)]
    struct Item {
        #[serde(default)]
        id: String,
        #[serde(default)]
        name: String,
    }
    #[derive(serde::Deserialize)]
    struct List {
        #[serde(default)]
        items: Vec<Item>,
    }
    let gl: List = serde_json::from_slice(&a.body)
        .map_err(|e| Error::other(format!("listing organizations: {e}")))?;
    let mut orgs: Vec<Organization> = gl
        .items
        .into_iter()
        .filter(|g| !g.id.is_empty() && !g.id.contains('/')) // root groups only
        .map(|g| Organization {
            id: g.id,
            name: g.name,
        })
        .collect();
    orgs.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(orgs)
}

/// The ID of the only organization the identity belongs to ([`Error::NoOrganization`] or
/// [`Error::AmbiguousOrganization`] otherwise).
pub async fn detect_instance(ts: Option<SharedTokenSource>) -> Result<String> {
    let orgs = list_organizations(ts).await?;
    match orgs.len() {
        0 => Err(Error::NoOrganization),
        1 => Ok(orgs[0].id.clone()),
        _ => Err(Error::AmbiguousOrganization {
            organizations: orgs,
        }),
    }
}

/// Mints tokens for the account API ([`PLATFORM_AUDIENCE`]) the same way
/// [`default_token_source`](super::default_token_source) does for sessions: from the saved browser
/// login, or with chainctl when `$CHAINGUARD_SANDBOX_AUTH=chainctl`.
pub fn platform_token_source() -> SharedTokenSource {
    if std::env::var("CHAINGUARD_SANDBOX_AUTH").as_deref() == Ok("chainctl") {
        return cached(Arc::new(ChainctlToken {
            audience: PLATFORM_AUDIENCE.into(),
            ..Default::default()
        }));
    }
    cached(Arc::new(BrowserLoginToken {
        audience: PLATFORM_AUDIENCE.into(),
        login: LoginOptions {
            connection: std::env::var("CHAINGUARD_SANDBOX_SOCIAL_LOGIN").unwrap_or_default(),
            ..Default::default()
        },
        no_browser: std::env::var("CHAINGUARD_SANDBOX_NO_BROWSER").as_deref() == Ok("1"),
        ..Default::default()
    }))
}
