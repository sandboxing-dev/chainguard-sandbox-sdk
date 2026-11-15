package sandbox

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"context"
	"errors"
	"fmt"
	"io"
	"io/fs"
	"os"
	"path"
	"path/filepath"
	"strconv"
	"strings"
	"time"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// Files reads and writes a sandbox's filesystem: sb.Files. Relative paths
// resolve against /work. Every call is a small shell program run in the
// sandbox, so each costs one round trip (about 0.15 s once the sandbox is
// warm); batch with WriteFiles and UploadDir where you can.
type Files struct{ sb *Sandbox }

// FileType is "file", "dir" or "symlink".
type FileType string

const (
	FileTypeFile    FileType = "file"
	FileTypeDir     FileType = "dir"
	FileTypeSymlink FileType = "symlink"
)

// WriteInfo describes a written file.
type WriteInfo struct {
	Name string
	Path string
	Type FileType
}

// EntryInfo describes a file, directory or symlink.
type EntryInfo struct {
	Name          string
	Path          string
	Type          FileType
	Size          int64
	Mode          fs.FileMode // permission bits
	Permissions   string      // e.g. "rwxr-xr-x"
	Owner, Group  string
	ModifiedTime  time.Time
	SymlinkTarget string
}

// WriteEntry is one file for WriteFiles.
type WriteEntry struct {
	Path string
	Data []byte
	// Mode defaults to 0644.
	Mode fs.FileMode
}

// script runs a script with args and stdin, returning stdout, the exit code
// and stderr. A transport error is returned as err.
func (f *Files) script(ctx context.Context, script string, stdin []byte, args ...string) ([]byte, int, []byte, error) {
	var stdout, stderr bytes.Buffer
	argv := append([]string{"sh", "-c", script, "_"}, args...)
	res, err := f.sb.c.Exec(ctx, f.sb.Name, argv, workspaces.ExecOptions{NoDir: true, Stdin: stdin, Stdout: &stdout, Stderr: &stderr})
	if err != nil {
		return nil, 0, nil, wrapErr(err)
	}
	return stdout.Bytes(), res.ExitCode, stderr.Bytes(), nil
}

// Read returns a file's contents. A missing file returns a *NotFoundError.
func (f *Files) Read(ctx context.Context, p string) ([]byte, error) {
	gp := guestPath(p)
	out, code, stderr, err := f.script(ctx, readScript, nil, gp)
	if err != nil {
		return nil, err
	}
	if code != 0 {
		return nil, scriptError(code, gp, stderr)
	}
	return out, nil
}

// ReadText returns a file's contents as a string.
func (f *Files) ReadText(ctx context.Context, p string) (string, error) {
	b, err := f.Read(ctx, p)
	return string(b), err
}

// Write creates or replaces a file (atomically, mode 0644), creating parent
// directories. Any size works: data over 15 MiB is staged in pieces.
func (f *Files) Write(ctx context.Context, p string, data []byte) (*WriteInfo, error) {
	return f.write(ctx, p, data, 0o644)
}

func (f *Files) write(ctx context.Context, p string, data []byte, mode fs.FileMode) (*WriteInfo, error) {
	gp := guestPath(p)
	if len(data) <= maxStdinPerExec {
		if err := f.sb.c.WriteFile(ctx, f.sb.Name, gp, data, mode); err != nil {
			var ee *workspaces.ExitError
			if errors.As(err, &ee) && ee.ExitCode == 21 {
				return nil, &InvalidPathError{Path: gp, Reason: "is a directory"}
			}
			return nil, wrapErr(err)
		}
	} else {
		tmp, err := f.stage(ctx, data)
		if err != nil {
			return nil, err
		}
		_, code, stderr, err := f.script(ctx, finishScript, nil, tmp, gp, strconv.FormatUint(uint64(mode.Perm()), 8))
		if err != nil {
			return nil, err
		}
		if code != 0 {
			return nil, scriptError(code, gp, stderr)
		}
	}
	return &WriteInfo{Name: path.Base(gp), Path: gp, Type: FileTypeFile}, nil
}

// stage uploads data to a temporary file in the sandbox, in pieces under the
// exec stdin limit, and returns its path.
func (f *Files) stage(ctx context.Context, data []byte) (string, error) {
	tmp := stateDir + "/upload/" + randomID()
	for off := 0; off == 0 || off < len(data); off += maxStdinPerExec {
		end := min(off+maxStdinPerExec, len(data))
		mode := "append"
		if off == 0 {
			mode = "trunc"
		}
		_, code, stderr, err := f.script(ctx, appendScript, data[off:end], tmp, mode)
		if err != nil {
			return "", err
		}
		if code != 0 {
			return "", scriptError(code, tmp, stderr)
		}
		if end == len(data) {
			break
		}
	}
	return tmp, nil
}

// untar extracts a gzip'd tar into dir in the sandbox.
func (f *Files) untar(ctx context.Context, tgz []byte, dir string) error {
	src, stdin := "-", tgz
	if len(tgz) > maxStdinPerExec {
		tmp, err := f.stage(ctx, tgz)
		if err != nil {
			return err
		}
		src, stdin = tmp, nil
	}
	_, code, stderr, err := f.script(ctx, untarScript, stdin, src, dir)
	if err != nil {
		return err
	}
	if code != 0 {
		return scriptError(code, dir, stderr)
	}
	return nil
}

// WriteFiles writes several files in one upload (one tar), creating parent
// directories. Files are owned by root.
func (f *Files) WriteFiles(ctx context.Context, files []WriteEntry) ([]WriteInfo, error) {
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	tw := tar.NewWriter(zw)
	infos := make([]WriteInfo, 0, len(files))
	now := time.Now()
	for _, e := range files {
		gp := guestPath(e.Path)
		if gp == "/" {
			return nil, fmt.Errorf("sandbox: invalid file path %q", e.Path)
		}
		mode := e.Mode.Perm()
		if mode == 0 {
			mode = 0o644
		}
		hdr := &tar.Header{Name: strings.TrimPrefix(gp, "/"), Mode: int64(mode), Size: int64(len(e.Data)), ModTime: now, Typeflag: tar.TypeReg, Format: tar.FormatPAX}
		if err := tw.WriteHeader(hdr); err != nil {
			return nil, err
		}
		if _, err := tw.Write(e.Data); err != nil {
			return nil, err
		}
		infos = append(infos, WriteInfo{Name: path.Base(gp), Path: gp, Type: FileTypeFile})
	}
	if err := tw.Close(); err != nil {
		return nil, err
	}
	if err := zw.Close(); err != nil {
		return nil, err
	}
	if err := f.untar(ctx, buf.Bytes(), "/"); err != nil {
		return nil, err
	}
	return infos, nil
}

// List returns the entries of a directory (one level), sorted by name.
func (f *Files) List(ctx context.Context, p string) ([]EntryInfo, error) {
	gp := guestPath(p)
	out, code, stderr, err := f.script(ctx, listDirScript, nil, gp)
	if err != nil {
		return nil, err
	}
	if code != 0 {
		return nil, scriptError(code, gp, stderr)
	}
	return parseEntries(out, gp)
}

// Exists reports whether a path exists (a dangling symlink counts).
func (f *Files) Exists(ctx context.Context, p string) (bool, error) {
	_, code, _, err := f.script(ctx, existsScript, nil, guestPath(p))
	if err != nil {
		return false, err
	}
	return code == 0, nil
}

// GetInfo describes a path without following a final symlink.
func (f *Files) GetInfo(ctx context.Context, p string) (*EntryInfo, error) {
	gp := guestPath(p)
	out, code, stderr, err := f.script(ctx, infoScript, nil, gp)
	if err != nil {
		return nil, err
	}
	if code != 0 {
		return nil, scriptError(code, gp, stderr)
	}
	es, err := parseEntries(out, "")
	if err != nil || len(es) != 1 {
		return nil, fmt.Errorf("sandbox: unexpected stat output for %s: %q", gp, out)
	}
	return &es[0], nil
}

// MakeDir creates a directory and its parents. It returns false if the
// directory already existed.
func (f *Files) MakeDir(ctx context.Context, p string) (bool, error) {
	gp := guestPath(p)
	out, code, stderr, err := f.script(ctx, makeDirScript, nil, gp)
	if err != nil {
		return false, err
	}
	if code != 0 {
		return false, scriptError(code, gp, stderr)
	}
	return strings.TrimSpace(string(out)) == "1", nil
}

// Remove deletes a file or a directory tree. A missing path returns a
// *NotFoundError.
func (f *Files) Remove(ctx context.Context, p string) error {
	gp := guestPath(p)
	_, code, stderr, err := f.script(ctx, removeScript, nil, gp)
	if err != nil {
		return err
	}
	if code != 0 {
		return scriptError(code, gp, stderr)
	}
	return nil
}

// Rename moves a file or directory, creating the destination's parents, and
// describes it at its new path.
func (f *Files) Rename(ctx context.Context, oldPath, newPath string) (*EntryInfo, error) {
	op, np := guestPath(oldPath), guestPath(newPath)
	out, code, stderr, err := f.script(ctx, renameScript, nil, op, np)
	if err != nil {
		return nil, err
	}
	if code != 0 {
		return nil, scriptError(code, op, stderr)
	}
	es, err := parseEntries(out, "")
	if err != nil || len(es) != 1 {
		return nil, fmt.Errorf("sandbox: unexpected stat output for %s: %q", np, out)
	}
	return &es[0], nil
}

// Upload copies a local file into the sandbox.
func (f *Files) Upload(ctx context.Context, localPath, remotePath string) error {
	st, err := os.Stat(localPath)
	if err != nil {
		return err
	}
	data, err := os.ReadFile(localPath)
	if err != nil {
		return err
	}
	_, err = f.write(ctx, remotePath, data, st.Mode().Perm())
	return err
}

// Download copies a file from the sandbox to localPath, creating local
// parent directories.
func (f *Files) Download(ctx context.Context, remotePath, localPath string) error {
	data, err := f.Read(ctx, remotePath)
	if err != nil {
		return err
	}
	if err := os.MkdirAll(filepath.Dir(localPath), 0o755); err != nil {
		return err
	}
	return os.WriteFile(localPath, data, 0o644)
}

// UploadDir copies a local directory tree into remoteDir (created if
// missing), as one tar. Symlinks are copied as links; sockets and devices are
// skipped. Files are owned by root.
func (f *Files) UploadDir(ctx context.Context, localDir, remoteDir string) error {
	tgz, err := workspaces.TarGzDir(localDir, nil)
	if err != nil {
		return err
	}
	return f.untar(ctx, tgz, guestPath(remoteDir))
}

// DownloadDir copies a directory tree from the sandbox into localDir
// (created if missing). Nothing is written outside localDir: absolute names
// and ".." are refused, an entry is never written through a symbolic link,
// and a link that leads outside localDir (directly or through other links)
// is removed and reported as an error.
func (f *Files) DownloadDir(ctx context.Context, remoteDir, localDir string) error {
	gp := guestPath(remoteDir)
	out, code, stderr, err := f.script(ctx, tarScript, nil, gp)
	if err != nil {
		return err
	}
	if code != 0 {
		return scriptError(code, gp, stderr)
	}
	return extractTarGz(out, localDir)
}

// parseEntries decodes statFn lines; base, if set, is joined with the name.
func parseEntries(out []byte, base string) ([]EntryInfo, error) {
	var es []EntryInfo
	for _, line := range strings.Split(strings.TrimRight(string(out), "\n"), "\n") {
		if line == "" {
			continue
		}
		f := strings.SplitN(line, "\t", 4)
		if len(f) != 4 {
			return nil, fmt.Errorf("sandbox: malformed entry %q", line)
		}
		st := strings.Fields(f[1])
		if len(st) != 5 {
			return nil, fmt.Errorf("sandbox: malformed entry %q", line)
		}
		size, _ := strconv.ParseInt(st[0], 10, 64)
		mode, _ := strconv.ParseUint(st[1], 8, 32)
		mtime, _ := strconv.ParseInt(st[4], 10, 64)
		p := f[3]
		if base != "" {
			p = path.Join(base, p)
		}
		e := EntryInfo{Name: path.Base(p), Path: p, Type: FileType(f[0]), Size: size, Mode: fs.FileMode(mode).Perm(),
			Owner: st[2], Group: st[3], ModifiedTime: time.Unix(mtime, 0), SymlinkTarget: f[2]}
		e.Permissions = e.Mode.String()[1:]
		es = append(es, e)
	}
	return es, nil
}

// extractTarGz unpacks a gzip'd tar into dir. Nothing it writes can land
// outside dir, and no symbolic link it leaves behind leads outside dir:
//
//   - names that are absolute or contain ".." are refused;
//   - an entry is never written through a symbolic link (one from the archive
//     or one already in dir), so a link can't redirect later entries;
//   - once everything is unpacked, every link from the archive is followed the
//     way the kernel would follow it, link by link. One that leaves dir (also
//     through other links, in whatever order they were unpacked) is removed
//     and the extraction fails.
func extractTarGz(tgz []byte, dir string) (err error) {
	if err := os.MkdirAll(dir, 0o755); err != nil {
		return err
	}
	root, err := filepath.Abs(dir)
	if err != nil {
		return err
	}
	// The real path, so the destination itself may be reached through a link.
	if root, err = filepath.EvalSymlinks(root); err != nil {
		return err
	}
	zr, err := gzip.NewReader(bytes.NewReader(tgz))
	if err != nil {
		return err
	}
	tr := tar.NewReader(zr)
	inside := func(p string) bool {
		rel, err := filepath.Rel(root, p)
		return err == nil && rel != ".." && !strings.HasPrefix(rel, ".."+string(filepath.Separator))
	}
	var links []string
	// Also when an entry is refused partway: no escaping link stays behind.
	defer func() {
		if lerr := removeEscapingLinks(root, links); err == nil {
			err = lerr
		}
	}()
	for {
		h, err := tr.Next()
		if errors.Is(err, io.EOF) {
			return nil
		}
		if err != nil {
			return err
		}
		name := path.Clean(strings.TrimPrefix(h.Name, "./"))
		if name == "." || name == "" {
			continue
		}
		if path.IsAbs(h.Name) || name == ".." || strings.HasPrefix(name, "../") {
			return fmt.Errorf("sandbox: refusing archive entry %q outside the target", h.Name)
		}
		rel := filepath.FromSlash(name)
		target := filepath.Join(root, rel)
		if !inside(target) {
			return fmt.Errorf("sandbox: refusing archive entry %q outside the target", h.Name)
		}
		if h.Typeflag != tar.TypeDir && h.Typeflag != tar.TypeReg && h.Typeflag != tar.TypeSymlink {
			continue // hard links, devices, FIFOs: skipped
		}
		if err := realParents(root, filepath.Dir(rel)); err != nil {
			return fmt.Errorf("sandbox: refusing archive entry %q: %w", h.Name, err)
		}
		switch h.Typeflag {
		case tar.TypeDir:
			if isSymlink(target) {
				return fmt.Errorf("sandbox: refusing archive entry %q: a symbolic link is in the way", h.Name)
			}
			if err := os.MkdirAll(target, 0o755); err != nil {
				return err
			}
		case tar.TypeReg:
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			if isSymlink(target) { // replace the link; don't write to what it points at
				if err := os.Remove(target); err != nil {
					return err
				}
			}
			w, err := os.OpenFile(target, os.O_CREATE|os.O_WRONLY|os.O_TRUNC, fs.FileMode(h.Mode).Perm()|0o200)
			if err != nil {
				return err
			}
			_, err = io.Copy(w, tr)
			if cerr := w.Close(); err == nil {
				err = cerr
			}
			if err != nil {
				return err
			}
		case tar.TypeSymlink:
			if filepath.IsAbs(h.Linkname) || path.IsAbs(h.Linkname) || !inside(filepath.Join(filepath.Dir(target), filepath.FromSlash(h.Linkname))) {
				return fmt.Errorf("sandbox: refusing symlink %q -> %q pointing outside the target", h.Name, h.Linkname)
			}
			if err := os.MkdirAll(filepath.Dir(target), 0o755); err != nil {
				return err
			}
			_ = os.Remove(target)
			if err := os.Symlink(h.Linkname, target); err != nil {
				return err
			}
			links = append(links, target)
		}
	}
}

func isSymlink(p string) bool {
	fi, err := os.Lstat(p)
	return err == nil && fi.Mode()&os.ModeSymlink != 0
}

// realParents checks that every existing component of rel (a directory path
// under root) is a real directory, not a symbolic link. Components that don't
// exist yet are fine: they get created as real directories.
func realParents(root, rel string) error {
	cur := root
	for _, part := range splitPath(rel) {
		if part == "." {
			continue
		}
		cur = filepath.Join(cur, part)
		fi, err := os.Lstat(cur)
		if errors.Is(err, fs.ErrNotExist) {
			return nil
		}
		if err != nil {
			return err
		}
		if fi.Mode()&os.ModeSymlink != 0 {
			return fmt.Errorf("%q is a symbolic link", part)
		}
		if !fi.IsDir() {
			return fmt.Errorf("%q is not a directory", part)
		}
	}
	return nil
}

func splitPath(p string) []string {
	return strings.FieldsFunc(p, func(r rune) bool { return r == '/' || r == filepath.Separator })
}

// removeEscapingLinks follows each unpacked link and removes those that lead
// outside root; it fails if there were any.
func removeEscapingLinks(root string, links []string) error {
	var bad []string
	for _, l := range links {
		if isSymlink(l) && linkEscapes(root, l) {
			bad = append(bad, l)
		}
	}
	if len(bad) == 0 {
		return nil
	}
	for _, l := range bad {
		_ = os.Remove(l)
	}
	rel, _ := filepath.Rel(root, bad[0])
	return fmt.Errorf("sandbox: refusing symlink %q: following it leads outside the target", filepath.ToSlash(rel))
}

// linkEscapes reports whether following the symbolic link at p leaves root.
// It walks the link's target one component at a time, as path resolution
// does, expanding any link it meets (root-relative, so the order links were
// created in doesn't matter) and treating missing components as plain names.
func linkEscapes(root, p string) bool {
	target, err := os.Readlink(p)
	if err != nil {
		return false
	}
	if filepath.IsAbs(target) || path.IsAbs(target) {
		return true
	}
	cur, todo := filepath.Dir(p), splitPath(target)
	for hops := 0; len(todo) > 0; {
		part := todo[0]
		todo = todo[1:]
		switch part {
		case ".":
			continue
		case "..":
			if cur == root {
				return true
			}
			cur = filepath.Dir(cur)
			continue
		}
		next := filepath.Join(cur, part)
		if isSymlink(next) {
			t, err := os.Readlink(next)
			if hops++; err != nil || hops > 40 || filepath.IsAbs(t) || path.IsAbs(t) {
				return true
			}
			todo = append(splitPath(t), todo...)
			continue
		}
		cur = next
	}
	return false
}
