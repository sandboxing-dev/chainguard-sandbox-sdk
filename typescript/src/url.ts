// Repository URL normalization for sandboxes started from a repository. Mirrors the Go SDK's
// NormalizeGitHubURL and RepositoryURL.

/** Split a git remote into host and path (scp "user@host:path", "scheme://[userinfo@]host[:port]/path", or bare "host/path"). */
function parseRemote(u: string): { host: string; path: string; form: "scp" | "url" | "bare" } | undefined {
  const s = u.trim();
  if (s.includes("://")) {
    try {
      const url = new URL(s);
      return { host: url.hostname.toLowerCase(), path: decodeURIComponent(url.pathname).replace(/^\/+/, ""), form: "url" };
    } catch {
      return undefined;
    }
  }
  const scp = s.match(/^(?:[^@/]+@)?([^:/]+):(.+)$/);
  if (scp) return { host: scp[1]!.toLowerCase(), path: scp[2]!, form: "scp" };
  const slash = s.indexOf("/");
  if (slash > 0) return { host: s.slice(0, slash).toLowerCase(), path: s.slice(slash + 1), form: "bare" };
  return undefined;
}

const cleanPath = (p: string) => p.replace(/\/+$/, "").replace(/\.git$/, "");

/** Whether u names a repository on github.com (any of the scp, ssh://, https://, git:// or bare forms). */
export function isGitHubUrl(u: string): boolean {
  const r = parseRemote(u);
  return !!r && (r.host === "github.com" || r.host === "www.github.com") && cleanPath(r.path).split("/").length === 2;
}

/**
 * GitHub remotes (git@github.com:o/r.git, ssh://git@github.com:22/o/r, git://,
 * https://user:token@GitHub.com/o/r.git, github.com/o/r) become
 * "https://github.com/o/r", dropping any credentials. Other remotes are
 * returned without a trailing ".git" (and without URL credentials).
 */
export function normalizeGitHubUrl(u: string): string {
  const r = parseRemote(u);
  if (r && isGitHubUrl(u)) return `https://github.com/${cleanPath(r.path)}`;
  if (r?.form === "url") {
    const url = new URL(u.trim());
    url.username = "";
    url.password = "";
    return cleanPath(url.toString());
  }
  return cleanPath(u.trim());
}
