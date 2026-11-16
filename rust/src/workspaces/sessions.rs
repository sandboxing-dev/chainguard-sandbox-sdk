//! Session resources: create, get, list, resolve, delete, park, heartbeat.

use std::time::Duration;

use super::pb::{
    CreateSessionRequest, DeleteSessionRequest, GetSessionRequest, HeartbeatSessionRequest,
    IdlePolicy, ListSessionsRequest, Provenance, Source, UpdateSessionRequest, Workspace, session,
};
use super::{Client, Session, exec::duration, repourl};
use crate::{Error, Result};

/// "instances/{id}"; an already-qualified name is returned unchanged.
pub fn instance_name(id: &str) -> String {
    if id.starts_with("instances/") {
        id.to_string()
    } else {
        format!("instances/{id}")
    }
}

/// Whether s is a full resource name "instances/{instance}/sessions/{session}".
pub fn is_session_name(s: &str) -> bool {
    let p: Vec<&str> = s.split('/').collect();
    p.len() == 4
        && p[0] == "instances"
        && p[2] == "sessions"
        && !p[1].is_empty()
        && !p[3].is_empty()
}

/// The environment document chainctl 0.2.398 uses for a directory without
/// .chainguard/develop.yaml or ci.yaml. Its check is named "develop".
pub const DEFAULT_ENVIRONMENT: &[u8] = include_bytes!("default_environment.yaml");

/// Describes a session opened from a repository URL, like `chainctl develop github.com/org/repo`:
/// the repository as `https://github.com/owner/repo`, idle AUTO_HOLD, and [`DEFAULT_ENVIRONMENT`] as
/// the fallback for repositories without .chainguard/ files. `revision` pins a commit (empty =
/// unpinned).
pub fn repository_session(repository: &str, revision: &str) -> Session {
    Session {
        idle: IdlePolicy::AutoHold as i32,
        environment: Some(session::Environment::Source(Source {
            repository: repository_url(repository),
            revision: revision.into(),
            fallback_document: DEFAULT_ENVIRONMENT.to_vec(),
            ..Default::default()
        })),
        ..Default::default()
    }
}

/// Normalizes "github.com/o/r", "git@github.com:o/r.git", "ssh://git@github.com:22/o/r" and
/// similar to `https://github.com/o/r`. Other URLs keep their scheme (credentials stripped);
/// scp-style remotes on other hosts are returned unchanged; a bare "host/path" gets https://.
pub fn repository_url(repo: &str) -> String {
    let u = repourl::normalize_github_url(repo);
    if u.contains("://") || repourl::is_scp_like(&u) {
        u
    } else {
        format!("https://{u}")
    }
}

/// Describes a session whose /work tree is uploaded on the first attach (see
/// [`Client::complete_first_attach`]): `label` identifies the workspace (the sandbox API uses
/// `/chainguard-sandbox/<name>`); `repository` and `revision` are optional metadata; `document` is
/// the environment document (YAML), `None` using [`DEFAULT_ENVIRONMENT`] with provenance
/// SYNTHESIZED. The idle policy is AUTO_HOLD, chainctl's default.
pub fn workspace_session(
    label: &str,
    repository: &str,
    revision: &str,
    document: Option<Vec<u8>>,
) -> Session {
    let (document, prov) = match document {
        Some(d) => (d, Provenance::Authored),
        None => (DEFAULT_ENVIRONMENT.to_vec(), Provenance::Synthesized),
    };
    Session {
        idle: IdlePolicy::AutoHold as i32,
        environment: Some(session::Environment::Workspace(Workspace {
            label: label.into(),
            repository: repository.into(),
            revision: revision.into(),
            document,
            provenance: prov as i32,
        })),
        ..Default::default()
    }
}

/// Filters [`Client::list`].
#[derive(Debug, Clone, Default)]
pub struct ListOptions {
    /// Lists another user's sessions (needs the owner role); empty means the caller's own.
    pub owner: String,
    pub page_size: i32,
}

impl Client {
    /// Creates a session in the client's instance.
    pub async fn create(&self, s: Session) -> Result<Session> {
        let parent = self.parent()?;
        let r = self
            .request(CreateSessionRequest {
                parent,
                session: Some(s),
            })
            .await?;
        Ok(self.check(self.raw().create_session(r).await)?.into_inner())
    }

    /// Fetches a session by full resource name.
    pub async fn get(&self, name: &str) -> Result<Session> {
        let r = self
            .request(GetSessionRequest { name: name.into() })
            .await?;
        Ok(self.check(self.raw().get_session(r).await)?.into_inner())
    }

    /// Every session in the instance, following page tokens.
    pub async fn list(&self, o: &ListOptions) -> Result<Vec<Session>> {
        let parent = self.parent()?;
        let mut out = Vec::new();
        let mut req = ListSessionsRequest {
            parent,
            page_size: o.page_size,
            owner: o.owner.clone(),
            ..Default::default()
        };
        loop {
            let r = self.request(req.clone()).await?;
            let resp = self.check(self.raw().list_sessions(r).await)?.into_inner();
            out.extend(resp.sessions);
            if resp.next_page_token.is_empty() {
                return Ok(out);
            }
            req.page_token = resp.next_page_token;
        }
    }

    /// Finds a session the way chainctl's SESSION argument does: a full resource name, a short ID,
    /// or a display name ("quiet-harbor", "app/quiet-harbor", "acme/app/quiet-harbor"). No match
    /// is [`Error::NotFound`] (what "session"); several are [`Error::Ambiguous`].
    pub async fn resolve(&self, reference: &str) -> Result<Session> {
        if is_session_name(reference) {
            return self.get(reference).await;
        }
        let all = self.list(&ListOptions::default()).await?;
        // Exact matches (resource ID, short ID, display name, or owner/display-name as chainctl
        // prints it) win over suffix matches.
        let (mut exact, mut suffix) = (Vec::new(), Vec::new());
        for s in all {
            match match_kind(&s, reference) {
                Match::Exact => exact.push(s),
                Match::Suffix => suffix.push(s),
                Match::None => {}
            }
        }
        let mut hits = if exact.is_empty() { suffix } else { exact };
        match hits.len() {
            0 => Err(Error::NotFound {
                what: "session".into(),
                name: reference.into(),
            }),
            1 => Ok(hits.remove(0)),
            _ => Err(Error::Ambiguous {
                reference: reference.into(),
            }),
        }
    }

    /// Tears down the guest and discards its snapshot (or removes a reaped tombstone).
    pub async fn delete(&self, name: &str) -> Result<()> {
        let r = self
            .request(DeleteSessionRequest { name: name.into() })
            .await?;
        self.check(self.raw().delete_session(r).await)?;
        Ok(())
    }

    /// Applies a field-masked update, e.g. paths "display_name".
    pub async fn update(&self, s: Session, paths: &[&str]) -> Result<Session> {
        let mask = prost_types::FieldMask {
            paths: paths.iter().map(|p| p.to_string()).collect(),
        };
        let r = self
            .request(UpdateSessionRequest {
                session: Some(s),
                update_mask: Some(mask),
            })
            .await?;
        Ok(self.check(self.raw().update_session(r).await)?.into_inner())
    }

    /// Asks the plane to snapshot and stop the VM ("Park now" in the menu).
    pub async fn park(&self, name: &str) -> Result<Session> {
        let s = Session {
            name: name.into(),
            park_request: session::ParkRequest::Park as i32,
            ..Default::default()
        };
        self.update(s, &["park_request"]).await
    }

    /// Marks the session in use; a non-zero `hold_for` keeps it running that long after detach
    /// (`--keep 8h`).
    pub async fn heartbeat(&self, name: &str, hold_for: Duration) -> Result<Session> {
        let req = HeartbeatSessionRequest {
            name: name.into(),
            hold_for: (!hold_for.is_zero()).then(|| duration(hold_for)),
        };
        let r = self.request(req).await?;
        Ok(self
            .check(self.raw().heartbeat_session(r).await)?
            .into_inner())
    }
}

enum Match {
    None,
    Suffix,
    Exact,
}

fn match_kind(s: &Session, reference: &str) -> Match {
    if reference.is_empty() {
        return Match::None;
    }
    if s.short_id == reference || s.name.rsplit('/').next() == Some(reference) {
        return Match::Exact;
    }
    let dn = &s.display_name;
    if dn.is_empty() {
        return Match::None;
    }
    if dn == reference {
        return Match::Exact;
    }
    // chainctl prints display names owner-qualified ("acme/app/quiet-harbor" for display name
    // "app/quiet-harbor"): one extra leading segment.
    if let Some((owner, rest)) = reference.split_once('/')
        && !owner.is_empty()
        && rest == dn
    {
        return Match::Exact;
    }
    if dn.ends_with(&format!("/{reference}")) {
        return Match::Suffix;
    }
    Match::None
}
