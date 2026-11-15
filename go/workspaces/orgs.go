package workspaces

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"io"
	"net/http"
	"os"
	"sort"
	"strings"
	"time"
)

// The organization ("instance") a client works in can be given three ways,
// in this order:
//
//  1. explicitly: Options.Instance (the 40-hex organization ID);
//  2. the environment: $CHAINCTL_SANDBOX_INSTANCE;
//  3. auto-detection: if neither is set, New lists the organizations your
//     identity belongs to (Chainguard's account API) and uses the only one.
//     With several, it reports them and you pick one with 1 or 2. Turn it off
//     with Options.NoDetectInstance or $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1.
//
// List the IDs yourself with ListOrganizations, `chainguard-sandbox orgs`, or
// `chainctl iam organizations list`.

// DefaultPlatformURL is Chainguard's account (IAM) API.
const DefaultPlatformURL = "https://console-api.enforce.dev"

// EnvNoDetectInstance turns off organization auto-detection when set to "1".
const EnvNoDetectInstance = "CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE"

// Organization is a Chainguard organization (a root IAM group).
type Organization struct {
	// ID is the organization's 40-hex ID, used as the sessions instance.
	ID   string
	Name string
}

// ListOrganizations returns the organizations visible to the token source's
// identity, sorted by name. ts must mint tokens for PlatformAudience (see
// PlatformTokenSource). It calls the IAM Groups API:
// GET {DefaultPlatformURL}/iam/v1/groups?uidp.in_root=true.
func ListOrganizations(ctx context.Context, ts TokenSource) ([]Organization, error) {
	if ts == nil {
		ts = PlatformTokenSource()
	}
	tok, err := ts.Token(ctx)
	if err != nil {
		return nil, fmt.Errorf("getting a token for the account API: %w", err)
	}
	base := os.Getenv("CHAINGUARD_SANDBOX_PLATFORM_URL")
	if base == "" {
		base = DefaultPlatformURL
	}
	req, err := http.NewRequestWithContext(ctx, http.MethodGet, strings.TrimSuffix(base, "/")+"/iam/v1/groups?uidp.in_root=true", nil)
	if err != nil {
		return nil, err
	}
	req.Header.Set("Authorization", "Bearer "+tok)
	req.Header.Set("Accept", "application/json")
	hc := &http.Client{Timeout: 30 * time.Second, CheckRedirect: func(*http.Request, []*http.Request) error { return http.ErrUseLastResponse }}
	resp, err := hc.Do(req)
	if err != nil {
		return nil, fmt.Errorf("listing organizations: %w", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 4<<20))
	if err != nil {
		return nil, err
	}
	if resp.StatusCode != http.StatusOK {
		var st struct {
			Message string `json:"message"`
		}
		_ = json.Unmarshal(body, &st)
		if st.Message == "" {
			st.Message = strings.TrimSpace(string(body))
		}
		return nil, fmt.Errorf("listing organizations: HTTP %d: %s", resp.StatusCode, st.Message)
	}
	var gl struct {
		Items []struct {
			ID   string `json:"id"`
			Name string `json:"name"`
		} `json:"items"`
	}
	if err := json.Unmarshal(body, &gl); err != nil {
		return nil, fmt.Errorf("listing organizations: %w", err)
	}
	var orgs []Organization
	for _, g := range gl.Items {
		if g.ID != "" && !strings.Contains(g.ID, "/") { // root groups only
			orgs = append(orgs, Organization{ID: g.ID, Name: g.Name})
		}
	}
	sort.Slice(orgs, func(i, j int) bool { return orgs[i].Name < orgs[j].Name })
	return orgs, nil
}

// ErrNoOrganization: the identity belongs to no organization.
var ErrNoOrganization = errors.New("workspaces: your identity belongs to no Chainguard organization")

// AmbiguousOrganizationError: several organizations; pick one explicitly.
type AmbiguousOrganizationError struct{ Organizations []Organization }

func (e *AmbiguousOrganizationError) Error() string {
	var b strings.Builder
	b.WriteString("workspaces: you belong to several organizations; choose one with Options.Instance or $" + EnvInstance + ":")
	for _, o := range e.Organizations {
		fmt.Fprintf(&b, "\n  %s  %s", o.ID, o.Name)
	}
	return b.String()
}

// DetectInstance returns the ID of the only organization ts's identity
// belongs to (ErrNoOrganization or *AmbiguousOrganizationError otherwise).
func DetectInstance(ctx context.Context, ts TokenSource) (string, error) {
	orgs, err := ListOrganizations(ctx, ts)
	if err != nil {
		return "", err
	}
	switch len(orgs) {
	case 0:
		return "", ErrNoOrganization
	case 1:
		return orgs[0].ID, nil
	}
	return "", &AmbiguousOrganizationError{Organizations: orgs}
}

// PlatformTokenSource mints tokens for the account API (PlatformAudience) the
// same way DefaultTokenSource does for sessions: from the library's saved
// browser login, or with chainctl when $CHAINGUARD_SANDBOX_AUTH=chainctl.
func PlatformTokenSource() TokenSource {
	if os.Getenv("CHAINGUARD_SANDBOX_AUTH") == "chainctl" {
		return Cached(ChainctlToken{Audience: PlatformAudience})
	}
	return Cached(&BrowserLoginToken{
		Audience:  PlatformAudience,
		Login:     LoginOptions{Connection: os.Getenv("CHAINGUARD_SANDBOX_SOCIAL_LOGIN")},
		NoBrowser: os.Getenv("CHAINGUARD_SANDBOX_NO_BROWSER") == "1",
	})
}

// noInstanceError explains every way to set the organization.
func noInstanceError(detectErr error) error {
	msg := "workspaces: no organization configured. Set one of:\n" +
		"  - Options.Instance (Python: instance=, TypeScript: instance) to the organization ID\n" +
		"  - $" + EnvInstance + "=<organization ID>\n" +
		"  - or let it be auto-detected when you belong to exactly one organization\n" +
		"List your organization IDs with `chainguard-sandbox orgs` or `chainctl iam organizations list`."
	if detectErr != nil {
		msg += "\nAuto-detection: " + detectErr.Error()
	}
	return errors.New(msg)
}
