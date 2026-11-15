package workspaces

import (
	"archive/tar"
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"net"
	"net/http"
	"net/http/httptest"
	"os"
	"path/filepath"
	"runtime"
	"strings"
	"sync"
	"sync/atomic"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/metadata"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

// fakeSessions is an in-process stand-in for the plane. It checks only what
// this client sends; it says nothing about the real service's behavior.
type fakeSessions struct {
	sessionv1.UnimplementedSessionsServer

	mu       sync.Mutex
	busyHits int
	execs    []*sessionv1.ExecRequest
	updates  []*sessionv1.UpdateSessionRequest
	// attachFn, when set, scripts AttachSession; extra sessions are listed on page 2.
	attachFn func(attachSrv) error
	extra    []*sessionv1.Session
}

type attachSrv = grpc.BidiStreamingServer[sessionv1.AttachRequest, sessionv1.AttachResponse]

func authorized(ctx context.Context) error {
	md, _ := metadata.FromIncomingContext(ctx)
	if got := md.Get("authorization"); len(got) != 1 || got[0] != "Bearer test-token" {
		return status.Errorf(codes.Unauthenticated, "authorization = %v", got)
	}
	return nil
}

func (f *fakeSessions) ListSessions(ctx context.Context, r *sessionv1.ListSessionsRequest) (*sessionv1.ListSessionsResponse, error) {
	if err := authorized(ctx); err != nil {
		return nil, err
	}
	if r.GetParent() != "instances/org1" {
		return nil, status.Errorf(codes.InvalidArgument, "parent %q", r.GetParent())
	}
	if r.GetPageToken() == "" {
		return &sessionv1.ListSessionsResponse{
			Sessions:      []*sessionv1.Session{{Name: "instances/org1/sessions/a", ShortId: "3f9a2c1e", DisplayName: "app/quiet-harbor"}},
			NextPageToken: "p2",
		}, nil
	}
	return &sessionv1.ListSessionsResponse{
		Sessions: append([]*sessionv1.Session{{Name: "instances/org1/sessions/b", ShortId: "77aa0011", DisplayName: "app/scratch"}}, f.extra...),
	}, nil
}

func (f *fakeSessions) UpdateSession(_ context.Context, r *sessionv1.UpdateSessionRequest) (*sessionv1.Session, error) {
	f.mu.Lock()
	f.updates = append(f.updates, r)
	f.mu.Unlock()
	return r.GetSession(), nil
}

func (f *fakeSessions) Exec(r *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
	if err := authorized(s.Context()); err != nil {
		return err
	}
	f.mu.Lock()
	f.execs = append(f.execs, r)
	f.mu.Unlock()
	argv := r.GetArgv()
	if len(argv) > 5 && argv[0] == "sh" && argv[2] == cdScript {
		argv = argv[5:]
	}
	if argv[0] == "busy-once" {
		f.mu.Lock()
		f.busyHits++
		hits := f.busyHits
		f.mu.Unlock()
		if hits == 1 {
			return status.Error(codes.FailedPrecondition, "exec: reattaching guest: k8s: reattach: microvm: instance held by another controller: rpc error: code = FailedPrecondition desc = supervisor: Channel already in use")
		}
		return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{Code: 0}}})
	}
	switch argv[0] {
	case "cat":
		_ = s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Stdout{Stdout: []byte("contents of " + argv[len(argv)-1])}})
		return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{Code: 0}}})
	case "false":
		_ = s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Stderr{Stderr: []byte("nope")}})
		return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{Code: 3}}})
	case "vanish":
		return nil
	}
	return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{Code: 0}}})
}

func (f *fakeSessions) AttachSession(s attachSrv) error {
	if f.attachFn != nil {
		return f.attachFn(s)
	}
	return status.Error(codes.Unimplemented, "no attach script")
}

func newTestClient(t *testing.T) (*Client, *fakeSessions) {
	t.Helper()
	lis := bufconn.Listen(1 << 20)
	srv := grpc.NewServer()
	fake := &fakeSessions{}
	sessionv1.RegisterSessionsServer(srv, fake)
	go srv.Serve(lis)
	t.Cleanup(srv.Stop)
	conn, err := grpc.NewClient("passthrough:///bufnet",
		grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return lis.DialContext(ctx) }),
		grpc.WithTransportCredentials(insecure.NewCredentials()),
		grpc.WithPerRPCCredentials(bearer{ts: StaticToken("test-token")}),
	)
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	return NewFromConn(conn, "org1"), fake
}

func settleGoroutines(max int) int {
	n := runtime.NumGoroutine()
	for i := 0; i < 100 && n > max; i++ {
		time.Sleep(20 * time.Millisecond)
		n = runtime.NumGoroutine()
	}
	return n
}

func TestListAndResolve(t *testing.T) {
	c, _ := newTestClient(t)
	ctx := context.Background()
	all, err := c.List(ctx, ListOptions{})
	if err != nil || len(all) != 2 {
		t.Fatalf("List = %v, %v; want 2 sessions across pages", all, err)
	}
	for ref, want := range map[string]string{
		"3f9a2c1e":         "instances/org1/sessions/a",
		"quiet-harbor":     "instances/org1/sessions/a",
		"app/scratch":      "instances/org1/sessions/b",
		"acme/app/scratch": "instances/org1/sessions/b",
	} {
		s, err := c.Resolve(ctx, ref)
		if err != nil || s.GetName() != want {
			t.Errorf("Resolve(%q) = %v, %v; want %s", ref, s.GetName(), err, want)
		}
	}
	if _, err := c.Resolve(ctx, "nothing"); !errors.Is(err, ErrNotFound) {
		t.Errorf("Resolve(nothing) err = %v; want ErrNotFound", err)
	}
}

func TestExec(t *testing.T) {
	c, fake := newTestClient(t)
	ctx := context.Background()

	out, err := c.Output(ctx, "instances/org1/sessions/a", []string{"cat", "--", "/work/notes.md"}, ExecOptions{})
	if err != nil || string(out) != "contents of /work/notes.md" {
		t.Fatalf("Output(cat) = %q, %v", out, err)
	}

	_, err = c.Output(ctx, "instances/org1/sessions/a", []string{"false"}, ExecOptions{})
	var ee *ExitError
	if !errors.As(err, &ee) || ee.ExitCode != 3 || string(ee.Stderr) != "nope" {
		t.Fatalf("Output(false) err = %v; want ExitError code 3", err)
	}

	res, err := c.Exec(ctx, "instances/org1/sessions/a", []string{"vanish"}, ExecOptions{})
	if !errors.Is(err, ErrOutcomeUnknown) || res.ExitCode != 255 {
		t.Fatalf("Exec(vanish) = %v, %v; want 255/ErrOutcomeUnknown", res, err)
	}

	_, err = c.Exec(ctx, "instances/org1/sessions/a", []string{"go", "test"}, ExecOptions{Dir: "sub", Timeout: time.Minute})
	if err != nil {
		t.Fatal(err)
	}
	last := fake.execs[len(fake.execs)-1]
	want := []string{"sh", "-c", `cd "$1" && shift && exec "$@"`, "_", "/work/sub", "go", "test"}
	if strings.Join(last.GetArgv(), "\x00") != strings.Join(want, "\x00") || last.GetDeadline().AsDuration() != time.Minute {
		t.Fatalf("exec argv = %q deadline %v", last.GetArgv(), last.GetDeadline())
	}

	_, _ = c.Exec(ctx, "instances/org1/sessions/a", []string{"true"}, ExecOptions{})
	if got := fake.execs[len(fake.execs)-1].GetArgv(); got[4] != "/work" {
		t.Fatalf("default dir argv = %q; want cd to /work", got)
	}
	_, _ = c.Exec(ctx, "instances/org1/sessions/a", []string{"true"}, ExecOptions{NoDir: true})
	if got := fake.execs[len(fake.execs)-1].GetArgv(); len(got) != 1 {
		t.Fatalf("NoDir argv = %q; want unwrapped", got)
	}

	if _, err := c.Exec(ctx, "x", []string{"cat"}, ExecOptions{Stdin: make([]byte, MaxExecStdin+1)}); err == nil {
		t.Fatal("oversized stdin was accepted")
	}
}

func TestParkUsesFieldMask(t *testing.T) {
	c, fake := newTestClient(t)
	if _, err := c.Park(context.Background(), "instances/org1/sessions/a"); err != nil {
		t.Fatal(err)
	}
	u := fake.updates[0]
	if u.GetUpdateMask().GetPaths()[0] != "park_request" || u.GetSession().GetParkRequest() != sessionv1.Session_PARK_REQUEST_PARK {
		t.Fatalf("update = %v", u)
	}
}

func TestJWTExpiry(t *testing.T) {
	// header {"alg":"none"}, payload {"exp":2000000000}
	exp, ok := jwtExpiry("eyJhbGciOiJub25lIn0.eyJleHAiOjIwMDAwMDAwMDB9.x")
	if !ok || exp.Unix() != 2000000000 {
		t.Fatalf("jwtExpiry = %v, %v", exp, ok)
	}
}

func TestIsGuestHeld(t *testing.T) {
	held := status.Error(codes.Aborted, `"instances/x/sessions/y": busy: the guest is held elsewhere (last claimed by a shell attach, still heartbeating) and was not let go within 10s; nothing ran`)
	if !IsGuestHeld(held) || IsGuestHeld(status.Error(codes.Aborted, "other")) || IsGuestHeld(errors.New("held")) {
		t.Fatal("IsGuestHeld misclassifies")
	}
}

func TestRepositorySession(t *testing.T) {
	for _, in := range []string{"github.com/acme/app", "https://github.com/acme/app.git", "git@github.com:acme/app.git"} {
		s := RepositorySession(in, "")
		if got := s.GetSource().GetRepository(); got != "https://github.com/acme/app" {
			t.Errorf("RepositorySession(%q).repository = %q", in, got)
		}
		if !bytes.Equal(s.GetSource().GetFallbackDocument(), DefaultEnvironment) || s.GetIdle() != sessionv1.IdlePolicy_AUTO_HOLD {
			t.Errorf("RepositorySession(%q) = %v; want chainctl's shape", in, s)
		}
	}
}

func TestWorkspaceSessionDefault(t *testing.T) {
	s := WorkspaceSession("/tmp/dir", "", "", nil)
	if s.GetIdle() != sessionv1.IdlePolicy_AUTO_HOLD || s.GetWorkspace().GetProvenance() != sessionv1.Provenance_SYNTHESIZED || !bytes.Contains(s.GetWorkspace().GetDocument(), []byte("develop:")) {
		t.Fatalf("WorkspaceSession default = %v", s)
	}
}

func TestIdentityExchange(t *testing.T) {
	var gotAuth, gotAud, gotIdentity, gotMethod string
	srv := httptest.NewServer(http.HandlerFunc(func(w http.ResponseWriter, r *http.Request) {
		if r.URL.Path != "/sts/exchange" {
			http.NotFound(w, r)
			return
		}
		gotMethod, gotAuth = r.Method, r.Header.Get("Authorization")
		gotAud, gotIdentity = r.URL.Query().Get("aud"), r.URL.Query().Get("identity")
		if gotAuth != "Bearer external-oidc" {
			w.WriteHeader(http.StatusUnauthorized)
			fmt.Fprint(w, `{"code":16,"message":"bad token"}`)
			return
		}
		fmt.Fprint(w, `{"token":"chainguard-token","expiry":"2030-01-01T00:00:00Z"}`)
	}))
	defer srv.Close()

	x := IdentityExchange{
		Identity: "org/abc123",
		Issuer:   srv.URL,
		IDToken:  func(context.Context) (string, error) { return "external-oidc", nil },
	}
	tok, err := x.Token(context.Background())
	if err != nil || tok != "chainguard-token" {
		t.Fatalf("Token = %q, %v", tok, err)
	}
	if gotMethod != http.MethodPost || gotAud != DefaultAudience || gotIdentity != "org/abc123" {
		t.Fatalf("request: %s aud=%q identity=%q", gotMethod, gotAud, gotIdentity)
	}

	x.IDToken = func(context.Context) (string, error) { return "wrong", nil }
	if _, err := x.Token(context.Background()); err == nil || !strings.Contains(err.Error(), "bad token") {
		t.Fatalf("expected STS error, got %v", err)
	}

	f := filepath.Join(t.TempDir(), "tok")
	os.WriteFile(f, []byte("from-file\n"), 0o600)
	if v, err := FileIDToken(f)(context.Background()); err != nil || v != "from-file" {
		t.Fatalf("FileIDToken = %q, %v", v, err)
	}
	t.Setenv("CI_ID_TOKEN", "from-env")
	if v, err := EnvIDToken("CI_ID_TOKEN")(context.Background()); err != nil || v != "from-env" {
		t.Fatalf("EnvIDToken = %q, %v", v, err)
	}
}

func TestExecRetriesWhileControllerBusy(t *testing.T) {
	c, fake := newTestClient(t)
	start := time.Now()
	res, err := c.Exec(context.Background(), "instances/org1/sessions/a", []string{"busy-once"}, ExecOptions{})
	if err != nil || res.ExitCode != 0 || fake.busyHits != 2 {
		t.Fatalf("Exec = %+v, %v, hits=%d", res, err, fake.busyHits)
	}
	if time.Since(start) < 2*time.Second {
		t.Fatalf("retried without backoff")
	}
	busy := status.Error(codes.FailedPrecondition, "instance held by another controller")
	if !IsGuestHeld(busy) {
		t.Fatal("IsGuestHeld should match the controller-busy FailedPrecondition")
	}
}

func TestExecReleasesStreams(t *testing.T) {
	c, _ := newTestClient(t)
	ctx := context.Background()
	if _, err := c.Exec(ctx, "instances/org1/sessions/a", []string{"true"}, ExecOptions{}); err != nil { // warm up
		t.Fatal(err)
	}
	before := settleGoroutines(0)
	for i := 0; i < 30; i++ {
		if _, err := c.Exec(ctx, "instances/org1/sessions/a", []string{"true"}, ExecOptions{}); err != nil {
			t.Fatal(err)
		}
	}
	if after := settleGoroutines(before + 5); after > before+5 {
		t.Fatalf("goroutines grew from %d to %d over 30 calls: streams leak", before, after)
	}
}

func TestCopyFileExactToleratesSizeChanges(t *testing.T) {
	p := filepath.Join(t.TempDir(), "f")
	os.WriteFile(p, []byte("12345"), 0o644)
	for _, size := range []int64{3, 8} { // grew past / shrank below the header size
		var buf bytes.Buffer
		tw := tar.NewWriter(&buf)
		if err := tw.WriteHeader(&tar.Header{Name: "f", Mode: 0o644, Size: size}); err != nil {
			t.Fatal(err)
		}
		if err := copyFileExact(tw, p, size); err != nil {
			t.Fatalf("size %d: %v", size, err)
		}
		if err := tw.Close(); err != nil {
			t.Fatalf("size %d: %v", size, err)
		}
		tr := tar.NewReader(&buf)
		if _, err := tr.Next(); err != nil {
			t.Fatal(err)
		}
		b, _ := io.ReadAll(tr)
		if int64(len(b)) != size {
			t.Fatalf("size %d: got %d bytes", size, len(b))
		}
	}
}

func TestResolveExactWinsOverSuffix(t *testing.T) {
	c, fake := newTestClient(t)
	fake.extra = []*sessionv1.Session{{Name: "instances/org1/sessions/c", ShortId: "aaaa1111", DisplayName: "other/3f9a2c1e"}}
	s, err := c.Resolve(context.Background(), "3f9a2c1e")
	if err != nil || s.GetName() != "instances/org1/sessions/a" {
		t.Fatalf("Resolve(short id) = %v, %v; the exact short-ID match must win", s.GetName(), err)
	}
	if _, err := c.Resolve(context.Background(), "x/y/quiet-harbor"); !errors.Is(err, ErrNotFound) {
		t.Fatalf("Resolve(x/y/quiet-harbor) err = %v; want ErrNotFound", err)
	}
}

func TestWriteFileRefusesDirsAndEmptyPaths(t *testing.T) {
	c, fake := newTestClient(t)
	if err := c.WriteFile(context.Background(), "instances/org1/sessions/a", "", nil, 0o644); err == nil {
		t.Fatal("empty path accepted")
	}
	_ = c.WriteFile(context.Background(), "instances/org1/sessions/a", "notes.md", []byte("x"), 0o644)
	fake.mu.Lock()
	defer fake.mu.Unlock()
	if argv := strings.Join(fake.execs[len(fake.execs)-1].GetArgv(), " "); !strings.Contains(argv, `[ -d "$p" ]`) {
		t.Fatalf("WriteFile script lacks the directory check: %s", argv)
	}
}

func TestCachedTokenSharesFetchAndInvalidates(t *testing.T) {
	var calls atomic.Int64
	release := make(chan struct{})
	src := TokenFunc(func(ctx context.Context) (string, error) {
		calls.Add(1)
		<-release
		return "tok", nil
	})
	ts := Cached(src)
	short, cancel := context.WithTimeout(context.Background(), 20*time.Millisecond)
	defer cancel()
	if _, err := ts.Token(short); !errors.Is(err, context.DeadlineExceeded) {
		t.Fatalf("a waiter must honor its own ctx; err = %v", err)
	}
	var wg sync.WaitGroup
	for i := 0; i < 5; i++ {
		wg.Add(1)
		go func() { defer wg.Done(); ts.Token(context.Background()) }()
	}
	time.Sleep(20 * time.Millisecond)
	close(release)
	wg.Wait()
	if n := calls.Load(); n != 1 {
		t.Fatalf("source called %d times; want one shared fetch", n)
	}
	ts.(Invalidator).Invalidate()
	ts.Token(context.Background())
	if n := calls.Load(); n != 2 {
		t.Fatalf("after Invalidate the source was called %d times; want a refetch", n)
	}
}
