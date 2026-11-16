# Simple Chainguard Sandbox SDK for Go

Create a sandbox on [Chainguard Workspaces](https://console.chainguard.dev/workspaces/signup), run commands in it, move files in and out, and delete it: the
same workflow as Vercel Sandbox, E2B or Modal.

- **Docs:** https://chainguard-sdk.sandboxing.dev/
- **Packages:** `sandbox` (the simple sandbox API) and `workspaces` (the low-level client it's built on).
- **Unofficial:** not affiliated with or supported by Chainguard. It's compatible with `chainctl` 0.2.398.

## Install

Into a new module:

```sh
mkdir my-app && cd my-app
go mod init example.com/my-app
go get github.com/sandboxing-dev/chainguard-sandbox-sdk/go@latest
```

In a module you already have, run only the `go get` line. The CLI runs with
`go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest` (`login`, `status`,
`enable`, `orgs`, `ls`, `rm`).

From a clone of this repository, everything runs from this directory (`go/`) with shorter commands:

```sh
git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
cd chainguard-sandbox-sdk/go
go run ./cmd/chainguard-sandbox login     # also: status, enable, orgs, ls, rm
go run ./examples/quickstart              # create a sandbox, run a command, use files
```

Go 1.23 or newer.

## Quick Start

```go
package main

import (
	"context"
	"fmt"
	"log"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/sandbox"
)

func main() {
	// log.Fatal skips deferred calls, so the work (and its deferred Kill) is in run.
	if err := run(context.Background()); err != nil {
		log.Fatal(err)
	}
}

func run(ctx context.Context) error {
	sb, err := sandbox.Create(ctx, &sandbox.CreateOptions{
		Packages: []string{"bash", "coreutils", "python-3.13"},
		// Instance: "XXXXXX", // your organization ID; overrides $CHAINCTL_SANDBOX_INSTANCE and auto-detection
	})
	if err != nil {
		return err
	}
	defer sb.Kill(context.Background()) // delete the sandbox when done, on success and on error

	res, err := sb.Commands.Run(ctx, `python3 -c "print('hello')"`, nil)
	if err != nil {
		return err
	}
	fmt.Print(res.Stdout)
	return nil
}
```

More: [`examples/quickstart`](examples/quickstart/main.go) and [`examples/tour`](examples/tour/main.go)
(every part of the API). From this directory: `go run ./examples/quickstart`.

## Logging In

The first call opens your browser to log in to Chainguard. The login is saved and shared by the Go,
Python, TypeScript and Rust SDKs, and lasts as long as it's used at least once every 20 hours. To log in ahead of
time, or to renew an expired login:

```sh
go run ./cmd/chainguard-sandbox login        # from this directory; optional: github, google-oauth2 or gitlab
# or, without a checkout:
go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest login
```

`chainguard-sandbox` also has `whoami`, `orgs`, `ls` (your sandboxes), `rm ID` and `logout`.

## Choosing the Organization

Every sandbox lives in a Chainguard organization. The first of these that's set wins:

1. **In code:** `CreateOptions.Instance` (or `ConnectOptions.Instance`).
2. **Environment variable:** `export CHAINCTL_SANDBOX_INSTANCE=XXXXXX`.
3. **Auto-detected:** if you belong to exactly one organization. Turn this off with
   `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1`.

List your organization IDs with `go run ./cmd/chainguard-sandbox orgs`, `sandbox.ListOrganizations(ctx)` or
`chainctl iam organizations list`.

## Environment Variables

| Variable | Effect |
|---|---|
| `CHAINCTL_SANDBOX_INSTANCE` | The organization ID (overrides auto-detection) |
| `CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1` | Never auto-detect the organization |
| `CHAINCTL_AUTH_TOKEN` | Use this sessions token as-is (audience `https://sessions.chainbox.dev`) |
| `CHAINGUARD_SANDBOX_NO_BROWSER=1` | Never open a browser: fail with a "login required" error instead |
| `CHAINGUARD_SANDBOX_SOCIAL_LOGIN` | Preselect the login provider (`github`, `google-oauth2`, `gitlab`) |
| `CHAINGUARD_SANDBOX_LOGIN_AUDIENCES` | Comma-separated audiences a new login asks for. Default: sessions + account API |
| `CHAINGUARD_SANDBOX_AUTH=chainctl` | Mint tokens with an installed, logged-in `chainctl` instead |
| `CHAINGUARD_SANDBOX_LOGIN_FILE` | Use another login file |

## CI and Servers

In CI, use a Chainguard **assumable identity**: the job's OIDC token (GitHub Actions, GitLab, a Kubernetes
service account) is exchanged for a Chainguard token. An organization owner creates the identity once:

```sh
chainctl iam identities create ci-sandboxes --parent <org> \
  --identity-issuer https://token.actions.githubusercontent.com \
  --subject repo:OWNER/REPO:ref:refs/heads/main \
  --role sandbox.user
```

In the job:

```go
ts := workspaces.Cached(workspaces.IdentityExchange{
	Identity: "<identity ID>",
	IDToken:  workspaces.GitHubActionsIDToken(""), // needs `permissions: id-token: write`
	// or: workspaces.EnvIDToken("CI_ID_TOKEN")
	// or: workspaces.FileIDToken("/var/run/secrets/tokens/chainguard")
})
c, err := workspaces.New(ctx, workspaces.Options{Instance: "XXXXXX", TokenSource: ts})
if err != nil {
	log.Fatal(err)
}
sb, err := sandbox.Create(ctx, &sandbox.CreateOptions{Client: c})
```

Set the organization explicitly in CI. Starter organizations can't create identities.

## The Low-Level Package

`workspaces` is the client `sandbox` is built on. It covers only what the simple sandbox API needs:

- **Client:** `New`, `Options` (URL, organization, token sources).
- **Sessions:** `Create`, `Get`, `List`, `Resolve`, `Update`, `Park`, `Heartbeat`, `Delete`, plus
  `WorkspaceSession` and `RepositorySession`.
- **Commands:** `Exec` and `Output`.
- **First attach:** `CompleteFirstAttach`.
- **Login and tokens:** `BrowserLogin`, `BrowserLoginToken`, `RefreshLogin`, `FileLoginStore`, `Cached`,
  `IdentityExchange`.
- **Organizations:** `ListOrganizations`, `DetectInstance`.

Interactive terminals, workspace syncing, forwarding and the rest of the Workspaces API aren't included.

## Regenerating the Protocol Code

`api/sessionv1` is generated from [`../proto`](../proto), the subset of the Sessions API the SDKs use:

```sh
go install google.golang.org/protobuf/cmd/protoc-gen-go@v1.36.6
go install google.golang.org/grpc/cmd/protoc-gen-go-grpc@v1.5.1
../scripts/gen-proto.sh go
```

## Testing

```sh
go test -race ./...
```

The tests use in-process fakes; they don't contact Chainguard.
