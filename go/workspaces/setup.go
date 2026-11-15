package workspaces

import (
	"bytes"
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
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
//  2. Accept the ones Workspaces needs (WorkspacesTerms):
//     POST /iam/v1/terms/{org}  {"documentIds": [...]}          (AcceptTerms)
//  3. Turn the feature on, with the plan's allowance:
//     POST /entitlements/v1alpha1/features/{org}:enableSandbox  (EnableSandbox)
//  4. Read the result:
//     GET /entitlements/v2beta1/organizationEntitlements?parent={org}
//     (WorkspacesEntitlement)
//
// chainctl also links a GitHub account to the organization between steps 2
// and 3. That is for Checks and for sessions that act on GitHub; this package
// does not do it. Creating the organization itself (sign-up) is not covered
// either: use the Chainguard Console or `chainctl develop init`.
//
// All of these need a token for the platform API (PlatformTokenSource), and
// steps 2 and 3 need the owner role in the organization.

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
	// MissingTerms is set when the call was refused because the organization
	// has not accepted these documents.
	MissingTerms []TermsDocument
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
			Missing []struct{ ID, Label, URL string } `json:"missing"`
		}
		if json.Unmarshal(raw, &d) != nil {
			continue
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
