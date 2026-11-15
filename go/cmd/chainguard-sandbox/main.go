// Command chainguard-sandbox manages the login and the sandboxes of the Simple Chainguard
// Sandbox SDK.
//
//	chainguard-sandbox login [github|google-oauth2|gitlab]   # browser login, saved for later runs
//	chainguard-sandbox whoami                                # who the saved login is
//	chainguard-sandbox orgs                                  # your organization IDs
//	chainguard-sandbox ls                                    # your sandboxes
//	chainguard-sandbox rm ID                                 # delete a sandbox
//	chainguard-sandbox logout                                # forget the saved login
//
// Run it from a checkout (cd go && go run ./cmd/chainguard-sandbox login) or with
// go run github.com/sandboxing-dev/chainguard-sandbox-sdk/go/cmd/chainguard-sandbox@latest login.
package main

import (
	"context"
	"fmt"
	"os"
	"os/signal"
	"syscall"
	"text/tabwriter"
	"time"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/sandbox"
	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

func main() {
	if len(os.Args) < 2 {
		usage()
	}
	ctx, stop := signal.NotifyContext(context.Background(), os.Interrupt, syscall.SIGTERM)
	defer stop()
	args := os.Args[2:]
	switch os.Args[1] {
	case "login":
		login(ctx, args)
	case "logout":
		if err := workspaces.Logout(nil, ""); err != nil {
			fail(err)
		}
		fmt.Println("logged out:", workspaces.DefaultLoginFile())
	case "whoami":
		whoami(ctx)
	case "orgs":
		orgs, err := workspaces.ListOrganizations(ctx, nil)
		if err != nil {
			fail(err)
		}
		tw := tabwriter.NewWriter(os.Stdout, 0, 4, 2, ' ', 0)
		fmt.Fprintln(tw, "ID\tNAME")
		for _, o := range orgs {
			fmt.Fprintf(tw, "%s\t%s\n", o.ID, o.Name)
		}
		tw.Flush()
	case "ls":
		infos, err := sandbox.List(ctx, nil)
		if err != nil {
			fail(err)
		}
		tw := tabwriter.NewWriter(os.Stdout, 0, 4, 2, ' ', 0)
		fmt.Fprintln(tw, "ID\tNAME\tSTATE\tCREATED")
		for _, i := range infos {
			fmt.Fprintf(tw, "%s\t%s\t%s\t%s\n", i.ID, i.DisplayName, i.State, i.CreatedAt.Local().Format(time.DateTime))
		}
		tw.Flush()
	case "rm":
		if len(args) != 1 {
			usage()
		}
		found, err := sandbox.Kill(ctx, args[0], nil)
		if err != nil {
			fail(err)
		}
		if !found {
			fail(fmt.Errorf("no sandbox %q", args[0]))
		}
		fmt.Println("deleted", args[0])
	default:
		usage()
	}
}

// login runs the browser login and saves it (replacing any saved login).
func login(ctx context.Context, args []string) {
	o := workspaces.LoginOptions{Connection: os.Getenv("CHAINGUARD_SANDBOX_SOCIAL_LOGIN")}
	if len(args) > 1 {
		usage()
	}
	if len(args) == 1 {
		o.Connection = args[0]
	}
	lctx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	res, err := workspaces.BrowserLogin(lctx, o)
	if err != nil {
		fail(err)
	}
	if res.RefreshToken == "" {
		fail(fmt.Errorf("the issuer returned no refresh token, so the login can't be saved"))
	}
	if err := (workspaces.FileLoginStore{}).Save("", res.RefreshToken); err != nil {
		fail(err)
	}
	fmt.Println("logged in; saved to", workspaces.DefaultLoginFile())
	whoami(ctx)
}

// whoami mints a sessions token from the saved login and prints its claims (not the token).
func whoami(ctx context.Context) {
	ts := &workspaces.BrowserLoginToken{NoBrowser: true}
	tok, err := ts.Token(ctx)
	if err != nil {
		fail(err)
	}
	c, err := workspaces.TokenClaims(tok)
	if err != nil {
		fail(err)
	}
	fmt.Printf("subject:  %s\nissuer:   %s\naudience: %v\nexpires:  %s\n", c.Subject, c.Issuer, c.Audience, c.Expiry.Local().Format(time.DateTime))
}

func usage() {
	fmt.Fprintln(os.Stderr, "usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | ls | rm ID")
	os.Exit(2)
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "chainguard-sandbox:", err)
	os.Exit(1)
}
