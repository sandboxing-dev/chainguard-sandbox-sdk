// Package sandbox is a simple sandbox API on Chainguard Workspaces, shaped
// like the Vercel Sandbox, E2B and Modal Sandbox SDKs: create a sandbox, run
// commands in it, move files in and out, and delete it.
//
//	sb, err := sandbox.Create(ctx, &sandbox.CreateOptions{Packages: []string{"python-3.13"}})
//	if err != nil { ... }
//	defer sb.Kill(context.Background())
//	res, err := sb.Commands.Run(ctx, "python3 -c 'print(2+2)'", nil)
//	fmt.Print(res.Stdout) // 4
//	_, err = sb.Files.Write(ctx, "data/input.txt", []byte("hello"))
//
// A sandbox is a Workspaces session created from an empty workspace: the
// package creates the session with an environment document (your package
// list), waits for the guest, and completes the required first upload with an
// empty tree. It is built on package workspaces; see the sandbox docs for
// how it maps to the Sessions API and what differs from other sandbox
// services (no exposed ports, one command at a time per sandbox, and more).
package sandbox

import (
	"context"
	"errors"
	"fmt"
	"path"
	"strings"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// DefaultPackages is the toolchain a sandbox gets when CreateOptions.Packages
// is nil: a shell, the core tools, git, curl, jq, Python and Node.js. Package
// names are Chainguard (Wolfi) package names, e.g. "python-3.12", "go",
// "nodejs-22", "bun". busybox, git, curl, bash and a few more are always
// installed.
var DefaultPackages = []string{"bash", "coreutils", "curl", "git", "jq", "python-3.13", "nodejs-22"}

// DefaultWorkdir is where commands run and relative paths resolve.
const DefaultWorkdir = "/work"

// labelPrefix marks sessions created by this package (the workspace label).
const labelPrefix = "/chainguard-sandbox/"

// GitSource starts the sandbox from a GitHub repository instead of an empty
// tree: the plane clones it into /work (no upload).
type GitSource struct {
	// URL of a github.com repository ("github.com/o/r", "https://github.com/o/r", "git@github.com:o/r.git").
	URL string
	// Revision to check out; empty means the default branch.
	Revision string
}

// CreateOptions configures Create. The zero value (or nil) is a sandbox with
// DefaultPackages and open egress.
type CreateOptions struct {
	// Packages installed in the sandbox (Chainguard package names). Nil means
	// DefaultPackages. They are resolved when the sandbox is created; an
	// unknown package fails Create.
	Packages []string
	// Envs are environment variables for every command (per-command Envs
	// override them). They are kept client-side, in this Sandbox value.
	Envs map[string]string
	// Name labels the sandbox. Its display name becomes "<name>/<word-word>".
	// Defaults to "sandbox".
	Name string
	// Egress is the outbound allowlist (hostnames; "*" allows all). Nil means
	// ["*"]. The platform always blocks cloud metadata, loopback and private
	// ranges. Enforcement of narrower allowlists is the plane's (not verified
	// by this package).
	Egress []string
	// EgressPolicy is what happens to a connection outside Egress: "deny"
	// (refuse it; the default) or "destroy" (tear the sandbox down).
	EgressPolicy string
	// Document replaces the generated environment document entirely (a
	// .chainguard/ci.yaml-style document). Packages, Egress and EgressPolicy
	// are then ignored.
	Document []byte
	// Source starts from a GitHub repository instead of an empty tree.
	Source *GitSource
	// StartTimeout bounds waiting for the guest to start (default 3 minutes).
	StartTimeout time.Duration
	// Client is the Workspaces client to use. Nil creates one with the
	// default login and the organization from Instance.
	Client *workspaces.Client
	// Instance is the organization ID (used when Client is nil). If empty,
	// $CHAINCTL_SANDBOX_INSTANCE is used; if that's empty too, the
	// organization is auto-detected when you belong to exactly one (turn off
	// with $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1). List IDs with ListOrganizations,
	// `chainguard-sandbox orgs` or `chainctl iam organizations list`.
	Instance string
}

// ConnectOptions configures Connect, List and the package-level Kill.
type ConnectOptions struct {
	// Envs for every command on the returned Sandbox (not stored remotely).
	Envs     map[string]string
	Client   *workspaces.Client
	Instance string
}

// Sandbox is a handle to one sandbox. Its methods are safe for concurrent
// use, but the sandbox runs one command at a time: concurrent calls queue in
// the plane.
type Sandbox struct {
	// ID is the sandbox's short ID (8 hex characters), usable with Connect.
	ID string
	// Name is the full resource name (instances/<org>/sessions/<id>).
	Name string
	// Commands runs commands in the sandbox.
	Commands *Commands
	// Files reads and writes the sandbox's filesystem.
	Files *Files

	c          *workspaces.Client
	ownsClient bool
	envs       map[string]string
}

// Info describes a sandbox.
type Info struct {
	ID          string
	Name        string
	DisplayName string
	// State is "creating", "running", "paused" or "stopped".
	State     string
	Region    string
	CreatedAt time.Time
	// ExpiresAt is when a paused sandbox is discarded (zero if not set).
	ExpiresAt time.Time
	CPUs      int
	MemoryMiB int64
}

func newClient(ctx context.Context, c *workspaces.Client, instance string) (*workspaces.Client, bool, error) {
	if c != nil {
		return c, false, nil
	}
	nc, err := workspaces.New(ctx, workspaces.Options{Instance: instance})
	if err != nil {
		return nil, false, err
	}
	return nc, true, nil
}

func newSandbox(c *workspaces.Client, owns bool, s *workspaces.Session, envs map[string]string) *Sandbox {
	sb := &Sandbox{ID: s.GetShortId(), Name: s.GetName(), c: c, ownsClient: owns, envs: copyEnv(envs)}
	sb.Commands = &Commands{sb: sb}
	sb.Files = &Files{sb: sb}
	return sb
}

// Document returns the environment document Create would send for o.
func Document(o *CreateOptions) []byte {
	if o == nil {
		o = &CreateOptions{}
	}
	if o.Document != nil {
		return o.Document
	}
	pkgs := o.Packages
	if pkgs == nil {
		pkgs = DefaultPackages
	}
	egress := o.Egress
	if egress == nil {
		egress = []string{"*"}
	}
	policy := o.EgressPolicy
	if policy == "" {
		policy = "deny"
	}
	var b strings.Builder
	b.WriteString("# Generated by chainguard-sandbox-sdk.\nversion: 1\nenvironment:\n  packages:\n")
	for _, p := range pkgs {
		fmt.Fprintf(&b, "    - %s\n", yamlString(p))
	}
	b.WriteString("checks:\n  develop:\n    network:\n      egress:")
	if len(egress) == 0 {
		b.WriteString(" []\n")
	} else {
		b.WriteString("\n")
		for _, h := range egress {
			fmt.Fprintf(&b, "        - %s\n", yamlString(h))
		}
	}
	fmt.Fprintf(&b, "      policy: %s\n", yamlString(policy))
	return []byte(b.String())
}

// yamlString quotes s as a YAML double-quoted scalar.
func yamlString(s string) string {
	r := strings.NewReplacer(`\`, `\\`, `"`, `\"`, "\n", `\n`)
	return `"` + r.Replace(s) + `"`
}

// Create starts a new sandbox and returns when it can run commands (about
// 8–20 seconds). On failure it deletes anything it created. A session the
// plane places on a host that never starts is deleted and recreated (up to
// three attempts).
func Create(ctx context.Context, o *CreateOptions) (*Sandbox, error) {
	if o == nil {
		o = &CreateOptions{}
	}
	c, owns, err := newClient(ctx, o.Client, o.Instance)
	if err != nil {
		return nil, err
	}
	sb, err := create(ctx, c, o)
	if err != nil {
		if owns {
			c.Close()
		}
		return nil, err
	}
	sb.ownsClient = owns
	return sb, nil
}

func create(ctx context.Context, c *workspaces.Client, o *CreateOptions) (*Sandbox, error) {
	doc := Document(o)
	name := o.Name
	if name == "" {
		name = "sandbox"
	}
	if strings.ContainsAny(name, "/\n") {
		return nil, fmt.Errorf("sandbox: Name %q must not contain '/' or newlines", name)
	}
	startTimeout := o.StartTimeout
	if startTimeout <= 0 {
		startTimeout = 3 * time.Minute
	}
	var lastErr error
	for attempt := 1; attempt <= 3; attempt++ {
		var req *workspaces.Session
		if o.Source != nil {
			req = workspaces.RepositorySession(o.Source.URL, o.Source.Revision)
			req.GetSource().FallbackDocument = doc
		} else {
			req = workspaces.WorkspaceSession(labelPrefix+name, "", "", doc)
		}
		s, err := c.Create(ctx, req)
		if err != nil {
			return nil, wrapErr(err)
		}
		// The name is captured now: s is replaced (by nil, on failure) below.
		created := s.GetName()
		cleanup := func() {
			dctx, cancel := context.WithTimeout(context.WithoutCancel(ctx), 30*time.Second)
			defer cancel()
			_ = c.Delete(dctx, created)
		}
		s, err = waitStarted(ctx, c, created, startTimeout)
		if errors.Is(err, errPlacement) {
			cleanup()
			lastErr = err
			continue
		}
		if err != nil {
			cleanup()
			return nil, err
		}
		if s.GetWorkspacePending() {
			if err := emptyUpload(ctx, c, s.GetName()); err != nil {
				cleanup()
				return nil, err
			}
		}
		return newSandbox(c, false, s, o.Envs), nil
	}
	return nil, fmt.Errorf("sandbox: the plane could not start a guest in 3 attempts: %w", lastErr)
}

// errPlacement: the session landed on a host that keeps failing to start it.
var errPlacement = errors.New("the guest failed to start")

// waitStarted polls until the guest has started. limit bounds the whole wait,
// including each GetSession call: a call that stalls is cut off at the
// deadline, so a late answer can never be reported as a start in time.
func waitStarted(ctx context.Context, c *workspaces.Client, name string, limit time.Duration) (*workspaces.Session, error) {
	t0 := time.Now()
	dctx, cancel := context.WithDeadline(ctx, t0.Add(limit))
	defer cancel()
	var last *workspaces.Session
	expired := func() (*workspaces.Session, error) {
		if err := ctx.Err(); err != nil { // the caller's own cancellation or deadline
			return nil, err
		}
		detail := fmt.Sprintf("the guest did not start within %s", limit)
		if last != nil {
			detail += fmt.Sprintf(" (phase %s: %s)", last.GetPhase(), last.GetMessage())
		}
		return nil, &TimeoutError{Op: "start", Detail: detail}
	}
	for {
		s, err := c.Get(dctx, name)
		if err != nil {
			if dctx.Err() != nil {
				return expired()
			}
			return nil, wrapErr(err)
		}
		last = s
		if s.GetGuestStartedTime() != nil {
			return s, nil
		}
		if strings.Contains(s.GetMessage(), "starting guest failed") && time.Since(t0) > placementGrace {
			return nil, fmt.Errorf("%w: %s", errPlacement, s.GetMessage())
		}
		select {
		case <-dctx.Done():
			return expired()
		case <-time.After(startPoll):
		}
	}
}

// Timings, as variables so tests can shorten them.
var (
	placementGrace = 30 * time.Second       // how long "starting guest failed" may show before giving up on a host
	startPoll      = time.Second            // between GetSession calls while starting
	killWait       = 30 * time.Second       // how long Kill waits for the session to disappear
	killPoll       = 500 * time.Millisecond // between GetSession calls while deleting
)

// emptyUpload completes the first attach with an empty tree. The plane
// refuses Exec until a shell attach has uploaded the workspace, and a no-shell
// attach must serve endpoints, so this is a short shell attach.
func emptyUpload(ctx context.Context, c *workspaces.Client, name string) error {
	ctx, cancel := context.WithTimeout(ctx, 2*time.Minute)
	defer cancel()
	if err := c.CompleteFirstAttach(ctx, name); err != nil {
		if ctx.Err() != nil {
			return &TimeoutError{Op: "start", Detail: "the first attach did not complete"}
		}
		return fmt.Errorf("sandbox: completing the first attach: %w", wrapErr(err))
	}
	return nil
}

// Connect returns a handle to an existing sandbox by short ID, display name
// or resource name. A paused sandbox resumes on its first command.
func Connect(ctx context.Context, id string, o *ConnectOptions) (*Sandbox, error) {
	if o == nil {
		o = &ConnectOptions{}
	}
	c, owns, err := newClient(ctx, o.Client, o.Instance)
	if err != nil {
		return nil, err
	}
	s, err := c.Resolve(ctx, id)
	if err != nil {
		if owns {
			c.Close()
		}
		if errors.Is(err, workspaces.ErrNotFound) {
			return nil, &NotFoundError{What: "sandbox", Name: id}
		}
		return nil, wrapErr(err)
	}
	if s.GetWorkspacePending() {
		if err := emptyUpload(ctx, c, s.GetName()); err != nil {
			if owns {
				c.Close()
			}
			return nil, err
		}
	}
	sb := newSandbox(c, owns, s, o.Envs)
	return sb, nil
}

// List returns the sandboxes this package created (sessions with a sandbox
// label) in the instance.
func List(ctx context.Context, o *ConnectOptions) ([]Info, error) {
	if o == nil {
		o = &ConnectOptions{}
	}
	c, owns, err := newClient(ctx, o.Client, o.Instance)
	if err != nil {
		return nil, err
	}
	if owns {
		defer c.Close()
	}
	ss, err := c.List(ctx, workspaces.ListOptions{})
	if err != nil {
		return nil, wrapErr(err)
	}
	var out []Info
	for _, s := range ss {
		if strings.HasPrefix(s.GetWorkspace().GetLabel(), labelPrefix) {
			out = append(out, info(s))
		}
	}
	return out, nil
}

// Kill deletes the sandbox with the given ID (short ID, display name or
// resource name). It returns false if there was none. Unlike Connect it does
// not attach to the sandbox first, so it also deletes one that never started
// or cannot be attached to. Errors as for Sandbox.Kill.
func Kill(ctx context.Context, id string, o *ConnectOptions) (bool, error) {
	if o == nil {
		o = &ConnectOptions{}
	}
	c, owns, err := newClient(ctx, o.Client, o.Instance)
	if err != nil {
		return false, err
	}
	if owns {
		defer c.Close()
	}
	s, err := c.Resolve(ctx, id)
	if errors.Is(err, workspaces.ErrNotFound) || status.Code(err) == codes.NotFound {
		return false, nil
	}
	if err != nil {
		return false, wrapErr(err)
	}
	return deleteAndWait(ctx, c, s.GetName())
}

// Kill deletes the sandbox and everything in it, and returns once it is
// gone, so the session slot is free for a new sandbox. It returns false if
// the sandbox was already gone. If the sandbox still exists 30 seconds after
// the delete request (or its disappearance can't be confirmed), Kill returns
// true with a *TimeoutError (Op "kill"): the deletion was requested but the
// slot may not be free yet. The handle's client is closed if Create or
// Connect created it.
func (sb *Sandbox) Kill(ctx context.Context) (bool, error) {
	defer sb.Close()
	return deleteAndWait(ctx, sb.c, sb.Name)
}

// deleteAndWait sends the delete request and waits until the session is gone.
func deleteAndWait(ctx context.Context, c *workspaces.Client, name string) (bool, error) {
	err := c.Delete(ctx, name)
	if status.Code(err) == codes.NotFound {
		return false, nil
	}
	if err != nil {
		return false, wrapErr(err)
	}
	// Deletion is asynchronous (the session disappears 1-8 s later). Each check
	// runs under the deadline, so a stalled call can't stretch the wait.
	dctx, cancel := context.WithTimeout(ctx, killWait)
	defer cancel()
	var lastErr error
	for {
		_, err := c.Get(dctx, name)
		if status.Code(err) == codes.NotFound {
			return true, nil
		}
		if lastErr = err; dctx.Err() == nil && err != nil {
			lastErr = wrapErr(err)
		}
		select {
		case <-dctx.Done():
			if err := ctx.Err(); err != nil { // the caller's own cancellation or deadline
				return true, err
			}
			detail := fmt.Sprintf("the sandbox still exists %s after the delete request", killWait)
			if lastErr != nil && !errors.Is(lastErr, context.DeadlineExceeded) && status.Code(lastErr) != codes.DeadlineExceeded {
				detail = fmt.Sprintf("could not confirm the sandbox is gone within %s (last check: %v)", killWait, lastErr)
			}
			return true, &TimeoutError{Op: "kill", Detail: detail}
		case <-time.After(killPoll):
		}
	}
}

// Close releases the handle's connection (if Create or Connect opened it)
// without touching the sandbox.
func (sb *Sandbox) Close() error {
	if sb.ownsClient {
		sb.ownsClient = false
		return sb.c.Close()
	}
	return nil
}

// Pause snapshots the sandbox (memory and disk) and stops it. Running
// processes, including background commands, resume where they were on the
// next command or file operation (about 80 seconds after an explicit pause).
func (sb *Sandbox) Pause(ctx context.Context) error {
	_, err := sb.c.Park(ctx, sb.Name)
	return wrapErr(err)
}

// KeepAlive asks the plane to hold the sandbox running (not park it when
// idle) for d. There is no hard deadline to set: an idle sandbox parks after
// about 10 minutes, and a parked one is discarded after 5 days.
func (sb *Sandbox) KeepAlive(ctx context.Context, d time.Duration) error {
	_, err := sb.c.Heartbeat(ctx, sb.Name, d)
	return wrapErr(err)
}

// GetInfo fetches the sandbox's current state.
func (sb *Sandbox) GetInfo(ctx context.Context) (Info, error) {
	s, err := sb.c.Get(ctx, sb.Name)
	if status.Code(err) == codes.NotFound {
		return Info{}, &NotFoundError{What: "sandbox", Name: sb.ID}
	}
	if err != nil {
		return Info{}, wrapErr(err)
	}
	return info(s), nil
}

// IsRunning reports whether the sandbox is running (not paused, stopped or
// still starting).
func (sb *Sandbox) IsRunning(ctx context.Context) (bool, error) {
	i, err := sb.GetInfo(ctx)
	var nf *NotFoundError
	if errors.As(err, &nf) {
		return false, nil
	}
	return i.State == "running", err
}

// Client returns the underlying Workspaces client, for anything this package
// doesn't wrap.
func (sb *Sandbox) Client() *workspaces.Client { return sb.c }

func info(s *workspaces.Session) Info {
	i := Info{ID: s.GetShortId(), Name: s.GetName(), DisplayName: s.GetDisplayName(), Region: s.GetRegion(),
		CPUs: int(s.GetMaxResourceClass().GetCpus()), MemoryMiB: s.GetMaxResourceClass().GetMemoryMib()}
	switch s.GetPhase() {
	case sessionv1.Phase_CREATING:
		i.State = "creating"
	case sessionv1.Phase_ATTACHED, sessionv1.Phase_IDLE:
		i.State = "running"
	case sessionv1.Phase_PARKED:
		i.State = "paused"
	default:
		i.State = "stopped"
	}
	if t := s.GetCreateTime(); t != nil {
		i.CreatedAt = t.AsTime()
	}
	if t := s.GetExpireTime(); t != nil {
		i.ExpiresAt = t.AsTime()
	}
	return i
}

// guestPath resolves p against DefaultWorkdir.
func guestPath(p string) string {
	if p == "" {
		return DefaultWorkdir
	}
	if path.IsAbs(p) {
		return path.Clean(p)
	}
	return path.Join(DefaultWorkdir, p)
}

func copyEnv(m map[string]string) map[string]string {
	out := make(map[string]string, len(m))
	for k, v := range m {
		out[k] = v
	}
	return out
}

// Organization is a Chainguard organization a sandbox can live in.
type Organization = workspaces.Organization

// ListOrganizations returns the organizations you belong to (their IDs are
// what CreateOptions.Instance and $CHAINCTL_SANDBOX_INSTANCE take). With
// exactly one, Create uses it automatically when no organization is set.
// It uses the saved login's account-API token (see the authentication docs).
func ListOrganizations(ctx context.Context) ([]Organization, error) {
	return workspaces.ListOrganizations(ctx, nil)
}
