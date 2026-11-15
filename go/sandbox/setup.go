package sandbox

import (
	"context"
	"errors"
	"fmt"
	"os"
	"strings"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// Workspaces has to be turned on for an organization before sandboxes can be
// created in it. Status reports whether it is, and Enable turns it on: the
// same steps `chainctl develop init --parent ORG` takes for an organization
// that already exists (accept the terms, then turn the feature on).

// ErrNotEnabled: Workspaces is not turned on for the organization. An owner
// can turn it on with Enable (or `chainguard-sandbox enable`).
var ErrNotEnabled = errors.New("sandbox: Workspaces is not turned on for this organization; an owner can turn it on with sandbox.Enable or `chainguard-sandbox enable`")

// TermsDocument is a legal document an organization accepts.
type TermsDocument = workspaces.TermsDocument

// TermsNotAcceptedError: the organization has not accepted the legal documents
// Workspaces is offered under. Enable accepts them when EnableOptions.AcceptTerms is set.
type TermsNotAcceptedError struct{ Missing []TermsDocument }

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
	// MissingTerms are the required legal documents not accepted yet.
	MissingTerms []TermsDocument
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
	missing, err := workspaces.MissingWorkspacesTerms(ctx, o.TokenSource, org)
	if err != nil {
		return nil, err
	}
	return &WorkspacesStatus{Organization: org, Enabled: ent.Active, MissingTerms: missing, Plan: ent.PlanName,
		MaxSandboxes: ent.MaxSandboxes, MaxSandboxesPerUser: ent.MaxSandboxesPerUser}, nil
}

// Enable turns Workspaces on for the organization, so sandboxes can be created
// in it. It is what `chainctl develop init --parent ORG` does for an existing
// organization, without chainctl: accept the terms Workspaces is offered
// under, then turn the feature on with the plan's allowance. It needs the
// owner role, and does nothing if everything is already in place.
//
// Terms are accepted only with o.AcceptTerms; without it, an organization
// with unaccepted terms gets a *TermsNotAcceptedError listing them. It also
// turns on Chainguard Checks, which the same switch controls.
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
	if !st.Enabled {
		err := workspaces.EnableSandbox(ctx, o.TokenSource, st.Organization)
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

// notEnabled reports whether err is the plane refusing a call because the
// organization is not entitled to Workspaces.
func notEnabled(err error) bool {
	return status.Code(err) == codes.PermissionDenied && strings.Contains(status.Convert(err).Message(), "not entitled to Chainguard Workspaces")
}
