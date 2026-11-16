//! Turning Workspaces on for an organization.
//!
//! Sessions can be created only in an organization that is entitled to Workspaces.
//! `chainctl develop init` gets an organization there in these steps, each a call to Chainguard's
//! platform API ([`DEFAULT_PLATFORM_URL`](super::DEFAULT_PLATFORM_URL)):
//!
//! 1. Find out which legal documents the organization has accepted:
//!    `GET /iam/v1/terms/{org}` ([`accepted_terms`])
//! 2. Accept the ones turning it on needs ([`ENABLE_TERMS`]):
//!    `POST /iam/v1/terms/{org}` ([`accept_terms`])
//! 3. Link a GitHub account (an organization, or your own user) to the organization. The service
//!    refuses step 4 without it. The Chainguard GitHub App must be installed on that account
//!    ([`GITHUB_APP_INSTALL_URL`]).
//!    `GET /guardener/v1alpha1/githubAssociations/{org}` ([`linked_github_organizations`]),
//!    `GET /guardener/v1alpha1/githubAssociations:begin` ([`begin_github_oauth`]),
//!    GitHub's authorize page, back to a loopback port ([`authorize_github`]),
//!    `POST /guardener/v1alpha1/githubAssociations/{org}:link` ([`link_github_organization`])
//! 4. Turn the feature on, with the plan's allowance:
//!    `POST /entitlements/v1alpha1/features/{org}:enableSandbox` ([`enable_sandbox`])
//! 5. Read the result: `GET /entitlements/v2beta1/organizationEntitlements?parent={org}`
//!    ([`workspaces_entitlement`])
//!
//! Creating the organization itself (sign-up) is not covered: use the Chainguard Console.
//!
//! All of these need a token for the platform API ([`platform_token_source`]), and steps 2 to 4
//! need the owner role in the organization. Step 3 also needs you to own the GitHub account.

use std::sync::{Arc, Mutex};

use base64::Engine as _;
use sha2::Digest as _;
use tokio::sync::oneshot;

use super::auth::SharedTokenSource;
use super::http;
use super::login::{MessageSink, UrlOpener, open_browser, say};
use super::loopback::{LoopbackReply, LoopbackServer};
use super::orgs::{platform_token_source, platform_url};
use crate::{Error, Result};

crate::opaque_debug!(GitHubAuthorizeOptions);

/// A legal document an organization accepts.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TermsDocument {
    pub id: String,
    pub label: String,
    pub url: String,
}

/// The documents an organization must accept to use Workspaces: the Secured Platform Terms and
/// the Master Service and License Agreement.
pub const WORKSPACES_TERMS: &[&str] = &["secure-platform-terms.v1", "msla.v1"];

/// The documents an organization accepts when Workspaces is turned on the way
/// `chainctl develop init` does it: the Workspaces terms, the Code Analysis Terms (the same switch
/// turns on Chainguard Checks), and the Terms of Service and Data Privacy Agreement that linking
/// a GitHub account requires.
pub const ENABLE_TERMS: &[&str] = &[
    "secure-platform-terms.v1",
    "msla.v1",
    "code-analysis-terms.v1",
    "guardener-tos.v1",
    "sfdpa.v1",
];

/// Where the Chainguard GitHub App is installed on a GitHub organization or user.
pub const GITHUB_APP_INSTALL_URL: &str = "https://github.com/apps/chainguard/installations/new";

/// The loopback port the GitHub authorization redirects to (the one chainctl uses).
pub const DEFAULT_GITHUB_OAUTH_PORT: u16 = 8989;

/// The label and URL of a known document ID, or a document with only the ID set.
pub fn terms_document_info(id: &str) -> TermsDocument {
    let known = |label: &str, url: &str| TermsDocument {
        id: id.into(),
        label: label.into(),
        url: url.into(),
    };
    match id {
        "secure-platform-terms.v1" => known(
            "Secured Platform Terms",
            "https://www.chainguard.dev/legal/secure-platform-terms",
        ),
        "msla.v1" => known(
            "Master Service and License Agreement",
            "https://www.chainguard.dev/legal/master-service-and-license-agreement",
        ),
        "code-analysis-terms.v1" => known(
            "Code Analysis Terms",
            "https://www.chainguard.dev/legal/code-analysis-terms",
        ),
        "guardener-tos.v1" => known(
            "Terms of Service",
            "https://www.chainguard.dev/legal/guardener",
        ),
        "sfdpa.v1" => known(
            "Data Privacy Agreement",
            "https://www.chainguard.dev/legal/supplemental-dpa",
        ),
        _ => TermsDocument {
            id: id.into(),
            ..Default::default()
        },
    }
}

/// A non-2xx answer from the platform API.
#[derive(Debug, Clone, Default)]
pub struct PlatformError {
    /// The response status.
    pub http_status: u16,
    /// The gRPC status code the gateway reports (7 = permission denied, 9 = failed precondition).
    pub code: i64,
    pub message: String,
    /// The machine-readable reason, when the service gives one.
    pub reason: String,
    /// Set when the call was refused because the organization has not accepted these documents.
    pub missing_terms: Vec<TermsDocument>,
}

impl PlatformError {
    /// The organization has to link a GitHub account first.
    pub fn github_link_required(&self) -> bool {
        self.reason == "GUARDENER_GITHUB_LINK_REQUIRED"
            || self.message.contains("must link a GitHub organization")
    }
    /// The Chainguard GitHub App is not installed on the GitHub account.
    pub fn github_app_not_installed(&self) -> bool {
        self.reason.ends_with("GITHUB_APP_NOT_INSTALLED")
            || self.message.contains("must be installed on the GitHub")
    }
    /// The GitHub account is linked to another Chainguard organization.
    pub fn github_already_linked(&self) -> bool {
        self.reason == "GUARDENER_GITHUB_ALREADY_LINKED"
            || self.message.contains("already linked to a different")
    }
}

impl std::fmt::Display for PlatformError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "platform API: HTTP {}: {}",
            self.http_status, self.message
        )
    }
}

impl std::error::Error for PlatformError {}

/// Sends one JSON request to the platform API and returns the decoded answer. `context` prefixes
/// errors ("listing accepted terms: platform API: HTTP 403: ...").
async fn platform_call(
    ts: &Option<SharedTokenSource>,
    method: reqwest::Method,
    path: &str,
    body: Option<serde_json::Value>,
    context: &str,
) -> Result<serde_json::Value> {
    let ts = ts.clone().unwrap_or_else(platform_token_source);
    let tok = ts.token().await?;
    let url = format!("{}{}", platform_url().trim_end_matches('/'), path);
    let mut req = http::client()?
        .request(method, url)
        .header("Authorization", format!("Bearer {tok}"))
        .header("Accept", "application/json");
    if let Some(b) = &body {
        req = req
            .header("Content-Type", "application/json")
            .body(b.to_string());
    }
    let a = http::send(req, 4 << 20, context).await?;
    if a.status == 401 {
        ts.invalidate();
    }
    if a.status / 100 != 2 {
        return Err(Error::Platform {
            context: context.into(),
            error: parse_platform_error(a.status, &a.body),
        });
    }
    if a.body.is_empty() {
        return Ok(serde_json::Value::Null);
    }
    serde_json::from_slice(&a.body).map_err(|e| Error::other(format!("{context}: {e}")))
}

pub(crate) fn parse_platform_error(status: u16, data: &[u8]) -> PlatformError {
    let v: serde_json::Value = serde_json::from_slice(data).unwrap_or_default();
    let mut e = PlatformError {
        http_status: status,
        code: v.get("code").and_then(|c| c.as_i64()).unwrap_or(0),
        message: v
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or_default()
            .to_string(),
        ..Default::default()
    };
    if e.message.is_empty() {
        e.message = String::from_utf8_lossy(data).trim().to_string();
    }
    // A refusal for unaccepted terms carries a detail listing the missing documents.
    for d in v
        .get("details")
        .and_then(|d| d.as_array())
        .into_iter()
        .flatten()
    {
        if let Some(r) = d.get("reason").and_then(|r| r.as_str())
            && !r.is_empty()
            && e.reason.is_empty()
        {
            e.reason = r.to_string();
        }
        for m in d
            .get("missing")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
        {
            let field = |k: &str| {
                m.get(k)
                    .or_else(|| m.get(k.to_uppercase()))
                    .or_else(|| m.get(format!("{}{}", &k[..1].to_uppercase(), &k[1..])))
                    .and_then(|x| x.as_str())
                    .unwrap_or_default()
                    .to_string()
            };
            let mut doc = terms_document_info(&field("id"));
            let (label, url) = (field("label"), field("url"));
            if !label.is_empty() {
                doc.label = label;
            }
            if !url.is_empty() {
                doc.url = url;
            }
            e.missing_terms.push(doc);
        }
    }
    e
}

fn org_path(org: &str) -> Result<String> {
    if org.is_empty() || org.contains('?') || org.contains('#') {
        return Err(Error::other(format!(
            "workspaces: invalid organization ID {org:?}"
        )));
    }
    Ok(http::path_escape(org))
}

/// The IDs of the legal documents the organization has accepted. `ts` must mint platform tokens
/// (`None`: [`platform_token_source`]).
pub async fn accepted_terms(ts: Option<SharedTokenSource>, org: &str) -> Result<Vec<String>> {
    let p = org_path(org)?;
    let out = platform_call(
        &ts,
        reqwest::Method::GET,
        &format!("/iam/v1/terms/{p}"),
        None,
        "listing accepted terms",
    )
    .await?;
    Ok(out
        .get("items")
        .and_then(|i| i.as_array())
        .into_iter()
        .flatten()
        .filter_map(|it| it.get("docId").and_then(|d| d.as_str()).map(String::from))
        .collect())
}

/// The documents Workspaces needs that the organization has not accepted yet.
pub async fn missing_workspaces_terms(
    ts: Option<SharedTokenSource>,
    org: &str,
) -> Result<Vec<TermsDocument>> {
    let have = accepted_terms(ts, org).await?;
    Ok(WORKSPACES_TERMS
        .iter()
        .filter(|id| !have.iter().any(|h| h == *id))
        .map(|id| terms_document_info(id))
        .collect())
}

/// Records that the organization accepts the documents with these IDs. This is a legal act on the
/// organization's behalf: call it only when the person running your program has agreed to the
/// documents. It needs the owner role; accepting a document again changes nothing.
pub async fn accept_terms(
    ts: Option<SharedTokenSource>,
    org: &str,
    document_ids: &[String],
) -> Result<()> {
    let p = org_path(org)?;
    if document_ids.is_empty() {
        return Err(Error::other("workspaces: no documents to accept"));
    }
    let body = serde_json::json!({ "documentIds": document_ids });
    platform_call(
        &ts,
        reqwest::Method::POST,
        &format!("/iam/v1/terms/{p}"),
        Some(body),
        "accepting terms",
    )
    .await?;
    Ok(())
}

/// Turns on Checks and Workspaces for the organization, with its plan's monthly allowance. An
/// allowance Chainguard already granted is kept. It needs the owner role, and the organization
/// must have accepted the terms first: otherwise the error's [`PlatformError::missing_terms`]
/// lists the documents.
pub async fn enable_sandbox(ts: Option<SharedTokenSource>, org: &str) -> Result<()> {
    let p = org_path(org)?;
    let path = format!("/entitlements/v1alpha1/features/{p}:enableSandbox");
    platform_call(
        &ts,
        reqwest::Method::POST,
        &path,
        Some(serde_json::json!({})),
        "turning on Workspaces",
    )
    .await?;
    Ok(())
}

/// An organization's Workspaces entitlement.
#[derive(Debug, Clone, Default)]
pub struct Entitlement {
    /// The plan's ID ("starter", "team", ...).
    pub plan: String,
    /// The plan's display name.
    pub plan_name: String,
    /// Whether Workspaces is turned on.
    pub active: bool,
    /// The raw state ("STATE_ACTIVE", "STATE_NOT_INCLUDED", or "" when the product isn't listed).
    pub state: String,
    /// The concurrent-session limits (0: not reported).
    pub max_sandboxes: i64,
    pub max_sandboxes_per_user: i64,
}

/// Whether Workspaces is turned on for the organization, its plan, and its session limits.
pub async fn workspaces_entitlement(
    ts: Option<SharedTokenSource>,
    org: &str,
) -> Result<Entitlement> {
    org_path(org)?;
    let q = http::query(&[("parent", org), ("products", "PRODUCT_SANDBOX_WORKSPACES")]);
    let out = platform_call(
        &ts,
        reqwest::Method::GET,
        &format!("/entitlements/v2beta1/organizationEntitlements?{q}"),
        None,
        "reading the organization's entitlements",
    )
    .await?;
    let s = |v: &serde_json::Value, k: &str| {
        v.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let plan = out.get("plan").cloned().unwrap_or_default();
    let mut e = Entitlement {
        plan: s(&plan, "id"),
        plan_name: s(&plan, "displayName"),
        ..Default::default()
    };
    for p in out
        .get("products")
        .and_then(|p| p.as_array())
        .into_iter()
        .flatten()
    {
        if s(p, "product") != "PRODUCT_SANDBOX_WORKSPACES" {
            continue;
        }
        e.state = s(p, "state");
        e.active = e.state == "STATE_ACTIVE";
        for l in p
            .get("limits")
            .and_then(|l| l.as_array())
            .into_iter()
            .flatten()
        {
            let n: i64 = match l.get("max") {
                Some(serde_json::Value::String(m)) => m.trim().parse().unwrap_or(0),
                Some(serde_json::Value::Number(m)) => m.as_i64().unwrap_or(0),
                _ => 0,
            };
            match s(l, "displayName").as_str() {
                "concurrent workspaces" => e.max_sandboxes = n,
                "concurrent workspaces per user" => e.max_sandboxes_per_user = n,
                _ => {}
            }
        }
    }
    Ok(e)
}

/// A GitHub account linked to a Chainguard organization.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinkedGitHub {
    /// The GitHub login: an organization, or a user.
    pub organization: String,
}

/// The GitHub accounts linked to the organization.
pub async fn linked_github_organizations(
    ts: Option<SharedTokenSource>,
    org: &str,
) -> Result<Vec<LinkedGitHub>> {
    let p = org_path(org)?;
    let out = platform_call(
        &ts,
        reqwest::Method::GET,
        &format!("/guardener/v1alpha1/githubAssociations/{p}"),
        None,
        "listing linked GitHub accounts",
    )
    .await?;
    Ok(out
        .get("organizations")
        .and_then(|o| o.as_array())
        .into_iter()
        .flatten()
        .map(|o| LinkedGitHub {
            organization: o
                .get("organization")
                .and_then(|x| x.as_str())
                .unwrap_or_default()
                .to_string(),
        })
        .collect())
}

/// What the service hands out to start a GitHub authorization: the Chainguard GitHub App's OAuth
/// client ID, and a state value the service signed and checks again when the account is linked.
#[derive(Debug, Clone)]
pub struct GitHubOAuthStart {
    pub client_id: String,
    pub state: String,
}

/// Starts a GitHub authorization.
pub async fn begin_github_oauth(ts: Option<SharedTokenSource>) -> Result<GitHubOAuthStart> {
    let ctx = "starting the GitHub authorization";
    let out = platform_call(
        &ts,
        reqwest::Method::GET,
        "/guardener/v1alpha1/githubAssociations:begin",
        None,
        ctx,
    )
    .await?;
    let s = |k: &str| {
        out.get(k)
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string()
    };
    let (client_id, state) = (s("clientId"), s("state"));
    if client_id.is_empty() || state.is_empty() {
        return Err(Error::other(format!(
            "{ctx}: the service returned no client ID or state"
        )));
    }
    Ok(GitHubOAuthStart { client_id, state })
}

/// The result of [`authorize_github`]. It holds no GitHub token: the service exchanges the code
/// itself when the account is linked.
#[derive(Clone)]
pub struct GitHubAuthorization {
    pub code: String,
    pub redirect_uri: String,
    pub verifier: String,
    pub state: String,
}

impl std::fmt::Debug for GitHubAuthorization {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GitHubAuthorization")
            .field("redirect_uri", &self.redirect_uri)
            .finish_non_exhaustive()
    }
}

/// Configures [`authorize_github`].
#[derive(Clone, Default)]
pub struct GitHubAuthorizeOptions {
    /// The loopback port to receive GitHub's redirect on (0: [`DEFAULT_GITHUB_OAUTH_PORT`]).
    pub port: u16,
    /// Print the URL instead of opening a browser.
    pub no_browser: bool,
    /// Opens the authorization page; returns false if it couldn't (the URL is then printed).
    /// Defaults to [`open_browser`].
    pub open_url: Option<UrlOpener>,
    /// Receives the instructions (default stderr).
    pub out: Option<MessageSink>,
}

/// Sends the person to GitHub to authorize the Chainguard GitHub App and waits for GitHub's
/// redirect to a loopback port. It is the flow chainctl uses: PKCE with S256, the account picker
/// forced, and the state checked on return. Bound it with `tokio::time::timeout`.
pub async fn authorize_github(
    start: &GitHubOAuthStart,
    o: GitHubAuthorizeOptions,
) -> Result<GitHubAuthorization> {
    let port = if o.port == 0 {
        DEFAULT_GITHUB_OAUTH_PORT
    } else {
        o.port
    };
    let redirect = format!("http://127.0.0.1:{port}/callback");
    let mut raw = [0u8; 32];
    getrandom::fill(&mut raw).map_err(|e| Error::other(e.to_string()))?;
    let verifier = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw);
    let challenge = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(sha2::Sha256::digest(verifier.as_bytes()));
    let auth_url = format!(
        "https://github.com/login/oauth/authorize?{}",
        http::query(&[
            ("client_id", &start.client_id),
            ("redirect_uri", &redirect),
            ("response_type", "code"),
            ("state", &start.state),
            ("code_challenge", &challenge),
            ("code_challenge_method", "S256"),
            ("prompt", "select_account"),
            ("allow_signup", "false"),
        ])
    );

    let (tx, rx) = oneshot::channel::<Result<String>>();
    let tx = Arc::new(Mutex::new(Some(tx)));
    let want_state = start.state.clone();
    let server = LoopbackServer::bind(port, move |req| {
        if req.path != "/callback" {
            return LoopbackReply::text(404, &[], "404 page not found");
        }
        let q = |k: &str| req.query.get(k).cloned().unwrap_or_default();
        // "Connection: close" (always sent): the browser must not keep this connection for a
        // later attempt, which gets a new server on the same port.
        let (reply, res) = if !q("error").is_empty() {
            (
                LoopbackReply::text(400, &[], "Authorization was denied. You can close this window."),
                Err(Error::other(format!("GitHub refused the authorization: {}: {}", q("error"), q("error_description")))),
            )
        } else if q("state") != want_state {
            (
                LoopbackReply::text(400, &[], "Authorization state mismatch. You can close this window."),
                Err(Error::other("GitHub's redirect carried the wrong state")),
            )
        } else if q("code").is_empty() {
            (
                LoopbackReply::text(400, &[], "Missing authorization code. You can close this window."),
                Err(Error::other("GitHub's redirect carried no authorization code")),
            )
        } else {
            (LoopbackReply::text(200, &[], "Authorization complete. You can close this window and return to the terminal.\n"), Ok(q("code")))
        };
        if let Some(tx) = tx.lock().unwrap().take() {
            let _ = tx.send(res);
        }
        reply
    })
    .await
    .map_err(|e| {
        Error::io(format!("listening on loopback port {port} for GitHub's redirect (choose another port if it is taken)"), e)
    })?;

    let opened = !o.no_browser
        && match &o.open_url {
            Some(f) => f(&auth_url),
            None => open_browser(&auth_url),
        };
    if opened {
        say(
            &o.out,
            &format!(
                "Opened a browser to authorize with GitHub. If it didn't open, visit:\n  {auth_url}"
            ),
        );
    } else {
        say(
            &o.out,
            &format!("Open this URL in a browser to authorize with GitHub:\n  {auth_url}"),
        );
    }
    let res = rx.await.unwrap_or_else(|_| {
        Err(Error::other(
            "waiting for the GitHub authorization: the server stopped",
        ))
    });
    // Let the browser finish loading the result page.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    server.stop();
    let code = res?;
    Ok(GitHubAuthorization {
        code,
        redirect_uri: redirect,
        verifier,
        state: start.state.clone(),
    })
}

/// Links a GitHub account to the Chainguard organization, using an authorization from
/// [`authorize_github`]. `github_login` is the account to link, an organization or a user; empty
/// means the GitHub user who authorized. The Chainguard GitHub App must be installed on the
/// account (otherwise [`PlatformError::github_app_not_installed`]), and you must own it. An
/// account can be linked to one Chainguard organization at a time.
pub async fn link_github_organization(
    ts: Option<SharedTokenSource>,
    org: &str,
    github_login: &str,
    a: &GitHubAuthorization,
) -> Result<LinkedGitHub> {
    let p = org_path(org)?;
    let mut body = serde_json::json!({ "code": a.code, "redirectUri": a.redirect_uri, "codeVerifier": a.verifier, "state": a.state });
    if !github_login.is_empty() {
        body["organization"] = serde_json::Value::String(github_login.into());
    }
    let path = format!("/guardener/v1alpha1/githubAssociations/{p}:link");
    let out = platform_call(
        &ts,
        reqwest::Method::POST,
        &path,
        Some(body),
        "linking the GitHub account",
    )
    .await?;
    Ok(LinkedGitHub {
        organization: out
            .get("organization")
            .and_then(|x| x.as_str())
            .unwrap_or_default()
            .to_string(),
    })
}
