"""A simple sandbox API on Chainguard Workspaces, shaped like the E2B, Vercel
Sandbox and Modal Sandbox SDKs: create a sandbox, run commands, move files
in and out, delete it.

    from chainguard_sandbox import Sandbox

    with Sandbox.create(packages=["python-3.13"]) as sbx:
        r = sbx.commands.run("python3 -c 'print(2+2)'")
        print(r.stdout)                       # 4
        sbx.files.write("data/input.txt", "hello")
        print(sbx.files.read("data/input.txt"))

A sandbox is a Workspaces session created from an empty workspace with an
environment document listing your packages. Commands run one at a time per
sandbox; there are no exposed ports. See the sandbox documentation for the
full guide and how it differs from other sandbox services.
"""

from chainguard_sandbox._sandbox import (
    DEFAULT_COMMAND_TIMEOUT,
    DEFAULT_PACKAGES,
    DEFAULT_WORKDIR,
    CommandHandle,
    Commands,
    EntryInfo,
    Files,
    ProcessInfo,
    Sandbox,
    SandboxInfo,
    WriteInfo,
    environment_document,
)
from chainguard_sandbox._setup import TermsDocument, WorkspacesStatus, enable_workspaces, workspaces_status
from chainguard_sandbox.workspaces.orgs import Organization, list_organizations
from chainguard_sandbox.errors import (
    CommandExitError,
    CommandResult,
    FileNotFoundError,
    GitHubAppNotInstalledError,
    GitHubLinkRequiredError,
    InvalidPathError,
    LimitReachedError,
    NotFoundError,
    OutcomeUnknownError,
    SandboxBusyError,
    SandboxError,
    SandboxNotFoundError,
    TermsNotAcceptedError,
    TimeoutError,
    WorkspacesNotEnabledError,
)

__all__ = [
    "DEFAULT_COMMAND_TIMEOUT", "DEFAULT_PACKAGES", "DEFAULT_WORKDIR",
    "CommandExitError", "CommandHandle", "CommandResult", "Commands", "EntryInfo", "FileNotFoundError", "Files",
    "InvalidPathError", "LimitReachedError", "NotFoundError", "OutcomeUnknownError", "ProcessInfo", "Sandbox",
    "SandboxBusyError", "SandboxError", "SandboxInfo", "SandboxNotFoundError", "TimeoutError", "WriteInfo",
    "environment_document", "Organization", "list_organizations",
    "GitHubAppNotInstalledError", "GitHubLinkRequiredError", "TermsDocument", "TermsNotAcceptedError", "WorkspacesNotEnabledError", "WorkspacesStatus",
    "enable_workspaces", "workspaces_status",
]
