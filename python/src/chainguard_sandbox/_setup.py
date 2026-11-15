"""Turning Workspaces on for an organization: workspaces_status() and enable_workspaces().

Workspaces has to be turned on for an organization before sandboxes can be
created in it. These do what ``chainctl develop init --parent ORG`` does for an
organization that already exists (accept the terms, link a GitHub account, then
turn the feature on), without chainctl.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field
from typing import Callable

from chainguard_sandbox.errors import GitHubAppNotInstalledError, GitHubLinkRequiredError, TermsNotAcceptedError
from chainguard_sandbox.workspaces import setup as _lib
from chainguard_sandbox.workspaces.auth import TokenSource
from chainguard_sandbox.workspaces.orgs import detect_instance
from chainguard_sandbox.workspaces.setup import TermsDocument


@dataclass
class WorkspacesStatus:
    """An organization's Workspaces setup."""

    #: The organization ID the status is for.
    organization: str
    #: Whether Workspaces is turned on.
    enabled: bool
    #: Required legal documents not accepted yet. While Workspaces is off these
    #: are all the documents turning it on needs.
    missing_terms: list[TermsDocument] = field(default_factory=list)
    #: GitHub organizations or users linked to the organization. Turning
    #: Workspaces on needs at least one.
    github_accounts: list[str] = field(default_factory=list)
    #: The organization's plan ("Starter", ...).
    plan: str = ""
    #: The plan's limits on sandboxes existing at once (0: not reported).
    max_sandboxes: int = 0
    max_sandboxes_per_user: int = 0

    @property
    def ready(self) -> bool:
        """Whether sandboxes can be created."""
        return self.enabled and not self.missing_terms


def _organization(instance: str | None, token_source: TokenSource | None) -> str:
    return instance or os.environ.get("CHAINCTL_SANDBOX_INSTANCE") or detect_instance(token_source)


def workspaces_status(*, instance: str | None = None, token_source: TokenSource | None = None) -> WorkspacesStatus:
    """Whether Workspaces is turned on for the organization, and which required
    terms it still has to accept. Changes nothing.

    instance: the organization ID. If None, $CHAINCTL_SANDBOX_INSTANCE is used,
    and failing that the only organization you belong to.
    """
    org = _organization(instance, token_source)
    ent = _lib.workspaces_entitlement(org, token_source)
    # Once Workspaces is on, only its own terms matter. While it is off, list
    # everything turning it on will need, so it can all be agreed to up front.
    required = _lib.WORKSPACES_TERMS if ent.active else _lib.ENABLE_TERMS
    have = set(_lib.accepted_terms(org, token_source))
    missing = [_lib.terms_document(i) for i in required if i not in have]
    try:
        github = _lib.linked_github_organizations(org, token_source)
    except _lib.PlatformError:  # a member who may not list the links still gets the rest
        github = []
    return WorkspacesStatus(org, ent.active, missing, github, ent.plan_name, ent.max_sandboxes, ent.max_sandboxes_per_user)


def enable_workspaces(*, accept_terms: bool = False, instance: str | None = None,
                      github_account: str = "", github_port: int = 0, open_browser: bool = True,
                      open_url: Callable[[str], object] | None = None, out=None,
                      token_source: TokenSource | None = None) -> WorkspacesStatus:
    """Turn Workspaces on for the organization, so sandboxes can be created in it.
    It needs the owner role, and does nothing if everything is already in place.

    1. Accept the terms (only with accept_terms=True).
    2. Link a GitHub account, if the organization has none: the service requires
       one. This opens a browser on GitHub, where you choose the account, and
       waits for you to finish (up to 10 minutes). The Chainguard GitHub App
       must be installed on the account; if it is not, GitHubAppNotInstalledError
       gives the page to install it from.
    3. Turn the feature on with the plan's allowance. This also turns on
       Chainguard Checks, which the same switch controls.

    accept_terms: accept, on the organization's behalf, the legal documents it
    has not accepted yet. Pass True only when the person running your program
    has read and agreed to them (workspaces_status() lists them, with URLs).
    Without it, an organization with unaccepted terms raises TermsNotAcceptedError.
    github_account: the GitHub organization or user to link (default: the GitHub
    user who authorizes in the browser). github_port: the loopback port for
    GitHub's redirect (default 8989).
    open_browser=False: don't start the GitHub authorization; raise
    GitHubLinkRequiredError if a link is needed.
    """
    st = workspaces_status(instance=instance, token_source=token_source)
    if st.ready:
        return st
    org = st.organization

    def accept(docs: list[TermsDocument]) -> None:
        if not accept_terms:
            raise TermsNotAcceptedError(docs)
        _lib.accept_terms(org, [d.id for d in docs], token_source)

    def link_github() -> None:
        if not open_browser:
            raise GitHubLinkRequiredError(
                f"organization {org} has to link a GitHub account before Workspaces can be turned on; run "
                "enable_workspaces() where it can open a browser, or `chainguard-sandbox enable`")
        start = _lib.begin_github_oauth(token_source)
        auth = _lib.authorize_github(start, port=github_port, open_url=open_url, out=out)
        try:
            _lib.link_github_organization(org, auth, github_account, token_source)
        except _lib.PlatformError as e:
            if e.github_app_not_installed:
                raise GitHubAppNotInstalledError(_lib.GITHUB_APP_INSTALL_URL, e.message) from e
            raise

    if st.missing_terms:
        accept(st.missing_terms)
    if not st.enabled:
        if not st.github_accounts:
            link_github()
        try:
            _lib.enable_sandbox(org, token_source)
        except _lib.PlatformError as e:
            if e.github_link_required and st.github_accounts:
                link_github()  # the link could not be listed, or was removed meanwhile
            elif e.missing_terms:
                accept(e.missing_terms)  # a document this package doesn't know about yet
            else:
                raise
            _lib.enable_sandbox(org, token_source)
    return workspaces_status(instance=org, token_source=token_source)
