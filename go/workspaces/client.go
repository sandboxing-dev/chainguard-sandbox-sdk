// Package workspaces is the low-level client under package sandbox: the
// parts of the Chainguard Workspaces Sessions API
// (`chainguard.sandbox.runtime.session.v1.Sessions`, the service
// `chainctl develop` uses) that the simple sandbox API needs, plus login and
// token handling. It is compatible with chainctl 0.2.398.
//
// It is unofficial: not published or supported by Chainguard. Most programs
// should use package sandbox instead.
//
// Typical use:
//
//	c, err := workspaces.New(ctx, workspaces.Options{Instance: orgID})
//	sessions, err := c.List(ctx, workspaces.ListOptions{})
//	res, err := c.Exec(ctx, sessions[0].GetName(), []string{"go", "test", "./..."},
//		workspaces.ExecOptions{Stdout: os.Stdout, Stderr: os.Stderr})
package workspaces

import (
	"context"
	"crypto/tls"
	"errors"
	"fmt"
	"net"
	"net/url"
	"os"
	"strings"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

const (
	// DefaultURL is the sandbox plane chainctl dials (overridable in chainctl
	// with $CHAINCTL_SESSIONS_URL).
	DefaultURL = "https://sessions.chainbox.dev"

	// DefaultAudience is the Chainguard token audience the plane accepts.
	DefaultAudience = "https://sessions.chainbox.dev"

	// DefaultWorkDir is where the repository lives inside the guest.
	DefaultWorkDir = "/work"

	// MaxExecStdin is the stdin size limit chainctl documents for exec.
	MaxExecStdin = 16 << 20

	// EnvSessionsURL, EnvInstance and EnvAuthToken mirror chainctl's
	// environment overrides.
	EnvSessionsURL = "CHAINCTL_SESSIONS_URL"
	EnvInstance    = "CHAINCTL_SANDBOX_INSTANCE"
	EnvAuthToken   = "CHAINCTL_AUTH_TOKEN"
)

// Options configures a Client.
type Options struct {
	// URL of the sandbox plane, e.g. "https://sessions.chainbox.dev" or
	// "http://localhost:8080" (plaintext). Defaults to $CHAINCTL_SESSIONS_URL,
	// then DefaultURL.
	URL string

	// Instance is the organization ID, used as "instances/{Instance}".
	// If empty, $CHAINCTL_SANDBOX_INSTANCE is used; if that is empty too, the
	// organization is auto-detected (see DetectInstance) unless
	// NoDetectInstance is set. List IDs with ListOrganizations, `chainguard-sandbox orgs`
	// or `chainctl iam organizations list`.
	Instance string

	// NoDetectInstance turns off organization auto-detection (also
	// $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1).
	NoDetectInstance bool

	// PlatformTokenSource mints account-API tokens for auto-detection.
	// Defaults to PlatformTokenSource() when TokenSource is unset and
	// $CHAINCTL_AUTH_TOKEN is empty; otherwise detection needs it set.
	PlatformTokenSource TokenSource

	// TokenSource supplies bearer tokens for DefaultAudience. Defaults to
	// DefaultTokenSource().
	TokenSource TokenSource

	// UserAgent is sent with every call.
	UserAgent string

	// DialOptions are appended to the options New builds.
	DialOptions []grpc.DialOption
}

// Client wraps a connection to the Sessions service.
type Client struct {
	conn      *grpc.ClientConn
	raw       sessionv1.SessionsClient
	instance  string
	detectErr error // why auto-detection didn't pick an organization
}

// New dials the sandbox plane.
func New(ctx context.Context, o Options) (*Client, error) {
	if o.URL == "" {
		o.URL = os.Getenv(EnvSessionsURL)
	}
	if o.URL == "" {
		o.URL = DefaultURL
	}
	if o.Instance == "" {
		o.Instance = os.Getenv(EnvInstance)
	}
	var detectErr error
	if o.Instance == "" && !o.NoDetectInstance && os.Getenv(EnvNoDetectInstance) != "1" {
		pts := o.PlatformTokenSource
		if pts == nil && o.TokenSource == nil && os.Getenv(EnvAuthToken) == "" {
			pts = PlatformTokenSource()
		}
		if pts == nil {
			detectErr = errors.New("needs Options.PlatformTokenSource when a custom token source or $" + EnvAuthToken + " is used")
		} else {
			dctx, cancel := context.WithTimeout(ctx, 11*time.Minute) // room for a browser login
			o.Instance, detectErr = DetectInstance(dctx, pts)
			cancel()
		}
	} else if o.Instance == "" {
		detectErr = errors.New("turned off")
	}
	if o.TokenSource == nil {
		o.TokenSource = DefaultTokenSource()
	}
	if o.UserAgent == "" {
		o.UserAgent = "chainguard-sandbox-sdk-go"
	}
	target, secure, err := parseURL(o.URL)
	if err != nil {
		return nil, err
	}
	tc := insecure.NewCredentials()
	if secure {
		tc = credentials.NewTLS(&tls.Config{MinVersion: tls.VersionTLS12})
	}
	opts := []grpc.DialOption{
		grpc.WithTransportCredentials(tc),
		grpc.WithPerRPCCredentials(bearer{ts: o.TokenSource, secure: secure}),
		grpc.WithUserAgent(o.UserAgent),
	}
	if inv, ok := o.TokenSource.(Invalidator); ok {
		// A token the plane rejects is dropped, so the next call fetches anew.
		opts = append(opts,
			grpc.WithChainUnaryInterceptor(invalidateUnary(inv)),
			grpc.WithChainStreamInterceptor(invalidateStream(inv)))
	}
	opts = append(opts, o.DialOptions...)
	conn, err := grpc.NewClient(target, opts...)
	if err != nil {
		return nil, fmt.Errorf("dialing %s: %w", o.URL, err)
	}
	c := NewFromConn(conn, o.Instance)
	c.conn = conn
	c.detectErr = detectErr
	return c, nil
}

// NewFromConn builds a Client on an existing connection (which must carry its
// own credentials). instance may be empty if only full resource names are used.
func NewFromConn(conn grpc.ClientConnInterface, instance string) *Client {
	return &Client{raw: sessionv1.NewSessionsClient(conn), instance: instance}
}

// Raw exposes the generated gRPC client. It covers only the SDK's subset of
// the Sessions API (see proto/ in the repository).
func (c *Client) Raw() sessionv1.SessionsClient { return c.raw }

// Instance returns the resource name "instances/{id}" used as the parent of
// sessions.
func (c *Client) Instance() string { return InstanceName(c.instance) }

// Close closes the connection if New created it.
func (c *Client) Close() error {
	if c.conn == nil {
		return nil
	}
	return c.conn.Close()
}

func (c *Client) parent() (string, error) {
	if c.instance == "" {
		return "", noInstanceError(c.detectErr)
	}
	return InstanceName(c.instance), nil
}

func parseURL(raw string) (target string, secure bool, err error) {
	if !strings.Contains(raw, "://") {
		raw = "https://" + raw
	}
	u, err := url.Parse(raw)
	if err != nil {
		return "", false, fmt.Errorf("parsing sessions URL %q: %w", raw, err)
	}
	switch u.Scheme {
	case "https":
		secure = true
	case "http":
	default:
		return "", false, fmt.Errorf("sessions URL %q: unsupported scheme %q", raw, u.Scheme)
	}
	host, port := u.Hostname(), u.Port()
	if port == "" {
		port = "443"
		if !secure {
			port = "80"
		}
	}
	return "dns:///" + net.JoinHostPort(host, port), secure, nil
}

func dropIfUnauthenticated(inv Invalidator, err error) {
	if status.Code(err) == codes.Unauthenticated {
		inv.Invalidate()
	}
}

func invalidateUnary(inv Invalidator) grpc.UnaryClientInterceptor {
	return func(ctx context.Context, method string, req, reply any, cc *grpc.ClientConn, invoker grpc.UnaryInvoker, opts ...grpc.CallOption) error {
		err := invoker(ctx, method, req, reply, cc, opts...)
		dropIfUnauthenticated(inv, err)
		return err
	}
}

func invalidateStream(inv Invalidator) grpc.StreamClientInterceptor {
	return func(ctx context.Context, desc *grpc.StreamDesc, cc *grpc.ClientConn, method string, streamer grpc.Streamer, opts ...grpc.CallOption) (grpc.ClientStream, error) {
		cs, err := streamer(ctx, desc, cc, method, opts...)
		dropIfUnauthenticated(inv, err)
		if err != nil {
			return nil, err
		}
		return invalidatingStream{ClientStream: cs, inv: inv}, nil
	}
}

type invalidatingStream struct {
	grpc.ClientStream
	inv Invalidator
}

func (s invalidatingStream) RecvMsg(m any) error {
	err := s.ClientStream.RecvMsg(m)
	dropIfUnauthenticated(s.inv, err)
	return err
}
