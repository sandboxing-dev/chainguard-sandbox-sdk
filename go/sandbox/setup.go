package sandbox

import (
	"context"
	"errors"
	"fmt"
	"io"
	"os"
	"strings"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// Workspaces has to be turned on for an organization before sandboxes can be
// created in it. Status reports whether it is, and Enable turns it on: the
// same steps `chainctl develop init --parent ORG` takes for an organization
// that already exists (accept the terms, link a GitHub account, then turn the
// feature on).

// ErrNotEnabled: Workspaces is not turned on for the organization. An owner
// can turn it on with Enable (or `chainguard-sandbox enable`).
var ErrNotEnabled = errors.New("sandbox: Workspaces is not turned on for this organization; an owner can turn it on with sandbox.Enable or `chainguard-sandbox enable`")

// TermsDocument is a legal document an organization accepts.
type TermsDocument = workspaces.TermsDocument

// TermsNotAcceptedError: the organization has not accepted the legal documents
// Workspaces is offered under. Enable accepts them when EnableOptions.AcceptTerms is set.
type TermsNotAcceptedError struct{ Missing []TermsDocument }

// GitHubLinkRequiredError: the organization has no GitHub account linked, which
// the service requires before it turns Workspaces on, and Enable was not
// allowed to open a browser to link one (SetupOptions.NoBrowser).
type GitHubLinkRequiredError struct{ Organization string }

func (e *GitHubLinkRequiredError) Error() string {
	return "sandbox: organization " + e.Organization + " has to link a GitHub account before Workspaces can be turned on; run Enable where it can open a browser, or `chainguard-sandbox enable`"
}

// GitHubAppNotInstalledError: the Chainguard GitHub App is not installed on the
// GitHub account being linked. Install it at InstallURL, then call Enable again.
type GitHubAppNotInstalledError struct {
	InstallURL string
	Err        error
}

func (e *GitHubAppNotInstalledError) Error() string {
	return fmt.Sprintf("sandbox: the Chainguard GitHub App is not installed on that GitHub account; install it at %s and try again (%v)", e.InstallURL, e.Err)
}
func (e *GitHubAppNotInstalledError) Unwrap() error { return e.Err }

func (e *TermsNotAcceptedError) Error() string {
	var names []string
	for _, d := range e.Missing {
		names = append(names, fmt.Sprintf("%s (%s)", d.Label, d.URL))
	}
	return "sandbox: the organization has not accepted the terms Workspaces is offered under: " + strings.Join(names, "; ")
}

// WorkspacesStatus is an organization's Workspaces setup.
type WorkspacesStatus struct {
	// Organization is the organization ID the status is for.
	Organization string
	// Enabled reports whether Workspaces is turned on. Sandboxes can be
	// created when it is and MissingTerms is empty.
	Enabled bool
	// MissingTerms are the required legal documents not accepted yet. While
	// Workspaces is off these are all the documents turning it on needs.
	MissingTerms []TermsDocument
	// GitHubAccounts are the GitHub organizations or users linked to the
	// organization. Turning Workspaces on needs at least one.
	GitHubAccounts []string
	// Plan is the organization's plan ("Starter", ...).
	Plan string
	// MaxSandboxes and MaxSandboxesPerUser are the plan's limits on sandboxes
	// existing at once (0: not reported).
	MaxSandboxes, MaxSandboxesPerUser int
}

// Ready reports whether sandboxes can be created.
func (s *WorkspacesStatus) Ready() bool { return s.Enabled && len(s.MissingTerms) == 0 }

// SetupOptions selects the organization for Status and Enable.
type SetupOptions struct {
	// Instance is the organization ID. If empty, $CHAINCTL_SANDBOX_INSTANCE is
	// used, and failing that the only organization you belong to.
	Instance string
	// TokenSource mints tokens for the platform API (default: the saved login).
	TokenSource workspaces.TokenSource
	// AcceptTerms lets Enable accept, on the organization's behalf, the legal
	// documents it has not accepted yet. Set it only when the person running
	// your program has read and agreed to them (Status lists them, with URLs).
	// Status ignores it.
	AcceptTerms bool
	// GitHubAccount is the GitHub organization or user to link when the
	// organization has none linked (Enable only). Empty means the GitHub user
	// who authorizes in the browser.
	GitHubAccount string
	// GitHubPort is the loopback port for GitHub's redirect (default 8989).
	GitHubPort int
	// NoBrowser stops Enable from opening a browser for the GitHub
	// authorization; it then fails with *GitHubLinkRequiredError if a link is needed.
	NoBrowser bool
	// Out receives instructions during the GitHub authorization (default os.Stderr).
	Out io.Writer
}

func setupOrg(ctx context.Context, o *SetupOptions) (string, error) {
	if o.Instance != "" {
		return o.Instance, nil
	}
	if v := os.Getenv(workspaces.EnvInstance); v != "" {
		return v, nil
	}
	return workspaces.DetectInstance(ctx, o.TokenSource)
}

// Status reports whether Workspaces is turned on for the organization and
// which required terms it still has to accept. It changes nothing.
func Status(ctx context.Context, o *SetupOptions) (*WorkspacesStatus, error) {
	if o == nil {
		o = &SetupOptions{}
	}
	org, err := setupOrg(ctx, o)
	if err != nil {
		return nil, err
	}
	ent, err := workspaces.WorkspacesEntitlement(ctx, o.TokenSource, org)
	if err != nil {
		return nil, err
	}
	st := &WorkspacesStatus{Organization: org, Enabled: ent.Active, Plan: ent.PlanName,
		MaxSandboxes: ent.MaxSandboxes, MaxSandboxesPerUser: ent.MaxSandboxesPerUser}
	// Once Workspaces is on, only its own terms matter. While it is off, list
	// everything turning it on will need, so it can all be agreed to up front.
	required := workspaces.WorkspacesTerms
	if !ent.Active {
		required = workspaces.EnableTerms
	}
	have, err := workspaces.AcceptedTerms(ctx, o.TokenSource, org)
	if err != nil {
		return nil, err
	}
	for _, id := range required {
		found := false
		for _, h := range have {
			found = found || h == id
		}
		if !found {
			st.MissingTerms = append(st.MissingTerms, workspaces.TermsDocumentInfo(id))
		}
	}
	// A member who may not list the links still gets the rest of the status.
	if linked, err := workspaces.LinkedGitHubOrganizations(ctx, o.TokenSource, org); err == nil {
		for _, l := range linked {
			st.GitHubAccounts = append(st.GitHubAccounts, l.Organization)
		}
	}
	return st, nil
}

// Enable turns Workspaces on for the organization, so sandboxes can be created
// in it. It is what `chainctl develop init --parent ORG` does for an existing
// organization, without chainctl. It needs the owner role, and does nothing if
// everything is already in place. The steps:
//
//  1. Accept the terms. Only with o.AcceptTerms; without it, an organization
//     with unaccepted terms gets a *TermsNotAcceptedError listing them.
//  2. Link a GitHub account, if the organization has none: the service
//     requires one. This opens a browser on GitHub, where you choose the
//     account, and waits for you to finish (up to 10 minutes). The Chainguard
//     GitHub App must be installed on the account; if it is not, the error is
//     a *GitHubAppNotInstalledError with the page to install it from.
//  3. Turn the feature on with the plan's allowance. This also turns on
//     Chainguard Checks, which the same switch controls.
func Enable(ctx context.Context, o *SetupOptions) (*WorkspacesStatus, error) {
	if o == nil {
		o = &SetupOptions{}
	}
	st, err := Status(ctx, o)
	if err != nil {
		return nil, err
	}
	if st.Ready() {
		return st, nil
	}
	accept := func(docs []TermsDocument) error {
		if !o.AcceptTerms {
			return &TermsNotAcceptedError{Missing: docs}
		}
		ids := make([]string, len(docs))
		for i, d := range docs {
			ids[i] = d.ID
		}
		return workspaces.AcceptTerms(ctx, o.TokenSource, st.Organization, ids)
	}
	if len(st.MissingTerms) > 0 {
		if err := accept(st.MissingTerms); err != nil {
			return nil, err
		}
	}
	if !st.Enabled && len(st.GitHubAccounts) == 0 {
		if err := linkGitHub(ctx, o, st.Organization); err != nil {
			return nil, err
		}
	}
	if !st.Enabled {
		err := workspaces.EnableSandbox(ctx, o.TokenSource, st.Organization)
		// The link could not be listed (or was removed meanwhile): make it now.
		var le *workspaces.PlatformError
		if errors.As(err, &le) && le.GitHubLinkRequired() && len(st.GitHubAccounts) > 0 {
			if err = linkGitHub(ctx, o, st.Organization); err == nil {
				err = workspaces.EnableSandbox(ctx, o.TokenSource, st.Organization)
			}
		}
		// The switch also covers Checks, whose terms the service may ask for too.
		var pe *workspaces.PlatformError
		if errors.As(err, &pe) && len(pe.MissingTerms) > 0 {
			if err = accept(pe.MissingTerms); err == nil {
				err = workspaces.EnableSandbox(ctx, o.TokenSource, st.Organization)
			}
		}
		if err != nil {
			return nil, err
		}
	}
	return Status(ctx, o)
}

// linkGitHub links a GitHub account to the organization: start the
// authorization, send the person to GitHub, and hand the result to the service.
func linkGitHub(ctx context.Context, o *SetupOptions, org string) error {
	if o.NoBrowser {
		return &GitHubLinkRequiredError{Organization: org}
	}
	start, err := workspaces.BeginGitHubOAuth(ctx, o.TokenSource)
	if err != nil {
		return err
	}
	actx, cancel := context.WithTimeout(ctx, 10*time.Minute)
	defer cancel()
	auth, err := workspaces.AuthorizeGitHub(actx, start, workspaces.GitHubAuthorizeOptions{Port: o.GitHubPort, Out: o.Out})
	if err != nil {
		return err
	}
	_, err = workspaces.LinkGitHubOrganization(ctx, o.TokenSource, org, o.GitHubAccount, auth)
	var pe *workspaces.PlatformError
	if errors.As(err, &pe) && pe.GitHubAppNotInstalled() {
		return &GitHubAppNotInstalledError{InstallURL: workspaces.GitHubAppInstallURL, Err: err}
	}
	return err
}

// notEnabled reports whether err is the plane refusing a call because the
// organization is not entitled to Workspaces.
func notEnabled(err error) bool {
	return status.Code(err) == codes.PermissionDenied && strings.Contains(status.Convert(err).Message(), "not entitled to Chainguard Workspaces")
}
