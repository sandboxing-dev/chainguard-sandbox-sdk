# Simple Chainguard Sandbox SDK for TypeScript (Node.js and Bun)

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in and out, and delete it. The
API is shaped like the E2B, Vercel Sandbox and Modal SDKs.

Documentation: https://chainguard-sdk.sandboxing.dev/

> **Unofficial.** This SDK isn't affiliated with or supported by Chainguard. It's compatible with
> `chainctl` 0.2.398 and the Sessions API it uses.

## Install

It runs on Bun 1.3+ and Node.js 20.10+. The package isn't published to npm yet, and neither Bun nor npm
installs from a subdirectory of a Git repository, so clone the repository next to your project and add it
by path. With Bun, Starting from the directory you
keep projects in:

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
mkdir my-app && cd my-app
bun init -y
bun add ../chainguard-sandbox-sdk/typescript
```

`../chainguard-sandbox-sdk/typescript` is the path from `my-app` to the clone's `typescript` directory. In a
project you already have, run only the `bun add` line there, with the path from that project to the clone.
The CLI is then `bun run chainguard-sandbox` (`login`, `status`, `enable`, `orgs`, `ls`, `rm`), run from that
project's directory: it is the copy installed in the project's `node_modules`, not a package from npm.

With Node.js, build the clone once (Node.js runs the compiled JavaScript in `dist/`; Bun runs the
TypeScript directly), then add it the same way:

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
(cd chainguard-sandbox-sdk/typescript && npm install && npm run build)
mkdir my-app && cd my-app
npm init -y && npm pkg set type=module
npm install ../chainguard-sandbox-sdk/typescript
```

The CLI is then `npx --no-install chainguard-sandbox` (`--no-install` keeps `npx` from looking on npm when
it is run outside the project).

To work in the clone itself, everything runs from this directory (`typescript/`):

```sh
cd chainguard-sandbox-sdk/typescript
bun install
bun bin/chainguard-sandbox.ts login       # also: status, enable, orgs, ls, rm
bun examples/quickstart.ts                # create a sandbox, run a command, use files
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

## Node.js Specifics

- **`await using`** needs Node.js 24, or TypeScript compiling it for older versions. In plain JavaScript on
  Node.js 20 or 22, delete the sandbox yourself:

    ```js
    const sbx = await Sandbox.create({ packages: ["bash", "coreutils"] });
    try {
      console.log((await sbx.commands.run("uname -a")).stdout);
    } finally {
      await sbx.kill();
    }
    ```

- **ES modules only:** the package has no CommonJS build. Use `import`, in a project with
  `"type": "module"` or in `.mjs` files.
- **Exit explicitly**, as under Bun (next section): the HTTP/2 connection keeps the process alive.

## Bun Specifics

- **Exit explicitly, outside `await using`.** connect-node keeps its HTTP/2 session open after calls, so a
  script should end with `process.exit()`. Call it after the block that holds `await using`: exiting inside
  the block skips the disposal, and the sandbox keeps running.
- **Output before exit:** data queued for a pipe can be dropped at exit. Wait for the write's callback
  before exiting (the CLI does this).

## Checks

```sh
bun test                          # unit tests (no network)
bunx tsc --noEmit                 # the package
bunx tsc --noEmit -p examples     # the examples
bun run build                     # dist/: the JavaScript and type declarations Node.js uses
```

`../scripts/publish-typescript.sh` runs all of these, packs the package, and installs the packed file into
empty Node.js and Bun projects; see [`../PUBLISHING.md`](../PUBLISHING.md).
