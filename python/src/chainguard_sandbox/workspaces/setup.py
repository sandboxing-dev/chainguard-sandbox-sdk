"""Turning Workspaces on for an organization.

Sessions can be created only in an organization that is entitled to Workspaces.
``chainctl develop init`` gets an organization there in these steps, each a call
to Chainguard's platform API (DEFAULT_PLATFORM_URL):

1. Find out which legal documents the organization has accepted:
   ``GET /iam/v1/terms/{org}`` (accepted_terms)
2. Accept the ones Workspaces needs (WORKSPACES_TERMS):
   ``POST /iam/v1/terms/{org}  {"documentIds": [...]}`` (accept_terms)
3. Turn the feature on, with the plan's allowance:
   ``POST /entitlements/v1alpha1/features/{org}:enableSandbox`` (enable_sandbox)
4. Read the result:
   ``GET /entitlements/v2beta1/organizationEntitlements?parent={org}`` (workspaces_entitlement)

chainctl also links a GitHub account to the organization between steps 2 and 3.
That is for Checks and for sessions that act on GitHub; this module does not do
it. Creating the organization itself (sign-up) is not covered either: use the
Chainguard Console or ``chainctl develop init``.

All of these need a token for the platform API (platform_token_source()), and
steps 2 and 3 need the owner role in the organization.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field

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

    def __init__(self, what: str, http_status: int, code: int, message: str, missing_terms: list[TermsDocument]):
        super().__init__(f"{what}: platform API: HTTP {http_status}: {message}")
        self.http_status, self.code, self.message, self.missing_terms = http_status, code, message, missing_terms


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
        missing = []
        # A refusal for unaccepted terms carries a detail listing the missing documents.
        for d in st.get("details") or []:
            for m in (d.get("missing") or []) if isinstance(d, dict) else []:
                doc = terms_document(m.get("id", ""))
                missing.append(TermsDocument(doc.id, m.get("label") or doc.label, m.get("url") or doc.url))
        raise PlatformError(what, e.code, int(st.get("code") or 0), st.get("message") or raw.decode(errors="replace").strip(), missing) from e
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
