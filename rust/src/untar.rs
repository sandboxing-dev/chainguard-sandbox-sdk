//! Safe local extraction of a sandbox's tar archive (`download_dir`).
//!
//! Nothing it writes can land outside the target directory, and no symbolic link it leaves behind
//! leads outside it:
//!
//! - names that are absolute or contain ".." are refused;
//! - an entry is never written through a symbolic link (one from the archive or one already in
//!   the directory), so a link can't redirect later entries;
//! - once everything is unpacked, every link from the archive is followed the way the kernel
//!   would follow it, link by link. One that leaves the directory (also through other links, in
//!   whatever order they were unpacked) is removed and the extraction fails.

use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

pub(crate) fn extract_tar_gz(tgz: &[u8], dir: &Path) -> Result<()> {
    let io = |ctx: &str, e: std::io::Error| Error::io(ctx.to_string(), e);
    std::fs::create_dir_all(dir).map_err(|e| io(&dir.display().to_string(), e))?;
    // The real path, so the destination itself may be reached through a link.
    let root = std::fs::canonicalize(dir).map_err(|e| io(&dir.display().to_string(), e))?;
    let mut links: Vec<PathBuf> = Vec::new();
    let r = extract(tgz, &root, &mut links);
    // Also when an entry is refused partway: no escaping link stays behind.
    let lr = remove_escaping_links(&root, &links);
    r.and(lr)
}

fn refuse(name: &str, why: &str) -> Error {
    Error::other(format!("sandbox: refusing archive entry {name:?}{why}"))
}

fn extract(tgz: &[u8], root: &Path, links: &mut Vec<PathBuf>) -> Result<()> {
    let io = |ctx: String, e: std::io::Error| Error::io(ctx, e);
    let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(tgz));
    for entry in ar
        .entries()
        .map_err(|e| io("reading the archive".into(), e))?
    {
        let mut entry = entry.map_err(|e| io("reading the archive".into(), e))?;
        let raw = String::from_utf8_lossy(&entry.path_bytes()).into_owned();
        let clean_rel = clean(raw.strip_prefix("./").unwrap_or(&raw));
        if clean_rel.is_empty() || clean_rel == "." {
            continue;
        }
        if raw.starts_with('/') || clean_rel == ".." || clean_rel.starts_with("../") {
            return Err(refuse(&raw, " outside the target"));
        }
        let rel = PathBuf::from(&clean_rel);
        let target = root.join(&rel);
        if !inside(root, &target) {
            return Err(refuse(&raw, " outside the target"));
        }
        let kind = entry.header().entry_type();
        if !(kind.is_dir() || kind.is_file() || kind.is_symlink()) {
            continue; // hard links, devices, FIFOs: skipped
        }
        if let Err(why) = real_parents(root, rel.parent().unwrap_or(Path::new(""))) {
            return Err(refuse(&raw, &format!(": {why}")));
        }
        if kind.is_dir() {
            if is_symlink(&target) {
                return Err(refuse(&raw, ": a symbolic link is in the way"));
            }
            create_dirs(&target)?;
        } else if kind.is_file() {
            if let Some(p) = target.parent() {
                create_dirs(p)?;
            }
            if is_symlink(&target) {
                // Replace the link; don't write to what it points at.
                std::fs::remove_file(&target).map_err(|e| io(target.display().to_string(), e))?;
            }
            let mode = entry.header().mode().unwrap_or(0o644) & 0o777 | 0o200;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(mode);
            }
            let mut f = opts
                .open(&target)
                .map_err(|e| io(target.display().to_string(), e))?;
            let mut buf = Vec::new();
            entry
                .read_to_end(&mut buf)
                .map_err(|e| io("reading the archive".into(), e))?;
            std::io::Write::write_all(&mut f, &buf)
                .map_err(|e| io(target.display().to_string(), e))?;
            #[cfg(not(unix))]
            let _ = mode;
        } else {
            let link = entry
                .link_name_bytes()
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default();
            let lp = Path::new(&link);
            let escapes = lp.is_absolute()
                || link.starts_with('/')
                || !inside(root, &lexical(&target.parent().unwrap_or(root).join(lp)));
            if escapes {
                return Err(Error::other(format!(
                    "sandbox: refusing symlink {raw:?} -> {link:?} pointing outside the target"
                )));
            }
            if let Some(p) = target.parent() {
                create_dirs(p)?;
            }
            let _ = std::fs::remove_file(&target);
            symlink(&link, &target).map_err(|e| io(target.display().to_string(), e))?;
            links.push(target);
        }
    }
    Ok(())
}

fn create_dirs(p: &Path) -> Result<()> {
    std::fs::create_dir_all(p).map_err(|e| Error::io(p.display().to_string(), e))
}

#[cfg(unix)]
fn symlink(target: &str, at: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, at)
}

#[cfg(windows)]
fn symlink(target: &str, at: &Path) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, at)
}

/// Go's `path.Clean` for a relative archive name.
fn clean(p: &str) -> String {
    crate::workspaces_clean_path(p)
}

/// Removes `.` and `..` without touching the filesystem.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            c => out.push(c.as_os_str()),
        }
    }
    out
}

fn inside(root: &Path, p: &Path) -> bool {
    lexical(p).starts_with(root)
}

fn is_symlink(p: &Path) -> bool {
    std::fs::symlink_metadata(p)
        .map(|m| m.file_type().is_symlink())
        .unwrap_or(false)
}

/// Every existing component of rel (a directory path under root) must be a real directory, not a
/// symbolic link. Components that don't exist yet are fine: they get created as real directories.
fn real_parents(root: &Path, rel: &Path) -> std::result::Result<(), String> {
    let mut cur = root.to_path_buf();
    for part in rel.components() {
        let Component::Normal(part) = part else {
            continue;
        };
        cur.push(part);
        match std::fs::symlink_metadata(&cur) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => return Err(e.to_string()),
            Ok(m) if m.file_type().is_symlink() => {
                return Err(format!("{:?} is a symbolic link", part.to_string_lossy()));
            }
            Ok(m) if !m.is_dir() => {
                return Err(format!("{:?} is not a directory", part.to_string_lossy()));
            }
            Ok(_) => {}
        }
    }
    Ok(())
}

/// Follows each unpacked link and removes those that lead outside root; fails if there were any.
fn remove_escaping_links(root: &Path, links: &[PathBuf]) -> Result<()> {
    let bad: Vec<&PathBuf> = links
        .iter()
        .filter(|l| is_symlink(l) && link_escapes(root, l))
        .collect();
    let Some(first) = bad.first() else {
        return Ok(());
    };
    for l in &bad {
        let _ = std::fs::remove_file(l);
    }
    let rel = first
        .strip_prefix(root)
        .unwrap_or(first)
        .to_string_lossy()
        .replace('\\', "/");
    Err(Error::other(format!(
        "sandbox: refusing symlink {rel:?}: following it leads outside the target"
    )))
}

fn split(p: &str) -> Vec<String> {
    p.split(['/', std::path::MAIN_SEPARATOR])
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// Whether following the symbolic link at p leaves root. It walks the link's target one component
/// at a time, as path resolution does, expanding any link it meets (root-relative, so the order
/// links were created in doesn't matter) and treating missing components as plain names.
fn link_escapes(root: &Path, p: &Path) -> bool {
    let Ok(target) = std::fs::read_link(p) else {
        return false;
    };
    let t = target.to_string_lossy().into_owned();
    if target.is_absolute() || t.starts_with('/') {
        return true;
    }
    let mut cur = p.parent().unwrap_or(root).to_path_buf();
    let mut todo: std::collections::VecDeque<String> = split(&t).into();
    let mut hops = 0;
    while let Some(part) = todo.pop_front() {
        match part.as_str() {
            "." => continue,
            ".." => {
                if cur == root {
                    return true;
                }
                cur.pop();
                continue;
            }
            _ => {}
        }
        let next = cur.join(&part);
        if is_symlink(&next) {
            hops += 1;
            let Ok(t) = std::fs::read_link(&next) else {
                return true;
            };
            let ts = t.to_string_lossy().into_owned();
            if hops > 40 || t.is_absolute() || ts.starts_with('/') {
                return true;
            }
            for (i, c) in split(&ts).into_iter().enumerate() {
                todo.insert(i, c);
            }
            continue;
        }
        cur = next;
    }
    false
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    fn tgz(entries: &[(&str, tar::EntryType, &str)]) -> Vec<u8> {
        let zw = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tw = tar::Builder::new(zw);
        for (name, kind, data) in entries {
            let mut h = tar::Header::new_gnu();
            h.set_entry_type(*kind);
            h.set_mode(0o644);
            if kind.is_symlink() {
                h.set_size(0);
                h.set_link_name(data).unwrap();
                h.set_path(name).unwrap();
                h.set_cksum();
                tw.append(&h, std::io::empty()).unwrap();
            } else {
                h.set_size(data.len() as u64);
                h.as_old_mut().name[..name.len()].copy_from_slice(name.as_bytes());
                h.set_cksum();
                tw.append(&h, data.as_bytes()).unwrap();
            }
        }
        tw.into_inner().unwrap().finish().unwrap()
    }

    fn tmp() -> PathBuf {
        let d = std::env::temp_dir().join(format!("cgs-untar-{}", crate::workspaces_random_id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn extracts_files_dirs_and_inside_links() {
        let d = tmp();
        let a = tgz(&[
            ("./sub/", tar::EntryType::Directory, ""),
            ("sub/a.txt", tar::EntryType::Regular, "hi"),
            ("ln", tar::EntryType::Symlink, "sub/a.txt"),
        ]);
        extract_tar_gz(&a, &d).unwrap();
        assert_eq!(std::fs::read_to_string(d.join("ln")).unwrap(), "hi");
        std::fs::remove_dir_all(&d).unwrap();
    }

    #[test]
    fn refuses_escapes() {
        let d = tmp();
        assert!(extract_tar_gz(&tgz(&[("../evil", tar::EntryType::Regular, "x")]), &d).is_err());
        assert!(extract_tar_gz(&tgz(&[("up", tar::EntryType::Symlink, "../..")]), &d).is_err());
        assert!(extract_tar_gz(&tgz(&[("abs", tar::EntryType::Symlink, "/etc")]), &d).is_err());
        // A chain of links that only escapes once both exist.
        let r = extract_tar_gz(
            &tgz(&[
                ("a/b", tar::EntryType::Symlink, ".."),
                ("c", tar::EntryType::Symlink, "a/b/.."),
            ]),
            &d,
        );
        assert!(r.is_err(), "{r:?}");
        assert!(!is_symlink(&d.join("c")));
        // Writing through a link already in place is refused.
        std::os::unix::fs::symlink("/tmp", d.join("out")).unwrap();
        assert!(extract_tar_gz(&tgz(&[("out/x", tar::EntryType::Regular, "x")]), &d).is_err());
        std::fs::remove_dir_all(&d).unwrap();
    }

    /// The Go SDK's cases: nothing is written, or left as a link, outside the target.
    #[test]
    #[allow(clippy::type_complexity)]
    fn link_chains_cannot_escape() {
        use tar::EntryType::{Directory as D, Regular as F, Symlink as L};
        let cases: &[(&str, &[(&str, tar::EntryType, &str)])] = &[
            // Lexically "a/../outside" stays inside; followed, a is "." and it is a sibling.
            (
                "write through a link chain",
                &[("a", L, "."), ("b", L, "a/../outside"), ("b/proof", F, "x")],
            ),
            (
                "link chain only",
                &[("a", L, "."), ("b", L, "a/../outside")],
            ),
            (
                "link chain, other order",
                &[("b", L, "a/../outside"), ("a", L, ".")],
            ),
            (
                "write through a plain link",
                &[("d", L, "."), ("d/../../proof", F, "x")],
            ),
            (
                "file under a link",
                &[("real/", D, ""), ("l", L, "real"), ("l/f", F, "x")],
            ),
        ];
        for (name, entries) in cases {
            let parent = tmp();
            let dir = parent.join("target");
            std::fs::create_dir(parent.join("outside")).unwrap();
            assert!(
                extract_tar_gz(&tgz(entries), &dir).is_err(),
                "{name}: accepted"
            );
            assert_eq!(
                std::fs::read_dir(parent.join("outside")).unwrap().count(),
                0,
                "{name}: wrote outside"
            );
            assert!(
                std::fs::symlink_metadata(parent.join("proof")).is_err(),
                "{name}: wrote next to the target"
            );
            assert!(
                std::fs::symlink_metadata(dir.join("b")).is_err(),
                "{name}: left the escaping link"
            );
            std::fs::remove_dir_all(&parent).unwrap();
        }
    }
}
