package sandbox

import (
	"context"
	"encoding/json"
	"errors"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"net/url"
	"strings"
	"sync"
	"testing"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

const testOrg = "0123456789abcdef0123456789abcdef01234567"

// fakePlatform stands in for the platform API's terms and entitlement calls.
// It checks only what this package sends; it says nothing about the real service.
type fakePlatform struct {
	mu       sync.Mutex
	accepted map[string]bool
	enabled  bool
	// needForEnable are documents enableSandbox insists on (beyond what Workspaces itself needs).
	needForEnable []string
	posts         []string
	// github holds the linked GitHub accounts; appInstalled says whether a link attempt succeeds.
	github       []string
	appInstalled bool
	linkBody     map[string]string
}

func (f *fakePlatform) ServeHTTP(w http.ResponseWriter, r *http.Request) {
	f.mu.Lock()
	defer f.mu.Unlock()
	if r.Header.Get("Authorization") != "Bearer platform-token" {
		http.Error(w, `{"code":16,"message":"unauthenticated"}`, http.StatusUnauthorized)
		return
	}
	reply := func(v any) { _ = json.NewEncoder(w).Encode(v) }
	switch {
	case r.Method == http.MethodGet && r.URL.Path == "/iam/v1/terms/"+testOrg:
		items := []map[string]string{}
		for id := range f.accepted {
			items = append(items, map[string]string{"docId": id})
		}
		reply(map[string]any{"items": items})
	case r.Method == http.MethodPost && r.URL.Path == "/iam/v1/terms/"+testOrg:
		var body struct {
			DocumentIds []string `json:"documentIds"`
		}
		_ = json.NewDecoder(r.Body).Decode(&body)
		f.posts = append(f.posts, "terms:"+strings.Join(body.DocumentIds, ","))
		for _, id := range body.DocumentIds {
			f.accepted[id] = true
		}
		reply(map[string]any{"group": testOrg, "documentIds": body.DocumentIds})
	case r.Method == http.MethodGet && r.URL.Path == "/guardener/v1alpha1/githubAssociations/"+testOrg:
		orgs := []map[string]string{}
		for _, g := range f.github {
			orgs = append(orgs, map[string]string{"organization": g, "installationId": "1"})
		}
		reply(map[string]any{"organizations": orgs})
	case r.Method == http.MethodGet && r.URL.Path == "/guardener/v1alpha1/githubAssociations:begin":
		reply(map[string]string{"clientId": "client-1", "state": "signed-state"})
	case r.Method == http.MethodPost && r.URL.Path == "/guardener/v1alpha1/githubAssociations/"+testOrg+":link":
		f.linkBody = map[string]string{}
		_ = json.NewDecoder(r.Body).Decode(&f.linkBody)
		f.posts = append(f.posts, "link")
		if !f.appInstalled {
			w.WriteHeader(http.StatusBadRequest)
			reply(map[string]any{"code": 9, "message": "Precondition failed: the guardener GitHub App must be installed on the GitHub account (organization or user) before it can be linked"})
			return
		}
		login := f.linkBody["organization"]
		if login == "" {
			login = "the-authorizing-user"
		}
		f.github = append(f.github, login)
		reply(map[string]string{"organization": login, "installationId": "1"})
	case r.Method == http.MethodPost && r.URL.Path == "/entitlements/v1alpha1/features/"+testOrg+":enableSandbox":
		f.posts = append(f.posts, "enable")
		if len(f.github) == 0 { // the service's real answer, as seen on 2026-11-15
			w.WriteHeader(http.StatusBadRequest)
			reply(map[string]any{"code": 9, "message": "Precondition failed: the organization must link a GitHub organization through the Chainguard GitHub App first"})
			return
		}
		var missing []map[string]string
		for _, id := range append(append([]string{}, workspaces.WorkspacesTerms...), f.needForEnable...) {
			if !f.accepted[id] {
				missing = append(missing, map[string]string{"id": id, "label": "Label of " + id, "url": "https://example.test/" + id})
			}
		}
		if len(missing) > 0 {
			w.WriteHeader(http.StatusBadRequest)
			reply(map[string]any{"code": 9, "message": "terms not accepted", "details": []any{map[string]any{"@type": "type.googleapis.com/chainguard.platform.iam.v2.TermsNotAcceptedDetail", "missing": missing}}})
			return
		}
		f.enabled = true
		reply(map[string]any{"featureEntitlements": []any{}})
	case r.Method == http.MethodGet && r.URL.Path == "/entitlements/v2beta1/organizationEntitlements" && r.URL.Query().Get("parent") == testOrg:
		state := "STATE_NOT_INCLUDED"
		if f.enabled {
			state = "STATE_ACTIVE"
		}
		reply(map[string]any{"plan": map[string]string{"id": "starter", "displayName": "Starter"}, "products": []any{map[string]any{
			"product": "PRODUCT_SANDBOX_WORKSPACES", "state": state,
			"limits": []any{map[string]string{"displayName": "concurrent workspaces", "max": "3"}, map[string]string{"displayName": "concurrent workspaces per user", "max": "1"}},
		}}})
	default:
		http.Error(w, `{"code":5,"message":"no such route"}`, http.StatusNotFound)
	}
}

func newFakePlatform(t *testing.T) (*fakePlatform, *SetupOptions) {
	f := &fakePlatform{accepted: map[string]bool{}, github: []string{"already-linked"}, appInstalled: true}
	srv := httptest.NewServer(f)
	t.Cleanup(srv.Close)
	t.Setenv("CHAINGUARD_SANDBOX_PLATFORM_URL", srv.URL)
	return f, &SetupOptions{Instance: testOrg, TokenSource: workspaces.StaticToken("platform-token")}
}

func TestStatusAndEnable(t *testing.T) {
	f, o := newFakePlatform(t)
	ctx := context.Background()

	st, err := Status(ctx, o)
	if err != nil || st.Enabled || st.Ready() || len(st.MissingTerms) != len(workspaces.EnableTerms) || st.Plan != "Starter" || st.MaxSandboxes != 3 || st.MaxSandboxesPerUser != 1 || len(st.GitHubAccounts) != 1 {
		t.Fatalf("Status of a new organization = %+v, %v", st, err)
	}
	if st.MissingTerms[0].URL == "" || st.MissingTerms[0].Label == "" {
		t.Fatalf("missing terms lack a label or URL: %+v", st.MissingTerms)
	}

	// Terms are never accepted without being asked to.
	_, err = Enable(ctx, o)
	var terms *TermsNotAcceptedError
	if !errors.As(err, &terms) || len(terms.Missing) != len(workspaces.EnableTerms) || len(f.posts) != 0 {
		t.Fatalf("Enable without AcceptTerms = %v, posts %q; want TermsNotAcceptedError and no change", err, f.posts)
	}

	o.AcceptTerms = true
	st, err = Enable(ctx, o)
	if err != nil || !st.Ready() {
		t.Fatalf("Enable = %+v, %v", st, err)
	}
	if got := strings.Join(f.posts, " | "); got != "terms:"+strings.Join(workspaces.EnableTerms, ",")+" | enable" {
		t.Fatalf("calls = %q", got)
	}

	// Already on: nothing is sent.
	f.posts = nil
	if st, err := Enable(ctx, o); err != nil || !st.Ready() || len(f.posts) != 0 {
		t.Fatalf("Enable when already on = %+v, %v, posts %q", st, err, f.posts)
	}
}

func TestEnableAcceptsTermsTheSwitchAsksFor(t *testing.T) {
	f, o := newFakePlatform(t)
	f.needForEnable = []string{"some-new-terms.v1"} // a document this package doesn't know about yet
	for _, id := range workspaces.EnableTerms {
		f.accepted[id] = true
	}

	_, err := Enable(context.Background(), o)
	var terms *TermsNotAcceptedError
	if !errors.As(err, &terms) || len(terms.Missing) != 1 || terms.Missing[0].ID != "some-new-terms.v1" || terms.Missing[0].URL == "" || f.enabled {
		t.Fatalf("Enable = %v; want TermsNotAcceptedError for the extra terms, and nothing turned on", err)
	}
	o.AcceptTerms = true
	f.posts = nil
	st, err := Enable(context.Background(), o)
	if err != nil || !st.Ready() {
		t.Fatalf("Enable = %+v, %v", st, err)
	}
	if got := strings.Join(f.posts, " | "); got != "enable | terms:some-new-terms.v1 | enable" {
		t.Fatalf("calls = %q", got)
	}
}

// An organization with no GitHub account linked: Enable authorizes with GitHub
// through a loopback redirect, links the account, and only then turns Workspaces on.
func TestEnableLinksGitHubFirst(t *testing.T) {
	f, o := newFakePlatform(t)
	f.github = nil
	o.AcceptTerms = true

	// Without a browser it says what is needed instead of failing at the switch.
	o.NoBrowser = true
	_, err := Enable(context.Background(), o)
	var need *GitHubLinkRequiredError
	if !errors.As(err, &need) || f.enabled {
		t.Fatalf("Enable without a browser = %v; want GitHubLinkRequiredError", err)
	}
	o.NoBrowser = false

	// Stand in for the person and GitHub: follow the printed URL's redirect_uri.
	ln, _ := net.Listen("tcp", "127.0.0.1:0")
	o.GitHubPort = ln.Addr().(*net.TCPAddr).Port
	ln.Close()
	pr, pw := io.Pipe()
	o.Out = pw
	t.Setenv("PATH", "") // no browser opener: the URL is printed instead
	var authorize url.Values
	go func() {
		buf := make([]byte, 4096)
		var text string
		for !strings.Contains(text, "allow_signup") {
			n, err := pr.Read(buf)
			if err != nil {
				return
			}
			text += string(buf[:n])
		}
		go io.Copy(io.Discard, pr)
		u, _ := url.Parse(strings.TrimSpace(text[strings.Index(text, "https://github.com/"):]))
		authorize = u.Query()
		resp, err := http.Get(authorize.Get("redirect_uri") + "?code=gh-code&state=" + authorize.Get("state"))
		if err == nil {
			resp.Body.Close()
		}
	}()

	f.appInstalled = false
	_, err = Enable(context.Background(), o)
	var app *GitHubAppNotInstalledError
	if !errors.As(err, &app) || app.InstallURL != workspaces.GitHubAppInstallURL || f.enabled {
		t.Fatalf("Enable with the app missing = %v; want GitHubAppNotInstalledError", err)
	}
	if authorize.Get("client_id") != "client-1" || authorize.Get("state") != "signed-state" || authorize.Get("code_challenge_method") != "S256" ||
		authorize.Get("code_challenge") == "" || authorize.Get("prompt") != "select_account" || !strings.HasSuffix(authorize.Get("redirect_uri"), "/callback") {
		t.Fatalf("authorize request = %v", authorize)
	}
	if f.linkBody["code"] != "gh-code" || f.linkBody["state"] != "signed-state" || f.linkBody["codeVerifier"] == "" || f.linkBody["redirectUri"] != authorize.Get("redirect_uri") {
		t.Fatalf("link request = %v", f.linkBody)
	}
}

func TestSetupErrorsKeepTheServiceMessage(t *testing.T) {
	_, o := newFakePlatform(t)
	o.TokenSource = workspaces.StaticToken("wrong")
	_, err := Status(context.Background(), o)
	var pe *workspaces.PlatformError
	if !errors.As(err, &pe) || pe.HTTPStatus != http.StatusUnauthorized || !strings.Contains(err.Error(), "unauthenticated") {
		t.Fatalf("Status with a bad token = %v", err)
	}
}

// Creating a sandbox in an organization without Workspaces is ErrNotEnabled, with the service's message kept.
func TestCreateInAnOrganizationWithoutWorkspaces(t *testing.T) {
	c, fake := newFakePlane(t)
	fake.createErr = status.Error(codes.PermissionDenied, "this organization is not entitled to Chainguard Workspaces; an organization owner can turn it on with chainctl develop init --parent "+testOrg+", or contact Chainguard")
	_, err := Create(context.Background(), &CreateOptions{Client: c})
	if !errors.Is(err, ErrNotEnabled) || status.Code(err) != codes.PermissionDenied || !strings.Contains(err.Error(), "not entitled") {
		t.Fatalf("Create = %v; want ErrNotEnabled wrapping the service's error", err)
	}
	// Another permission error is left alone.
	fake.createErr = status.Error(codes.PermissionDenied, "caller lacks sessions.create")
	if _, err := Create(context.Background(), &CreateOptions{Client: c}); errors.Is(err, ErrNotEnabled) {
		t.Fatalf("an unrelated permission error became %v", err)
	}
}
