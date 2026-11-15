"""Turning Workspaces on for an organization: workspaces_status() and enable_workspaces().

Workspaces has to be turned on for an organization before sandboxes can be
created in it. These do what ``chainctl develop init --parent ORG`` does for an
organization that already exists (accept the terms, then turn the feature on),
without chainctl.
"""

from __future__ import annotations

import os
from dataclasses import dataclass, field

from chainguard_sandbox.errors import TermsNotAcceptedError
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
    #: Required legal documents not accepted yet.
    missing_terms: list[TermsDocument] = field(default_factory=list)
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
    return WorkspacesStatus(org, ent.active, _lib.missing_workspaces_terms(org, token_source), ent.plan_name,
                            ent.max_sandboxes, ent.max_sandboxes_per_user)


def enable_workspaces(*, accept_terms: bool = False, instance: str | None = None,
                      token_source: TokenSource | None = None) -> WorkspacesStatus:
    """Turn Workspaces on for the organization, so sandboxes can be created in it.

    It accepts the terms Workspaces is offered under, then turns the feature on
    with the plan's allowance. It needs the owner role, and does nothing if
    everything is already in place. It also turns on Chainguard Checks, which
    the same switch controls.

    accept_terms: accept, on the organization's behalf, the legal documents it
    has not accepted yet. Pass True only when the person running your program
    has read and agreed to them (workspaces_status() lists them, with URLs).
    Without it, an organization with unaccepted terms raises TermsNotAcceptedError.
    """
    st = workspaces_status(instance=instance, token_source=token_source)
    if st.ready:
        return st
    org = st.organization

    def accept(docs: list[TermsDocument]) -> None:
        if not accept_terms:
            raise TermsNotAcceptedError(docs)
        _lib.accept_terms(org, [d.id for d in docs], token_source)

    if st.missing_terms:
        accept(st.missing_terms)
    if not st.enabled:
        try:
            _lib.enable_sandbox(org, token_source)
        except _lib.PlatformError as e:
            if not e.missing_terms:
                raise
            # The switch also covers Checks, whose terms the service may ask for too.
            accept(e.missing_terms)
            _lib.enable_sandbox(org, token_source)
    return workspaces_status(instance=org, token_source=token_source)
