"""Session resource helpers (names, constructors, reference matching)."""

from __future__ import annotations

import posixpath
import re
import urllib.parse
from importlib import resources

from chainguard_sandbox.workspaces.pb import session as spb

#: The document chainctl 0.2.398 synthesizes when a directory has no
#: .chainguard/develop.yaml or ci.yaml (captured from a live session).
DEFAULT_ENVIRONMENT: bytes = resources.files("chainguard_sandbox.workspaces").joinpath("default_environment.yaml").read_bytes()
#: The check in DEFAULT_ENVIRONMENT; the plane selects it itself.
DEFAULT_JOB = "develop"

_SCP = re.compile(r"^(?:[^@/:]+@)?([^/:]+):(.+)$")  # user@host:path (scp-like)
_GH_HOSTS = ("github.com", "www.github.com")


def instance_name(id: str) -> str:
    """'instances/{id}'; an already-qualified name is returned unchanged."""
    return id if id.startswith("instances/") else "instances/" + id


def is_session_name(s: str) -> bool:
    p = s.split("/")
    return len(p) == 4 and p[0] == "instances" and p[2] == "sessions" and bool(p[1]) and bool(p[3])


def github_path(u: str) -> str | None:
    """"owner/repo" when u names a github.com repository (https, http, ssh,
    git, scp-like or bare "github.com/o/r"; any case; any userinfo/port),
    otherwise None."""
    u = u.strip()
    path = None
    if "://" in u:
        p = urllib.parse.urlsplit(u)
        if p.scheme.lower() in ("https", "http", "ssh", "git", "git+ssh", "ssh+git") and (p.hostname or "").lower() in _GH_HOSTS:
            path = p.path
    elif m := _SCP.match(u):
        if m.group(1).lower() in _GH_HOSTS:
            path = m.group(2)
    elif u.lower().startswith(tuple(h + "/" for h in _GH_HOSTS)):
        path = u.split("/", 1)[1]
    if path is None:
        return None
    path = path.strip("/").removesuffix(".git").strip("/")
    return path if path.count("/") >= 1 and all(path.split("/")) else None


def normalize_github_url(u: str) -> str:
    """Any github.com remote form -> "https://github.com/owner/repo" (userinfo
    such as an embedded token is dropped). Other URLs keep their form, minus a
    trailing ".git" and any userinfo; scp-like non-GitHub remotes are unchanged."""
    if gp := github_path(u):
        return "https://github.com/" + gp
    u = u.strip()
    if "://" in u:
        p = urllib.parse.urlsplit(u)
        netloc = p.hostname or ""
        if ":" in netloc:
            netloc = f"[{netloc}]"
        if p.port:
            netloc += f":{p.port}"
        u = urllib.parse.urlunsplit((p.scheme, netloc, p.path, p.query, p.fragment))
        return u.rstrip("/").removesuffix(".git")
    return u


def repository_url(repo: str) -> str:
    """Normalize any github.com form ("github.com/o/r", "git@github.com:o/r.git",
    "ssh://git@github.com:22/o/r", mixed case, embedded tokens) to
    "https://github.com/o/r". Other URLs keep their scheme (userinfo dropped);
    scp-like non-GitHub remotes are returned unchanged; anything else bare gets
    "https://"."""
    if gp := github_path(repo):
        return "https://github.com/" + gp
    if "://" in repo:
        return normalize_github_url(repo)
    if _SCP.match(repo.strip()):
        return repo.strip()
    return "https://" + repo.strip().rstrip("/").removesuffix(".git")


def repository_session(repository: str, revision: str = "") -> spb.Session:
    """A session opened from a repository URL, like `chainctl develop
    github.com/org/repo`, shaped like chainctl's request: an https repository
    URL (the plane can't fetch the bare "github.com/…" form), idle AUTO_HOLD,
    and DEFAULT_ENVIRONMENT as the fallback for repos without .chainguard/
    files. revision pins a commit; empty lets the plane use the default
    branch's HEAD (verified live)."""
    return spb.Session(
        idle=spb.AUTO_HOLD,
        source=spb.Source(repository=repository_url(repository), revision=revision, fallback_document=DEFAULT_ENVIRONMENT),
    )


def workspace_session(label: str, repository: str = "", revision: str = "", document: bytes | None = None) -> spb.Session:
    """A session whose /work is uploaded on first attach, as chainctl sends it.

    label is the checkout's absolute local path; revision the HEAD SHA for a git
    checkout; repository the origin remote ("https://github.com/owner/repo");
    document the environment document (None = DEFAULT_ENVIRONMENT, SYNTHESIZED).
    The idle policy is AUTO_HOLD, chainctl's default.
    """
    prov = spb.AUTHORED
    if document is None:
        document, prov = DEFAULT_ENVIRONMENT, spb.SYNTHESIZED
    return spb.Session(
        idle=spb.AUTO_HOLD,
        workspace=spb.Workspace(label=label, repository=repository, revision=revision, document=document, provenance=prov),
    )


def matches(s: spb.Session, ref: str) -> bool:
    """Whether ref names s the way chainctl's SESSION argument does: the
    session ID, short ID, display name, a "/"-suffix of the display name
    ("quiet-harbor" for "app/quiet-harbor"), or the display name qualified by
    exactly one owner segment, as chainctl prints it ("acme/app/quiet-harbor"
    for "app/quiet-harbor"; not "x/y/quiet-harbor")."""
    if not ref:
        return False
    if s.short_id == ref or posixpath.basename(s.name) == ref:
        return True
    dn = s.display_name
    if not dn:
        return False
    if dn == ref or dn.endswith("/" + ref):
        return True
    return ref.endswith("/" + dn) and ref.count("/") == dn.count("/") + 1


def resolve_matches(sessions: list[spb.Session], ref: str) -> list[spb.Session]:
    """Sessions matching ref, with exact matches (ID, short ID, display name)
    taking precedence over display-name suffix matches."""
    exact = [s for s in sessions if ref and (s.short_id == ref or posixpath.basename(s.name) == ref or s.display_name == ref)]
    return exact or [s for s in sessions if matches(s, ref)]
