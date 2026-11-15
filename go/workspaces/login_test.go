package workspaces

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"net/http/httptest"
	"net/url"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"testing"
	"time"
)

func fakeJWT(aud ...string) string {
	enc := base64.RawURLEncoding.EncodeToString
	cl, _ := json.Marshal(map[string]any{"sub": "user1", "iss": DefaultIssuer, "aud": aud, "exp": time.Now().Add(time.Hour).Unix()})
	return enc([]byte(`{"alg":"none"}`)) + "." + enc(cl) + ".sig"
}

// fakeIssuer serves /sts/exchange_refresh_token with rotating refresh tokens.
type fakeIssuer struct {
	mu       sync.Mutex
	valid    map[string]bool // refresh tokens that may be spent
	n        int
	refreshs int
	lastAud  string
}

func (f *fakeIssuer) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r.URL.Path != "/sts/exchange_refresh_token" || r.Method != http.MethodPost {
		http.NotFound(w, r)
		return
	}
	f.refreshs++
	aud := r.URL.Query().Get("aud")
	if aud == "" {
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"code":3,"message":"Audience must be specified"}`)
		return
	}
	rt := strings.TrimPrefix(r.Header.Get("Authorization"), "Bearer ")
	if strings.HasPrefix(rt, "expired") { // what the real issuer sends for an expired refresh token
		w.WriteHeader(http.StatusBadRequest)
		io.WriteString(w, `{"code":3,"message":"invalid refresh token: refresh token expired at 2026-11-10 23:10:16 +0000 UTC"}`)
		return
	}
	if !f.valid[rt] {
		w.WriteHeader(http.StatusUnauthorized)
		io.WriteString(w, `{"code":16,"message":"invalid refresh token"}`)
		return
	}
	delete(f.valid, rt) // rotation: each refresh token is spent once
	f.n++
	next := fmt.Sprintf("rt-%d", f.n)
	f.valid[next] = true
	f.lastAud = aud
	json.NewEncoder(w).Encode(map[string]any{
		"token":        map[string]string{"token": fakeJWT(aud)},
		"refreshToken": map[string]string{"token": next},
	})
}

// completeLogin plays the browser: it follows the issuer's redirect to the
// callback with the given query, then loads "/" as the browser would.
func completeLogin(t *testing.T, query url.Values, seen *url.Values) func(string) error {
	return func(u string) error {
		pu, err := url.Parse(u)
		if err != nil {
			return err
		}
		if seen != nil {
			*seen = pu.Query()
		}
		cb, err := url.Parse(pu.Query().Get("redirect"))
		if err != nil {
			return err
		}
		// The issuer fills in the callback's query (token=…, or error=…).
		cb.RawQuery = query.Encode()
		go func() {
			resp, err := http.Get(cb.String()) // follows the redirect to "/"
			if err != nil {
				t.Errorf("callback: %v", err)
				return
			}
			b, _ := io.ReadAll(resp.Body)
			resp.Body.Close()
			if query.Get("token") != "" && (resp.StatusCode != 200 || !strings.Contains(string(b), "login complete")) {
				t.Errorf("success page: %d %q", resp.StatusCode, b)
			}
			if strings.Contains(resp.Request.URL.String(), "token=") {
				t.Errorf("the final URL still carries the token: %s", resp.Request.URL)
			}
		}()
		return nil
	}
}

func TestBrowserLoginSendsChainctlParametersAndReturnsTokens(t *testing.T) {
	var seen url.Values
	res, err := BrowserLogin(context.Background(), LoginOptions{
		Connection: "github",
		OpenURL:    completeLogin(t, url.Values{"token": {"tok"}, "refresh_token": {"rt"}}, &seen),
		Out:        io.Discard,
	})
	if err != nil {
		t.Fatal(err)
	}
	if res.Token != "tok" || res.RefreshToken != "rt" {
		t.Fatalf("got %+v", res)
	}
	want := map[string]string{
		"client_id": "auth0", "connection": "github", "audience": DefaultAudience + "," + PlatformAudience,
		"create_refresh_token": "true", "refresh_token_rotation": "true", "skip_registration": "true", "exit": "redirect",
	}
	for k, v := range want {
		if seen.Get(k) != v {
			t.Errorf("%s = %q, want %q", k, seen.Get(k), v)
		}
	}
	if r := seen.Get("redirect"); !strings.HasPrefix(r, "http://localhost:") || !strings.HasSuffix(r, "/callback?token=true&error=true") {
		t.Errorf("redirect = %q", r)
	}
}

func TestBrowserLoginErrors(t *testing.T) {
	_, err := BrowserLogin(context.Background(), LoginOptions{OpenURL: completeLogin(t, url.Values{"status_code": {"404"}}, nil), Out: io.Discard})
	if !errors.Is(err, ErrAccountNotFound) {
		t.Errorf("404: %v", err)
	}
	_, err = BrowserLogin(context.Background(), LoginOptions{OpenURL: completeLogin(t, url.Values{"status_code": {"500"}, "error": {"boom"}}, nil), Out: io.Discard})
	if err == nil || !strings.Contains(err.Error(), "boom") {
		t.Errorf("500: %v", err)
	}
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	var out strings.Builder
	_, err = BrowserLogin(ctx, LoginOptions{OpenURL: func(string) error { return errors.New("no browser") }, Out: &out})
	if !errors.Is(err, context.DeadlineExceeded) {
		t.Errorf("timeout: %v", err)
	}
	if !strings.Contains(out.String(), "https://issuer.enforce.dev/oauth?") {
		t.Errorf("the URL should be printed when the browser can't open: %q", out.String())
	}
}

func TestLoginServerIgnoresStrayRequests(t *testing.T) {
	s, err := newLoginServer()
	if err != nil {
		t.Fatal(err)
	}
	defer s.close()
	base := strings.TrimSuffix(strings.SplitN(s.callbackURL, "/callback", 2)[0], "/")
	for _, tc := range []struct {
		method, path string
		code         int
	}{{"GET", "/", 404}, {"POST", "/callback?token=x", 405}, {"GET", "/other", 404}} {
		req, _ := http.NewRequest(tc.method, base+tc.path, nil)
		resp, err := http.DefaultClient.Do(req)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
		if resp.StatusCode != tc.code {
			t.Errorf("%s %s = %d, want %d", tc.method, tc.path, resp.StatusCode, tc.code)
		}
	}
	select {
	case r := <-s.result:
		t.Fatalf("stray requests produced a result: %+v", r)
	default:
	}
	// Two callbacks: the first one wins, the second doesn't block or panic.
	for _, tok := range []string{"a", "b"} {
		resp, err := http.Get(base + "/callback?token=" + tok)
		if err != nil {
			t.Fatal(err)
		}
		resp.Body.Close()
	}
	if r := <-s.result; r.Token != "a" {
		t.Errorf("token = %q", r.Token)
	}
}

func TestBrowserLoginTokenRefreshesRotatesAndRelogs(t *testing.T) {
	fi := &fakeIssuer{valid: map[string]bool{}}
	ts := httptest.NewServer(fi)
	defer ts.Close()
	store := FileLoginStore{Path: filepath.Join(t.TempDir(), "sub", "login.json")}

	logins := 0
	src := &BrowserLoginToken{
		Store: store,
		Login: LoginOptions{Issuer: ts.URL, Out: io.Discard, OpenURL: func(u string) error {
			logins++
			fi.mu.Lock()
			fi.valid["rt-login"] = true
			fi.mu.Unlock()
			// The login's own token is for another audience, so a refresh follows.
			return completeLogin(t, url.Values{"token": {fakeJWT("https://console-api.enforce.dev")}, "refresh_token": {"rt-login"}}, nil)(u)
		}},
	}
	ctx := context.Background()

	// 1. No saved login: browser login, then a refresh for the sessions audience.
	tok, err := src.Token(ctx)
	if err != nil {
		t.Fatal(err)
	}
	if !tokenHasAudience(tok, DefaultAudience) || logins != 1 || fi.lastAud != DefaultAudience {
		t.Fatalf("aud ok=%v logins=%d lastAud=%q", tokenHasAudience(tok, DefaultAudience), logins, fi.lastAud)
	}
	saved, _ := store.Load(ts.URL)
	if saved != "rt-1" {
		t.Fatalf("saved = %q, want the rotated rt-1", saved)
	}
	if st, _ := os.Stat(store.Path); st.Mode().Perm() != 0o600 {
		t.Errorf("login file mode %v", st.Mode().Perm())
	}
	if st, _ := os.Stat(filepath.Dir(store.Path)); st.Mode().Perm() != 0o700 {
		t.Errorf("login dir mode %v", st.Mode().Perm())
	}

	// 2. Saved login: no browser; the refresh token rotates again.
	if _, err := src.Token(ctx); err != nil {
		t.Fatal(err)
	}
	if saved, _ := store.Load(ts.URL); saved != "rt-2" || logins != 1 {
		t.Fatalf("saved=%q logins=%d", saved, logins)
	}

	// 3. A revoked login is forgotten; NoBrowser reports ErrLoginRequired.
	fi.mu.Lock()
	fi.valid = map[string]bool{}
	fi.mu.Unlock()
	nb := *src
	nb.NoBrowser = true
	if _, err := nb.Token(ctx); !errors.Is(err, ErrLoginRequired) {
		t.Fatalf("NoBrowser: %v", err)
	}
	if saved, _ := store.Load(ts.URL); saved != "" {
		t.Fatalf("the revoked login should be dropped, got %q", saved)
	}
	// 4. ...and the browser source logs in again.
	if _, err := src.Token(ctx); err != nil || logins != 2 {
		t.Fatalf("relogin: %v logins=%d", err, logins)
	}
}

func TestBrowserLoginTokenSerializesRefreshes(t *testing.T) {
	fi := &fakeIssuer{valid: map[string]bool{"rt-0": true}}
	ts := httptest.NewServer(fi)
	defer ts.Close()
	store := FileLoginStore{Path: filepath.Join(t.TempDir(), "login.json")}
	if err := store.Save(ts.URL, "rt-0"); err != nil {
		t.Fatal(err)
	}
	// Separate token sources (as separate processes would be) sharing one file.
	var wg sync.WaitGroup
	errs := make(chan error, 8)
	for i := 0; i < 8; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			src := &BrowserLoginToken{Store: store, NoBrowser: true, Login: LoginOptions{Issuer: ts.URL}}
			_, err := src.Token(context.Background())
			errs <- err
		}()
	}
	wg.Wait()
	close(errs)
	for err := range errs {
		if err != nil {
			t.Fatalf("a concurrent refresh failed (spent token reused?): %v", err)
		}
	}
	if saved, _ := store.Load(ts.URL); saved != "rt-8" {
		t.Fatalf("saved = %q, want rt-8", saved)
	}
}

func TestLoginStoreStaleLockAndLogout(t *testing.T) {
	store := FileLoginStore{Path: filepath.Join(t.TempDir(), "login.json")}
	if err := os.WriteFile(store.Path+".lock", []byte("1\n"), 0o600); err != nil {
		t.Fatal(err)
	}
	old := time.Now().Add(-3 * time.Minute)
	os.Chtimes(store.Path+".lock", old, old)
	ctx, cancel := context.WithTimeout(context.Background(), time.Second)
	defer cancel()
	unlock, err := store.Lock(ctx)
	if err != nil {
		t.Fatalf("a stale lock should be taken over: %v", err)
	}
	unlock()
	if err := store.Save("", "rt"); err != nil {
		t.Fatal(err)
	}
	if err := store.Save("https://other.example/", "rt2"); err != nil {
		t.Fatal(err)
	}
	if err := Logout(store, ""); err != nil {
		t.Fatal(err)
	}
	if v, _ := store.Load(""); v != "" {
		t.Errorf("logout left %q", v)
	}
	if v, _ := store.Load("https://other.example"); v != "rt2" {
		t.Errorf("other issuer's login = %q", v)
	}
}

func TestRefreshLoginSendsAudienceAsQuery(t *testing.T) {
	fi := &fakeIssuer{valid: map[string]bool{"r": true}}
	ts := httptest.NewServer(fi)
	defer ts.Close()
	tok, next, err := RefreshLogin(context.Background(), ts.URL, "r", "")
	if err != nil {
		t.Fatal(err)
	}
	if !tokenHasAudience(tok, DefaultAudience) || next != "rt-1" {
		t.Fatalf("tok aud ok=%v next=%q", tokenHasAudience(tok, DefaultAudience), next)
	}
	if _, _, err := RefreshLogin(context.Background(), ts.URL, "r", ""); !errors.Is(err, ErrLoginExpired) {
		t.Fatalf("spent token: %v", err)
	}
}

func TestExpiredRefreshTokenAnswered400IsALoginExpiry(t *testing.T) {
	fi := &fakeIssuer{valid: map[string]bool{}}
	ts := httptest.NewServer(fi)
	defer ts.Close()
	if _, _, err := RefreshLogin(context.Background(), ts.URL, "expired-1", ""); !errors.Is(err, ErrLoginExpired) {
		t.Fatalf("400 'invalid refresh token': %v, want ErrLoginExpired", err)
	}
	store := FileLoginStore{Path: filepath.Join(t.TempDir(), "login.json")}
	if err := store.Save(ts.URL, "expired-1"); err != nil {
		t.Fatal(err)
	}
	src := &BrowserLoginToken{Store: store, NoBrowser: true, Login: LoginOptions{Issuer: ts.URL}}
	if _, err := src.Token(context.Background()); !errors.Is(err, ErrLoginRequired) {
		t.Fatalf("expired saved login: %v, want ErrLoginRequired", err)
	}
	if v, _ := store.Load(ts.URL); v != "" {
		t.Fatalf("the expired login should be forgotten, got %q", v)
	}
	// Other 400s (a malformed request) are not a login expiry.
	if refreshRefused(400, "Audience must be specified") {
		t.Fatal("a 400 about the request must not count as an expired login")
	}
}
