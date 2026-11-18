# Chainguard Workspaces for the OpenAI Agents SDK

A sandbox provider for the [OpenAI Agents SDK](https://github.com/openai/openai-agents-python): sandbox
agents run their shell commands and file operations in a [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup)
sandbox, a Linux microVM with the Chainguard packages you list. It's built on the
[Simple Chainguard Sandbox SDK](../../python/).

Documentation: https://chainguard-sdk.sandboxing.dev/guides/openai-agents/

> **Unofficial.** Not affiliated with or supported by Chainguard or OpenAI. It's compatible with
> `chainctl` 0.2.398 and the Sessions API it uses, and tested with `openai-agents` 0.24.0.

## Install

It needs Python 3.12 or later. The package isn't on PyPI yet; uv installs it from the repository, and takes
the Chainguard SDK it uses from the same repository:

```sh
uv add "chainguard-sandbox-openai-agents @ git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=integrations/openai-agents"
```

Add only this package: adding the SDK separately as well makes uv report conflicting sources for it. From a
clone, `uv add ../chainguard-sandbox-sdk/integrations/openai-agents` works the same way.

Then log in once and make sure Workspaces is on for your organization (the CLI comes with the SDK):

```sh
uv run chainguard-sandbox login
uv run chainguard-sandbox status    # `enable` turns it on, if it's off
```

## Use It

Pass the client to the runner's sandbox configuration:

```python
import asyncio

from agents import RunConfig, Runner
from agents.sandbox import Manifest, SandboxAgent, SandboxRunConfig
from agents.sandbox.capabilities import Shell
from agents.sandbox.entries import File

from chainguard_sandbox_openai_agents import ChainguardSandboxClient, ChainguardSandboxClientOptions


async def main() -> None:
    agent = SandboxAgent(
        name="Analyst",
        instructions="Answer with the help of the files in your workspace.",
        capabilities=[Shell()],
        default_manifest=Manifest(root="/work", entries={"data/notes.txt": File(content=b"...")}),
    )
    async with ChainguardSandboxClient() as client:
        result = await Runner.run(
            agent,
            "Summarize data/notes.txt.",
            run_config=RunConfig(
                sandbox=SandboxRunConfig(
                    client=client,
                    options=ChainguardSandboxClientOptions(packages=["bash", "coreutils", "python-3.13"]),
                ),
            ),
        )
    print(result.final_output)


asyncio.run(main())
```

The runner creates the sandbox when the run starts and deletes it when the run ends.
`examples/demo.py` runs a complete agent against a real sandbox with a scripted model, so it needs no
OpenAI API key:

```sh
uv run python examples/demo.py
```

## Options

`ChainguardSandboxClientOptions`:

| Option | Default | What it does |
|---|---|---|
| `packages` | the SDK's defaults (bash, coreutils, curl, git, jq, Python, Node.js) | Chainguard (Wolfi) packages to install |
| `egress` | open | Outbound allowlist (hostnames) |
| `egress_policy` | `"deny"` | What happens outside the allowlist: `"deny"` or `"destroy"` (tear the sandbox down) |
| `env_vars` | none | Environment variables for every command (the manifest's environment is added on top) |
| `name` | `"agents"` | A label; the display name becomes `<name>/<word-word>` |
| `source`, `source_revision` | none | Start from a GitHub repository instead of an empty workspace |
| `start_timeout` | 180 | Seconds to wait for the sandbox to start |
| `pause_on_exit` | `False` | Pause the sandbox when the run ends instead of deleting it, so a resumed run continues in it |

`ChainguardSandboxClient(instance=..., client=...)`: `instance` is the organization ID (otherwise
`$CHAINCTL_SANDBOX_INSTANCE`, or the only organization you belong to); `client` passes a Chainguard SDK
`workspaces.Client` you configured yourself, for example with a CI token source.

The workspace root is `/work` (`Manifest(root="/work")`). Another root works too; it's created when the
sandbox starts.

## What It Supports

| Agents SDK feature | Support |
|---|---|
| Shell commands (`Shell`), with timeouts | yes |
| Reading and writing files, the filesystem tools, manifest files and directories | yes |
| Workspace snapshots (persist and restore) | yes, as a tar of the workspace |
| Resuming a saved run | yes: it reconnects to the same sandbox when it still exists, otherwise it creates one and restores the snapshot |
| Interactive terminals (PTY) | no: Chainguard Workspaces' exec has no interactive stdin |
| Exposed ports | no |
| Mounts (cloud buckets) | no: refused when the sandbox is created |
| Users and groups other than root | no: refused (commands run as root) |

Errors from Chainguard map to the Agents SDK's types: `ExecTimeoutError`, `ExecTransportError` (with
`retryable` set), `WorkspaceReadNotFoundError`, `WorkspaceArchiveReadError` and so on.

**Limits to know about:** the Workspaces beta (Starter plan) allows one sandbox per user and three per
organization at a time, a sandbox runs one command at a time, and creating one takes about 6–10 seconds.

## Checks

```sh
uv run pytest -q                                          # no network
CHAINGUARD_SANDBOX_LIVE=1 uv run pytest -q tests/test_live.py   # against the real service
uv run pyright
```

`../../scripts/publish-openai-agents.sh` runs these, builds the package, and installs it into an empty
environment; see [`../../PUBLISHING.md`](../../PUBLISHING.md).
