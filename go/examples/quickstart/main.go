// Quickstart: create a sandbox, run a command, write and read a file, delete it.
//
// Before anything else: your Chainguard organization must exist and have
// Workspaces turned on (one time, by an owner: `chainguard-sandbox enable`).
// Otherwise Create fails with "PermissionDenied: this organization is not
// entitled to Chainguard Workspaces". See the Quickstart in the docs.
//
// Before you run it (from the go/ directory of this repository):
//
//  1. Log in once. It opens your browser; the login is saved and shared by
//     the Go, Python, TypeScript and Rust libraries:
//
//     go run ./cmd/chainguard-sandbox login        # optional: add github, google-oauth2 or gitlab
//
//     A saved login lasts as long as it's used at least once every 20 hours.
//     If a run opens your browser, the login expired: approve it and the run
//     continues, or renew it ahead of time with the same login command.
//
//  2. Choose the organization (the "instance"). Any one of these works; the
//     first one set wins:
//
//     a. in code: CreateOptions.Instance (commented out below);
//     b. an environment variable, for this terminal session only:
//     export CHAINCTL_SANDBOX_INSTANCE=XXXXXX    # your organization ID
//     c. nothing: if you belong to exactly one organization, it is detected
//     automatically (turn that off with CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1).
//
//     List your organization IDs with either of:
//
//     go run ./cmd/chainguard-sandbox orgs
//     chainctl iam organizations list
//
//  3. Run it:
//
//     go run ./examples/quickstart

// --8<-- [start:program]
package main

import (
	"context"
	"fmt"
	"log"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/sandbox"
)

// --8<-- [start:quickstart]
func main() {
	// log.Fatal exits without running deferred calls, so the work is in run:
	// its deferred Kill has already deleted the sandbox when an error gets here.
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

	res, err := sb.Commands.Run(ctx, `python3 -c "print('hello from', __import__('platform').node())"`, nil)
	if err != nil {
		return err
	}
	fmt.Print(res.Stdout)

	if _, err := sb.Files.Write(ctx, "notes/hello.txt", []byte("hi")); err != nil {
		return err
	}
	text, err := sb.Files.ReadText(ctx, "notes/hello.txt")
	if err != nil {
		return err
	}
	fmt.Println(text)
	return nil
}

// --8<-- [end:quickstart]
// --8<-- [end:program]
