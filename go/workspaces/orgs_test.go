package workspaces

import (
	"context"
	"encoding/base64"
	"encoding/json"
	"errors"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"
	"time"
)

func fakeIAM(t *testing.T, groups string) {
	t.Helper()
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/iam/v1/groups" || r.URL.Query().Get("uidp.in_root") != "true" || r.Header.Get("Authorization") != "Bearer platform-tok" {
			w.WriteHeader(http.StatusForbidden)
			w.Write([]byte(`{"code":7,"message":"nope"}`))
			return
		}
		w.Write([]byte(groups))
	}))
	t.Cleanup(srv.Close)
	t.Setenv("CHAINGUARD_SANDBOX_PLATFORM_URL", srv.URL)
}

func TestListAndDetectOrganizations(t *testing.T) {
	ts := StaticToken("platform-tok")
	fakeIAM(t, `{"items":[{"id":"b2","name":"zeta"},{"id":"a1","name":"alpha"},{"id":"a1/child","name":"sub"}]}`)
	orgs, err := ListOrganizations(context.Background(), ts)
	if err != nil || len(orgs) != 2 || orgs[0].Name != "alpha" || orgs[1].ID != "b2" {
		t.Fatalf("%+v %v", orgs, err)
	}
	_, err = DetectInstance(context.Background(), ts)
	var amb *AmbiguousOrganizationError
	if !errors.As(err, &amb) || !strings.Contains(err.Error(), "a1  alpha") {
		t.Fatalf("ambiguous: %v", err)
	}

	fakeIAM(t, `{"items":[{"id":"only1","name":"my-org"}]}`)
	if id, err := DetectInstance(context.Background(), ts); err != nil || id != "only1" {
		t.Fatalf("%q %v", id, err)
	}
	fakeIAM(t, `{"items":[]}`)
	if _, err := DetectInstance(context.Background(), ts); !errors.Is(err, ErrNoOrganization) {
		t.Fatalf("none: %v", err)
	}
	if _, err := ListOrganizations(context.Background(), StaticToken("wrong")); err == nil || !strings.Contains(err.Error(), "HTTP 403") {
		t.Fatalf("forbidden: %v", err)
	}
}

func TestNewDetectsTheInstance(t *testing.T) {
	t.Setenv(EnvInstance, "")
	t.Setenv(EnvNoDetectInstance, "")
	fakeIAM(t, `{"items":[{"id":"only1","name":"my-org"}]}`)
	ctx := context.Background()
	c, err := New(ctx, Options{URL: "http://127.0.0.1:1", TokenSource: StaticToken("x"), PlatformTokenSource: StaticToken("platform-tok")})
	if err != nil {
		t.Fatal(err)
	}
	if c.Instance() != "instances/only1" {
		t.Fatalf("instance %q", c.Instance())
	}
	c.Close()

	// Explicit and environment settings win over detection.
	t.Setenv(EnvInstance, "fromenv")
	c, _ = New(ctx, Options{URL: "http://127.0.0.1:1", TokenSource: StaticToken("x"), PlatformTokenSource: StaticToken("platform-tok")})
	if c.Instance() != "instances/fromenv" {
		t.Fatalf("env: %q", c.Instance())
	}
	c.Close()
	c, _ = New(ctx, Options{URL: "http://127.0.0.1:1", Instance: "explicit", TokenSource: StaticToken("x"), PlatformTokenSource: StaticToken("platform-tok")})
	if c.Instance() != "instances/explicit" {
		t.Fatalf("explicit: %q", c.Instance())
	}
	c.Close()

	// Turned off, or no platform token with a custom token source: a clear error.
	t.Setenv(EnvInstance, "")
	for _, o := range []Options{
		{URL: "http://127.0.0.1:1", TokenSource: StaticToken("x"), NoDetectInstance: true},
		{URL: "http://127.0.0.1:1", TokenSource: StaticToken("x")},
	} {
		c, _ := New(ctx, o)
		_, err := c.parent()
		if err == nil || !strings.Contains(err.Error(), "$CHAINCTL_SANDBOX_INSTANCE") || !strings.Contains(err.Error(), "chainctl iam organizations list") {
			t.Fatalf("error: %v", err)
		}
		c.Close()
	}
}

func refreshTok(exp time.Time, aud ...string) string {
	b, _ := json.Marshal(map[string]any{"exp": exp.Unix(), "aud": aud})
	return base64.StdEncoding.EncodeToString(b)
}

func TestSavedLoginKeptWhenItOnlyLacksAnAudience(t *testing.T) {
	// The fake issuer refuses unknown tokens with 401.
	fi := &fakeIssuer{valid: map[string]bool{}}
	ts := httptest.NewServer(fi)
	defer ts.Close()
	store := FileLoginStore{Path: filepath.Join(t.TempDir(), "login.json")}
	rt := refreshTok(time.Now().Add(time.Hour), DefaultAudience)
	store.Save(ts.URL, rt)
	src := &BrowserLoginToken{Store: store, NoBrowser: true, Audience: PlatformAudience, Login: LoginOptions{Issuer: ts.URL}}
	if _, err := src.Token(context.Background()); !errors.Is(err, ErrLoginRequired) || !strings.Contains(err.Error(), "doesn't cover") {
		t.Fatalf("%v", err)
	}
	if v, _ := store.Load(ts.URL); v != rt {
		t.Fatal("a login that only lacks an audience must be kept")
	}
	// Refused for an audience it covers: really invalid, so it's dropped.
	src.Audience = DefaultAudience
	if _, err := src.Token(context.Background()); !errors.Is(err, ErrLoginRequired) {
		t.Fatalf("%v", err)
	}
	if v, _ := store.Load(ts.URL); v != "" {
		t.Fatal("an invalid login should be dropped")
	}
}

func TestDefaultLoginAudiences(t *testing.T) {
	t.Setenv("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", "")
	if a := DefaultLoginAudiences(); len(a) != 2 || a[1] != PlatformAudience {
		t.Fatalf("%v", a)
	}
	t.Setenv("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", DefaultAudience)
	if a := DefaultLoginAudiences(); len(a) != 1 || a[0] != DefaultAudience {
		t.Fatalf("%v", a)
	}
}
