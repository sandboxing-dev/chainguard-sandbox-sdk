package workspaces

import (
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

// DefaultIssuer is Chainguard's security token service (STS).
const DefaultIssuer = "https://issuer.enforce.dev"

// IdentityExchange is a TokenSource for machines without chainctl: CI jobs,
// servers, Kubernetes pods. It trades a token from an external OIDC issuer
// (GitHub Actions, GitLab, a Kubernetes service account, ...) for a
// Chainguard token for the sessions audience, acting as a Chainguard
// *assumable identity* that trusts that issuer and subject.
//
// One-time setup, by an organization owner:
//
//	chainctl iam identities create <name> --parent <org> \
//	  --identity-issuer <issuer URL> --subject <subject> --role sandbox.user
//
// It calls the same SecurityTokenService/Exchange API chainctl and
// chainguard.dev/sdk/sts use, over its HTTP binding (POST /sts/exchange).
type IdentityExchange struct {
	// Identity is the assumable identity's ID (from `chainctl iam identities list`).
	Identity string
	// IDToken returns the external OIDC token presented to the STS. See
	// GitHubActionsIDToken, EnvIDToken and FileIDToken.
	IDToken func(ctx context.Context) (string, error)
	// Issuer defaults to DefaultIssuer; Audience to DefaultAudience.
	Issuer, Audience string
	// HTTPClient defaults to a client with a 30s timeout.
	HTTPClient *http.Client
}

// Token performs the exchange. Wrap it in Cached to reuse tokens until they
// near expiry.
func (x IdentityExchange) Token(ctx context.Context) (string, error) {
	if x.Identity == "" || x.IDToken == nil {
		return "", errors.New("workspaces: IdentityExchange needs Identity and IDToken")
	}
	issuer, aud := x.Issuer, x.Audience
	if issuer == "" {
		issuer = DefaultIssuer
	}
	if aud == "" {
		aud = DefaultAudience
	}
	hc := x.HTTPClient
	if hc == nil {
		hc = &http.Client{Timeout: 30 * time.Second}
	}
	idt, err := x.IDToken(ctx)
	if err != nil {
		return "", fmt.Errorf("getting the OIDC token to exchange: %w", err)
	}
	q := url.Values{"aud": {aud}, "identity": {x.Identity}}
	req, err := http.NewRequestWithContext(ctx, http.MethodPost, strings.TrimSuffix(issuer, "/")+"/sts/exchange?"+q.Encode(), nil)
	if err != nil {
		return "", err
	}
	req.Header.Set("Authorization", "Bearer "+idt)
	req.Header.Set("Accept", "application/json")
	resp, err := hc.Do(req)
	if err != nil {
		return "", fmt.Errorf("STS exchange: %w", err)
	}
	defer resp.Body.Close()
	body, err := io.ReadAll(io.LimitReader(resp.Body, 1<<20))
	if err != nil {
		return "", err
	}
	if resp.StatusCode != http.StatusOK {
		var st struct {
			Code    int    `json:"code"`
			Message string `json:"message"`
		}
		_ = json.Unmarshal(body, &st)
		if st.Message == "" {
			st.Message = strings.TrimSpace(string(body))
		}
		return "", fmt.Errorf("STS exchange for identity %s: HTTP %d: %s", x.Identity, resp.StatusCode, st.Message)
	}
	var tok struct {
		Token string `json:"token"`
	}
	if err := json.Unmarshal(body, &tok); err != nil || tok.Token == "" {
		return "", fmt.Errorf("STS exchange: response has no token")
	}
	return tok.Token, nil
}

// GitHubActionsIDToken fetches a GitHub Actions OIDC token whose audience is
// the Chainguard issuer. The workflow needs `permissions: id-token: write`.
func GitHubActionsIDToken(issuer string) func(context.Context) (string, error) {
	if issuer == "" {
		issuer = DefaultIssuer
	}
	return func(ctx context.Context) (string, error) {
		reqURL, reqTok := os.Getenv("ACTIONS_ID_TOKEN_REQUEST_URL"), os.Getenv("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
		if reqURL == "" || reqTok == "" {
			return "", errors.New("not in a GitHub Actions job with `permissions: id-token: write`")
		}
		u, err := url.Parse(reqURL)
		if err != nil {
			return "", err
		}
		q := u.Query()
		q.Set("audience", issuer)
		u.RawQuery = q.Encode()
		req, err := http.NewRequestWithContext(ctx, http.MethodGet, u.String(), nil)
		if err != nil {
			return "", err
		}
		req.Header.Set("Authorization", "Bearer "+reqTok)
		resp, err := (&http.Client{Timeout: 30 * time.Second}).Do(req)
		if err != nil {
			return "", err
		}
		defer resp.Body.Close()
		var out struct {
			Value string `json:"value"`
		}
		if resp.StatusCode != http.StatusOK || json.NewDecoder(resp.Body).Decode(&out) != nil || out.Value == "" {
			return "", fmt.Errorf("GitHub Actions OIDC token request failed: HTTP %d", resp.StatusCode)
		}
		return out.Value, nil
	}
}

// EnvIDToken reads the OIDC token from an environment variable (e.g. a
// GitLab CI `id_tokens:` variable).
func EnvIDToken(name string) func(context.Context) (string, error) {
	return func(context.Context) (string, error) {
		if v := strings.TrimSpace(os.Getenv(name)); v != "" {
			return v, nil
		}
		return "", fmt.Errorf("$%s is empty", name)
	}
}

// FileIDToken reads the OIDC token from a file on every call (e.g. a
// Kubernetes projected service-account token, which is rotated in place).
func FileIDToken(path string) func(context.Context) (string, error) {
	return func(context.Context) (string, error) {
		b, err := os.ReadFile(path)
		if err != nil {
			return "", err
		}
		if v := strings.TrimSpace(string(b)); v != "" {
			return v, nil
		}
		return "", fmt.Errorf("%s is empty", path)
	}
}
