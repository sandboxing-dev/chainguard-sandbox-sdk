"""Chainguard Workspaces as a sandbox provider for the OpenAI Agents SDK (unofficial).

>>> from agents import RunConfig
>>> from agents.sandbox import SandboxRunConfig
>>> from chainguard_sandbox_openai_agents import ChainguardSandboxClient, ChainguardSandboxClientOptions
>>> run_config = RunConfig(sandbox=SandboxRunConfig(
...     client=ChainguardSandboxClient(),
...     options=ChainguardSandboxClientOptions(packages=["bash", "coreutils", "python-3.13"]),
... ))

Importing this package registers the options and state types, so saved runs that use them can be
parsed and resumed.
"""

from chainguard_sandbox_openai_agents._sandbox import (
    BACKEND_ID,
    DEFAULT_CHAINGUARD_WORKSPACE_ROOT,
    ChainguardSandboxClient,
    ChainguardSandboxClientOptions,
    ChainguardSandboxSession,
    ChainguardSandboxSessionState,
)

__all__ = [
    "BACKEND_ID",
    "DEFAULT_CHAINGUARD_WORKSPACE_ROOT",
    "ChainguardSandboxClient",
    "ChainguardSandboxClientOptions",
    "ChainguardSandboxSession",
    "ChainguardSandboxSessionState",
]
