"""Your Chainguard organizations, and auto-detecting the one to use.

The organization ("instance") a Client works in can be given three ways, in this order:

1. explicitly: ``Client(instance="<org id>")`` (or ``Sandbox.create(instance=...)``);
2. the environment: ``$CHAINCTL_SANDBOX_INSTANCE``;
3. auto-detection: if neither is set, the client lists the organizations your
   identity belongs to (Chainguard's account API) and uses the only one. With
   several it reports them. Turn it off with ``detect_instance=False`` or
   ``$CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1``.

List IDs with ``list_organizations()``, ``chainguard-sandbox orgs`` or ``chainctl iam organizations list``.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request
from dataclasses import dataclass

from chainguard_sandbox.workspaces.auth import AuthError, Cached, ChainctlToken, TokenSource, _urlopen
from chainguard_sandbox.workspaces.login import PLATFORM_AUDIENCE, BrowserLoginToken

DEFAULT_PLATFORM_URL = "https://console-api.enforce.dev"
ENV_NO_DETECT_INSTANCE = "CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE"


@dataclass
class Organization:
    #: The 40-hex organization ID, used as the sessions instance.
    id: str
    name: str


class NoOrganizationError(LookupError):
    """Your identity belongs to no Chainguard organization."""


class AmbiguousOrganizationError(LookupError):
    """You belong to several organizations; choose one explicitly."""

    def __init__(self, organizations: list[Organization]):
        lines = "".join(f"\n  {o.id}  {o.name}" for o in organizations)
        super().__init__("you belong to several organizations; choose one with instance= or "
                         f"$CHAINCTL_SANDBOX_INSTANCE:{lines}")
        self.organizations = organizations


def platform_token_source() -> TokenSource:
    """Tokens for the account API, the same way default_token_source() mints
    sessions tokens: from the saved browser login, or chainctl ($CHAINGUARD_SANDBOX_AUTH=chainctl)."""
    if os.environ.get("CHAINGUARD_SANDBOX_AUTH") == "chainctl":
        return Cached(ChainctlToken(audience=PLATFORM_AUDIENCE))
    return Cached(BrowserLoginToken(audience=PLATFORM_AUDIENCE, connection=os.environ.get("CHAINGUARD_SANDBOX_SOCIAL_LOGIN", ""),
                                    no_browser=os.environ.get("CHAINGUARD_SANDBOX_NO_BROWSER") == "1"))


def list_organizations(token_source: TokenSource | None = None) -> list[Organization]:
    """Organizations visible to the token's identity, sorted by name. The token
    must be for the account API (default: platform_token_source()). Calls
    GET https://console-api.enforce.dev/iam/v1/groups?uidp.in_root=true."""
    tok = (token_source or platform_token_source()).token()
    base = os.environ.get("CHAINGUARD_SANDBOX_PLATFORM_URL") or DEFAULT_PLATFORM_URL
    req = urllib.request.Request(f"{base.rstrip('/')}/iam/v1/groups?uidp.in_root=true",
                                 headers={"Authorization": f"Bearer {tok}", "Accept": "application/json"})
    try:
        with _urlopen(req, 30) as resp:
            body = resp.read(4 << 20)
    except urllib.error.HTTPError as e:
        raw = e.read(1 << 20) if e.fp else b""
        msg = raw.decode(errors="replace").strip()
        try:
            msg = json.loads(raw).get("message") or msg
        except (ValueError, AttributeError):
            pass
        raise AuthError(f"listing organizations: HTTP {e.code}: {msg}") from e
    except urllib.error.URLError as e:
        raise AuthError(f"listing organizations: {e.reason}") from e
    items = json.loads(body).get("items") or []
    orgs = [Organization(g.get("id", ""), g.get("name", "")) for g in items if g.get("id") and "/" not in g["id"]]
    return sorted(orgs, key=lambda o: o.name)


def detect_instance(token_source: TokenSource | None = None) -> str:
    """The ID of the only organization the identity belongs to."""
    orgs = list_organizations(token_source)
    if not orgs:
        raise NoOrganizationError("your identity belongs to no Chainguard organization")
    if len(orgs) > 1:
        raise AmbiguousOrganizationError(orgs)
    return orgs[0].id


def no_instance_message(detect_error: BaseException | str | None) -> str:
    msg = ("no organization configured. Set one of:\n"
           "  - instance= (Client or Sandbox.create) to the organization ID\n"
           "  - $CHAINCTL_SANDBOX_INSTANCE=<organization ID>\n"
           "  - or let it be auto-detected when you belong to exactly one organization\n"
           "List your organization IDs with `chainguard-sandbox orgs` or `chainctl iam organizations list`.")
    if detect_error:
        msg += f"\nAuto-detection: {detect_error}"
    return msg
