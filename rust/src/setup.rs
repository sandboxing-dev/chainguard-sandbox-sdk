//! Turning Workspaces on for an organization.
//!
//! Workspaces has to be turned on for an organization before sandboxes can be created in it.
//! [`workspaces_status`] reports whether it is, and [`enable_workspaces`] turns it on: the same
//! steps `chainctl develop init --parent ORG` takes for an organization that already exists
//! (accept the terms, link a GitHub account, then turn the feature on).

use std::time::Duration;

use crate::workspaces::{self, MessageSink, SharedTokenSource, TermsDocument, UrlOpener};
use crate::{Error, Result};

crate::opaque_debug!(SetupOptions);

/// Selects the organization for [`workspaces_status`] and [`enable_workspaces`].
#[derive(Clone, Default)]
pub struct SetupOptions {
    /// The organization ID. If empty, `$CHAINCTL_SANDBOX_INSTANCE` is used, and failing that the
    /// only organization you belong to.
    pub instance: String,
    /// Mints tokens for the platform API (default: the saved login).
    pub token_source: Option<SharedTokenSource>,
    /// Lets [`enable_workspaces`] accept, on the organization's behalf, the legal documents it has
    /// not accepted yet. Set it only when the person running your program has read and agreed to
    /// them ([`workspaces_status`] lists them, with URLs). Status ignores it.
    pub accept_terms: bool,
    /// The GitHub organization or user to link when the organization has none linked (enable
    /// only). Empty means the GitHub user who authorizes in the browser.
    pub github_account: String,
    /// The loopback port for GitHub's redirect (0: 8989).
    pub github_port: u16,
    /// Stops enabling from opening a browser for the GitHub authorization; it then fails with
    /// [`Error::GitHubLinkRequired`] if a link is needed.
    pub no_browser: bool,
    /// Receives instructions during the GitHub authorization (default stderr).
    pub out: Option<MessageSink>,
    /// Opens GitHub's authorization page (default: the system browser).
    pub open_url: Option<UrlOpener>,
}

/// An organization's Workspaces setup.
#[derive(Debug, Clone, Default)]
pub struct WorkspacesStatus {
    /// The organization ID the status is for.
    pub organization: String,
    /// Whether Workspaces is turned on. Sandboxes can be created when it is and `missing_terms`
    /// is empty.
    pub enabled: bool,
    /// The required legal documents not accepted yet. While Workspaces is off these are all the
    /// documents turning it on needs.
    pub missing_terms: Vec<TermsDocument>,
    /// The GitHub organizations or users linked to the organization. Turning Workspaces on needs
    /// at least one.
    pub github_accounts: Vec<String>,
    /// The organization's plan ("Starter", ...).
    pub plan: String,
    /// The plan's limits on sandboxes existing at once (0: not reported).
    pub max_sandboxes: i64,
    pub max_sandboxes_per_user: i64,
}

impl WorkspacesStatus {
    /// Whether sandboxes can be created.
    pub fn ready(&self) -> bool {
        self.enabled && self.missing_terms.is_empty()
    }
}

async fn setup_org(o: &SetupOptions) -> Result<String> {
    if !o.instance.is_empty() {
        return Ok(o.instance.clone());
    }
    if let Ok(v) = std::env::var(workspaces::ENV_INSTANCE)
        && !v.is_empty()
    {
        return Ok(v);
    }
    workspaces::detect_instance(o.token_source.clone()).await
}

/// Whether Workspaces is turned on for the organization and which required terms it still has to
/// accept. It changes nothing.
pub async fn workspaces_status(o: &SetupOptions) -> Result<WorkspacesStatus> {
    let org = setup_org(o).await?;
    let ts = o.token_source.clone();
    let ent = workspaces::workspaces_entitlement(ts.clone(), &org).await?;
    let mut st = WorkspacesStatus {
        organization: org.clone(),
        enabled: ent.active,
        plan: ent.plan_name,
        max_sandboxes: ent.max_sandboxes,
        max_sandboxes_per_user: ent.max_sandboxes_per_user,
        ..Default::default()
    };
    // Once Workspaces is on, only its own terms matter. While it is off, list everything turning
    // it on will need, so it can all be agreed to up front.
    let required = if ent.active {
        workspaces::WORKSPACES_TERMS
    } else {
        workspaces::ENABLE_TERMS
    };
    let have = workspaces::accepted_terms(ts.clone(), &org).await?;
    st.missing_terms = required
        .iter()
        .filter(|id| !have.iter().any(|h| h == *id))
        .map(|id| workspaces::terms_document_info(id))
        .collect();
    // A member who may not list the links still gets the rest of the status.
    if let Ok(linked) = workspaces::linked_github_organizations(ts, &org).await {
        st.github_accounts = linked.into_iter().map(|l| l.organization).collect();
    }
    Ok(st)
}

/// Turns Workspaces on for the organization, so sandboxes can be created in it. It is what
/// `chainctl develop init --parent ORG` does for an existing organization, without chainctl. It
/// needs the owner role, and does nothing if everything is already in place. The steps:
///
/// 1. Accept the terms. Only with `accept_terms`; without it, an organization with unaccepted
///    terms gets [`Error::TermsNotAccepted`] listing them.
/// 2. Link a GitHub account, if the organization has none: the service requires one. This opens a
///    browser on GitHub, where you choose the account, and waits for you to finish (up to 10
///    minutes). The Chainguard GitHub App must be installed on the account; if it is not, the
///    error is [`Error::GitHubAppNotInstalled`] with the page to install it from.
/// 3. Turn the feature on with the plan's allowance. This also turns on Chainguard Checks, which
///    the same switch controls.
pub async fn enable_workspaces(o: &SetupOptions) -> Result<WorkspacesStatus> {
    let st = workspaces_status(o).await?;
    if st.ready() {
        return Ok(st);
    }
    let ts = o.token_source.clone();
    let accept = async |docs: &[TermsDocument]| -> Result<()> {
        if !o.accept_terms {
            return Err(Error::TermsNotAccepted {
                missing: docs.to_vec(),
            });
        }
        let ids: Vec<String> = docs.iter().map(|d| d.id.clone()).collect();
        workspaces::accept_terms(ts.clone(), &st.organization, &ids).await
    };
    if !st.missing_terms.is_empty() {
        accept(&st.missing_terms).await?;
    }
    if !st.enabled && st.github_accounts.is_empty() {
        link_github(o, &st.organization).await?;
    }
    if !st.enabled {
        let mut r = workspaces::enable_sandbox(ts.clone(), &st.organization).await;
        // The link could not be listed (or was removed meanwhile): make it now.
        if let Err(e) = &r
            && e.platform_error().is_some_and(|p| p.github_link_required())
            && !st.github_accounts.is_empty()
        {
            r = match link_github(o, &st.organization).await {
                Ok(()) => workspaces::enable_sandbox(ts.clone(), &st.organization).await,
                Err(e) => Err(e),
            };
        }
        // The switch also covers Checks, whose terms the service may ask for too.
        if let Err(e) = &r
            && let Some(missing) = e
                .platform_error()
                .map(|p| p.missing_terms.clone())
                .filter(|m| !m.is_empty())
        {
            r = match accept(&missing).await {
                Ok(()) => workspaces::enable_sandbox(ts.clone(), &st.organization).await,
                Err(e) => Err(e),
            };
        }
        r?;
    }
    workspaces_status(o).await
}

/// Links a GitHub account to the organization: start the authorization, send the person to
/// GitHub, and hand the result to the service.
async fn link_github(o: &SetupOptions, org: &str) -> Result<()> {
    if o.no_browser {
        return Err(Error::GitHubLinkRequired {
            organization: org.into(),
        });
    }
    let start = workspaces::begin_github_oauth(o.token_source.clone()).await?;
    let ao = workspaces::GitHubAuthorizeOptions {
        port: o.github_port,
        out: o.out.clone(),
        open_url: o.open_url.clone(),
        no_browser: false,
    };
    let auth = tokio::time::timeout(
        Duration::from_secs(600),
        workspaces::authorize_github(&start, ao),
    )
    .await
    .map_err(|_| {
        Error::other("waiting for the GitHub authorization: no answer within 10 minutes")
    })??;
    match workspaces::link_github_organization(
        o.token_source.clone(),
        org,
        &o.github_account,
        &auth,
    )
    .await
    {
        Err(e)
            if e.platform_error()
                .is_some_and(|p| p.github_app_not_installed()) =>
        {
            Err(Error::GitHubAppNotInstalled {
                install_url: workspaces::GITHUB_APP_INSTALL_URL.into(),
                cause: Box::new(e),
            })
        }
        r => r.map(|_| ()),
    }
}
