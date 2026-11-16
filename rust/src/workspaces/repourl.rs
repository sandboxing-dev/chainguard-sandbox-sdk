//! Repository URL normalization.

/// Turns any GitHub remote form — `git@github.com:o/r.git`, `ssh://git@github.com[:22]/o/r`,
/// `git://github.com/o/r`, `https://token@GitHub.com/o/r.git`, `github.com/o/r` — into
/// `https://github.com/o/r`. Credentials are never kept. Other URLs with a scheme lose their
/// credentials and trailing ".git"; scp-style remotes on other hosts are returned unchanged.
pub fn normalize_github_url(u: &str) -> String {
    let u = u.trim();
    let (host, p, kind) = split_remote(u);
    let p = p.trim_matches('/');
    let p = p.strip_suffix(".git").unwrap_or(p);
    let h = host.to_lowercase();
    if h == "github.com" || h == "www.github.com" {
        return format!("https://github.com/{p}");
    }
    match kind {
        Kind::Url => match url::Url::parse(u) {
            Ok(mut pu) => {
                let _ = pu.set_username("");
                let _ = pu.set_password(None);
                let path = pu.path().to_string();
                let path = path.strip_suffix('/').unwrap_or(&path);
                let path = path.strip_suffix(".git").unwrap_or(path).to_string();
                pu.set_path(&path);
                let s = pu.to_string();
                // url adds a "/" path to "scheme://host"; Go keeps it bare.
                if path.is_empty() {
                    s.trim_end_matches('/').to_string()
                } else {
                    s
                }
            }
            Err(_) => u.to_string(),
        },
        Kind::Scp => u.to_string(),
        Kind::Bare => {
            let t = u.strip_suffix('/').unwrap_or(u);
            t.strip_suffix(".git").unwrap_or(t).to_string()
        }
    }
}

enum Kind {
    /// host/path or a local path
    Bare,
    /// scheme://[user@]host[:port]/path
    Url,
    /// [user@]host:path
    Scp,
}

fn split_remote(u: &str) -> (String, String, Kind) {
    if u.contains("://") {
        return match url::Url::parse(u) {
            Ok(pu) => (
                pu.host_str()
                    .unwrap_or_default()
                    .trim_matches(['[', ']'])
                    .to_string(),
                pu.path().to_string(),
                Kind::Url,
            ),
            Err(_) => (String::new(), u.to_string(), Kind::Url),
        };
    }
    if is_scp_like(u) {
        let (host_part, p) = u.split_once(':').unwrap_or((u, ""));
        let host_part = host_part
            .split_once('@')
            .map(|(_, h)| h)
            .unwrap_or(host_part);
        return (host_part.to_string(), p.to_string(), Kind::Scp);
    }
    let (host, p) = u.split_once('/').unwrap_or((u, ""));
    (host.to_string(), p.to_string(), Kind::Bare)
}

/// "[user@]host:path" with no slash before the colon.
pub(crate) fn is_scp_like(u: &str) -> bool {
    match u.find(':') {
        Some(i) => i > 0 && !u[..i].contains('/') && !u.contains("://"),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_github_forms() {
        for i in [
            "git@github.com:o/r.git",
            "ssh://git@github.com:22/o/r",
            "git://github.com/o/r",
            "https://token@GitHub.com/o/r.git",
            "github.com/o/r",
            "  https://www.github.com/o/r/ ",
        ] {
            assert_eq!(normalize_github_url(i), "https://github.com/o/r", "{i}");
        }
        assert_eq!(
            normalize_github_url("https://u:p@gitlab.com/o/r.git"),
            "https://gitlab.com/o/r"
        );
        assert_eq!(
            normalize_github_url("git@gitlab.com:o/r.git"),
            "git@gitlab.com:o/r.git"
        );
        assert_eq!(
            super::super::repository_url("gitlab.com/o/r"),
            "https://gitlab.com/o/r"
        );
    }
}
