package workspaces

import (
	"bytes"
	"context"
	"crypto/rand"
	"crypto/sha256"
	"encoding/base64"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/url"
	"os"
	"strings"
	"time"
)

// Turning Workspaces on for an organization.
//
// Sessions can be created only in an organization that is entitled to
// Workspaces. `chainctl develop init` gets an organization there in these
// steps, each a call to Chainguard's platform API (DefaultPlatformURL):
//
//  1. Find out which legal documents the organization has accepted:
//     GET /iam/v1/terms/{org}                                  (AcceptedTerms)
//  2. Accept the ones turning it on needs (EnableTerms):
//     POST /iam/v1/terms/{org}  {"documentIds": [...]}          (AcceptTerms)
//  3. Link a GitHub account (an organization, or your own user) to the
//     organization. The service refuses step 4 without it. The Chainguard
//     GitHub App must be installed on that account (GitHubAppInstallURL).
//     GET  /guardener/v1alpha1/githubAssociations/{org}         (LinkedGitHubOrganizations)
//     GET  /guardener/v1alpha1/githubAssociations:begin         (BeginGitHubOAuth)
//     browser: GitHub's authorize page, back to a loopback port (AuthorizeGitHub)
//     POST /guardener/v1alpha1/githubAssociations/{org}:link    (LinkGitHubOrganization)
//  4. Turn the feature on, with the plan's allowance:
//     POST /entitlements/v1alpha1/features/{org}:enableSandbox  (EnableSandbox)
//  5. Read the result:
//     GET /entitlements/v2beta1/organizationEntitlements?parent={org}
//     (WorkspacesEntitlement)
//
// Creating the organization itself (sign-up) is not covered: use the
// Chainguard Console or `chainctl develop init`.
//
// All of these need a token for the platform API (PlatformTokenSource), and
// steps 2 to 4 need the owner role in the organization. Step 3 also needs
// you to own the GitHub account.

// TermsDocument is a legal document an organization accepts.
type TermsDocument struct {
	ID    string
	Label string
	URL   string
}

// WorkspacesTerms are the IDs of the documents an organization must accept to
// use Workspaces: the Secured Platform Terms and the Master Service and License
// Agreement. (Source: chainguard.dev/sdk, proto/chainguard/platform/iam/terms.)
var WorkspacesTerms = []string{"secure-platform-terms.v1", "msla.v1"}

// EnableTerms are the documents an organization accepts when Workspaces is
// turned on for it the way `chainctl develop init` does: the Workspaces terms,
// the Code Analysis Terms (the same switch turns on Chainguard Checks), and
// the Terms of Service and Data Privacy Agreement that linking a GitHub
// account requires.
var EnableTerms = []string{"secure-platform-terms.v1", "msla.v1", "code-analysis-terms.v1", "guardener-tos.v1", "sfdpa.v1"}

// GitHubAppInstallURL is where the Chainguard GitHub App is installed on a
// GitHub organization or user. Linking that account fails until it is.
const GitHubAppInstallURL = "https://github.com/apps/chainguard/installations/new"

// DefaultGitHubOAuthPort is the loopback port the GitHub authorization
// redirects to (the one chainctl uses). Another free port works too.
const DefaultGitHubOAuthPort = 8989

var knownTerms = map[string]TermsDocument{
	"secure-platform-terms.v1": {ID: "secure-platform-terms.v1", Label: "Secured Platform Terms", URL: "https://www.chainguard.dev/legal/secure-platform-terms"},
	"msla.v1":                  {ID: "msla.v1", Label: "Master Service and License Agreement", URL: "https://www.chainguard.dev/legal/master-service-and-license-agreement"},
	"code-analysis-terms.v1":   {ID: "code-analysis-terms.v1", Label: "Code Analysis Terms", URL: "https://www.chainguard.dev/legal/code-analysis-terms"},
	"guardener-tos.v1":         {ID: "guardener-tos.v1", Label: "Terms of Service", URL: "https://www.chainguard.dev/legal/guardener"},
	"sfdpa.v1":                 {ID: "sfdpa.v1", Label: "Data Privacy Agreement", URL: "https://www.chainguard.dev/legal/supplemental-dpa"},
}

// TermsDocumentInfo returns the label and URL of a known document ID, or a
// document with only the ID set.
func TermsDocumentInfo(id string) TermsDocument {
	if d, ok := knownTerms[id]; ok {
		return d
	}
	return TermsDocument{ID: id}
}

// PlatformError is a non-2xx answer from the platform API.
type PlatformError struct {
	// HTTPStatus is the response status; Code is the gRPC status code the
	// gateway reports (7 = permission denied, 9 = failed precondition, ...).
	HTTPStatus int
	Code       int
	Message    string
	// Reason is the machine-readable reason, when the service gives one
	// (for example "GUARDENER_GITHUB_APP_NOT_INSTALLED").
	Reason string
	// MissingTerms is set when the call was refused because the organization
	// has not accepted these documents.
	MissingTerms []TermsDocument
}

// GitHubLinkRequired: the organization has to link a GitHub account first.
func (e *PlatformError) GitHubLinkRequired() bool {
	return e.Reason == "GUARDENER_GITHUB_LINK_REQUIRED" || strings.Contains(e.Message, "must link a GitHub organization")
}

// GitHubAppNotInstalled: the Chainguard GitHub App is not installed on the GitHub account.
func (e *PlatformError) GitHubAppNotInstalled() bool {
	return strings.HasSuffix(e.Reason, "GITHUB_APP_NOT_INSTALLED") || strings.Contains(e.Message, "must be installed on the GitHub")
}

// GitHubAlreadyLinked: the GitHub account is linked to another Chainguard organization.
func (e *PlatformError) GitHubAlreadyLinked() bool {
	return e.Reason == "GUARDENER_GITHUB_ALREADY_LINKED" || strings.Contains(e.Message, "already linked to a different")
}

func (e *PlatformError) Error() string {
	return fmt.Sprintf("platform API: HTTP %d: %s", e.HTTPStatus, e.Message)
}

// platformCall sends one JSON request to the platform API and decodes the answer into out (if not nil).
func platformCall(ctx context.Context, ts TokenSource, method, path string, body, out any) error {
	if ts == nil {
		ts = PlatformTokenSource()
	}
	tok, err := ts.Token(ctx)
	if err != nil {
		return fmt.Errorf("getting a token for the account API: %w", err)
	}
	base := os.Getenv("CHAINGUARD_SANDBOX_PLATFORM_URL")
	if base == "" {
		base = DefaultPlatformURL
	}
	var rd io.Reader
	if body != nil {
		b, err := json.Marshal(body)
		if err != nil {
			return err
		}
		rd = bytes.NewReader(b)
	}
	req, err := http.NewRequestWithContext(ctx, method, strings.TrimSuffix(base, "/")+path, rd)
	if err != nil {
		return err
	}
	req.Header.Set("Authorization", "Bearer "+tok)
	req.Header.Set("Accept", "application/json")
	if body != nil {
		req.Header.Set("Content-Type", "application/json")
	}
	hc := &http.Client{Timeout: 30 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	resp, err := hc.Do(req)
	if err != nil {
		return err
	}
	defer resp.Body.Close()
	data, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return err
	}
	if resp.StatusCode/100 != 2 {
		return parsePlatformError(resp.StatusCode, data)
	}
	if out == nil {
		return nil
	}
	return json.Unmarshal(data, out)
}

func parsePlatformError(status int, data []byte) *PlatformError {
	var st struct {
		Code    int               `json:"code"`
		Message string            `json:"message"`
		Details []json.RawMessage `json:"details"`
	}
	_ = json.Unmarshal(data, &st)
	e := &PlatformError{HTTPStatus: status, Code: st.Code, Message: st.Message}
	if e.Message == "" {
		e.Message = strings.TrimSpace(string(data))
	}
	// A refusal for unaccepted terms carries a detail listing the missing documents.
	for _, raw := range st.Details {
		var d struct {
			Reason  string                            `json:"reason"`
			Missing []struct{ ID, Label, URL string } `json:"missing"`
		}
		if json.Unmarshal(raw, &d) != nil {
			continue
		}
		if d.Reason != "" && e.Reason == "" {
			e.Reason = d.Reason
		}
		for _, m := range d.Missing {
			doc := TermsDocumentInfo(m.ID)
			if m.Label != "" {
				doc.Label = m.Label
			}
			if m.URL != "" {
				doc.URL = m.URL
			}
			e.MissingTerms = append(e.MissingTerms, doc)
		}
	}
	return e
}

func orgPath(org string) (string, error) {
	if org == "" || strings.ContainsAny(org, "?#") {
		return "", fmt.Errorf("workspaces: invalid organization ID %q", org)
	}
	return url.PathEscape(org), nil
}

// AcceptedTerms returns the IDs of the legal documents the organization has
// accepted. ts must mint platform tokens (nil: PlatformTokenSource).
func AcceptedTerms(ctx context.Context, ts TokenSource, org string) ([]string, error) {
	p, err := orgPath(org)
	if err != nil {
		return nil, err
	}
	var out struct {
		Items []struct {
			DocID string `json:"docId"`
		} `json:"items"`
	}
	if err := platformCall(ctx, ts, http.MethodGet, "/iam/v1/terms/"+p, nil, &out); err != nil {
		return nil, fmt.Errorf("listing accepted terms: %w", err)
	}
	ids := make([]string, 0, len(out.Items))
	for _, it := range out.Items {
		ids = append(ids, it.DocID)
	}
	return ids, nil
}

// MissingWorkspacesTerms returns the documents Workspaces needs that the
// organization has not accepted yet.
func MissingWorkspacesTerms(ctx context.Context, ts TokenSource, org string) ([]TermsDocument, error) {
	have, err := AcceptedTerms(ctx, ts, org)
	if err != nil {
		return nil, err
	}
	var missing []TermsDocument
	for _, id := range WorkspacesTerms {
		found := false
		for _, h := range have {
			found = found || h == id
		}
		if !found {
			missing = append(missing, TermsDocumentInfo(id))
		}
	}
	return missing, nil
}

// AcceptTerms records that the organization accepts the documents with these
// IDs. This is a legal act on the organization's behalf: call it only when
// the person running your program has agreed to the documents. It needs the
// owner role; accepting a document again changes nothing.
func AcceptTerms(ctx context.Context, ts TokenSource, org string, documentIDs []string) error {
	p, err := orgPath(org)
	if err != nil {
		return err
	}
	if len(documentIDs) == 0 {
		return errors.New("workspaces: no documents to accept")
	}
	body := map[string]any{"documentIds": documentIDs}
	if err := platformCall(ctx, ts, http.MethodPost, "/iam/v1/terms/"+p, body, nil); err != nil {
		return fmt.Errorf("accepting terms: %w", err)
	}
	return nil
}

// EnableSandbox turns on Checks and Workspaces for the organization, with its
// plan's monthly allowance (what step 5 of `chainctl develop init` does). An
// allowance Chainguard already granted is kept. It needs the owner role, and
// the organization must have accepted the terms first: otherwise the error is
// a *PlatformError whose MissingTerms lists the documents.
func EnableSandbox(ctx context.Context, ts TokenSource, org string) error {
	p, err := orgPath(org)
	if err != nil {
		return err
	}
	if err := platformCall(ctx, ts, http.MethodPost, "/entitlements/v1alpha1/features/"+p+":enableSandbox", map[string]any{}, nil); err != nil {
		return fmt.Errorf("turning on Workspaces: %w", err)
	}
	return nil
}

// Entitlement is an organization's Workspaces entitlement.
type Entitlement struct {
	// Plan is the organization's plan ID ("starter", "team", ...) and PlanName its display name.
	Plan, PlanName string
	// Active reports whether Workspaces is turned on. State is the raw state
	// ("STATE_ACTIVE", "STATE_NOT_INCLUDED", or "" when the product isn't listed).
	Active bool
	State  string
	// MaxSandboxes and MaxSandboxesPerUser are the concurrent-session limits (0: not reported).
	MaxSandboxes, MaxSandboxesPerUser int
}

// WorkspacesEntitlement reads whether Workspaces is turned on for the
// organization, its plan, and its session limits.
func WorkspacesEntitlement(ctx context.Context, ts TokenSource, org string) (Entitlement, error) {
	if _, err := orgPath(org); err != nil {
		return Entitlement{}, err
	}
	var out struct {
		Plan struct {
			ID          string `json:"id"`
			DisplayName string `json:"displayName"`
		} `json:"plan"`
		Products []struct {
			Product string `json:"product"`
			State   string `json:"state"`
			Limits  []struct {
				DisplayName string `json:"displayName"`
				Max         string `json:"max"`
			} `json:"limits"`
		} `json:"products"`
	}
	q := url.Values{"parent": {org}, "products": {"PRODUCT_SANDBOX_WORKSPACES"}}
	if err := platformCall(ctx, ts, http.MethodGet, "/entitlements/v2beta1/organizationEntitlements?"+q.Encode(), nil, &out); err != nil {
		return Entitlement{}, fmt.Errorf("reading the organization's entitlements: %w", err)
	}
	e := Entitlement{Plan: out.Plan.ID, PlanName: out.Plan.DisplayName}
	for _, p := range out.Products {
		if p.Product != "PRODUCT_SANDBOX_WORKSPACES" {
			continue
		}
		e.State, e.Active = p.State, p.State == "STATE_ACTIVE"
		for _, l := range p.Limits {
			var n int
			fmt.Sscan(l.Max, &n)
			switch l.DisplayName {
			case "concurrent workspaces":
				e.MaxSandboxes = n
			case "concurrent workspaces per user":
				e.MaxSandboxesPerUser = n
			}
		}
	}
	return e, nil
}

// LinkedGitHub is a GitHub account linked to a Chainguard organization.
type LinkedGitHub struct {
	// Organization is the GitHub login: an organization, or a user.
	Organization string
}

// LinkedGitHubOrganizations returns the GitHub accounts linked to the organization.
func LinkedGitHubOrganizations(ctx context.Context, ts TokenSource, org string) ([]LinkedGitHub, error) {
	p, err := orgPath(org)
	if err != nil {
		return nil, err
	}
	var out struct {
		Organizations []struct {
			Organization string `json:"organization"`
		} `json:"organizations"`
	}
	if err := platformCall(ctx, ts, http.MethodGet, "/guardener/v1alpha1/githubAssociations/"+p, nil, &out); err != nil {
		return nil, fmt.Errorf("listing linked GitHub accounts: %w", err)
	}
	linked := make([]LinkedGitHub, 0, len(out.Organizations))
	for _, o := range out.Organizations {
		linked = append(linked, LinkedGitHub{Organization: o.Organization})
	}
	return linked, nil
}

// GitHubOAuthStart is what the service hands out to start a GitHub
// authorization: the Chainguard GitHub App's OAuth client ID, and a state
// value the service signed and checks again when the account is linked.
type GitHubOAuthStart struct{ ClientID, State string }

// BeginGitHubOAuth starts a GitHub authorization.
func BeginGitHubOAuth(ctx context.Context, ts TokenSource) (GitHubOAuthStart, error) {
	var out struct {
		ClientID string `json:"clientId"`
		State    string `json:"state"`
	}
	if err := platformCall(ctx, ts, http.MethodGet, "/guardener/v1alpha1/githubAssociations:begin", nil, &out); err != nil {
		return GitHubOAuthStart{}, fmt.Errorf("starting the GitHub authorization: %w", err)
	}
	if out.ClientID == "" || out.State == "" {
		return GitHubOAuthStart{}, errors.New("starting the GitHub authorization: the service returned no client ID or state")
	}
	return GitHubOAuthStart{ClientID: out.ClientID, State: out.State}, nil
}

// GitHubAuthorization is the result of AuthorizeGitHub. It holds no GitHub
// token: the service exchanges the code itself when the account is linked.
type GitHubAuthorization struct{ Code, RedirectURI, Verifier, State string }

// GitHubAuthorizeOptions configures AuthorizeGitHub.
type GitHubAuthorizeOptions struct {
	// Port is the loopback port to receive GitHub's redirect on (default DefaultGitHubOAuthPort).
	Port int
	// NoBrowser prints the URL instead of opening a browser.
	NoBrowser bool
	// Out receives the instructions (default os.Stderr).
	Out io.Writer
}

// AuthorizeGitHub sends the person to GitHub to authorize the Chainguard
// GitHub App and waits (until ctx is done) for GitHub's redirect to a loopback
// port. It is the flow chainctl uses (chainguard.dev/sdk/githubflow): PKCE
// with S256, the account picker forced, and the state checked on return.
func AuthorizeGitHub(ctx context.Context, start GitHubOAuthStart, o GitHubAuthorizeOptions) (GitHubAuthorization, error) {
	port := o.Port
	if port == 0 {
		port = DefaultGitHubOAuthPort
	}
	out := o.Out
	if out == nil {
		out = os.Stderr
	}
	ln, err := net.Listen("tcp", fmt.Sprintf("127.0.0.1:%d", port))
	if err != nil {
		return GitHubAuthorization{}, fmt.Errorf("listening on loopback port %d for GitHub's redirect (choose another port if it is taken): %w", port, err)
	}
	defer ln.Close()
	redirect := fmt.Sprintf("http://127.0.0.1:%d/callback", port)
	raw := make([]byte, 32)
	if _, err := rand.Read(raw); err != nil {
		return GitHubAuthorization{}, err
	}
	verifier := base64.RawURLEncoding.EncodeToString(raw)
	sum := sha256.Sum256([]byte(verifier))
	q := url.Values{
		"client_id": {start.ClientID}, "redirect_uri": {redirect}, "response_type": {"code"}, "state": {start.State},
		"code_challenge": {base64.RawURLEncoding.EncodeToString(sum[:])}, "code_challenge_method": {"S256"},
		"prompt": {"select_account"}, "allow_signup": {"false"},
	}
	authURL := "https://github.com/login/oauth/authorize?" + q.Encode()

	type result struct {
		code string
		err  error
	}
	got := make(chan result, 1)
	mux := http.NewServeMux()
	mux.HandleFunc("/callback", func(w http.ResponseWriter, r *http.Request) {
		v := r.URL.Query()
		var res result
		switch {
		case v.Get("error") != "":
			http.Error(w, "Authorization was denied. You can close this window.", http.StatusBadRequest)
			res.err = fmt.Errorf("GitHub refused the authorization: %s: %s", v.Get("error"), v.Get("error_description"))
		case v.Get("state") != start.State:
			http.Error(w, "Authorization state mismatch. You can close this window.", http.StatusBadRequest)
			res.err = errors.New("GitHub's redirect carried the wrong state")
		case v.Get("code") == "":
			http.Error(w, "Missing authorization code. You can close this window.", http.StatusBadRequest)
			res.err = errors.New("GitHub's redirect carried no authorization code")
		default:
			fmt.Fprintln(w, "Authorization complete. You can close this window and return to the terminal.")
			res.code = v.Get("code")
		}
		if f, ok := w.(http.Flusher); ok {
			f.Flush()
		}
		select {
		case got <- res:
		default:
		}
	})
	srv := &http.Server{Handler: mux, ReadHeaderTimeout: 10 * time.Second}
	go func() { _ = srv.Serve(ln) }()
	defer func() {
		sctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
		defer cancel()
		_ = srv.Shutdown(sctx) // lets the browser finish loading the result page
	}()

	if o.NoBrowser || OpenBrowser(authURL) != nil {
		fmt.Fprintf(out, "Open this URL in a browser to authorize with GitHub:\n  %s\n", authURL)
	} else {
		fmt.Fprintf(out, "Opened a browser to authorize with GitHub. If it didn't open, visit:\n  %s\n", authURL)
	}
	select {
	case <-ctx.Done():
		return GitHubAuthorization{}, fmt.Errorf("waiting for the GitHub authorization: %w", ctx.Err())
	case res := <-got:
		if res.err != nil {
			return GitHubAuthorization{}, res.err
		}
		return GitHubAuthorization{Code: res.code, RedirectURI: redirect, Verifier: verifier, State: start.State}, nil
	}
}

// LinkGitHubOrganization links a GitHub account to the Chainguard
// organization, using an authorization from AuthorizeGitHub. githubLogin is
// the account to link, an organization or a user; empty means the GitHub user
// who authorized. The Chainguard GitHub App must be installed on the account
// (otherwise the *PlatformError reports GitHubAppNotInstalled), and you must
// own it. An account can be linked to one Chainguard organization at a time.
func LinkGitHubOrganization(ctx context.Context, ts TokenSource, org, githubLogin string, a GitHubAuthorization) (LinkedGitHub, error) {
	p, err := orgPath(org)
	if err != nil {
		return LinkedGitHub{}, err
	}
	body := map[string]any{"code": a.Code, "redirectUri": a.RedirectURI, "codeVerifier": a.Verifier, "state": a.State}
	if githubLogin != "" {
		body["organization"] = githubLogin
	}
	var out struct {
		Organization string `json:"organization"`
	}
	if err := platformCall(ctx, ts, http.MethodPost, "/guardener/v1alpha1/githubAssociations/"+p+":link", body, &out); err != nil {
		return LinkedGitHub{}, fmt.Errorf("linking the GitHub account: %w", err)
	}
	return LinkedGitHub{Organization: out.Organization}, nil
}
