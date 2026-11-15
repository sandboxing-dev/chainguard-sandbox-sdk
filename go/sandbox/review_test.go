package sandbox

import (
	"archive/tar"
	"context"
	"errors"
	"net"
	"os"
	"path/filepath"
	"strconv"
	"strings"
	"sync"
	"testing"
	"time"

	"google.golang.org/grpc"
	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/credentials/insecure"
	"google.golang.org/grpc/status"
	"google.golang.org/grpc/test/bufconn"
	"google.golang.org/protobuf/types/known/emptypb"
	"google.golang.org/protobuf/types/known/timestamppb"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// fakePlane is an in-process stand-in for the plane. It checks only what this
// package sends; it says nothing about the real service's behavior.
type fakePlane struct {
	sessionv1.UnimplementedSessionsServer

	mu        sync.Mutex
	deleted   []string
	attaches  int
	createErr error
	get       func(ctx context.Context, name string) (*sessionv1.Session, error)
	exec      func(r *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error
}

const fakeName = "instances/org1/sessions/0123456789abcdef"

func (f *fakePlane) CreateSession(context.Context, *sessionv1.CreateSessionRequest) (*sessionv1.Session, error) {
	if f.createErr != nil {
		return nil, f.createErr
	}
	return &sessionv1.Session{Name: fakeName, ShortId: "01234567"}, nil
}

func (f *fakePlane) GetSession(ctx context.Context, r *sessionv1.GetSessionRequest) (*sessionv1.Session, error) {
	return f.get(ctx, r.GetName())
}

func (f *fakePlane) DeleteSession(_ context.Context, r *sessionv1.DeleteSessionRequest) (*emptypb.Empty, error) {
	f.mu.Lock()
	defer f.mu.Unlock()
	f.deleted = append(f.deleted, r.GetName())
	return &emptypb.Empty{}, nil
}

func (f *fakePlane) AttachSession(grpc.BidiStreamingServer[sessionv1.AttachRequest, sessionv1.AttachResponse]) error {
	f.mu.Lock()
	f.attaches++
	f.mu.Unlock()
	return status.Error(codes.Unavailable, "the guest cannot be attached")
}

func (f *fakePlane) Exec(r *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
	return f.exec(r, s)
}

func (f *fakePlane) deletedNames() []string {
	f.mu.Lock()
	defer f.mu.Unlock()
	return append([]string(nil), f.deleted...)
}

func newFakePlane(t *testing.T) (*workspaces.Client, *fakePlane) {
	t.Helper()
	lis := bufconn.Listen(1 << 20)
	srv := grpc.NewServer()
	fake := &fakePlane{}
	sessionv1.RegisterSessionsServer(srv, fake)
	go srv.Serve(lis)
	t.Cleanup(srv.Stop)
	conn, err := grpc.NewClient("passthrough:///bufnet",
		grpc.WithContextDialer(func(ctx context.Context, _ string) (net.Conn, error) { return lis.DialContext(ctx) }),
		grpc.WithTransportCredentials(insecure.NewCredentials()))
	if err != nil {
		t.Fatal(err)
	}
	t.Cleanup(func() { conn.Close() })
	return workspaces.NewFromConn(conn, "org1"), fake
}

// shorten sets a timing variable for one test.
func shorten(t *testing.T, v *time.Duration, d time.Duration) {
	old := *v
	*v = d
	t.Cleanup(func() { *v = old })
}

func stdout(s string) *sessionv1.ExecResponse {
	return &sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Stdout{Stdout: []byte(s)}}
}

// 1. An archive can't write, or leave a link, outside the destination.
func TestExtractTarGzSymlinksCannotEscape(t *testing.T) {
	link := func(name, to string) tar.Header {
		return tar.Header{Name: name, Typeflag: tar.TypeSymlink, Linkname: to}
	}
	file := func(name string) tar.Header { return tar.Header{Name: name, Typeflag: tar.TypeReg, Mode: 0o644} }
	for name, entries := range map[string][]tar.Header{
		// Lexically "a/../outside" stays inside; followed, a is "." and it is a sibling of the target.
		"write through a link chain": {link("a", "."), link("b", "a/../outside"), file("b/proof")},
		"link chain only":            {link("a", "."), link("b", "a/../outside")},
		"link chain, other order":    {link("b", "a/../outside"), link("a", ".")},
		"write through a plain link": {link("d", "."), file("d/../../proof")},
		"file under a link":          {tar.Header{Name: "real/", Typeflag: tar.TypeDir, Mode: 0o755}, link("l", "real"), file("l/f")},
	} {
		parent := t.TempDir()
		dir := filepath.Join(parent, "target")
		if err := os.Mkdir(filepath.Join(parent, "outside"), 0o755); err != nil {
			t.Fatal(err)
		}
		if err := extractTarGz(tgz(t, entries...), dir); err == nil {
			t.Errorf("%s: accepted", name)
		}
		if got, _ := os.ReadDir(filepath.Join(parent, "outside")); len(got) != 0 {
			t.Errorf("%s: wrote outside the target: %v", name, got)
		}
		if _, err := os.Lstat(filepath.Join(parent, "proof")); err == nil {
			t.Errorf("%s: wrote next to the target", name)
		}
		if _, err := os.Lstat(filepath.Join(dir, "b")); err == nil {
			t.Errorf("%s: left the escaping link in place", name)
		}
	}

	// A link already in the destination is not written through either.
	parent := t.TempDir()
	dir, elsewhere := filepath.Join(parent, "target"), filepath.Join(parent, "elsewhere")
	for _, d := range []string{dir, elsewhere} {
		if err := os.Mkdir(d, 0o755); err != nil {
			t.Fatal(err)
		}
	}
	if err := os.Symlink(elsewhere, filepath.Join(dir, "sub")); err != nil {
		t.Fatal(err)
	}
	if err := extractTarGz(tgz(t, file("sub/f")), dir); err == nil {
		t.Error("wrote through a link that was already in the destination")
	}
	if got, _ := os.ReadDir(elsewhere); len(got) != 0 {
		t.Errorf("wrote through the existing link: %v", got)
	}

	// Links that stay inside are kept, including ones through other links.
	dir = t.TempDir()
	ok := tgz(t, tar.Header{Name: "a/", Typeflag: tar.TypeDir, Mode: 0o755}, file("a/f.txt"), link("a/up", ".."), link("l", "a/up/a/f.txt"), link("dangling", "a/missing"))
	if err := extractTarGz(ok, dir); err != nil {
		t.Fatal(err)
	}
	if b, err := os.ReadFile(filepath.Join(dir, "l")); err != nil || string(b) != "x" {
		t.Fatalf("link inside the target: %q %v", b, err)
	}
}

// 2. A sandbox that fails to start is deleted (by its name, not an empty one).
func TestCreateDeletesTheSessionWhenStartupFails(t *testing.T) {
	c, fake := newFakePlane(t)
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		return nil, status.Error(codes.Internal, "boom")
	}
	if _, err := Create(context.Background(), &CreateOptions{Client: c}); err == nil {
		t.Fatal("Create succeeded")
	}
	if got := fake.deletedNames(); len(got) != 1 || got[0] != fakeName {
		t.Fatalf("deleted %q; want the created session", got)
	}
}

// 3. Kill by ID deletes without attaching, so it works for a sandbox that can't be attached to.
func TestKillByIDDoesNotAttach(t *testing.T) {
	c, fake := newFakePlane(t)
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		if len(fake.deletedNames()) > 0 {
			return nil, status.Error(codes.NotFound, "gone")
		}
		return &sessionv1.Session{Name: fakeName, ShortId: "01234567", WorkspacePending: true}, nil
	}
	found, err := Kill(context.Background(), fakeName, &ConnectOptions{Client: c})
	if err != nil || !found {
		t.Fatalf("Kill = %v, %v", found, err)
	}
	if got := fake.deletedNames(); len(got) != 1 || got[0] != fakeName || fake.attaches != 0 {
		t.Fatalf("deleted %q after %d attaches; want one delete and no attach", got, fake.attaches)
	}
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		return nil, status.Error(codes.NotFound, "gone")
	}
	if found, err := Kill(context.Background(), fakeName, &ConnectOptions{Client: c}); err != nil || found {
		t.Fatalf("Kill of a missing sandbox = %v, %v; want false, nil", found, err)
	}
}

// 4. The start timeout cuts off a stalled call, and a late "started" isn't a success.
func TestStartTimeoutBoundsEachCall(t *testing.T) {
	c, fake := newFakePlane(t)
	fake.get = func(ctx context.Context, _ string) (*sessionv1.Session, error) {
		select { // answers "started", but only after the limit
		case <-time.After(2 * time.Second):
		case <-ctx.Done():
		}
		return &sessionv1.Session{Name: fakeName, GuestStartedTime: timestamppb.Now()}, nil
	}
	t0 := time.Now()
	_, err := Create(context.Background(), &CreateOptions{Client: c, StartTimeout: 50 * time.Millisecond})
	var to *TimeoutError
	if !errors.As(err, &to) || to.Op != "start" {
		t.Fatalf("Create = %v; want a start TimeoutError", err)
	}
	if d := time.Since(t0); d > time.Second {
		t.Fatalf("Create took %s with a 50ms start timeout", d)
	}
	if got := fake.deletedNames(); len(got) != 1 || got[0] != fakeName {
		t.Fatalf("deleted %q; want the created session", got)
	}
}

// 5. Kill doesn't report success while the session still exists.
func TestKillReportsUnconfirmedDeletion(t *testing.T) {
	shorten(t, &killWait, 150*time.Millisecond)
	shorten(t, &killPoll, 10*time.Millisecond)
	c, fake := newFakePlane(t)
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		return &sessionv1.Session{Name: fakeName}, nil // never goes away
	}
	sb := newSandbox(c, false, &sessionv1.Session{Name: fakeName, ShortId: "01234567"}, nil)
	found, err := sb.Kill(context.Background())
	var to *TimeoutError
	if !found || !errors.As(err, &to) || to.Op != "kill" {
		t.Fatalf("Kill = %v, %v; want true and a kill TimeoutError", found, err)
	}
	// Checks that fail are not taken as "gone" either.
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		return nil, status.Error(codes.Internal, "boom")
	}
	if _, err := sb.Kill(context.Background()); !errors.As(err, &to) || !strings.Contains(err.Error(), "boom") {
		t.Fatalf("Kill with failing checks = %v; want a TimeoutError naming the failure", err)
	}
	// And it still returns as soon as the session is gone.
	fake.get = func(context.Context, string) (*sessionv1.Session, error) {
		return nil, status.Error(codes.NotFound, "gone")
	}
	if found, err := sb.Kill(context.Background()); !found || err != nil {
		t.Fatalf("Kill = %v, %v; want true, nil", found, err)
	}
}

// 6. A connection that fails after the command produced output leaves the outcome unknown.
func TestInterruptedCommandIsOutcomeUnknown(t *testing.T) {
	c, fake := newFakePlane(t)
	sb := newSandbox(c, false, &sessionv1.Session{Name: fakeName, ShortId: "01234567"}, nil)
	fake.exec = func(_ *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
		_ = s.Send(stdout("partial"))
		return status.Error(codes.Unavailable, "connection lost")
	}
	res, err := sb.Commands.Run(context.Background(), "do-something", nil)
	if !errors.Is(err, ErrOutcomeUnknown) {
		t.Fatalf("Run = %v; want ErrOutcomeUnknown", err)
	}
	if status.Code(err) != codes.Unavailable || !strings.Contains(err.Error(), "connection lost") {
		t.Fatalf("the transport error is not preserved: %v (code %s)", err, status.Code(err))
	}
	if res == nil || res.Stdout != "partial" {
		t.Fatalf("partial output lost: %+v", res)
	}
	// The plane refusing the command is an answer, not an unknown outcome.
	fake.exec = func(*sessionv1.ExecRequest, grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
		return status.Error(codes.InvalidArgument, "bad argv")
	}
	if _, err := sb.Commands.Run(context.Background(), "x", nil); errors.Is(err, ErrOutcomeUnknown) || status.Code(err) != codes.InvalidArgument {
		t.Fatalf("a refusal became %v", err)
	}
}

// 7. A character split across two chunks reaches the callback whole.
func TestCallbacksGetWholeCharacters(t *testing.T) {
	c, fake := newFakePlane(t)
	sb := newSandbox(c, false, &sessionv1.Session{Name: fakeName, ShortId: "01234567"}, nil)
	euro := "€" // three bytes
	fake.exec = func(_ *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
		_ = s.Send(stdout("a" + euro[:1]))
		_ = s.Send(stdout(euro[1:2]))
		_ = s.Send(stdout(euro[2:] + "b"))
		return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{}}})
	}
	var got []string
	res, err := sb.Commands.Run(context.Background(), "x", &RunOptions{OnStdout: func(s string) { got = append(got, s) }})
	if err != nil || res.Stdout != "a€b" {
		t.Fatalf("Run = %+v, %v", res, err)
	}
	if strings.Join(got, "|") != "a|€b" {
		t.Fatalf("callbacks got %q; want the euro sign whole", got)
	}
	// An incomplete character at the very end is still delivered.
	var tail []string
	ts := &textStream{fn: func(s string) { tail = append(tail, s) }}
	ts.write([]byte("x" + euro[:2]))
	ts.flush()
	if strings.Join(tail, "|") != "x|"+euro[:2] {
		t.Fatalf("flush delivered %q", tail)
	}
}

// 8. Overlapping polls don't fetch the same output twice.
func TestConcurrentPollsDoNotDuplicateOutput(t *testing.T) {
	c, fake := newFakePlane(t)
	sb := newSandbox(c, false, &sessionv1.Session{Name: fakeName, ShortId: "01234567"}, nil)
	const content = "abc"
	fake.exec = func(r *sessionv1.ExecRequest, s grpc.ServerStreamingServer[sessionv1.ExecResponse]) error {
		argv := r.GetArgv() // sh -c <poll script> _ <dir> <outOff> <errOff> <wait> <max>
		off, _ := strconv.Atoi(argv[5])
		time.Sleep(100 * time.Millisecond) // long enough for the polls to overlap
		_ = s.Send(stdout("- " + strconv.Itoa(len(content)) + " 0\n" + content[off:]))
		return s.Send(&sessionv1.ExecResponse{Event: &sessionv1.ExecResponse_Exited_{Exited: &sessionv1.ExecResponse_Exited{}}})
	}
	h := &CommandHandle{PID: 7, ID: "x", dir: "/tmp/x", sb: sb}
	var wg sync.WaitGroup
	for i := 0; i < 4; i++ {
		wg.Add(1)
		go func() {
			defer wg.Done()
			if _, err := h.Poll(context.Background()); err != nil {
				t.Error(err)
			}
		}()
	}
	wg.Wait()
	if got := h.Stdout(); got != content {
		t.Fatalf("stdout = %q; want %q", got, content)
	}
}
