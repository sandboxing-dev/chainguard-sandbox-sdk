//! File helpers: an atomic write over Exec, and tar archives of local directories.

use std::io::{Read, Write};
use std::path::Path;

use super::Client;
use super::exec::{ExecOptions, guest_path};
use crate::{Error, Result};

/// Decides whether to leave out a path (relative, slash-separated) and, for a directory,
/// everything under it. The second argument is whether it is a directory.
pub type SkipFn<'a> = &'a dyn Fn(&str, bool) -> bool;

impl Client {
    /// Atomically replaces `path` (creating parent directories) with `data`, with permission bits
    /// `mode`. It refuses an empty path and an existing directory (exit code 21, as an
    /// [`Error::Exit`]).
    pub async fn write_file(&self, name: &str, path: &str, data: Vec<u8>, mode: u32) -> Result<()> {
        if path.trim().is_empty() {
            return Err(Error::other("workspaces: write_file needs a file path"));
        }
        const SCRIPT: &str = concat!(
            r#"p=$1; if [ -d "$p" ]; then echo "$p: is a directory" >&2; cat >/dev/null; exit 21; fi; "#,
            r#"d=$(dirname -- "$p") && mkdir -p -- "$d" && t="$d/.chainguard-sandbox-write.$$" && cat > "$t" && chmod "$2" "$t" && mv -f -- "$t" "$p" || { rm -f -- "$t"; exit 1; }"#
        );
        let o = ExecOptions {
            stdin: data,
            ..Default::default()
        };
        self.shell(
            name,
            SCRIPT,
            o,
            &[guest_path(path), format!("{:o}", mode & 0o777)],
        )
        .await?;
        Ok(())
    }
}

/// Writes `dir` as an uncompressed tar stream, with the rules chainctl documents for its workspace
/// upload: symlinks travel as links; sockets, devices and FIFOs are skipped; entries are owned by
/// root. A symlinked `dir` is resolved first. A file that changes size while it is being written is
/// truncated or zero-padded to the size in its header, rather than aborting the tar.
pub fn tar_dir<W: Write>(w: W, dir: &Path, skip: Option<SkipFn<'_>>) -> Result<W> {
    let root = std::fs::canonicalize(dir).map_err(|e| Error::io(dir.display().to_string(), e))?;
    let md = std::fs::metadata(&root).map_err(|e| Error::io(dir.display().to_string(), e))?;
    if !md.is_dir() {
        return Err(Error::other(format!("{}: not a directory", dir.display())));
    }
    let mut tw = tar::Builder::new(w);
    tw.follow_symlinks(false);
    walk(&mut tw, &root, "", skip)?;
    tw.into_inner()
        .map_err(|e| Error::io("writing the tar archive", e))
}

/// [`tar_dir`], gzip'd, in memory (the sandbox API's `upload_dir` sends it).
pub fn tar_gz_dir(dir: &Path, skip: Option<SkipFn<'_>>) -> Result<Vec<u8>> {
    let zw = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    let zw = tar_dir(zw, dir, skip)?;
    zw.finish().map_err(|e| Error::io("gzip", e))
}

fn walk<W: Write>(
    tw: &mut tar::Builder<W>,
    root: &Path,
    rel: &str,
    skip: Option<SkipFn<'_>>,
) -> Result<()> {
    let dir = if rel.is_empty() {
        root.to_path_buf()
    } else {
        root.join(rel)
    };
    let mut names: Vec<std::ffi::OsString> = std::fs::read_dir(&dir)
        .map_err(|e| Error::io(dir.display().to_string(), e))?
        .map(|e| e.map(|e| e.file_name()))
        .collect::<std::io::Result<_>>()
        .map_err(|e| Error::io(dir.display().to_string(), e))?;
    names.sort(); // lexical order, like Go's filepath.WalkDir
    for name in names {
        let name = name.to_string_lossy().into_owned();
        let r = if rel.is_empty() {
            name.clone()
        } else {
            format!("{rel}/{name}")
        };
        let p = root.join(&r);
        let md =
            std::fs::symlink_metadata(&p).map_err(|e| Error::io(p.display().to_string(), e))?;
        let ft = md.file_type();
        if skip.is_some_and(|s| s(&r, ft.is_dir())) {
            continue;
        }
        if !(ft.is_dir() || ft.is_file() || ft.is_symlink()) {
            continue; // sockets, devices, FIFOs
        }
        let mut h = tar::Header::new_gnu();
        h.set_metadata_in_mode(&md, tar::HeaderMode::Complete);
        // Root-owned, like chainctl's upload: entries carrying the laptop's uid make git in the
        // guest refuse /work ("dubious ownership").
        h.set_uid(0);
        h.set_gid(0);
        let _ = h.set_username("");
        let _ = h.set_groupname("");
        let io = |e| Error::io(format!("adding {} to the tar archive", p.display()), e);
        if ft.is_symlink() {
            let target =
                std::fs::read_link(&p).map_err(|e| Error::io(p.display().to_string(), e))?;
            h.set_size(0);
            tw.append_link(&mut h, &r, &target).map_err(io)?;
        } else if ft.is_dir() {
            h.set_size(0);
            tw.append_data(&mut h, format!("{r}/"), std::io::empty())
                .map_err(io)?;
            walk(tw, root, &r, skip)?;
        } else {
            let size = md.len();
            let f = std::fs::File::open(&p).map_err(|e| Error::io(p.display().to_string(), e))?;
            h.set_size(size);
            // Exactly size bytes: a file that grew is truncated, one that shrank is zero-padded.
            let data = f.take(size).chain(std::io::repeat(0)).take(size);
            tw.append_data(&mut h, &r, data).map_err(io)?;
        }
    }
    Ok(())
}
