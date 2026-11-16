//! Reading and writing a sandbox's filesystem: `sbx.files`.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::sandbox::{Inner, guest_path, wrap};
use crate::scripts::*;
use crate::workspaces::{self, ExecOptions};
use crate::{Error, Result};

crate::opaque_debug!(Files);

/// "file", "dir" or "symlink".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileType {
    File,
    Dir,
    Symlink,
}

impl FileType {
    /// "file", "dir" or "symlink", as the other SDKs spell it.
    pub fn as_str(&self) -> &'static str {
        match self {
            FileType::File => "file",
            FileType::Dir => "dir",
            FileType::Symlink => "symlink",
        }
    }
}

impl std::fmt::Display for FileType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Describes a written file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WriteInfo {
    pub name: String,
    pub path: String,
    pub file_type: FileType,
}

/// Describes a file, directory or symlink.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryInfo {
    pub name: String,
    pub path: String,
    pub file_type: FileType,
    pub size: i64,
    /// Permission bits, e.g. 0o644.
    pub mode: u32,
    /// e.g. "rwxr-xr-x".
    pub permissions: String,
    pub owner: String,
    pub group: String,
    pub modified_time: SystemTime,
    pub symlink_target: String,
}

/// One file for [`Files::write_files`].
#[derive(Debug, Clone, Default)]
pub struct WriteEntry {
    pub path: String,
    pub data: Vec<u8>,
    /// Permission bits; 0 means 0o644.
    pub mode: u32,
}

/// Reads and writes a sandbox's filesystem. Relative paths resolve against /work. Every call is a
/// small shell program run in the sandbox, so each costs one round trip (about 0.15 s once the
/// sandbox is warm); batch with [`write_files`](Files::write_files) and
/// [`upload_dir`](Files::upload_dir) where you can.
#[derive(Clone)]
pub struct Files {
    sb: Arc<Inner>,
}

struct ScriptOut {
    stdout: Vec<u8>,
    code: i32,
    stderr: Vec<u8>,
}

/// Maps a script's reserved exit codes to typed errors.
fn script_error(code: i32, p: &str, stderr: &[u8]) -> Error {
    match code {
        EXIT_NOT_FOUND => Error::NotFound {
            what: "path".into(),
            name: p.into(),
        },
        EXIT_IS_DIR => Error::InvalidPath {
            path: p.into(),
            reason: "is a directory".into(),
        },
        EXIT_NOT_DIR => Error::InvalidPath {
            path: p.into(),
            reason: "not a directory".into(),
        },
        _ => {
            let msg = String::from_utf8_lossy(stderr).trim().to_string();
            let msg = if msg.is_empty() {
                format!("exit {code}")
            } else {
                msg
            };
            Error::other(format!("sandbox: {p}: {msg}"))
        }
    }
}

impl Files {
    pub(crate) fn new(sb: Arc<Inner>) -> Self {
        Files { sb }
    }

    /// Runs a script with args and stdin. A transport error is the error; a non-zero exit is in
    /// the result.
    async fn script(&self, script: &str, stdin: Vec<u8>, args: &[&str]) -> Result<ScriptOut> {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut argv = vec!["sh".to_string(), "-c".into(), script.into(), "_".into()];
        argv.extend(args.iter().map(|a| a.to_string()));
        let res = {
            let mut fo = |b: &[u8]| out.extend_from_slice(b);
            let mut fe = |b: &[u8]| err.extend_from_slice(b);
            let o = ExecOptions {
                no_dir: true,
                stdin,
                stdout: Some(&mut fo),
                stderr: Some(&mut fe),
                ..Default::default()
            };
            self.sb
                .client
                .exec(&self.sb.name, &argv, o)
                .await
                .map_err(wrap)?
        };
        Ok(ScriptOut {
            stdout: out,
            code: res.exit_code,
            stderr: err,
        })
    }

    async fn checked(
        &self,
        script: &str,
        stdin: Vec<u8>,
        args: &[&str],
        p: &str,
    ) -> Result<Vec<u8>> {
        let r = self.script(script, stdin, args).await?;
        if r.code != 0 {
            return Err(script_error(r.code, p, &r.stderr));
        }
        Ok(r.stdout)
    }

    /// A file's contents. A missing file is [`Error::NotFound`].
    pub async fn read(&self, p: &str) -> Result<Vec<u8>> {
        let gp = guest_path(p);
        self.checked(READ_SCRIPT, Vec::new(), &[&gp], &gp).await
    }

    /// A file's contents as a string (invalid UTF-8 is replaced).
    pub async fn read_text(&self, p: &str) -> Result<String> {
        let b = self.read(p).await?;
        Ok(String::from_utf8(b)
            .unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned()))
    }

    /// Creates or replaces a file (atomically, mode 0644), creating parent directories. Any size
    /// works: data over 15 MiB is staged in pieces.
    pub async fn write(&self, p: &str, data: impl Into<Vec<u8>>) -> Result<WriteInfo> {
        self.write_mode(p, data.into(), 0o644).await
    }

    async fn write_mode(&self, p: &str, data: Vec<u8>, mode: u32) -> Result<WriteInfo> {
        let gp = guest_path(p);
        if data.len() <= MAX_STDIN_PER_EXEC {
            match self
                .sb
                .client
                .write_file(&self.sb.name, &gp, data, mode)
                .await
            {
                Err(Error::Exit { exit_code: 21, .. }) => {
                    return Err(Error::InvalidPath {
                        path: gp,
                        reason: "is a directory".into(),
                    });
                }
                Err(e) => return Err(wrap(e)),
                Ok(()) => {}
            }
        } else {
            let tmp = self.stage(&data).await?;
            self.checked(
                FINISH_SCRIPT,
                Vec::new(),
                &[&tmp, &gp, &format!("{:o}", mode & 0o777)],
                &gp,
            )
            .await?;
        }
        Ok(WriteInfo {
            name: base(&gp),
            path: gp,
            file_type: FileType::File,
        })
    }

    /// Uploads data to a temporary file in the sandbox, in pieces under the exec stdin limit, and
    /// returns its path.
    async fn stage(&self, data: &[u8]) -> Result<String> {
        let tmp = format!("{STATE_DIR}/upload/{}", crate::workspaces_random_id());
        let mut off = 0;
        loop {
            let end = (off + MAX_STDIN_PER_EXEC).min(data.len());
            let mode = if off == 0 { "trunc" } else { "append" };
            self.checked(APPEND_SCRIPT, data[off..end].to_vec(), &[&tmp, mode], &tmp)
                .await?;
            if end == data.len() {
                return Ok(tmp);
            }
            off = end;
        }
    }

    /// Extracts a gzip'd tar into dir in the sandbox.
    async fn untar(&self, tgz: Vec<u8>, dir: &str) -> Result<()> {
        if tgz.len() > MAX_STDIN_PER_EXEC {
            let tmp = self.stage(&tgz).await?;
            self.checked(UNTAR_SCRIPT, Vec::new(), &[&tmp, dir], dir)
                .await?;
        } else {
            self.checked(UNTAR_SCRIPT, tgz, &["-", dir], dir).await?;
        }
        Ok(())
    }

    /// Writes several files in one upload (one tar), creating parent directories. Files are
    /// owned by root.
    pub async fn write_files(&self, files: Vec<WriteEntry>) -> Result<Vec<WriteInfo>> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let zw = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tw = tar::Builder::new(zw);
        let mut infos = Vec::with_capacity(files.len());
        let io = |e| Error::io("building the upload", e);
        for e in &files {
            let gp = guest_path(&e.path);
            if gp == "/" {
                return Err(Error::other(format!(
                    "sandbox: invalid file path {:?}",
                    e.path
                )));
            }
            let mode = if e.mode & 0o777 == 0 {
                0o644
            } else {
                e.mode & 0o777
            };
            let mut h = tar::Header::new_gnu();
            h.set_mode(mode);
            h.set_size(e.data.len() as u64);
            h.set_mtime(now);
            h.set_entry_type(tar::EntryType::Regular);
            tw.append_data(&mut h, gp.trim_start_matches('/'), e.data.as_slice())
                .map_err(io)?;
            infos.push(WriteInfo {
                name: base(&gp),
                path: gp,
                file_type: FileType::File,
            });
        }
        let zw = tw.into_inner().map_err(io)?;
        let tgz = zw.finish().map_err(io)?;
        self.untar(tgz, "/").await?;
        Ok(infos)
    }

    /// The entries of a directory (one level), sorted by name.
    pub async fn list(&self, p: &str) -> Result<Vec<EntryInfo>> {
        let gp = guest_path(p);
        let out = self
            .checked(LIST_DIR_SCRIPT, Vec::new(), &[&gp], &gp)
            .await?;
        parse_entries(&out, &gp)
    }

    /// Whether a path exists (a dangling symlink counts).
    pub async fn exists(&self, p: &str) -> Result<bool> {
        Ok(self
            .script(EXISTS_SCRIPT, Vec::new(), &[&guest_path(p)])
            .await?
            .code
            == 0)
    }

    /// Describes a path without following a final symlink.
    pub async fn get_info(&self, p: &str) -> Result<EntryInfo> {
        let gp = guest_path(p);
        let out = self.checked(INFO_SCRIPT, Vec::new(), &[&gp], &gp).await?;
        one_entry(&out, &gp)
    }

    /// Creates a directory and its parents. Returns false if it already existed.
    pub async fn make_dir(&self, p: &str) -> Result<bool> {
        let gp = guest_path(p);
        let out = self
            .checked(MAKE_DIR_SCRIPT, Vec::new(), &[&gp], &gp)
            .await?;
        Ok(String::from_utf8_lossy(&out).trim() == "1")
    }

    /// Deletes a file or a directory tree. A missing path is [`Error::NotFound`].
    pub async fn remove(&self, p: &str) -> Result<()> {
        let gp = guest_path(p);
        self.checked(REMOVE_SCRIPT, Vec::new(), &[&gp], &gp).await?;
        Ok(())
    }

    /// Moves a file or directory, creating the destination's parents, and describes it at its new
    /// path.
    pub async fn rename(&self, old_path: &str, new_path: &str) -> Result<EntryInfo> {
        let (op, np) = (guest_path(old_path), guest_path(new_path));
        let out = self
            .checked(RENAME_SCRIPT, Vec::new(), &[&op, &np], &op)
            .await?;
        one_entry(&out, &np)
    }

    /// Copies a local file into the sandbox, keeping its permission bits.
    pub async fn upload(&self, local_path: impl AsRef<Path>, remote_path: &str) -> Result<()> {
        let lp = local_path.as_ref();
        let ctx = || lp.display().to_string();
        let md = tokio::fs::metadata(lp)
            .await
            .map_err(|e| Error::io(ctx(), e))?;
        let data = tokio::fs::read(lp).await.map_err(|e| Error::io(ctx(), e))?;
        self.write_mode(remote_path, data, perm_bits(&md)).await?;
        Ok(())
    }

    /// Copies a file from the sandbox to `local_path`, creating local parent directories.
    pub async fn download(&self, remote_path: &str, local_path: impl AsRef<Path>) -> Result<()> {
        let data = self.read(remote_path).await?;
        let lp = local_path.as_ref();
        if let Some(d) = lp.parent().filter(|d| !d.as_os_str().is_empty()) {
            tokio::fs::create_dir_all(d)
                .await
                .map_err(|e| Error::io(d.display().to_string(), e))?;
        }
        tokio::fs::write(lp, data)
            .await
            .map_err(|e| Error::io(lp.display().to_string(), e))
    }

    /// Copies a local directory tree into `remote_dir` (created if missing), as one tar. Symlinks
    /// are copied as links; sockets and devices are skipped. Files are owned by root.
    pub async fn upload_dir(&self, local_dir: impl AsRef<Path>, remote_dir: &str) -> Result<()> {
        let dir = local_dir.as_ref().to_path_buf();
        let tgz = tokio::task::spawn_blocking(move || workspaces::tar_gz_dir(&dir, None))
            .await
            .map_err(|e| Error::other(format!("sandbox: building the upload: {e}")))??;
        self.untar(tgz, &guest_path(remote_dir)).await
    }

    /// Copies a directory tree from the sandbox into `local_dir` (created if missing). Nothing is
    /// written outside `local_dir`: absolute names and ".." are refused, an entry is never written
    /// through a symbolic link, and a link that leads outside `local_dir` (directly or through
    /// other links) is removed and reported as an error.
    pub async fn download_dir(&self, remote_dir: &str, local_dir: impl AsRef<Path>) -> Result<()> {
        let gp = guest_path(remote_dir);
        let out = self.checked(TAR_SCRIPT, Vec::new(), &[&gp], &gp).await?;
        let dir = local_dir.as_ref().to_path_buf();
        tokio::task::spawn_blocking(move || crate::untar::extract_tar_gz(&out, &dir))
            .await
            .map_err(|e| Error::other(format!("sandbox: extracting: {e}")))?
    }
}

#[cfg(unix)]
fn perm_bits(md: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    md.permissions().mode() & 0o777
}

#[cfg(not(unix))]
fn perm_bits(md: &std::fs::Metadata) -> u32 {
    if md.permissions().readonly() {
        0o444
    } else {
        0o644
    }
}

fn base(p: &str) -> String {
    let t = p.trim_end_matches('/');
    if t.is_empty() {
        return "/".into();
    }
    t.rsplit('/').next().unwrap_or(t).to_string()
}

fn one_entry(out: &[u8], p: &str) -> Result<EntryInfo> {
    match parse_entries(out, "") {
        Ok(mut es) if es.len() == 1 => Ok(es.remove(0)),
        _ => Err(Error::other(format!(
            "sandbox: unexpected stat output for {p}: {:?}",
            String::from_utf8_lossy(out)
        ))),
    }
}

/// Decodes stat lines; `base_dir`, if set, is joined with the name.
pub(crate) fn parse_entries(out: &[u8], base_dir: &str) -> Result<Vec<EntryInfo>> {
    let text = String::from_utf8_lossy(out);
    let mut es = Vec::new();
    for line in text.trim_end_matches('\n').split('\n') {
        if line.is_empty() {
            continue;
        }
        let malformed = || Error::other(format!("sandbox: malformed entry {line:?}"));
        let f: Vec<&str> = line.splitn(4, '\t').collect();
        if f.len() != 4 {
            return Err(malformed());
        }
        let st: Vec<&str> = f[1].split_whitespace().collect();
        if st.len() != 5 {
            return Err(malformed());
        }
        let p = if base_dir.is_empty() {
            f[3].to_string()
        } else {
            workspaces::guest_path(&format!("{base_dir}/{}", f[3]))
        };
        let mode = u32::from_str_radix(st[1], 8).unwrap_or(0) & 0o777;
        let file_type = match f[0] {
            "dir" => FileType::Dir,
            "symlink" => FileType::Symlink,
            _ => FileType::File,
        };
        let mtime: i64 = st[4].parse().unwrap_or(0);
        es.push(EntryInfo {
            name: base(&p),
            path: p,
            file_type,
            size: st[0].parse().unwrap_or(0),
            mode,
            permissions: permissions(mode),
            owner: st[2].into(),
            group: st[3].into(),
            modified_time: UNIX_EPOCH + Duration::from_secs(mtime.max(0) as u64),
            symlink_target: f[2].into(),
        });
    }
    Ok(es)
}

/// "rwxr-xr-x" for 0o755.
fn permissions(mode: u32) -> String {
    let mut s = String::with_capacity(9);
    for shift in [6, 3, 0] {
        let b = (mode >> shift) & 7;
        s.push(if b & 4 != 0 { 'r' } else { '-' });
        s.push(if b & 2 != 0 { 'w' } else { '-' });
        s.push(if b & 1 != 0 { 'x' } else { '-' });
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_entries_vectors() {
        let out = b"file\t12 644 root root 1700000000\t\ta.txt\ndir\t4096 755 root root 1700000001\t\tsub\nsymlink\t3 777 root root 1700000002\ta.txt\tlink\n";
        let es = parse_entries(out, "/work").unwrap();
        assert_eq!(es.len(), 3);
        assert_eq!(
            (es[0].path.as_str(), es[0].size, es[0].permissions.as_str()),
            ("/work/a.txt", 12, "rw-r--r--")
        );
        assert_eq!(
            (es[1].file_type, es[1].name.as_str()),
            (FileType::Dir, "sub")
        );
        assert_eq!(
            (es[2].file_type, es[2].symlink_target.as_str()),
            (FileType::Symlink, "a.txt")
        );
        assert!(parse_entries(b"file\tbad\t\tx\n", "").is_err());
    }

    #[test]
    fn guest_paths() {
        assert_eq!(guest_path(""), "/work");
        assert_eq!(guest_path("a/b"), "/work/a/b");
        assert_eq!(guest_path("/tmp/../etc"), "/etc");
        assert_eq!(guest_path("./x"), "/work/x");
    }

    #[test]
    fn write_files_tar_is_readable() {
        // The same archive layout write_files sends, read back.
        let zw = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        let mut tw = tar::Builder::new(zw);
        let mut h = tar::Header::new_gnu();
        h.set_mode(0o600);
        h.set_size(2);
        tw.append_data(
            &mut h,
            "work/very/".to_string() + &"long/".repeat(40) + "f",
            &b"hi"[..],
        )
        .unwrap();
        let tgz = tw.into_inner().unwrap().finish().unwrap();
        let mut ar = tar::Archive::new(flate2::read::GzDecoder::new(&tgz[..]));
        let e = ar.entries().unwrap().next().unwrap().unwrap();
        assert!(e.path().unwrap().to_string_lossy().ends_with("long/f"));
    }
}
