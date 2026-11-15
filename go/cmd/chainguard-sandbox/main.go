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
	"errors"
	"fmt"
	"os"
	"os/signal"
	"strings"
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
	case "status":
		workspacesStatus(ctx, args)
	case "enable":
		enable(ctx, args)
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

// setupArgs reads [--accept-terms] [--github ACCOUNT] [--port N] [ORG].
func setupArgs(args []string) *sandbox.SetupOptions {
	o := &sandbox.SetupOptions{}
	for i := 0; i < len(args); i++ {
		a := args[i]
		switch {
		case a == "--accept-terms" || a == "--yes":
			o.AcceptTerms = true
		case a == "--github" && i+1 < len(args):
			i++
			o.GitHubAccount = args[i]
		case a == "--port" && i+1 < len(args):
			i++
			if _, err := fmt.Sscan(args[i], &o.GitHubPort); err != nil {
				usage()
			}
		case strings.HasPrefix(a, "-") || o.Instance != "":
			usage()
		default:
			o.Instance = a
		}
	}
	return o
}

func printStatus(st *sandbox.WorkspacesStatus) {
	fmt.Printf("organization: %s\nplan:         %s\n", st.Organization, st.Plan)
	if st.Enabled {
		fmt.Println("workspaces:   on")
	} else {
		fmt.Println("workspaces:   off")
	}
	if st.MaxSandboxes > 0 || st.MaxSandboxesPerUser > 0 {
		fmt.Printf("sandboxes:    up to %d per user, %d in the organization\n", st.MaxSandboxesPerUser, st.MaxSandboxes)
	}
	if len(st.GitHubAccounts) > 0 {
		fmt.Println("github:       linked to", strings.Join(st.GitHubAccounts, ", "))
	} else if !st.Enabled {
		fmt.Println("github:       no account linked yet")
	}
	if len(st.MissingTerms) == 0 {
		fmt.Println("terms:        accepted")
	} else {
		fmt.Println("terms:        not accepted yet:")
		for _, d := range st.MissingTerms {
			fmt.Printf("  - %s: %s\n", d.Label, d.URL)
		}
	}
}

// workspacesStatus prints whether Workspaces is turned on for the organization.
func workspacesStatus(ctx context.Context, args []string) {
	st, err := sandbox.Status(ctx, setupArgs(args))
	if err != nil {
		fail(err)
	}
	printStatus(st)
	if !st.Ready() {
		fmt.Println("\nSandboxes can't be created yet. An organization owner can turn Workspaces on with: chainguard-sandbox enable")
		os.Exit(1)
	}
}

// enable turns Workspaces on: it shows the terms the organization still has to
// accept and asks before accepting them (or takes --accept-terms as the answer).
func enable(ctx context.Context, args []string) {
	o := setupArgs(args)
	st, err := sandbox.Status(ctx, o)
	if err != nil {
		fail(err)
	}
	if st.Ready() {
		printStatus(st)
		fmt.Println("\nWorkspaces is already on.")
		return
	}
	fmt.Printf("Turning on Chainguard Workspaces (and Checks) for organization %s.\n", st.Organization)
	if len(st.MissingTerms) > 0 && !o.AcceptTerms {
		fmt.Println("\nThe organization has to accept these documents first:")
		for _, d := range st.MissingTerms {
			fmt.Printf("  - %s: %s\n", d.Label, d.URL)
		}
		fmt.Print("\nDo you accept them on behalf of the organization? Type \"yes\" to accept: ")
		var answer string
		fmt.Scanln(&answer)
		if strings.ToLower(strings.TrimSpace(answer)) != "yes" {
			fail(errors.New("not accepted; nothing was changed"))
		}
		o.AcceptTerms = true
	}
	if len(st.GitHubAccounts) == 0 {
		fmt.Println("\nNext, a GitHub account is linked to the organization: Chainguard requires one before it turns")
		fmt.Println("Workspaces on. Your browser opens GitHub, where you choose the account (your own user, unless you")
		fmt.Println("passed --github ORG). The Chainguard GitHub App has to be installed on that account:")
		fmt.Println(" ", workspaces.GitHubAppInstallURL)
	}
	for attempt := 1; ; attempt++ {
		st, err = sandbox.Enable(ctx, o)
		var terms *sandbox.TermsNotAcceptedError
		if errors.As(err, &terms) {
			fail(fmt.Errorf("%w\nRe-run with --accept-terms once you have read them", err))
		}
		// The app isn't on the account yet: install it, then authorize again.
		var app *sandbox.GitHubAppNotInstalledError
		if errors.As(err, &app) && attempt < 3 {
			fmt.Println("\nThe Chainguard GitHub App is not installed on that GitHub account yet.")
			fmt.Println("Install it here (opening it in your browser), then come back:")
			fmt.Println(" ", app.InstallURL)
			_ = workspaces.OpenBrowser(app.InstallURL)
			fmt.Print("\nPress Enter once the app is installed, to authorize again: ")
			var line string
			fmt.Scanln(&line)
			continue
		}
		if err != nil {
			fail(err)
		}
		break
	}
	fmt.Println()
	printStatus(st)
	fmt.Println("\nWorkspaces is on. Create a sandbox with the SDK, or list them with: chainguard-sandbox ls")
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
	fmt.Fprintln(os.Stderr, "usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | status [ORG] | enable [--accept-terms] [--github ACCOUNT] [--port N] [ORG] | ls | rm ID")
	os.Exit(2)
}

func fail(err error) {
	fmt.Fprintln(os.Stderr, "chainguard-sandbox:", err)
	os.Exit(1)
}
