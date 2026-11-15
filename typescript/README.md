# Simple Chainguard Sandbox SDK for TypeScript (Bun)

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in and out, and delete it. The
API is shaped like the E2B, Vercel Sandbox and Modal SDKs.

Documentation: https://chainguard-sdk.sandboxing.dev/

> **Unofficial.** This SDK isn't affiliated with or supported by Chainguard. It's compatible with
> `chainctl` 0.2.398 and the Sessions API it uses.

## Install

The package isn't published to npm yet. Use it from a clone of the repository (Bun 1.3+):

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
cd chainguard-sandbox-sdk/typescript && bun install

# in your own project:
bun add ../chainguard-sandbox-sdk/typescript
```

## Quick Example

```ts
import { Sandbox } from "chainguard-sandbox-sdk";

{
  await using sbx = await Sandbox.create({ packages: ["bash", "coreutils", "python-3.13"] }); // deleted at the end of the block
  const r = await sbx.commands.run("python3 -c 'print(2+2)'");
  console.log(r.stdout); // 4
  await sbx.files.write("notes/hello.txt", "hi");
  console.log(await sbx.files.read("notes/hello.txt"));
}
process.exit(0); // connect-node keeps an HTTP/2 connection open; exit outside the block (see Bun Specifics)
```

More: `examples/quickstart.ts` and `examples/tour.ts` (every part of the API). Run the quickstart with
`bun examples/quickstart.ts` from this directory.

## Logging In

The first call opens your browser to log in to Chainguard; the login is saved and refreshed, and shared
with the Go and Python SDKs. To log in ahead of time, or check the saved login:

```sh
bun bin/chainguard-sandbox.ts login      # optional: github, google-oauth2 or gitlab
bun bin/chainguard-sandbox.ts whoami
bun bin/chainguard-sandbox.ts logout
```

A saved login lasts as long as it's used at least once every 20 hours. If a run opens your browser, the
login expired: approve it and the run continues.

The CLI also lists your organizations (`orgs`) and your sandboxes (`ls`), and deletes one (`rm ID`).

## Choosing the Organization

Every sandbox lives in a Chainguard organization. The first of these that's set wins:

1. **In code:** `Sandbox.create({ instance: "XXXXXX" })` (also `connect`, `list` and `kill`).
2. **Environment variable:** `export CHAINCTL_SANDBOX_INSTANCE=XXXXXX`.
3. **Auto-detected:** if you belong to exactly one organization, it's used. With several, the error lists
   them. Turn detection off with `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` or `{ detectInstance: false }` on a `Client`.

List your organization IDs with `bun bin/chainguard-sandbox.ts orgs`, `listOrganizations()`, or
`chainctl iam organizations list`.

## Environment Variables

| Variable | Effect |
|---|---|
| `CHAINCTL_SANDBOX_INSTANCE` | The organization ID (overrides auto-detection) |
| `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` | Never auto-detect the organization |
| `CHAINCTL_AUTH_TOKEN` | Use this token as-is (audience `https://sessions.chainbox.dev`) |
| `CHAINGUARD_SANDBOX_NO_BROWSER=1` | Never open a browser: fail with a "login required" error instead (scripts and CI) |
| `CHAINGUARD_SANDBOX_SOCIAL_LOGIN` | Preselect the login provider (`github`, `google-oauth2`, `gitlab`) |
| `CHAINGUARD_SANDBOX_LOGIN_AUDIENCES` | Comma-separated audiences a new login asks for (default: sessions and the account API) |
| `CHAINGUARD_SANDBOX_AUTH=chainctl` | Mint tokens with an installed, logged-in `chainctl` instead |
| `CHAINGUARD_SANDBOX_LOGIN_FILE` | Use another login file |
| `CHAINCTL_SESSIONS_URL` | Another Sessions API URL (tests) |

## CI and Servers

For unattended use, give the client a token source:

- **A fixed token:** `CHAINCTL_AUTH_TOKEN`, or `staticToken(tok)`.
- **An assumable identity:** a CI job trades its OIDC token for a Chainguard token. An organization owner
  sets up the identity once:

  ```sh
  chainctl iam identities create NAME --parent ORG \
    --identity-issuer ISSUER --subject SUBJECT --role sandbox.user
  ```

  ```ts
  import { Client, identityExchange, githubActionsIdToken } from "chainguard-sandbox-sdk/workspaces";
  import { Sandbox } from "chainguard-sandbox-sdk";

  const client = new Client({
    instance: "XXXXXX",
    tokenSource: identityExchange({ identity: "<identity id>", idToken: githubActionsIdToken() }),
  });
  await using sbx = await Sandbox.create({ client });
  ```

  `envIdToken(name)` and `fileIdToken(path)` read the OIDC token from a variable or a file (GitLab,
  Kubernetes). Starter organizations can't create identities. In CI, set the organization explicitly.

## Package Layout

| Path | What |
|---|---|
| `src/sandbox/` | The simple sandbox API (`chainguard-sandbox-sdk`) |
| `src/` | The lower-level client (`chainguard-sandbox-sdk/workspaces`): login and tokens, organizations, sessions, Exec, the first attach |
| `src/gen/` | Generated from `../proto/` (the subset of the Sessions API the SDK uses) |
| `bin/chainguard-sandbox.ts` | The CLI |
| `examples/` | The quickstart and the tour, used by the docs |
| `test/` | Unit tests against local fakes (`bun test`) |

## Regenerating the API Code

```sh
bun run gen     # ../scripts/gen-proto.sh typescript: protoc + protoc-gen-es from ../proto into src/gen
```

## Bun Specifics

- **Exit explicitly, outside `await using`.** connect-node keeps its HTTP/2 session open after calls, so a
  script should end with `process.exit()`. Call it after the block that holds `await using`: exiting inside
  the block skips the disposal, and the sandbox keeps running.
- **Output before exit:** under Bun, data queued for a pipe can be dropped at exit. Write with
  `await Bun.write(Bun.stdout, …)` before exiting (the CLI does this).

## Checks

```sh
bun test                          # unit tests (no network)
bunx tsc --noEmit                 # the package
bunx tsc --noEmit -p examples     # the examples
```
