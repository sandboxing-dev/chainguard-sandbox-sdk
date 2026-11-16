# Simple Chainguard Sandbox SDK for Rust

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in and out, and delete it. The
API is shaped like the E2B, Vercel Sandbox and Modal SDKs, and matches the Go, Python and TypeScript SDKs in
this repository.

Documentation: https://chainguard-sdk.sandboxing.dev/

> **Unofficial.** This SDK isn't affiliated with or supported by Chainguard. It's compatible with
> `chainctl` 0.2.398 and the Sessions API it uses.

## Install

It needs Rust 1.85 or later. The crate isn't published to crates.io yet. Cargo can take it straight from the
Git repository (it finds the crate in the `rust/` directory by name). Starting from the directory you keep
projects in:

```sh
cargo new my-app && cd my-app
cargo add chainguard-sandbox-sdk --git https://github.com/sandboxing-dev/chainguard-sandbox-sdk
cargo add tokio --features macros,rt-multi-thread
```

Or from a clone, by path:

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
cargo new my-app && cd my-app
cargo add chainguard-sandbox-sdk --path ../chainguard-sandbox-sdk/rust
cargo add tokio --features macros,rt-multi-thread
```

`../chainguard-sandbox-sdk/rust` is the path from `my-app` to the clone's `rust` directory. `tokio` is the
async runtime the SDK runs on; programs without one use the `blocking` module instead (below). The crate is
imported as `chainguard_sandbox`.

The CLI is the crate's `chainguard-sandbox` binary (`login`, `status`, `enable`, `orgs`, `ls`, `rm`). Cargo
doesn't run a dependency's binaries, so install it once, into `~/.cargo/bin`:

```sh
cargo install chainguard-sandbox-sdk --git https://github.com/sandboxing-dev/chainguard-sandbox-sdk
# or, from a clone:
cargo install --path ../chainguard-sandbox-sdk/rust
```

Then run `chainguard-sandbox login`. Or, without installing anything, run it from the clone's `rust/`
directory with `cargo run --bin chainguard-sandbox -- login`.

To work in the clone itself, everything runs from this directory (`rust/`):

```sh
cd chainguard-sandbox-sdk/rust
cargo run --bin chainguard-sandbox -- login     # also: status, enable, orgs, ls, rm
cargo run --example quickstart                  # create a sandbox, run a command, use files
```

## Quick Example

```rust
use chainguard_sandbox::{CreateOptions, Sandbox};

#[tokio::main]
async fn main() -> chainguard_sandbox::Result<()> {
    let opts = CreateOptions {
        packages: Some(vec!["bash".into(), "coreutils".into(), "python-3.13".into()]),
        ..Default::default()
    };
    // The sandbox is deleted when the closure returns, on success and on error.
    Sandbox::with(opts, async |sbx| {
        let r = sbx.commands.run("python3 -c 'print(2+2)'").await?;
        print!("{}", r.stdout); // 4
        sbx.files.write("notes/hello.txt", "hi").await?;
        println!("{}", sbx.files.read_text("notes/hello.txt").await?);
        Ok(())
    })
    .await
}
```

More: `examples/quickstart.rs` and `examples/tour.rs` (every part of the API). Run them with
`cargo run --example quickstart` from this directory.

## Deleting Sandboxes

Rust has no asynchronous `Drop`, so the async API never deletes a sandbox behind your back:

- **`Sandbox::with(opts, async |sbx| { ... })`** creates a sandbox, runs the closure, and deletes the sandbox
  when it returns, also on error. If the future is dropped early or the closure panics, the delete is started
  in the background.
- **`sbx.kill().await`** deletes it explicitly. A dropped `Sandbox` leaves the sandbox running (it parks when
  idle and is discarded after 5 days); find it later with `chainguard-sandbox ls`.

## Without an Async Runtime

`chainguard_sandbox::blocking` has the same API with plain calls. Its `Sandbox` deletes the sandbox when it's
dropped, like Python's `with` block; `detach()` keeps it.

```rust
use chainguard_sandbox::{CreateOptions, blocking::Sandbox};

fn main() -> chainguard_sandbox::Result<()> {
    let sbx = Sandbox::create(CreateOptions::default())?; // deleted when sbx is dropped
    println!("{}", sbx.commands.run("echo hello")?.stdout);
    Ok(())
}
```

The blocking calls refuse to run inside an async runtime; use the async API there.

## Errors

Every call returns `chainguard_sandbox::Result`. Match on `Error`'s variants, which mirror the other SDKs'
error types:

```rust
use chainguard_sandbox::Error;

match sbx.commands.run("exit 3").await {
    Err(Error::CommandExit(r)) => println!("exited {}: {}", r.exit_code, r.stderr),
    Err(Error::Timeout { result: Some(r), .. }) => println!("timed out after: {}", r.stdout),
    Err(Error::NotEnabled { .. }) => println!("Workspaces is off: chainguard-sandbox enable"),
    Err(e) => println!("{e}"),
    Ok(r) => println!("{}", r.stdout),
}
```

`e.grpc_code()` reports the plane's gRPC status code behind an error, and `e.command_result()` the output a
failed command produced.

## Logging In

The first call opens your browser to log in to Chainguard; the login is saved and refreshed, and shared
with the Go, Python and TypeScript SDKs. To log in ahead of time, or check the saved login:

```sh
chainguard-sandbox login      # optional: github, google-oauth2 or gitlab
chainguard-sandbox whoami
chainguard-sandbox logout
```

(From the clone, `cargo run --bin chainguard-sandbox -- login` and so on.)

A saved login lasts as long as it's used at least once every 20 hours. If a run opens your browser, the
login expired: approve it and the run continues.

## Enabling Workspaces

An organization starts with Workspaces off. An owner turns it on once, with the CLI or from code:

```sh
chainguard-sandbox status
chainguard-sandbox enable     # asks before accepting the terms; links a GitHub account
```

```rust
let st = chainguard_sandbox::workspaces_status(&Default::default()).await?;
for d in &st.missing_terms {
    println!("{}: {}", d.label, d.url);
}
// Only after the person running the program has agreed to the documents:
let o = chainguard_sandbox::SetupOptions { accept_terms: true, ..Default::default() };
chainguard_sandbox::enable_workspaces(&o).await?;
```

Turning it on links a GitHub account to the organization, through the browser; the Chainguard GitHub App
must be installed on that account (`Error::GitHubAppNotInstalled` gives the page).

## Choosing the Organization

Every sandbox lives in a Chainguard organization. The first of these that's set wins:

1. **In code:** `CreateOptions { instance: "XXXXXX".into(), .. }` (also `ConnectOptions` for `connect`,
   `list` and `kill`).
2. **Environment variable:** `export CHAINCTL_SANDBOX_INSTANCE=XXXXXX`.
3. **Auto-detected:** if you belong to exactly one organization, it's used. With several, the error lists
   them. Turn detection off with `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` or `no_detect_instance` on
   `workspaces::Options`.

List your organization IDs with `chainguard-sandbox orgs`,
`chainguard_sandbox::list_organizations()`, or `chainctl iam organizations list`.

## Environment Variables

| Variable | Effect |
|---|---|
| `CHAINCTL_SANDBOX_INSTANCE` | The organization ID (overrides auto-detection) |
| `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` | Never auto-detect the organization |
| `CHAINCTL_AUTH_TOKEN` | Use this token as-is (audience `https://sessions.chainbox.dev`) |
| `CHAINGUARD_SANDBOX_NO_BROWSER=1` | Never open a browser: fail with `Error::LoginRequired` instead (scripts and CI) |
| `CHAINGUARD_SANDBOX_SOCIAL_LOGIN` | Preselect the login provider (`github`, `google-oauth2`, `gitlab`) |
| `CHAINGUARD_SANDBOX_LOGIN_AUDIENCES` | Comma-separated audiences a new login asks for (default: sessions and the account API) |
| `CHAINGUARD_SANDBOX_AUTH=chainctl` | Mint tokens with an installed, logged-in `chainctl` instead |
| `CHAINGUARD_SANDBOX_LOGIN_FILE` | Use another login file |
| `CHAINGUARD_SANDBOX_PLATFORM_URL` | Another platform (account) API URL (tests) |
| `CHAINCTL_SESSIONS_URL` | Another Sessions API URL (tests) |

## CI and Servers

For unattended use, give the client a token source:

- **A fixed token:** `CHAINCTL_AUTH_TOKEN`, or `workspaces::StaticToken`.
- **An assumable identity:** a CI job trades its OIDC token for a Chainguard token. An organization owner
  sets up the identity once:

  ```sh
  chainctl iam identities create NAME --parent ORG \
    --identity-issuer ISSUER --subject SUBJECT --role sandbox.user
  ```

  ```rust
  use std::sync::Arc;
  use chainguard_sandbox::workspaces::{self, Client, IdentityExchange, Options};

  let ts = workspaces::cached(Arc::new(IdentityExchange {
      identity: "<identity id>".into(),
      id_token: workspaces::github_actions_id_token(""),
      issuer: String::new(),
      audience: String::new(),
  }));
  let client = Client::new(Options { instance: "XXXXXX".into(), token_source: Some(ts), ..Default::default() }).await?;
  let sbx = chainguard_sandbox::Sandbox::create(chainguard_sandbox::CreateOptions { client: Some(client), ..Default::default() }).await?;
  ```

  `env_id_token(name)` and `file_id_token(path)` read the OIDC token from a variable or a file (GitLab,
  Kubernetes). Starter organizations can't create identities. In CI, set the organization explicitly.

## Crate Layout

| Path | What |
|---|---|
| `src/lib.rs`, `src/sandbox.rs`, `src/commands.rs`, `src/files.rs` | The simple sandbox API |
| `src/blocking.rs` | The same API without async |
| `src/setup.rs` | Turning Workspaces on |
| `src/workspaces/` | The lower-level client (`chainguard_sandbox::workspaces`): login and tokens, organizations, sessions, Exec, the first attach, the platform API |
| `src/workspaces/gen/` | Generated from `../proto/` (the subset of the Sessions API the SDK uses) |
| `src/scripts.rs` | Generated from `../go/sandbox/scripts.go` (the in-sandbox shell programs) |
| `src/bin/chainguard-sandbox.rs` | The CLI |
| `examples/` | The quickstart and the tour, used by the docs |
| `tests/` | Tests against local stand-ins (`cargo test`); `tests/live.rs` runs against the real service when asked |
| `gen/` | The code generator (not published) |

## Regenerating the Code

```sh
../scripts/gen-proto.sh rust                       # protoc + tonic-prost-build from ../proto into src/workspaces/gen
python3 ../scripts/gen_sandbox_scripts.py --only rust   # src/scripts.rs from ../go/sandbox/scripts.go
```

## Checks

```sh
cargo test                                        # unit and integration tests (no network)
CHAINGUARD_SANDBOX_LIVE=1 cargo test --test live  # against the real service: creates and deletes sandboxes
cargo clippy --all-targets
cargo doc --no-deps --open                        # the API reference
```

`../scripts/publish-rust.sh` runs these, packages the crate, and builds the packaged copy in an empty
project; see [`../PUBLISHING.md`](../PUBLISHING.md).
