package sandbox

import (
	"archive/tar"
	"bytes"
	"compress/gzip"
	"os"
	"path/filepath"
	"strings"
	"testing"
)

func TestDocument(t *testing.T) {
	d := string(Document(&CreateOptions{Packages: []string{"python-3.13", `we"ird`}, Egress: []string{"example.com"}, EgressPolicy: "destroy"}))
	for _, want := range []string{
		"version: 1\n", "  packages:\n    - \"python-3.13\"\n    - \"we\\\"ird\"\n",
		"      egress:\n        - \"example.com\"\n", "      policy: \"destroy\"\n",
	} {
		if !strings.Contains(d, want) {
			t.Errorf("document lacks %q:\n%s", want, d)
		}
	}
	def := string(Document(nil))
	if !strings.Contains(def, `- "python-3.13"`) || !strings.Contains(def, `- "*"`) || !strings.Contains(def, `policy: "deny"`) {
		t.Errorf("default document:\n%s", def)
	}
	if !strings.Contains(string(Document(&CreateOptions{Egress: []string{}})), "egress: []") {
		t.Error("an empty allowlist must be explicit")
	}
	if got := string(Document(&CreateOptions{Document: []byte("custom")})); got != "custom" {
		t.Errorf("custom document replaced: %q", got)
	}
}

func TestGuestPathAndEnv(t *testing.T) {
	for in, want := range map[string]string{"": "/work", "a/b": "/work/a/b", "/tmp/../etc": "/etc", "./x": "/work/x"} {
		if got := guestPath(in); got != want {
			t.Errorf("guestPath(%q) = %q, want %q", in, got, want)
		}
	}
	sb := &Sandbox{envs: map[string]string{"A": "1", "B": "2"}}
	got := strings.Join(sb.envArgv(map[string]string{"B": "x y"}, []string{"cmd"}), "|")
	if got != "env|A=1|B=x y|cmd" {
		t.Errorf("envArgv = %q", got)
	}
	if got := (&Sandbox{}).envArgv(nil, []string{"cmd"}); len(got) != 1 {
		t.Errorf("no env should mean no wrapper: %q", got)
	}
}

func TestParsePoll(t *testing.T) {
	p, err := parsePoll([]byte("- 5 3\nhelloerr"), 0, 0)
	if err != nil || p.exit != nil || string(p.stdout) != "hello" || string(p.stderr) != "err" || p.outEnd != 5 || p.errEnd != 3 {
		t.Fatalf("%+v %v", p, err)
	}
	p, err = parsePoll([]byte("7 8 3\nxyz"), 5, 3)
	if err != nil || p.exit == nil || *p.exit != 7 || string(p.stdout) != "xyz" || len(p.stderr) != 0 || p.truncated {
		t.Fatalf("%+v %v", p, err)
	}
	// Fewer bytes than announced: keep what arrived, don't trust the exit yet.
	p, err = parsePoll([]byte("0 10 0\nabc"), 0, 0)
	if err != nil || !p.truncated || string(p.stdout) != "abc" || p.outEnd != 3 {
		t.Fatalf("%+v %v", p, err)
	}
	if _, err := parsePoll([]byte("garbage"), 0, 0); err == nil {
		t.Fatal("garbage accepted")
	}
}

func TestParseEntries(t *testing.T) {
	out := "file\t12 644 root root 1700000000\t\tnotes.txt\n" +
		"dir\t4096 755 root root 1700000001\t\tsub dir\n" +
		"symlink\t9 777 root root 1700000002\tnotes.txt\tlink\n"
	es, err := parseEntries([]byte(out), "/work")
	if err != nil || len(es) != 3 {
		t.Fatalf("%+v %v", es, err)
	}
	if es[0].Path != "/work/notes.txt" || es[0].Size != 12 || es[0].Permissions != "rw-r--r--" || es[0].ModifiedTime.Unix() != 1700000000 {
		t.Errorf("%+v", es[0])
	}
	if es[1].Name != "sub dir" || es[1].Type != FileTypeDir {
		t.Errorf("%+v", es[1])
	}
	if es[2].Type != FileTypeSymlink || es[2].SymlinkTarget != "notes.txt" {
		t.Errorf("%+v", es[2])
	}
}

func tgz(t *testing.T, hdrs ...tar.Header) []byte {
	var b bytes.Buffer
	zw := gzip.NewWriter(&b)
	tw := tar.NewWriter(zw)
	for _, h := range hdrs {
		h := h
		body := []byte("x")
		if h.Typeflag == tar.TypeReg {
			h.Size = 1
		}
		if err := tw.WriteHeader(&h); err != nil {
			t.Fatal(err)
		}
		if h.Typeflag == tar.TypeReg {
			tw.Write(body)
		}
	}
	tw.Close()
	zw.Close()
	return b.Bytes()
}

func TestExtractTarGzRefusesEscapes(t *testing.T) {
	ok := tgz(t, tar.Header{Name: "./a/", Typeflag: tar.TypeDir, Mode: 0o755},
		tar.Header{Name: "./a/f.txt", Typeflag: tar.TypeReg, Mode: 0o600},
		tar.Header{Name: "./a/l", Typeflag: tar.TypeSymlink, Linkname: "f.txt"})
	dir := t.TempDir()
	if err := extractTarGz(ok, dir); err != nil {
		t.Fatal(err)
	}
	if b, err := os.ReadFile(filepath.Join(dir, "a", "l")); err != nil || string(b) != "x" {
		t.Fatalf("%q %v", b, err)
	}
	for name, bad := range map[string][]byte{
		"dotdot":      tgz(t, tar.Header{Name: "../evil", Typeflag: tar.TypeReg}),
		"absolute":    tgz(t, tar.Header{Name: "/etc/evil", Typeflag: tar.TypeReg}),
		"symlink out": tgz(t, tar.Header{Name: "l", Typeflag: tar.TypeSymlink, Linkname: "../../outside"}),
		"symlink abs": tgz(t, tar.Header{Name: "l", Typeflag: tar.TypeSymlink, Linkname: "/etc/passwd"}),
	} {
		if err := extractTarGz(bad, t.TempDir()); err == nil {
			t.Errorf("%s: accepted", name)
		}
	}
}
