package workspaces

import (
	"context"
	_ "embed"
	"errors"
	"fmt"
	"path"
	"strings"
	"time"

	"google.golang.org/protobuf/types/known/durationpb"
	"google.golang.org/protobuf/types/known/fieldmaskpb"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

// Session is the plane's session resource.
type Session = sessionv1.Session

// InstanceName returns "instances/{id}"; an already-qualified name is returned
// unchanged.
func InstanceName(id string) string {
	if strings.HasPrefix(id, "instances/") {
		return id
	}
	return "instances/" + id
}

// IsSessionName reports whether s is a full resource name
// "instances/{instance}/sessions/{session}".
func IsSessionName(s string) bool {
	p := strings.Split(s, "/")
	return len(p) == 4 && p[0] == "instances" && p[2] == "sessions" && p[1] != "" && p[3] != ""
}

// RepositorySession describes a session opened from a repository URL, like
// `chainctl develop github.com/org/repo`. It matches chainctl's request:
// the repository as https://github.com/owner/repo (bare "github.com/…" and SSH
// forms are normalized; the plane can't fetch the bare form), idle AUTO_HOLD,
// and DefaultEnvironment as the fallback for repos without .chainguard/ files.
// revision pins a commit; chainctl resolves the default branch's HEAD with
// `git ls-remote`, which this library leaves to the caller (empty = unpinned).
func RepositorySession(repository, revision string) *Session {
	return &Session{Idle: sessionv1.IdlePolicy_AUTO_HOLD, Environment: &sessionv1.Session_Source{Source: &sessionv1.Source{
		Repository:       RepositoryURL(repository),
		Revision:         revision,
		FallbackDocument: DefaultEnvironment,
	}}}
}

// RepositoryURL normalizes "github.com/o/r", "git@github.com:o/r.git",
// "ssh://git@github.com:22/o/r", "git://github.com/o/r" and similar (any host
// case, credentials stripped) to "https://github.com/o/r". Other URLs keep
// their scheme (credentials stripped); scp-style remotes on other hosts are
// returned unchanged; a bare "host/path" gets https://.
func RepositoryURL(repo string) string {
	u := NormalizeGitHubURL(repo)
	if strings.Contains(u, "://") || isSCPLike(u) {
		return u
	}
	return "https://" + u
}

// DefaultEnvironment is the environment document chainctl 0.2.398 uses for a
// directory without .chainguard/develop.yaml or ci.yaml. Its check is named
// "develop".
//
//go:embed default_environment.yaml
var DefaultEnvironment []byte

// WorkspaceSession describes a session whose /work tree is uploaded on the
// first attach (see CompleteFirstAttach):
//
//   - label identifies the session's workspace (the sandbox package uses
//     "/chainguard-sandbox/<name>");
//   - repository and revision are optional metadata;
//   - document is the environment document (YAML); nil uses
//     DefaultEnvironment with provenance SYNTHESIZED.
//
// The idle policy is AUTO_HOLD, chainctl's default.
func WorkspaceSession(label, repository, revision string, document []byte) *Session {
	prov := sessionv1.Provenance_AUTHORED
	if document == nil {
		document, prov = DefaultEnvironment, sessionv1.Provenance_SYNTHESIZED
	}
	return &Session{Idle: sessionv1.IdlePolicy_AUTO_HOLD, Environment: &sessionv1.Session_Workspace{Workspace: &sessionv1.Workspace{
		Label:      label,
		Repository: repository,
		Revision:   revision,
		Document:   document,
		Provenance: prov,
	}}}
}

// Create creates a session in the client's instance.
func (c *Client) Create(ctx context.Context, s *Session) (*Session, error) {
	parent, err := c.parent()
	if err != nil {
		return nil, err
	}
	return c.raw.CreateSession(ctx, &sessionv1.CreateSessionRequest{Parent: parent, Session: s})
}

// Get fetches a session by full resource name.
func (c *Client) Get(ctx context.Context, name string) (*Session, error) {
	return c.raw.GetSession(ctx, &sessionv1.GetSessionRequest{Name: name})
}

// ListOptions filters List.
type ListOptions struct {
	// Owner lists another user's sessions (needs the owner role); empty means
	// the caller's own.
	Owner    string
	PageSize int32
}

// List returns every session in the instance, following page tokens.
func (c *Client) List(ctx context.Context, o ListOptions) ([]*Session, error) {
	parent, err := c.parent()
	if err != nil {
		return nil, err
	}
	var out []*Session
	req := &sessionv1.ListSessionsRequest{Parent: parent, PageSize: o.PageSize, Owner: o.Owner}
	for {
		resp, err := c.raw.ListSessions(ctx, req)
		if err != nil {
			return nil, err
		}
		out = append(out, resp.GetSessions()...)
		if resp.GetNextPageToken() == "" {
			return out, nil
		}
		req.PageToken = resp.GetNextPageToken()
	}
}

// ErrNotFound and ErrAmbiguous are returned by Resolve.
var (
	ErrNotFound  = errors.New("workspaces: no matching session")
	ErrAmbiguous = errors.New("workspaces: reference matches more than one session")
)

// Resolve finds a session the way chainctl's SESSION argument does: a full
// resource name, a short ID, or a display name ("quiet-harbor",
// "app/quiet-harbor", "acme/app/quiet-harbor").
func (c *Client) Resolve(ctx context.Context, ref string) (*Session, error) {
	if IsSessionName(ref) {
		return c.Get(ctx, ref)
	}
	all, err := c.List(ctx, ListOptions{})
	if err != nil {
		return nil, err
	}
	// Exact matches (resource ID, short ID, display name, or
	// owner/display-name as chainctl prints it) win over suffix matches.
	var exact, suffix []*Session
	for _, s := range all {
		switch matchKind(s, ref) {
		case matchExact:
			exact = append(exact, s)
		case matchSuffix:
			suffix = append(suffix, s)
		}
	}
	hits := exact
	if len(hits) == 0 {
		hits = suffix
	}
	switch len(hits) {
	case 0:
		return nil, fmt.Errorf("%w: %q", ErrNotFound, ref)
	case 1:
		return hits[0], nil
	default:
		return nil, fmt.Errorf("%w: %q", ErrAmbiguous, ref)
	}
}

type match int

const (
	matchNone match = iota
	matchSuffix
	matchExact
)

func matchKind(s *Session, ref string) match {
	if ref == "" {
		return matchNone
	}
	if s.GetShortId() == ref || path.Base(s.GetName()) == ref {
		return matchExact
	}
	dn := s.GetDisplayName()
	if dn == "" {
		return matchNone
	}
	if dn == ref {
		return matchExact
	}
	// chainctl prints display names owner-qualified ("acme/app/quiet-harbor"
	// for display name "app/quiet-harbor"): one extra leading segment.
	if owner, rest, ok := strings.Cut(ref, "/"); ok && owner != "" && rest == dn {
		return matchExact
	}
	if strings.HasSuffix(dn, "/"+ref) {
		return matchSuffix
	}
	return matchNone
}

// Delete tears down the guest and discards its snapshot (or removes a reaped
// tombstone).
func (c *Client) Delete(ctx context.Context, name string) error {
	_, err := c.raw.DeleteSession(ctx, &sessionv1.DeleteSessionRequest{Name: name})
	return err
}

// Update applies a field-masked update, e.g. paths "display_name".
func (c *Client) Update(ctx context.Context, s *Session, paths ...string) (*Session, error) {
	return c.raw.UpdateSession(ctx, &sessionv1.UpdateSessionRequest{
		Session:    s,
		UpdateMask: &fieldmaskpb.FieldMask{Paths: paths},
	})
}

// Park asks the plane to snapshot and stop the VM ("Park now" in the menu).
func (c *Client) Park(ctx context.Context, name string) (*Session, error) {
	return c.Update(ctx, &Session{Name: name, ParkRequest: sessionv1.Session_PARK_REQUEST_PARK}, "park_request")
}

// Heartbeat marks the session in use; a non-zero holdFor keeps it running
// that long after detach (`--keep 8h`).
func (c *Client) Heartbeat(ctx context.Context, name string, holdFor time.Duration) (*Session, error) {
	req := &sessionv1.HeartbeatSessionRequest{Name: name}
	if holdFor > 0 {
		req.HoldFor = durationpb.New(holdFor)
	}
	return c.raw.HeartbeatSession(ctx, req)
}
