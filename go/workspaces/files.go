package workspaces

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
	"path/filepath"
	"strings"
)

// File helpers run small shell programs over Exec. Paths are absolute in the
// guest or relative to /work. Each transfer is bounded by MaxExecStdin.

// WriteFile atomically replaces path (creating parent directories) with data.
// It refuses an empty path and an existing directory.
func (c *Client) WriteFile(ctx context.Context, name, path string, data []byte, perm fs.FileMode) error {
	if strings.TrimSpace(path) == "" {
		return errors.New("workspaces: WriteFile needs a file path")
	}
	const script = `p=$1; if [ -d "$p" ]; then echo "$p: is a directory" >&2; cat >/dev/null; exit 21; fi; ` +
		`d=$(dirname -- "$p") && mkdir -p -- "$d" && t="$d/.chainguard-sandbox-write.$$" && cat > "$t" && chmod "$2" "$t" && mv -f -- "$t" "$p" || { rm -f -- "$t"; exit 1; }`
	_, err := c.Shell(ctx, name, script, ExecOptions{Stdin: data}, guestPath(path), fmt.Sprintf("%o", perm.Perm()))
	return err
}

// TarDir writes dir as an uncompressed tar stream, using the same rules
// chainctl documents for its workspace upload: symlinks travel as links;
// sockets, devices and FIFOs are skipped. skip, if set, excludes a path
// (relative, slash-separated) and, for a directory, everything under it.
//
// A symlinked dir is resolved first. A file that changes size while it is
// being written is truncated or zero-padded to the size in its header, rather
// than aborting the tar (logs and editor swap files do this).
func TarDir(w io.Writer, dir string, skip func(rel string, d fs.DirEntry) bool) error {
	root, err := filepath.EvalSymlinks(dir)
	if err != nil {
		return err
	}
	if fi, err := os.Stat(root); err != nil {
		return err
	} else if !fi.IsDir() {
		return fmt.Errorf("%s: not a directory", dir)
	}
	dir = root
	tw := tar.NewWriter(w)
	err = filepath.WalkDir(dir, func(p string, d fs.DirEntry, err error) error {
		if err != nil {
			return err
		}
		rel, err := filepath.Rel(dir, p)
		if err != nil || rel == "." {
			return err
		}
		rel = filepath.ToSlash(rel)
		if skip != nil && skip(rel, d) {
			if d.IsDir() {
				return filepath.SkipDir
			}
			return nil
		}
		info, err := d.Info()
		if err != nil {
			return err
		}
		mode := info.Mode()
		if mode&(fs.ModeSocket|fs.ModeDevice|fs.ModeNamedPipe|fs.ModeCharDevice) != 0 {
			return nil
		}
		link := ""
		if mode&fs.ModeSymlink != 0 {
			if link, err = os.Readlink(p); err != nil {
				return err
			}
		}
		hdr, err := tar.FileInfoHeader(info, link)
		if err != nil {
			return err
		}
		// Root-owned, like chainctl's upload: entries carrying the laptop's
		// uid make git in the guest refuse /work ("dubious ownership").
		hdr.Uid, hdr.Gid, hdr.Uname, hdr.Gname = 0, 0, "", ""
		hdr.Name = rel
		if d.IsDir() {
			hdr.Name += "/"
		}
		if err := tw.WriteHeader(hdr); err != nil {
			return err
		}
		if !mode.IsRegular() {
			return nil
		}
		return copyFileExact(tw, p, hdr.Size)
	})
	if err != nil {
		return err
	}
	return tw.Close()
}

// TarGzDir is TarDir, gzip'd, in memory (sandbox's UploadDir sends it).
func TarGzDir(dir string, skip func(rel string, d fs.DirEntry) bool) ([]byte, error) {
	var buf bytes.Buffer
	zw := gzip.NewWriter(&buf)
	if err := TarDir(zw, dir, skip); err != nil {
		return nil, err
	}
	if err := zw.Close(); err != nil {
		return nil, err
	}
	return buf.Bytes(), nil
}

// copyFileExact writes exactly size bytes of file p to tw: a file that grew
// is truncated to its header size, one that shrank is zero-padded.
func copyFileExact(tw *tar.Writer, p string, size int64) error {
	f, err := os.Open(p)
	if err != nil {
		return err
	}
	defer f.Close()
	n, err := io.CopyN(tw, f, size)
	if errors.Is(err, io.EOF) {
		_, err = io.CopyN(tw, zeroReader{}, size-n)
	}
	return err
}

type zeroReader struct{}

func (zeroReader) Read(p []byte) (int, error) {
	clear(p)
	return len(p), nil
}
