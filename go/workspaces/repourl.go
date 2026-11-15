package workspaces

import (
	"net/url"
	"strings"
)

// NormalizeGitHubURL turns any GitHub remote form — git@github.com:o/r.git,
// ssh://git@github.com[:22]/o/r, git://github.com/o/r,
// https://token@GitHub.com/o/r.git, github.com/o/r — into
// https://github.com/o/r. Credentials are never kept. Other URLs with a
// scheme lose their credentials and trailing ".git"; scp-style remotes on
// other hosts are returned unchanged.
func NormalizeGitHubURL(u string) string {
	u = strings.TrimSpace(u)
	host, p, kind := splitRemote(u)
	p = strings.TrimSuffix(strings.Trim(p, "/"), ".git")
	if h := strings.ToLower(host); h == "github.com" || h == "www.github.com" {
		return "https://github.com/" + p
	}
	switch kind {
	case remoteURL:
		pu, err := url.Parse(u)
		if err != nil {
			return u
		}
		pu.User = nil
		pu.Path = strings.TrimSuffix(strings.TrimSuffix(pu.Path, "/"), ".git")
		pu.RawPath = ""
		return pu.String()
	case remoteSCP:
		return u
	}
	return strings.TrimSuffix(strings.TrimSuffix(u, "/"), ".git")
}

type remoteKind int

const (
	remoteBare remoteKind = iota // host/path or a local path
	remoteURL                    // scheme://[user@]host[:port]/path
	remoteSCP                    // [user@]host:path
)

func splitRemote(u string) (host, p string, kind remoteKind) {
	if strings.Contains(u, "://") {
		pu, err := url.Parse(u)
		if err != nil {
			return "", u, remoteURL
		}
		return pu.Hostname(), pu.Path, remoteURL
	}
	if isSCPLike(u) {
		hostPart, p, _ := strings.Cut(u, ":")
		if _, h, ok := strings.Cut(hostPart, "@"); ok {
			hostPart = h
		}
		return hostPart, p, remoteSCP
	}
	host, p, _ = strings.Cut(u, "/")
	return host, p, remoteBare
}

// isSCPLike: "[user@]host:path" with no slash before the colon.
func isSCPLike(u string) bool {
	i := strings.Index(u, ":")
	return i > 0 && !strings.Contains(u[:i], "/") && !strings.Contains(u, "://")
}
