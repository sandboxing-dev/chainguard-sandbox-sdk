# Simple Chainguard Sandbox SDK for Python

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in and out, and delete it, with
an API shaped like E2B, Vercel Sandbox and Modal.

- **Documentation:** https://chainguard-sdk.sandboxing.dev/
- **Distribution:** `chainguard-sandbox-sdk`; **import:** `chainguard_sandbox` (the sandbox API is the
  top-level package; the lower-level client is `chainguard_sandbox.workspaces`).
- **Requires:** Python 3.12+.

> **Unofficial:** not affiliated with or supported by Chainguard. It's compatible with the Workspaces
> Sessions API as used by `chainctl` 0.2.398.

## Install

Into a new project:

```sh
mkdir my-app && cd my-app
uv init
uv add "chainguard-sandbox-sdk @ git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=python"
```

In a project you already have, run only the `uv add` line in its directory (the one with `pyproject.toml`).
To use a local clone of this repository instead, give `uv add --editable` the path from your project to the
clone's `python` directory, for example `uv add --editable ../chainguard-sandbox-sdk/python` when the clone
sits next to your project. The CLI is then `uv run chainguard-sandbox` (`login`, `status`, `enable`, `orgs`,
`ls`, `rm`).

From a clone of this repository, everything runs from this directory (`python/`) with no project of your
own; `uv run` installs what it needs on first use:

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
cd chainguard-sandbox-sdk/python
uv run chainguard-sandbox login           # also: status, enable, orgs, ls, rm
uv run python examples/quickstart.py      # create a sandbox, run a command, use files
```

With pip: `pip install "git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=python"`.

## Quick Example

```python
from chainguard_sandbox import Sandbox

with Sandbox.create(packages=["bash", "coreutils", "python-3.13"]) as sbx:  # deleted on exit
    print(sbx.commands.run("python3 -c 'print(2 + 2)'").stdout, end="")
    sbx.files.write("notes/hello.txt", "hi")
    print(sbx.files.read("notes/hello.txt"))
```

More in [`examples/`](examples/): `quickstart.py` runs as-is; `tour.py` touches every part of the API.

## Log In

The first call opens your browser to log in to Chainguard, and the login is saved for later runs. You can
also log in ahead of time with the bundled CLI (from this directory):

```sh
uv run chainguard-sandbox login     # optional: github, google-oauth2 or gitlab to skip the provider choice
uv run chainguard-sandbox whoami
uv run chainguard-sandbox logout
```

- **Shared:** the saved login (`~/Library/Application Support/chainguard-sandbox/login.json` on macOS,
  `~/.config/chainguard-sandbox/login.json` on Linux) is shared with the Go and TypeScript SDKs.
- **Lifetime:** it lasts as long as it's used at least once every 20 hours. If a run opens your browser,
  the login expired: approve it and the run continues.

## Choosing the Organization

Every sandbox lives in a Chainguard organization (the "instance"). The first one set wins:

1. **In code:** `Sandbox.create(instance="XXXXXX")`.
2. **Environment variable:** `export CHAINCTL_SANDBOX_INSTANCE=XXXXXX`.
3. **Auto-detected:** if you belong to exactly one organization, nothing to set (turn that off with
   `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1`).

List your organization IDs with `uv run chainguard-sandbox orgs` (or `chainctl iam organizations list`), or in
code with `chainguard_sandbox.list_organizations()`.

`chainguard-sandbox ls` lists this SDK's sandboxes, and `chainguard-sandbox rm SANDBOX_ID` deletes one.

## Environment Variables

| Variable | Effect |
|---|---|
| `CHAINCTL_SANDBOX_INSTANCE` | The organization ID (overrides auto-detection) |
| `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` | Never auto-detect the organization |
| `CHAINCTL_AUTH_TOKEN` | Use this token as-is (audience `https://sessions.chainbox.dev`) |
| `CHAINGUARD_SANDBOX_NO_BROWSER=1` | Never open a browser: fail with a "login required" error instead. For scripts and CI |
| `CHAINGUARD_SANDBOX_SOCIAL_LOGIN` | Preselect the login provider (`github`, `google-oauth2`, `gitlab`) |
| `CHAINGUARD_SANDBOX_LOGIN_AUDIENCES` | Comma-separated audiences a new login asks for. Default: sessions + account API |
| `CHAINGUARD_SANDBOX_AUTH=chainctl` | Mint tokens with an installed, logged-in `chainctl` instead |
| `CHAINGUARD_SANDBOX_LOGIN_FILE` | Use another login file |
| `CHAINCTL_SESSIONS_URL` | Another Sessions API endpoint (default `https://sessions.chainbox.dev`) |
| `CHAINGUARD_SANDBOX_PLATFORM_URL` | Another account API endpoint, for organization detection |

## CI and Servers

A CI job can trade its OIDC token for a Chainguard token as an **assumable identity**, with no saved
login. An organization owner creates the identity once:

```sh
chainctl iam identities create ci --parent XXXXXX \
  --identity-issuer https://token.actions.githubusercontent.com \
  --subject repo:OWNER/REPO:ref:refs/heads/main --role sandbox.user
```

Then, in the job (GitHub Actions needs `permissions: id-token: write`):

```python
from chainguard_sandbox import Sandbox
from chainguard_sandbox.workspaces import Cached, Client, IdentityExchange, github_actions_id_token

ts = Cached(IdentityExchange(identity="<identity ID>", id_token=github_actions_id_token()))
with Sandbox.create(client=Client(instance="XXXXXX", token_source=ts)) as sbx:
    ...
```

Set the organization explicitly in CI: auto-detection needs a second, account-API token. `env_id_token`
and `file_id_token` read an OIDC token from an environment variable or a file (GitLab, Kubernetes).
Starter organizations can't create identities.

## Lower-Level Client

`chainguard_sandbox.workspaces.Client` is the thin gRPC client the sandbox API is built on: `create`, `get`, `list`,
`resolve`, `delete`, `park`, `heartbeat`, `exec`, `output`, `write_file` and `first_attach`. It covers
only what the simple sandbox API needs; see the
[Protocol](https://chainguard-sdk.sandboxing.dev/protocol/) section of the docs.

## Development

```sh
uv sync
uv run pytest -q          # offline: a fake Sessions server, no network
uv run pyright            # src and examples
```

- **Protobuf code:** `src/chainguard_sandbox/workspaces/_gen` is generated from `../proto` with
  `../scripts/gen-proto.sh python`.
- **Sandbox scripts:** `src/chainguard_sandbox/_scripts.py` is generated from the Go source
  (`../go/sandbox/scripts.go`) with `python3 ../scripts/gen_sandbox_scripts.py --only python`.
  Don't edit either by hand.
