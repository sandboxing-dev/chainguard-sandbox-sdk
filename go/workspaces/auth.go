package workspaces

import (
	"bytes"
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"os/exec"
	"strings"
	"sync"
	"time"
)

// TokenSource returns a Chainguard bearer token whose audience includes
// DefaultAudience.
type TokenSource interface {
	Token(ctx context.Context) (string, error)
}

// TokenFunc adapts a function to TokenSource.
type TokenFunc func(ctx context.Context) (string, error)

func (f TokenFunc) Token(ctx context.Context) (string, error) { return f(ctx) }

// StaticToken always returns the same token.
type StaticToken string

func (t StaticToken) Token(context.Context) (string, error) {
	if t == "" {
		return "", errors.New("workspaces: empty token")
	}
	return string(t), nil
}

// ChainctlToken mints tokens by running
// `chainctl auth token --audience https://sessions.chainbox.dev`.
// It requires a prior `chainctl auth login --audience https://sessions.chainbox.dev`.
type ChainctlToken struct {
	// Path to chainctl; defaults to "chainctl" on $PATH.
	Path string
	// Audience defaults to DefaultAudience.
	Audience string
}

func (c ChainctlToken) Token(ctx context.Context) (string, error) {
	path, aud := c.Path, c.Audience
	if path == "" {
		path = "chainctl"
	}
	if aud == "" {
		aud = DefaultAudience
	}
	var stdout, stderr bytes.Buffer
	cmd := exec.CommandContext(ctx, path, "auth", "token", "--audience", aud)
	cmd.Stdout, cmd.Stderr = &stdout, &stderr
	if err := cmd.Run(); err != nil {
		return "", fmt.Errorf("chainctl auth token --audience %s: %w: %s", aud, err, strings.TrimSpace(stderr.String()))
	}
	tok := strings.TrimSpace(stdout.String())
	if tok == "" {
		return "", errors.New("chainctl auth token returned no token")
	}
	return tok, nil
}

// DefaultTokenSource uses $CHAINCTL_AUTH_TOKEN when set, otherwise the
// library's own login (a cached BrowserLoginToken): tokens are minted from
// the saved login, and a browser login runs when there is none. chainctl is
// not used; set $CHAINGUARD_SANDBOX_AUTH=chainctl to mint tokens with chainctl instead.
// $CHAINGUARD_SANDBOX_SOCIAL_LOGIN preselects the login's social provider (github,
// google-oauth2, gitlab), and $CHAINGUARD_SANDBOX_NO_BROWSER=1 makes a missing login an
// error (ErrLoginRequired) instead of opening a browser.
func DefaultTokenSource() TokenSource {
	if t := os.Getenv(EnvAuthToken); t != "" {
		return StaticToken(t)
	}
	if os.Getenv("CHAINGUARD_SANDBOX_AUTH") == "chainctl" {
		return Cached(ChainctlToken{})
	}
	return Cached(&BrowserLoginToken{
		Login:     LoginOptions{Connection: os.Getenv("CHAINGUARD_SANDBOX_SOCIAL_LOGIN")},
		NoBrowser: os.Getenv("CHAINGUARD_SANDBOX_NO_BROWSER") == "1",
	})
}

// Cached reuses a token until shortly before the "exp" claim of the JWT.
// Tokens whose expiry can't be read are cached for five minutes. Concurrent
// callers share one fetch, but each waits only as long as its own context
// allows. The client drops the cached token when the plane answers
// Unauthenticated (see Invalidator).
func Cached(src TokenSource) TokenSource {
	return &cachedToken{src: src}
}

// Invalidator is implemented by caching token sources; Invalidate forgets
// the cached token so the next call fetches a fresh one.
type Invalidator interface{ Invalidate() }

type cachedToken struct {
	src TokenSource
	mu  sync.Mutex
	tok string
	exp time.Time
	// inflight is the shared fetch in progress, if any.
	inflight *tokenFetch
}

type tokenFetch struct {
	done chan struct{}
	tok  string
	err  error
}

func (c *cachedToken) Token(ctx context.Context) (string, error) {
	c.mu.Lock()
	if c.tok != "" && time.Until(c.exp) > time.Minute {
		tok := c.tok
		c.mu.Unlock()
		return tok, nil
	}
	f := c.inflight
	if f == nil {
		f = &tokenFetch{done: make(chan struct{})}
		c.inflight = f
		// The fetch runs on its own context so one caller's cancellation
		// doesn't fail the others; it is bounded by the source itself.
		go c.fetch(f)
	}
	c.mu.Unlock()
	select {
	case <-f.done:
		return f.tok, f.err
	case <-ctx.Done():
		return "", ctx.Err()
	}
}

func (c *cachedToken) fetch(f *tokenFetch) {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Minute)
	defer cancel()
	tok, err := c.src.Token(ctx)
	c.mu.Lock()
	if err == nil {
		exp, ok := jwtExpiry(tok)
		if !ok {
			exp = time.Now().Add(5 * time.Minute)
		}
		c.tok, c.exp = tok, exp
	}
	c.inflight = nil
	c.mu.Unlock()
	f.tok, f.err = tok, err
	close(f.done)
}

// Invalidate forgets the cached token.
func (c *cachedToken) Invalidate() {
	c.mu.Lock()
	c.tok, c.exp = "", time.Time{}
	c.mu.Unlock()
}

// jwtExpiry reads the unverified "exp" claim.
func jwtExpiry(tok string) (time.Time, bool) {
	parts := strings.Split(tok, ".")
	if len(parts) != 3 {
		return time.Time{}, false
	}
	b, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return time.Time{}, false
	}
	var claims struct {
		Exp int64 `json:"exp"`
	}
	if json.Unmarshal(b, &claims) != nil || claims.Exp == 0 {
		return time.Time{}, false
	}
	return time.Unix(claims.Exp, 0), true
}

// bearer implements credentials.PerRPCCredentials.
type bearer struct {
	ts     TokenSource
	secure bool
}

func (b bearer) GetRequestMetadata(ctx context.Context, _ ...string) (map[string]string, error) {
	tok, err := b.ts.Token(ctx)
	if err != nil {
		return nil, err
	}
	return map[string]string{"authorization": "Bearer " + tok}, nil
}

func (b bearer) RequireTransportSecurity() bool { return b.secure }
