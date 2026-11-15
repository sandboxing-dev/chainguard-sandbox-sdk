"""Turning Workspaces on for an organization.

Sessions can be created only in an organization that is entitled to Workspaces.
``chainctl develop init`` gets an organization there in these steps, each a call
to Chainguard's platform API (DEFAULT_PLATFORM_URL):

1. Find out which legal documents the organization has accepted:
   ``GET /iam/v1/terms/{org}`` (accepted_terms)
2. Accept the ones turning it on needs (ENABLE_TERMS):
   ``POST /iam/v1/terms/{org}  {"documentIds": [...]}`` (accept_terms)
3. Link a GitHub account (an organization, or your own user) to the
   organization. The service refuses step 4 without it. The Chainguard GitHub
   App must be installed on that account (GITHUB_APP_INSTALL_URL).
   ``GET /guardener/v1alpha1/githubAssociations/{org}`` (linked_github_organizations),
   ``GET /guardener/v1alpha1/githubAssociations:begin`` (begin_github_oauth),
   GitHub's authorize page in the browser, back to a loopback port (authorize_github),
   ``POST /guardener/v1alpha1/githubAssociations/{org}:link`` (link_github_organization)
4. Turn the feature on, with the plan's allowance:
   ``POST /entitlements/v1alpha1/features/{org}:enableSandbox`` (enable_sandbox)
5. Read the result:
   ``GET /entitlements/v2beta1/organizationEntitlements?parent={org}`` (workspaces_entitlement)

Creating the organization itself (sign-up) is not covered: use the Chainguard
Console or ``chainctl develop init``.

All of these need a token for the platform API (platform_token_source()), and
steps 2 to 4 need the owner role in the organization. Step 3 also needs you to
own the GitHub account.
"""

from __future__ import annotations

import base64
import hashlib
import json
import os
import secrets
import sys
import threading
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Callable

from chainguard_sandbox.workspaces.auth import TokenSource, _urlopen
from chainguard_sandbox.workspaces.orgs import DEFAULT_PLATFORM_URL, platform_token_source


@dataclass
class TermsDocument:
    """A legal document an organization accepts."""

    id: str
    label: str = ""
    url: str = ""


#: The documents an organization must accept to use Workspaces: the Secured
#: Platform Terms and the Master Service and License Agreement. (Source:
#: chainguard.dev/sdk, proto/chainguard/platform/iam/terms.)
WORKSPACES_TERMS = ("secure-platform-terms.v1", "msla.v1")

#: The documents an organization accepts when Workspaces is turned on for it the
#: way ``chainctl develop init`` does: the Workspaces terms, the Code Analysis
#: Terms (the same switch turns on Chainguard Checks), and the Terms of Service
#: and Data Privacy Agreement that linking a GitHub account requires.
ENABLE_TERMS = ("secure-platform-terms.v1", "msla.v1", "code-analysis-terms.v1", "guardener-tos.v1", "sfdpa.v1")

#: Where the Chainguard GitHub App is installed on a GitHub organization or
#: user. Linking that account fails until it is.
GITHUB_APP_INSTALL_URL = "https://github.com/apps/chainguard/installations/new"

#: The loopback port the GitHub authorization redirects to (the one chainctl
#: uses). Another free port works too.
DEFAULT_GITHUB_OAUTH_PORT = 8989

_KNOWN_TERMS = {
    "secure-platform-terms.v1": ("Secured Platform Terms", "https://www.chainguard.dev/legal/secure-platform-terms"),
    "msla.v1": ("Master Service and License Agreement", "https://www.chainguard.dev/legal/master-service-and-license-agreement"),
    "code-analysis-terms.v1": ("Code Analysis Terms", "https://www.chainguard.dev/legal/code-analysis-terms"),
    "guardener-tos.v1": ("Terms of Service", "https://www.chainguard.dev/legal/guardener"),
    "sfdpa.v1": ("Data Privacy Agreement", "https://www.chainguard.dev/legal/supplemental-dpa"),
}


def terms_document(doc_id: str) -> TermsDocument:
    """The label and URL of a known document ID, or a document with only the ID."""
    label, url = _KNOWN_TERMS.get(doc_id, ("", ""))
    return TermsDocument(doc_id, label, url)


class PlatformError(Exception):
    """A non-2xx answer from the platform API. ``code`` is the gRPC status code
    the gateway reports (7 = permission denied, 9 = failed precondition, ...).
    ``missing_terms`` is set when the call was refused because the organization
    has not accepted those documents."""

    def __init__(self, what: str, http_status: int, code: int, message: str, missing_terms: list[TermsDocument],
                 reason: str = ""):
        super().__init__(f"{what}: platform API: HTTP {http_status}: {message}")
        self.http_status, self.code, self.message, self.missing_terms = http_status, code, message, missing_terms
        #: The machine-readable reason, when the service gives one.
        self.reason = reason

    @property
    def github_link_required(self) -> bool:
        """The organization has to link a GitHub account first."""
        return self.reason == "GUARDENER_GITHUB_LINK_REQUIRED" or "must link a GitHub organization" in self.message

    @property
    def github_app_not_installed(self) -> bool:
        """The Chainguard GitHub App is not installed on the GitHub account."""
        return self.reason.endswith("GITHUB_APP_NOT_INSTALLED") or "must be installed on the GitHub" in self.message

    @property
    def github_already_linked(self) -> bool:
        """The GitHub account is linked to another Chainguard organization."""
        return self.reason == "GUARDENER_GITHUB_ALREADY_LINKED" or "already linked to a different" in self.message


def _call(what: str, method: str, path: str, body: dict | None, token_source: TokenSource | None) -> dict:
    tok = (token_source or platform_token_source()).token()
    base = os.environ.get("CHAINGUARD_SANDBOX_PLATFORM_URL") or DEFAULT_PLATFORM_URL
    headers = {"Authorization": f"Bearer {tok}", "Accept": "application/json"}
    data = None
    if body is not None:
        data = json.dumps(body).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(base.rstrip("/") + path, data=data, headers=headers, method=method)
    try:
        with _urlopen(req, 30) as resp:
            raw = resp.read(4 << 20)
    except urllib.error.HTTPError as e:
        raw = e.read(1 << 20) if e.fp else b""
        try:
            st = json.loads(raw)
        except ValueError:
            st = {}
        if not isinstance(st, dict):
            st = {}
        missing, reason = [], ""
        # A refusal for unaccepted terms carries a detail listing the missing documents.
        for d in st.get("details") or []:
            if not isinstance(d, dict):
                continue
            reason = reason or d.get("reason") or ""
            for m in d.get("missing") or []:
                doc = terms_document(m.get("id", ""))
                missing.append(TermsDocument(doc.id, m.get("label") or doc.label, m.get("url") or doc.url))
        raise PlatformError(what, e.code, int(st.get("code") or 0), st.get("message") or raw.decode(errors="replace").strip(),
                            missing, reason) from e
    except urllib.error.URLError as e:
        raise PlatformError(what, 0, 0, str(e.reason), []) from e
    return json.loads(raw) if raw.strip() else {}


def _org(org: str) -> str:
    if not org or "?" in org or "#" in org:
        raise ValueError(f"invalid organization ID {org!r}")
    return urllib.parse.quote(org, safe="")


def accepted_terms(org: str, token_source: TokenSource | None = None) -> list[str]:
    """IDs of the legal documents the organization has accepted."""
    out = _call("listing accepted terms", "GET", f"/iam/v1/terms/{_org(org)}", None, token_source)
    return [it.get("docId", "") for it in out.get("items") or []]


def missing_workspaces_terms(org: str, token_source: TokenSource | None = None) -> list[TermsDocument]:
    """The documents Workspaces needs that the organization has not accepted yet."""
    have = set(accepted_terms(org, token_source))
    return [terms_document(i) for i in WORKSPACES_TERMS if i not in have]


def accept_terms(org: str, document_ids: list[str], token_source: TokenSource | None = None) -> None:
    """Record that the organization accepts these documents. This is a legal
    act on the organization's behalf: call it only when the person running your
    program has agreed to the documents. Needs the owner role; accepting a
    document again changes nothing."""
    if not document_ids:
        raise ValueError("no documents to accept")
    _call("accepting terms", "POST", f"/iam/v1/terms/{_org(org)}", {"documentIds": list(document_ids)}, token_source)


def enable_sandbox(org: str, token_source: TokenSource | None = None) -> None:
    """Turn on Checks and Workspaces for the organization, with its plan's
    monthly allowance (step 5 of ``chainctl develop init``). An allowance
    Chainguard already granted is kept. Needs the owner role, and the
    organization must have accepted the terms first: otherwise PlatformError's
    ``missing_terms`` lists the documents."""
    _call("turning on Workspaces", "POST", f"/entitlements/v1alpha1/features/{_org(org)}:enableSandbox", {}, token_source)


@dataclass
class Entitlement:
    """An organization's Workspaces entitlement."""

    #: Plan ID ("starter", "team", ...) and display name.
    plan: str = ""
    plan_name: str = ""
    #: Whether Workspaces is turned on.
    active: bool = False
    #: The raw state ("STATE_ACTIVE", "STATE_NOT_INCLUDED", or "" when the product isn't listed).
    state: str = ""
    #: Concurrent-session limits (0: not reported).
    max_sandboxes: int = 0
    max_sandboxes_per_user: int = 0
    limits: dict[str, int] = field(default_factory=dict)


def workspaces_entitlement(org: str, token_source: TokenSource | None = None) -> Entitlement:
    """Whether Workspaces is turned on for the organization, its plan, and its session limits."""
    _org(org)
    q = urllib.parse.urlencode({"parent": org, "products": "PRODUCT_SANDBOX_WORKSPACES"})
    out = _call("reading the organization's entitlements", "GET", f"/entitlements/v2beta1/organizationEntitlements?{q}", None, token_source)
    plan = out.get("plan") or {}
    e = Entitlement(plan=plan.get("id", ""), plan_name=plan.get("displayName", ""))
    for p in out.get("products") or []:
        if p.get("product") != "PRODUCT_SANDBOX_WORKSPACES":
            continue
        e.state, e.active = p.get("state", ""), p.get("state") == "STATE_ACTIVE"
        for lim in p.get("limits") or []:
            try:
                e.limits[lim.get("displayName", "")] = int(lim.get("max"))
            except (TypeError, ValueError):
                pass
        e.max_sandboxes = e.limits.get("concurrent workspaces", 0)
        e.max_sandboxes_per_user = e.limits.get("concurrent workspaces per user", 0)
    return e


# --- linking a GitHub account ------------------------------------------------------------


def linked_github_organizations(org: str, token_source: TokenSource | None = None) -> list[str]:
    """The GitHub logins (organizations or users) linked to the organization."""
    out = _call("listing linked GitHub accounts", "GET", f"/guardener/v1alpha1/githubAssociations/{_org(org)}", None, token_source)
    return [o.get("organization", "") for o in out.get("organizations") or []]


@dataclass
class GitHubOAuthStart:
    """What the service hands out to start a GitHub authorization: the Chainguard
    GitHub App's OAuth client ID, and a state value the service signed and
    checks again when the account is linked."""

    client_id: str
    state: str


def begin_github_oauth(token_source: TokenSource | None = None) -> GitHubOAuthStart:
    """Start a GitHub authorization."""
    out = _call("starting the GitHub authorization", "GET", "/guardener/v1alpha1/githubAssociations:begin", None, token_source)
    if not out.get("clientId") or not out.get("state"):
        raise PlatformError("starting the GitHub authorization", 200, 0, "the service returned no client ID or state", [])
    return GitHubOAuthStart(out["clientId"], out["state"])


@dataclass
class GitHubAuthorization:
    """The result of authorize_github(). It holds no GitHub token: the service
    exchanges the code itself when the account is linked."""

    code: str
    redirect_uri: str
    verifier: str
    state: str


class GitHubAuthorizationError(Exception):
    """The GitHub authorization did not complete (refused, timed out, or the port was taken)."""


def authorize_github(start: GitHubOAuthStart, *, port: int = 0, timeout: float = 600,
                     open_url: Callable[[str], object] | None = None, out=None) -> GitHubAuthorization:
    """Send the person to GitHub to authorize the Chainguard GitHub App and wait
    for GitHub's redirect to a loopback port. It is the flow chainctl uses
    (chainguard.dev/sdk/githubflow): PKCE with S256, the account picker forced,
    and the state checked on return.

    port: the loopback port (default DEFAULT_GITHUB_OAUTH_PORT).
    open_url: opens the page (default webbrowser.open); return False to have the URL printed only.
    out: where instructions are written (default sys.stderr).
    """
    port = port or DEFAULT_GITHUB_OAUTH_PORT
    out = out or sys.stderr
    redirect = f"http://127.0.0.1:{port}/callback"
    verifier = base64.urlsafe_b64encode(secrets.token_bytes(32)).rstrip(b"=").decode()
    challenge = base64.urlsafe_b64encode(hashlib.sha256(verifier.encode()).digest()).rstrip(b"=").decode()
    url = "https://github.com/login/oauth/authorize?" + urllib.parse.urlencode({
        "client_id": start.client_id, "redirect_uri": redirect, "response_type": "code", "state": start.state,
        "code_challenge": challenge, "code_challenge_method": "S256", "prompt": "select_account", "allow_signup": "false"})
    done = threading.Event()
    result: dict = {}

    class Handler(BaseHTTPRequestHandler):
        def do_GET(self):  # noqa: N802 - http.server's naming
            u = urllib.parse.urlparse(self.path)
            if u.path != "/callback":
                self.send_error(404)
                return
            q = {k: v[0] for k, v in urllib.parse.parse_qs(u.query).items()}
            if q.get("error"):
                status, page = 400, "Authorization was denied. You can close this window."
                result["error"] = f"GitHub refused the authorization: {q['error']}: {q.get('error_description', '')}"
            elif q.get("state") != start.state:
                status, page = 400, "Authorization state mismatch. You can close this window."
                result["error"] = "GitHub's redirect carried the wrong state"
            elif not q.get("code"):
                status, page = 400, "Missing authorization code. You can close this window."
                result["error"] = "GitHub's redirect carried no authorization code"
            else:
                status, page = 200, "Authorization complete. You can close this window and return to the terminal."
                result["code"] = q["code"]
            body = page.encode()
            self.send_response(status)
            self.send_header("Content-Type", "text/plain; charset=utf-8")
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)
            done.set()

        def log_message(self, format, *args):  # noqa: A002 - http.server's signature
            pass

    try:
        srv = ThreadingHTTPServer(("127.0.0.1", port), Handler)
    except OSError as e:
        raise GitHubAuthorizationError(f"listening on loopback port {port} for GitHub's redirect (choose another port if it is taken): {e}") from e
    threading.Thread(target=srv.serve_forever, name="chainguard-sandbox-github", daemon=True).start()
    try:
        opened = False
        try:
            opened = (open_url or webbrowser.open)(url) is not False
        except Exception:  # noqa: BLE001 - no browser: print the URL instead
            opened = False
        if opened:
            print(f"Opened a browser to authorize with GitHub. If it didn't open, visit:\n  {url}", file=out)
        else:
            print(f"Open this URL in a browser to authorize with GitHub:\n  {url}", file=out)
        if not done.wait(timeout):
            raise GitHubAuthorizationError(f"the GitHub authorization was not finished within {timeout:g}s")
    finally:
        srv.shutdown()
        srv.server_close()
    if "error" in result:
        raise GitHubAuthorizationError(result["error"])
    return GitHubAuthorization(result["code"], redirect, verifier, start.state)


def link_github_organization(org: str, authorization: GitHubAuthorization, github_login: str = "",
                             token_source: TokenSource | None = None) -> str:
    """Link a GitHub account to the Chainguard organization, using an
    authorization from authorize_github(). github_login is the account to link,
    an organization or a user; empty means the GitHub user who authorized.
    Returns the linked login.

    The Chainguard GitHub App must be installed on the account (otherwise the
    PlatformError has ``github_app_not_installed``), and you must own it. An
    account can be linked to one Chainguard organization at a time."""
    body = {"code": authorization.code, "redirectUri": authorization.redirect_uri,
            "codeVerifier": authorization.verifier, "state": authorization.state}
    if github_login:
        body["organization"] = github_login
    out = _call("linking the GitHub account", "POST", f"/guardener/v1alpha1/githubAssociations/{_org(org)}:link", body, token_source)
    return out.get("organization", "")

