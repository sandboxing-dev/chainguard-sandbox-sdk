package workspaces

// Browser login, without chainctl.
//
// The login flow is adapted from chainguard.dev/sdk/auth/login (v0.1.50,
// Copyright 2023 Chainguard, Inc., Apache-2.0): a callback server on a random
// localhost port, and the issuer's /oauth page opened in the browser with
// redirect=<callback>. The issuer redirects back with ?token=…&refresh_token=….
// The request parameters match what chainctl 0.2.398 sends (client_id=auth0,
// connection=<social login>, create_refresh_token, refresh_token_rotation,
// skip_registration). Differences from the SDK: the result is handed over
// once (the SDK can return an empty token if "/" is fetched first, and panics
// on a second hit), only GET is served, responses aren't cached, and the
// browser is opened without the github.com/pkg/browser dependency.

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"os/exec"
	"path/filepath"
	"runtime"
	"slices"
	"strconv"
	"strings"
	"sync"
	"time"
)

// LoginOptions configures BrowserLogin.
type LoginOptions struct {
	// Issuer defaults to DefaultIssuer.
	Issuer string
	// Audience is what the login's tokens may be minted for. Defaults to
	// DefaultLoginAudiences(): the sessions service plus Chainguard's
	// account API (to list your organizations). chainctl asks for every
	// platform audience.
	Audience []string
	// Connection preselects a social login: "github", "google-oauth2" or
	// "gitlab" (chainctl's --social-login). Empty lets the login page ask.
	Connection string
	// ClientID defaults to "auth0", as chainctl sends. Ignored with
	// IdentityProvider or OrgName.
	ClientID string
	// IdentityProvider is the ID of an organization's custom identity
	// provider; OrgName names an organization that has one (verified).
	// At most one of them.
	IdentityProvider, OrgName string
	// Identity is an assumable identity to log in as (optional).
	Identity string
	// NoRefreshToken skips asking for a refresh token. Without one, every
	// token after the first needs another browser login.
	NoRefreshToken bool
	// OpenURL opens the login page. Defaults to OpenBrowser. Return an error
	// to have the URL printed instead.
	OpenURL func(url string) error
	// Out receives the "open this URL" message. Defaults to os.Stderr.
	Out io.Writer
}

// LoginResult is what the issuer hands back.
type LoginResult struct {
	// Token is a Chainguard token for the requested audiences.
	Token string
	// RefreshToken mints further tokens (see RefreshLogin); empty with NoRefreshToken.
	RefreshToken string
}

// ErrAccountNotFound means the issuer knows no Chainguard account for the
// person who logged in (sign up once in the Chainguard Console, or register with chainctl).
var ErrAccountNotFound = errors.New("workspaces: login: no Chainguard account for this login (sign up for Workspaces at https://console.chainguard.dev/workspaces/signup, or register with `chainctl auth login`)")

// BrowserLogin runs the interactive login: it opens the issuer's login page
// in a browser and waits (until ctx is done) for the redirect back to a
// localhost callback.
func BrowserLogin(ctx context.Context, o LoginOptions) (LoginResult, error) {
	issuer := o.Issuer
	if issuer == "" {
		issuer = DefaultIssuer
	}
	aud := o.Audience
	if len(aud) == 0 {
		aud = DefaultLoginAudiences()
	}
	if o.IdentityProvider != "" && o.OrgName != "" {
		return LoginResult{}, errors.New("workspaces: login: set at most one of IdentityProvider and OrgName")
	}
	out := o.Out
	if out == nil {
		out = os.Stderr
	}
	open := o.OpenURL
	if open == nil {
		open = OpenBrowser
	}

	srv, err := newLoginServer()
	if err != nil {
		return LoginResult{}, fmt.Errorf("workspaces: login: starting the localhost callback server: %w", err)
	}
	defer srv.close()

	p := url.Values{}
	switch {
	case o.IdentityProvider != "":
		p.Set("idp_id", o.IdentityProvider)
	case o.OrgName != "":
		p.Set("idp_id", o.OrgName) // the issuer takes a verified org name here too
	default:
		cid := o.ClientID
		if cid == "" {
			cid = "auth0"
		}
		p.Set("client_id", cid)
		if o.Connection != "" {
			p.Set("connection", o.Connection)
		}
	}
	p.Set("audience", strings.Join(aud, ","))
	if !o.NoRefreshToken {
		p.Set("create_refresh_token", "true")
		p.Set("refresh_token_rotation", "true")
	}
	if o.Identity != "" {
		p.Set("identity", o.Identity)
	}
	p.Set("skip_registration", "true")
	p.Set("exit", "redirect")
	p.Set("redirect", srv.callbackURL)
	u := strings.TrimSuffix(issuer, "/") + "/oauth?" + p.Encode()

	if err := open(u); err != nil {
		fmt.Fprintf(out, "Open this URL in a browser to log in to Chainguard:\n  %s\n", u)
	} else {
		fmt.Fprintf(out, "Opened a browser to log in to Chainguard. If it didn't open, visit:\n  %s\n", u)
	}
	select {
	case r := <-srv.result:
		if r.err == nil {
			// Keep serving until the browser has loaded the success page
			// (the callback redirects there), so it doesn't show an error.
			select {
			case <-srv.pageServed:
			case <-time.After(5 * time.Second):
			case <-ctx.Done():
			}
		}
		return r.LoginResult, r.err
	case <-ctx.Done():
		return LoginResult{}, fmt.Errorf("workspaces: login: %w", ctx.Err())
	}
}

type loginOutcome struct {
	LoginResult
	err error
}

type loginServer struct {
	l           net.Listener
	hs          *http.Server
	callbackURL string
	result      chan loginOutcome // buffered; written once
	pageServed  chan struct{}     // closed once the success page was served
	pageOnce    sync.Once
	once        sync.Once
	mu          sync.Mutex
	got         bool
}

func newLoginServer() (*loginServer, error) {
	l, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		return nil, err
	}
	port := l.Addr().(*net.TCPAddr).Port
	s := &loginServer{
		l: l,
		// The issuer only redirects to localhost; the host name is what chainctl sends.
		callbackURL: fmt.Sprintf("http://localhost:%d/callback?token=true&error=true", port),
		result:      make(chan loginOutcome, 1),
		pageServed:  make(chan struct{}),
	}
	s.hs = &http.Server{Handler: s, ReadHeaderTimeout: 10 * time.Second}
	go s.hs.Serve(l) //nolint:errcheck // ends with close()
	return s, nil
}

// close lets a response in flight (an error or success page) finish, then
// closes everything; a hard Close could cut the browser off mid-response.
func (s *loginServer) close() {
	ctx, cancel := context.WithTimeout(context.Background(), 2*time.Second)
	defer cancel()
	if s.hs.Shutdown(ctx) != nil {
		_ = s.hs.Close()
	}
}

func (s *loginServer) finish(o loginOutcome) {
	s.once.Do(func() {
		s.mu.Lock()
		s.got = true
		s.mu.Unlock()
		s.result <- o
	})
}

func (s *loginServer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	w.Header().Set("Cache-Control", "no-store")
	w.Header().Set("Referrer-Policy", "no-referrer")
	if r.Method != http.MethodGet {
		http.Error(w, "method not allowed", http.StatusMethodNotAllowed)
		return
	}
	switch r.URL.Path {
	case "/callback":
		q := r.URL.Query()
		tok := q.Get("token")
		if tok == "" {
			code, _ := strconv.Atoi(q.Get("status_code"))
			if code == http.StatusNotFound {
				s.finish(loginOutcome{err: ErrAccountNotFound})
				http.Error(w, "No Chainguard account was found for this login.", http.StatusNotFound)
				return
			}
			msg := q.Get("error")
			if msg == "" {
				msg = "login failed"
			}
			s.finish(loginOutcome{err: fmt.Errorf("workspaces: login: the issuer reported an error (status %d): %s", code, msg)})
			http.Error(w, "Login failed: "+msg, http.StatusBadRequest)
			return
		}
		s.finish(loginOutcome{LoginResult: LoginResult{Token: tok, RefreshToken: q.Get("refresh_token")}})
		// Redirect so the tokens leave the address bar and history entry.
		http.Redirect(w, r, "/", http.StatusSeeOther)
	case "/":
		s.mu.Lock()
		done := s.got
		s.mu.Unlock()
		if !done {
			http.NotFound(w, r)
			return
		}
		w.Header().Set("Content-Type", "text/html; charset=utf-8")
		io.WriteString(w, loginDonePage) //nolint:errcheck
		s.pageOnce.Do(func() { close(s.pageServed) })
	default:
		http.NotFound(w, r)
	}
}

const loginDonePage = `<!doctype html><html><head><meta charset="utf-8"><title>Login complete</title>
<style>:root{color-scheme:light dark;font-family:system-ui,sans-serif}body{display:grid;place-items:center;height:100vh;margin:0}</style>
</head><body><div><h1>Chainguard login complete</h1><p>You can close this tab and return to your program.</p></div></body></html>`

// OpenBrowser opens u in the default browser (open on macOS, xdg-open on
// Linux and BSDs, the URL handler on Windows). No shell is involved.
func OpenBrowser(u string) error {
	var cmd *exec.Cmd
	switch runtime.GOOS {
	case "darwin":
		cmd = exec.Command("open", u)
	case "windows":
		cmd = exec.Command("rundll32", "url.dll,FileProtocolHandler", u)
	default:
		cmd = exec.Command("xdg-open", u)
	}
	return cmd.Run()
}

// --- refresh tokens -----------------------------------------------------------

// RefreshLogin trades a refresh token for a token for aud (DefaultAudience
// if empty). With rotation the issuer returns a new refresh token, which
// replaces the old one (newRefresh is empty if it didn't rotate). It calls
// SecurityTokenService/ExchangeRefreshToken over its HTTP binding.
func RefreshLogin(ctx context.Context, issuer, refreshToken, aud string) (token, newRefresh string, err error) {
	if issuer == "" {
		issuer = DefaultIssuer
	}
	if aud == "" {
		aud = DefaultAudience
	}
	// Like /sts/exchange, the HTTP binding takes the request fields as query
	// parameters (a JSON body is ignored: "Audience must be specified").
	q := url.Values{"aud": {aud}}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, strings.TrimSuffix(issuer, "/")+"/sts/exchange_refresh_token?"+q.Encode(), nil)
	if err != nil {
		return "", "", err
	}
	req.Header.Set("Authorization", "Bearer "+refreshToken)
	req.Header.Set("Accept", "application/json")
	hc := &http.Client{
		Timeout: 30 * time.Second,
		// Never re-send the refresh token to wherever a redirect points.
		CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse },
	}
	resp, err := hc.Do(req)
	if err != nil {
		return "", "", fmt.Errorf("refreshing the login: %w", err)
	}
	defer resp.Body.Close()
	b, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", "", err
	}
	if resp.StatusCode != http.StatusOK {
		var st struct {
			Message string `json:"message"`
		}
		_ = json.Unmarshal(b, &st)
		if st.Message == "" {
			st.Message = strings.TrimSpace(string(b))
		}
		e := fmt.Errorf("refreshing the login: HTTP %d: %s", resp.StatusCode, st.Message)
		if refreshRefused(resp.StatusCode, st.Message) {
			return "", "", fmt.Errorf("%w: %w", ErrLoginExpired, e)
		}
		return "", "", e
	}
	// TokenPair{token: RawToken{token}, refresh_token: RawToken{token}} in protojson.
	var pair struct {
		Token        struct{ Token string } `json:"token"`
		RefreshToken struct{ Token string } `json:"refreshToken"`
		RefreshAlt   struct{ Token string } `json:"refresh_token"`
	}
	if err := json.Unmarshal(b, &pair); err != nil || pair.Token.Token == "" {
		return "", "", errors.New("refreshing the login: response has no token")
	}
	nr := pair.RefreshToken.Token
	if nr == "" {
		nr = pair.RefreshAlt.Token
	}
	return pair.Token.Token, nr, nil
}

// refreshRefused reports whether the issuer refused the refresh token itself
// (expired, revoked, already spent), as opposed to a malformed request or an
// outage. It answers 401/403, and also 400 with "invalid refresh token: refresh
// token expired at …" (observed 2026-11-11).
func refreshRefused(status int, msg string) bool {
	switch status {
	case http.StatusUnauthorized, http.StatusForbidden:
		return true
	case http.StatusBadRequest:
		return strings.Contains(strings.ToLower(msg), "refresh token")
	}
	return false
}

// PlatformAudience is Chainguard's account (IAM) API, used to list your
// organizations (console-api.enforce.dev).
const PlatformAudience = "https://console-api.enforce.dev"

// DefaultLoginAudiences is what a browser login asks for: the sessions
// service and the account API. $CHAINGUARD_SANDBOX_LOGIN_AUDIENCES (comma-separated)
// overrides it; set it to DefaultAudience alone to keep logins from reaching
// the account API (organization auto-detection then needs another token).
func DefaultLoginAudiences() []string {
	if v := strings.TrimSpace(os.Getenv("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES")); v != "" {
		var out []string
		for _, a := range strings.Split(v, ",") {
			if a = strings.TrimSpace(a); a != "" {
				out = append(out, a)
			}
		}
		return out
	}
	return []string{DefaultAudience, PlatformAudience}
}

// refreshClaims reads a refresh token's (unverified) expiry and audiences.
// Chainguard refresh tokens are base64-encoded JSON.
func refreshClaims(rt string) (exp time.Time, aud []string, ok bool) {
	b, err := base64.StdEncoding.DecodeString(rt)
	if err != nil {
		if b, err = base64.RawStdEncoding.DecodeString(rt); err != nil {
			return time.Time{}, nil, false
		}
	}
	var c struct {
		Exp int64           `json:"exp"`
		Aud json.RawMessage `json:"aud"`
	}
	if json.Unmarshal(b, &c) != nil || c.Exp == 0 {
		return time.Time{}, nil, false
	}
	var one string
	if json.Unmarshal(c.Aud, &one) == nil {
		aud = []string{one}
	} else {
		_ = json.Unmarshal(c.Aud, &aud)
	}
	return time.Unix(c.Exp, 0), aud, true
}

// ErrLoginExpired: the saved login was refused; log in again.
var ErrLoginExpired = errors.New("workspaces: the saved login has expired or was revoked")

// ErrLoginRequired: no saved login, and the token source may not open a browser.
var ErrLoginRequired = errors.New("workspaces: not logged in (run `chainguard-sandbox login`, or use a BrowserLoginToken that may open a browser)")

// --- the saved login -------------------------------------------------------------

// LoginStore keeps refresh tokens between runs, per issuer.
type LoginStore interface {
	Load(issuer string) (refreshToken string, err error) // "" when there is none
	Save(issuer, refreshToken string) error              // "" deletes it
	// Lock serializes refresh-and-save across processes (refresh tokens
	// rotate, so two processes must not spend the same one).
	Lock(ctx context.Context) (unlock func(), err error)
}

// FileLoginStore stores logins in a JSON file (0600, in a 0700 directory).
// The Go, Python and TypeScript libraries share DefaultLoginFile, so one
// login serves all three.
type FileLoginStore struct{ Path string }

// DefaultLoginFile is <user config dir>/chainguard-sandbox/login.json: on macOS
// ~/Library/Application Support/chainguard-sandbox/login.json, on Linux
// $XDG_CONFIG_HOME (or ~/.config)/chainguard-sandbox/login.json. Overridden by
// $CHAINGUARD_SANDBOX_LOGIN_FILE.
func DefaultLoginFile() string {
	if p := os.Getenv("CHAINGUARD_SANDBOX_LOGIN_FILE"); p != "" {
		return p
	}
	dir, err := os.UserConfigDir()
	if err != nil {
		dir = os.TempDir()
	}
	return filepath.Join(dir, "chainguard-sandbox", "login.json")
}

type loginFile struct {
	Version int                   `json:"version"`
	Logins  map[string]savedLogin `json:"logins"`
}

type savedLogin struct {
	RefreshToken string    `json:"refresh_token"`
	Updated      time.Time `json:"updated"`
}

func (f FileLoginStore) path() string {
	if f.Path != "" {
		return f.Path
	}
	return DefaultLoginFile()
}

func (f FileLoginStore) read() (loginFile, error) {
	var lf loginFile
	b, err := os.ReadFile(f.path())
	if errors.Is(err, os.ErrNotExist) {
		return loginFile{Version: 1, Logins: map[string]savedLogin{}}, nil
	}
	if err != nil {
		return lf, err
	}
	if err := json.Unmarshal(b, &lf); err != nil {
		return lf, fmt.Errorf("%s: %w", f.path(), err)
	}
	if lf.Logins == nil {
		lf.Logins = map[string]savedLogin{}
	}
	return lf, nil
}

func (f FileLoginStore) Load(issuer string) (string, error) {
	lf, err := f.read()
	if err != nil {
		return "", err
	}
	return lf.Logins[normIssuer(issuer)].RefreshToken, nil
}

func (f FileLoginStore) Save(issuer, refreshToken string) error {
	lf, err := f.read()
	if err != nil {
		lf = loginFile{Logins: map[string]savedLogin{}} // a corrupt file is replaced
	}
	lf.Version = 1
	if refreshToken == "" {
		delete(lf.Logins, normIssuer(issuer))
	} else {
		lf.Logins[normIssuer(issuer)] = savedLogin{RefreshToken: refreshToken, Updated: time.Now().UTC()}
	}
	b, _ := json.MarshalIndent(lf, "", "  ")
	p := f.path()
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		return err
	}
	tmp, err := os.CreateTemp(filepath.Dir(p), ".login-*.json")
	if err != nil {
		return err
	}
	defer os.Remove(tmp.Name())
	if err := tmp.Chmod(0o600); err != nil {
		tmp.Close()
		return err
	}
	if _, err := tmp.Write(append(b, '\n')); err != nil {
		tmp.Close()
		return err
	}
	if err := tmp.Close(); err != nil {
		return err
	}
	return os.Rename(tmp.Name(), p)
}

// Lock takes <file>.lock, created exclusively; a lock older than two
// minutes is taken to be left over from a crashed process. The same scheme
// is used by the Python and TypeScript libraries.
func (f FileLoginStore) Lock(ctx context.Context) (func(), error) {
	p := f.path() + ".lock"
	if err := os.MkdirAll(filepath.Dir(p), 0o700); err != nil {
		return nil, err
	}
	for {
		fh, err := os.OpenFile(p, os.O_CREATE|os.O_EXCL|os.O_WRONLY, 0o600)
		if err == nil {
			fmt.Fprintf(fh, "%d\n", os.Getpid())
			fh.Close()
			return func() { os.Remove(p) }, nil
		}
		if !errors.Is(err, os.ErrExist) {
			return nil, err
		}
		if st, err := os.Stat(p); err == nil && time.Since(st.ModTime()) > 2*time.Minute {
			os.Remove(p)
			continue
		}
		select {
		case <-ctx.Done():
			return nil, fmt.Errorf("waiting for %s: %w", p, ctx.Err())
		case <-time.After(100 * time.Millisecond):
		}
	}
}

func normIssuer(s string) string {
	if s == "" {
		s = DefaultIssuer
	}
	return strings.TrimSuffix(s, "/")
}

// --- the token source ---------------------------------------------------------------

// BrowserLoginToken is a TokenSource backed by the library's own login: it
// mints tokens from the saved refresh token, and when there is none (or it
// has expired) runs BrowserLogin, unless NoBrowser is set. Wrap it in
// Cached. Concurrent processes share the saved login safely.
type BrowserLoginToken struct {
	Login LoginOptions
	// Store defaults to FileLoginStore{} (DefaultLoginFile).
	Store LoginStore
	// Audience of the returned tokens; defaults to DefaultAudience.
	Audience string
	// NoBrowser makes Token fail with ErrLoginRequired instead of opening a browser.
	NoBrowser bool
	// LoginTimeout bounds a browser login (default 10 minutes).
	LoginTimeout time.Duration
}

func (b *BrowserLoginToken) store() LoginStore {
	if b.Store != nil {
		return b.Store
	}
	return FileLoginStore{}
}

func (b *BrowserLoginToken) Token(ctx context.Context) (string, error) {
	aud := b.Audience
	if aud == "" {
		aud = DefaultAudience
	}
	tok, ok, err := b.fromSaved(ctx, aud)
	if err != nil || ok {
		return tok, err
	}
	if b.NoBrowser {
		return "", ErrLoginRequired
	}
	to := b.LoginTimeout
	if to <= 0 {
		to = 10 * time.Minute
	}
	lctx, cancel := context.WithTimeout(ctx, to)
	defer cancel()
	o := b.Login
	if len(o.Audience) == 0 {
		o.Audience = DefaultLoginAudiences()
	}
	if !slices.Contains(o.Audience, aud) {
		o.Audience = append(slices.Clone(o.Audience), aud)
	}
	// No lock while the person logs in (that can take minutes).
	res, err := BrowserLogin(lctx, o)
	if err != nil {
		return "", err
	}
	return b.finishLogin(ctx, res, aud)
}

// fromSaved mints a token from the saved login, under the store's lock.
// ok is false when there is no usable saved login.
func (b *BrowserLoginToken) fromSaved(ctx context.Context, aud string) (tok string, ok bool, err error) {
	st := b.store()
	unlock, err := st.Lock(ctx)
	if err != nil {
		return "", false, err
	}
	defer unlock()
	rt, err := st.Load(b.Login.Issuer)
	if err != nil || rt == "" {
		return "", false, err
	}
	tok, nrt, err := RefreshLogin(ctx, b.Login.Issuer, rt, aud)
	if errors.Is(err, ErrLoginExpired) {
		// Forget the login only if it really is unusable: expired, or refused
		// for an audience it covers. A login made without this audience is
		// still good for its own audiences, so it's kept.
		if exp, auds, ok := refreshClaims(rt); ok && time.Now().Before(exp) && !slices.Contains(auds, aud) {
			if b.NoBrowser {
				return "", false, fmt.Errorf("%w: the saved login doesn't cover %s; log in again (chainguard-sandbox login) to add it", ErrLoginRequired, aud)
			}
			return "", false, nil // a browser login asks for it, then replaces the saved one
		}
		return "", false, st.Save(b.Login.Issuer, "")
	}
	if err != nil {
		return "", false, err
	}
	if nrt != "" && nrt != rt {
		if err := st.Save(b.Login.Issuer, nrt); err != nil {
			return "", false, fmt.Errorf("saving the rotated login: %w", err)
		}
	}
	return tok, true, nil
}

// finishLogin saves a fresh login and returns a token for aud.
func (b *BrowserLoginToken) finishLogin(ctx context.Context, res LoginResult, aud string) (string, error) {
	st := b.store()
	unlock, err := st.Lock(ctx)
	if err != nil {
		return "", err
	}
	defer unlock()
	if res.RefreshToken != "" {
		if err := st.Save(b.Login.Issuer, res.RefreshToken); err != nil {
			return "", fmt.Errorf("saving the login: %w", err)
		}
	}
	if tokenHasAudience(res.Token, aud) {
		return res.Token, nil
	}
	if res.RefreshToken == "" {
		return "", fmt.Errorf("workspaces: login: the token isn't for %s and there's no refresh token to mint one", aud)
	}
	tok, nrt, err := RefreshLogin(ctx, b.Login.Issuer, res.RefreshToken, aud)
	if err != nil {
		return "", err
	}
	if nrt != "" {
		if err := st.Save(b.Login.Issuer, nrt); err != nil {
			return "", fmt.Errorf("saving the rotated login: %w", err)
		}
	}
	return tok, nil
}

// Logout forgets the saved login for issuer ("" = DefaultIssuer).
func Logout(store LoginStore, issuer string) error {
	if store == nil {
		store = FileLoginStore{}
	}
	return store.Save(issuer, "")
}

// Claims are a token's unverified identity claims (for display).
type Claims struct {
	Subject, Issuer string
	Audience        []string
	Expiry          time.Time
}

// TokenClaims decodes a JWT's claims without verifying it.
func TokenClaims(tok string) (Claims, error) {
	parts := strings.Split(tok, ".")
	if len(parts) != 3 {
		return Claims{}, errors.New("not a JWT")
	}
	b, err := base64.RawURLEncoding.DecodeString(parts[1])
	if err != nil {
		return Claims{}, err
	}
	var c struct {
		Sub string          `json:"sub"`
		Iss string          `json:"iss"`
		Aud json.RawMessage `json:"aud"`
		Exp int64           `json:"exp"`
	}
	if err := json.Unmarshal(b, &c); err != nil {
		return Claims{}, err
	}
	out := Claims{Subject: c.Sub, Issuer: c.Iss, Expiry: time.Unix(c.Exp, 0)}
	var one string
	if json.Unmarshal(c.Aud, &one) == nil {
		out.Audience = []string{one}
	} else {
		_ = json.Unmarshal(c.Aud, &out.Audience)
	}
	return out, nil
}

// tokenHasAudience reads the (unverified) "aud" claim.
func tokenHasAudience(tok, aud string) bool {
	c, err := TokenClaims(tok)
	if err != nil {
		return false
	}
	for _, a := range c.Audience {
		if a == aud {
			return true
		}
	}
	return false
}
