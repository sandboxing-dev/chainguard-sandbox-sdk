# Simple Chainguard Sandbox SDK

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in
and out, and delete it. It's the same workflow as Vercel Sandbox, E2B or Modal, in Go, Python,
TypeScript and Rust.

**Documentation:** https://chainguard-sdk.sandboxing.dev/

> **Unofficial.** Not affiliated with, endorsed by or supported by Chainguard. The SDK is compatible with
> the Workspaces Sessions API as used by `chainctl` 0.2.398.

```python
from chainguard_sandbox import Sandbox

with Sandbox.create(packages=["bash", "coreutils", "python-3.13"]) as sbx:   # deleted on exit
    print(sbx.commands.run("python3 -c 'print(2 + 2)'").stdout)
    sbx.files.write("notes/hello.txt", "hi")
    print(sbx.files.read("notes/hello.txt"))
```

## Packages

| Language | Directory | Install | Import |
|---|---|---|---|
| Go 1.23+ | [`go/`](go/) | `go get github.com/sandboxing-dev/chainguard-sandbox-sdk/go@latest` | `github.com/sandboxing-dev/chainguard-sandbox-sdk/go/sandbox` |
| Python 3.12+ | [`python/`](python/) | `uv add "chainguard-sandbox-sdk @ git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=python"` | `from chainguard_sandbox import Sandbox` |
| TypeScript (Node.js 20.10+ or Bun 1.3+) | [`typescript/`](typescript/) | clone this repository, then `bun add ./chainguard-sandbox-sdk/typescript` | `import { Sandbox } from "chainguard-sandbox-sdk"` |
| Rust 1.85+ | [`rust/`](rust/) | `cargo add chainguard-sandbox-sdk --git https://github.com/sandboxing-dev/chainguard-sandbox-sdk` | `use chainguard_sandbox::Sandbox;` |

Each directory has its own README, tests and example programs (`examples/`).

## Integrations

| Integration | Directory | Install |
|---|---|---|
| [OpenAI Agents SDK](https://github.com/openai/openai-agents-python) sandbox provider (Python) | [`integrations/openai-agents/`](integrations/openai-agents/) | `uv add "chainguard-sandbox-openai-agents @ git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=integrations/openai-agents"` |

## Getting Started

The [Quickstart](https://chainguard-sdk.sandboxing.dev/quickstart/) has every step with commands for each
language. In short:

1. **Create a Chainguard account and organization** at
   [console.chainguard.dev/workspaces/signup](https://console.chainguard.dev/workspaces/signup). The sign-up
   flow asks for your new organization's details: finish creating the organization before you go on.
2. **Install the SDK into a project** (see each language's README for the exact commands).
3. **Log in** once, with the CLI that comes with any of the packages. It opens your browser, and all four
   packages share the saved login:

    ```sh
    go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest login
    # or, from the go/ directory of a clone of this repository:
    go run ./cmd/chainguard-sandbox login
    ```

   The same CLI comes with the other packages. In a clone: `uv run chainguard-sandbox login` from `python/`, or
   `bun bin/chainguard-sandbox.ts login` from `typescript/` (after `bun install`), or
   `cargo run --bin chainguard-sandbox -- login` from `rust/`.

4. **Turn on Workspaces for the organization**, once, as one of its owners. A new organization starts with it
   off, and every create then fails with "this organization is not entitled to Chainguard Workspaces".

    ```sh
    go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest status   # where it stands
    go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest enable   # turn it on
    # or, from the go/ directory of a clone:
    go run ./cmd/chainguard-sandbox status
    go run ./cmd/chainguard-sandbox enable
    ```

   `enable` lists the legal documents the organization has to accept and asks before accepting them. The
   same can be done from code (`enable_workspaces` in Python and Rust, `enableWorkspaces`, `sandbox.Enable`) or with Chainguard's
   own CLI (`chainctl develop init --parent XXXXXX`).
5. **Choose the organization**, if you belong to more than one: `export CHAINCTL_SANDBOX_INSTANCE=XXXXXX`, or pass
   it in code. List your organization IDs with `… chainguard-sandbox orgs`. With a single organization it's
   detected automatically.
6. **Create a sandbox** with the example above.

## What's Inside

Each package has two layers:

- **The sandbox API** (create, commands, files, pause, delete), shaped like the Vercel Sandbox, E2B and
  Modal SDKs: Go `…/go/sandbox`, Python `chainguard_sandbox`, TypeScript `chainguard-sandbox-sdk`, Rust
  `chainguard_sandbox`.
- **`workspaces`, the lower level it's built on:** a client for the Workspaces Sessions API (gRPC), the
  browser login and token refresh, and organization detection, covering only what the sandbox API needs.
  Go `…/go/workspaces`, Python `chainguard_sandbox.workspaces`, TypeScript `chainguard-sandbox-sdk/workspaces`,
  Rust `chainguard_sandbox::workspaces`.

The protocol is in [`proto/sandbox/runtime/session/v1/session.proto`](proto/sandbox/runtime/session/v1/session.proto):
the subset of the Sessions API the SDK uses (eight RPCs), with the service's real names and field numbers.
Turning Workspaces on for an organization uses four calls to Chainguard's platform API over HTTPS.
Interactive terminals, workspace syncing, forwarding and the other parts of the Workspaces API aren't
included. The [Protocol](https://chainguard-sdk.sandboxing.dev/protocol/) pages describe
it.

## Development

```sh
scripts/gen-proto.sh                    # regenerate the Go, Python, TypeScript and Rust code from proto/
python3 scripts/gen_sandbox_scripts.py  # regenerate the sandbox shell scripts from go/sandbox/scripts.go

(cd go && go test -race ./...)
(cd python && uv run pytest -q)
(cd typescript && bun install && bun test)
(cd rust && cargo test)
(cd integrations/openai-agents && uv run pytest -q)
```

Publishing the Python, TypeScript and Rust packages and the integrations: [`PUBLISHING.md`](PUBLISHING.md).

The unit tests use in-process fake servers and don't contact the service. `scripts/gen-proto.sh` lists
the code generators it needs.

## License

Apache-2.0 (see [`LICENSE`](LICENSE)). Third-party notices are in [`NOTICE`](NOTICE).
