package sandbox

import (
	"bytes"
	"context"
	"crypto/rand"
	"encoding/hex"
	"errors"
	"fmt"
	"path"
	"sort"
	"strconv"
	"strings"
	"sync"
	"time"
	"unicode/utf8"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// DefaultCommandTimeout bounds a foreground command when RunOptions.Timeout
// is zero. A negative Timeout means no limit.
const DefaultCommandTimeout = 60 * time.Second

// Commands runs commands in a sandbox: sb.Commands.
type Commands struct{ sb *Sandbox }

// RunOptions configures Run and RunArgs.
type RunOptions struct {
	// Cwd is the working directory (default /work; relative paths resolve
	// against /work).
	Cwd string
	// Envs are added to the sandbox-level Envs (and override them).
	Envs map[string]string
	// Timeout kills the command after this long (default 60s; negative means
	// no limit). The plane enforces it.
	Timeout time.Duration
	// Stdin is sent to the command (at most 16 MiB), then closed.
	Stdin []byte
	// OnStdout and OnStderr receive output as it arrives. A character whose
	// bytes arrive in two pieces is delivered whole, with the later piece.
	OnStdout, OnStderr func(data string)
}

// StartOptions configures a background command (Start / StartArgs).
type StartOptions struct {
	Cwd  string
	Envs map[string]string
}

// CommandResult is a finished command's output and exit code.
type CommandResult struct {
	ExitCode int
	Stdout   string
	Stderr   string
}

// Run runs cmd with bash (a login shell, so shell syntax works) and waits for
// it. A non-zero exit returns a *CommandExitError holding the result; a
// timeout returns a *TimeoutError.
func (c *Commands) Run(ctx context.Context, cmd string, o *RunOptions) (*CommandResult, error) {
	return c.RunArgs(ctx, []string{"bash", "-lc", cmd}, o)
}

// RunArgs runs argv directly, without a shell (like Vercel's runCommand and
// Modal's exec). Errors as for Run.
func (c *Commands) RunArgs(ctx context.Context, argv []string, o *RunOptions) (*CommandResult, error) {
	if len(argv) == 0 {
		return nil, errors.New("sandbox: empty command")
	}
	if o == nil {
		o = &RunOptions{}
	}
	if len(o.Stdin) > 16<<20 {
		return nil, fmt.Errorf("sandbox: stdin is %d bytes; the limit is 16 MiB (write a file instead)", len(o.Stdin))
	}
	timeout := o.Timeout
	if timeout == 0 {
		timeout = DefaultCommandTimeout
	}
	if timeout < 0 {
		timeout = 0
	}
	cwd := guestPath(o.Cwd)
	full := append([]string{"sh", "-c", runScript, "_", cwd}, c.sb.envArgv(o.Envs, argv)...)
	var stdout, stderr bytes.Buffer
	textOut, textErr := &textStream{fn: o.OnStdout}, &textStream{fn: o.OnStderr}
	res, err := c.sb.c.Exec(ctx, c.sb.Name, full, workspaces.ExecOptions{
		NoDir:   true,
		Stdin:   o.Stdin,
		Timeout: timeout,
		Stdout:  &teeWriter{buf: &stdout, text: textOut},
		Stderr:  &teeWriter{buf: &stderr, text: textErr},
	})
	textOut.flush()
	textErr.flush()
	r := &CommandResult{ExitCode: res.ExitCode, Stdout: stdout.String(), Stderr: stderr.String()}
	if err != nil {
		return r, wrapErr(err)
	}
	if res.TimedOut {
		return r, &TimeoutError{Op: "command", Detail: fmt.Sprintf("command did not finish within %s", timeout), Result: r}
	}
	if res.ExitCode == exitCantChdir && strings.HasPrefix(r.Stderr, "sandbox: no such directory:") {
		return r, &NotFoundError{What: "directory", Name: cwd}
	}
	if res.ExitCode != 0 {
		return r, &CommandExitError{CommandResult: *r}
	}
	return r, nil
}

// envArgv prefixes argv with `env K=V ...` for the merged environment.
func (sb *Sandbox) envArgv(extra map[string]string, argv []string) []string {
	merged := copyEnv(sb.envs)
	for k, v := range extra {
		merged[k] = v
	}
	if len(merged) == 0 {
		return argv
	}
	keys := make([]string, 0, len(merged))
	for k := range merged {
		keys = append(keys, k)
	}
	sort.Strings(keys)
	out := []string{"env"}
	for _, k := range keys {
		out = append(out, k+"="+merged[k])
	}
	return append(out, argv...)
}

type teeWriter struct {
	buf  *bytes.Buffer
	text *textStream
}

func (w *teeWriter) Write(p []byte) (int, error) {
	w.buf.Write(p)
	w.text.write(p)
	return len(p), nil
}

// textStream passes a byte stream to a callback as strings. Output arrives
// in arbitrary chunks, so a multi-byte UTF-8 character can be split across
// two of them; the incomplete tail is held back and delivered with the next
// chunk. flush delivers whatever is still held when the stream ends.
type textStream struct {
	fn      func(string)
	pending []byte
}

func (t *textStream) write(p []byte) {
	if t.fn == nil || len(p) == 0 {
		return
	}
	b := p
	if len(t.pending) > 0 {
		b = append(t.pending, p...)
		t.pending = nil
	}
	n := completeUTF8(b)
	if n < len(b) {
		t.pending = append([]byte(nil), b[n:]...)
	}
	if n > 0 {
		t.fn(string(b[:n]))
	}
}

func (t *textStream) flush() {
	if t.fn != nil && len(t.pending) > 0 {
		t.fn(string(t.pending))
	}
	t.pending = nil
}

// completeUTF8 returns the length of b without a trailing incomplete UTF-8
// sequence (a lead byte still waiting for some of its continuation bytes).
func completeUTF8(b []byte) int {
	for i := 1; i < utf8.UTFMax && i <= len(b); i++ {
		if tail := b[len(b)-i:]; utf8.RuneStart(tail[0]) {
			if !utf8.FullRune(tail) {
				return len(b) - i
			}
			break
		}
	}
	return len(b)
}

// Start runs cmd with bash in the background and returns at once. The
// command keeps running after this call (and across a pause); its output goes
// to files in the sandbox, read with the returned handle.
func (c *Commands) Start(ctx context.Context, cmd string, o *StartOptions) (*CommandHandle, error) {
	return c.StartArgs(ctx, []string{"bash", "-lc", cmd}, o)
}

// StartArgs is Start without a shell.
func (c *Commands) StartArgs(ctx context.Context, argv []string, o *StartOptions) (*CommandHandle, error) {
	if len(argv) == 0 {
		return nil, errors.New("sandbox: empty command")
	}
	if o == nil {
		o = &StartOptions{}
	}
	id := randomID()
	dir := stateDir + "/cmd/" + id
	full := append([]string{"sh", "-c", startScript, "_", dir, guestPath(o.Cwd)}, c.sb.envArgv(o.Envs, argv)...)
	out, err := c.sb.c.Output(ctx, c.sb.Name, full, workspaces.ExecOptions{NoDir: true, Timeout: 30 * time.Second})
	if err != nil {
		return nil, fmt.Errorf("sandbox: starting the command: %w", wrapErr(err))
	}
	pid, err := strconv.Atoi(strings.TrimSpace(string(out)))
	if err != nil {
		return nil, fmt.Errorf("sandbox: starting the command: unexpected output %q", out)
	}
	return &CommandHandle{PID: pid, ID: id, dir: dir, sb: c.sb}, nil
}

// ProcessInfo describes a background command.
type ProcessInfo struct {
	PID     int
	ID      string
	Cmd     string
	Running bool
	// ExitCode is set once the command has exited.
	ExitCode *int
}

// List returns the background commands started in this sandbox (running and
// finished).
func (c *Commands) List(ctx context.Context) ([]ProcessInfo, error) {
	out, err := c.sb.c.Output(ctx, c.sb.Name, []string{"sh", "-c", listScript, "_", stateDir}, workspaces.ExecOptions{NoDir: true, Timeout: 30 * time.Second})
	if err != nil {
		return nil, wrapErr(err)
	}
	var ps []ProcessInfo
	for _, line := range strings.Split(strings.TrimRight(string(out), "\n"), "\n") {
		f := strings.SplitN(line, "\t", 4)
		if len(f) != 4 {
			continue
		}
		pid, _ := strconv.Atoi(f[1])
		p := ProcessInfo{PID: pid, ID: f[0], Cmd: f[3], Running: f[2] == "-"}
		if code, err := strconv.Atoi(f[2]); err == nil {
			p.ExitCode = &code
		}
		ps = append(ps, p)
	}
	return ps, nil
}

// Connect returns a handle to a background command by pid.
func (c *Commands) Connect(ctx context.Context, pid int) (*CommandHandle, error) {
	out, err := c.sb.c.Output(ctx, c.sb.Name, []string{"sh", "-c", resolvePidScript, "_", stateDir, strconv.Itoa(pid)}, workspaces.ExecOptions{NoDir: true, Timeout: 30 * time.Second})
	var ee *workspaces.ExitError
	if errors.As(err, &ee) && ee.ExitCode == exitNotFound {
		return nil, &NotFoundError{What: "command", Name: strconv.Itoa(pid)}
	}
	if err != nil {
		return nil, wrapErr(err)
	}
	dir := strings.TrimSpace(string(out))
	return &CommandHandle{PID: pid, ID: path.Base(dir), dir: dir, sb: c.sb}, nil
}

// Kill sends SIGKILL to a background command (and its process group). It
// returns false if the command had already exited.
func (c *Commands) Kill(ctx context.Context, pid int) (bool, error) {
	h, err := c.Connect(ctx, pid)
	if err != nil {
		return false, err
	}
	return h.Kill(ctx)
}

// CommandHandle is a background command.
type CommandHandle struct {
	// PID of the command's process group leader in the sandbox.
	PID int
	// ID names the command's state directory.
	ID string

	sb  *Sandbox
	dir string
	// pollMu makes polls take turns: each one reads from the offsets the one
	// before it left, so overlapping Poll and Wait calls never fetch (and
	// append) the same bytes twice. mu guards the fields below.
	pollMu         sync.Mutex
	mu             sync.Mutex
	stdout, stderr bytes.Buffer
	outOff, errOff int64
	exitCode       *int
}

// WaitOptions configures CommandHandle.Wait.
type WaitOptions struct {
	// OnStdout and OnStderr receive the output as Wait polls for it (about
	// every 2 seconds while the command runs), starting with any output an
	// earlier Poll already fetched. Output a concurrent Poll fetches is
	// delivered too, once and in order.
	OnStdout, OnStderr func(data string)
}

// Stdout returns the standard output received so far.
func (h *CommandHandle) Stdout() string { h.mu.Lock(); defer h.mu.Unlock(); return h.stdout.String() }

// Stderr returns the standard error received so far.
func (h *CommandHandle) Stderr() string { h.mu.Lock(); defer h.mu.Unlock(); return h.stderr.String() }

// ExitCode returns the exit code once known (after Wait or a Poll that saw the exit).
func (h *CommandHandle) ExitCode() *int { h.mu.Lock(); defer h.mu.Unlock(); return h.exitCode }

// Poll fetches new output and returns the exit code, or nil while the command runs.
func (h *CommandHandle) Poll(ctx context.Context) (*int, error) {
	return h.poll(ctx, 0)
}

// Wait waits for the command to exit, delivering output to the callbacks as it
// arrives. A non-zero exit returns a *CommandExitError with the result.
func (h *CommandHandle) Wait(ctx context.Context, o *WaitOptions) (*CommandResult, error) {
	if o == nil {
		o = &WaitOptions{}
	}
	// The callbacks are fed from the handle's accumulated output, from where
	// this Wait last left off: first whatever arrived before Wait (via Poll),
	// then everything new, whichever call fetched it.
	textOut, textErr := &textStream{fn: o.OnStdout}, &textStream{fn: o.OnStderr}
	var sentOut, sentErr int
	deliver := func() {
		h.mu.Lock()
		newOut := append([]byte(nil), h.stdout.Bytes()[sentOut:]...)
		newErr := append([]byte(nil), h.stderr.Bytes()[sentErr:]...)
		sentOut, sentErr = h.stdout.Len(), h.stderr.Len()
		h.mu.Unlock()
		textOut.write(newOut)
		textErr.write(newErr)
	}
	deliver()
	for {
		code, err := h.poll(ctx, 20)
		deliver()
		if err != nil {
			return nil, err
		}
		if code != nil {
			textOut.flush()
			textErr.flush()
			r := &CommandResult{ExitCode: *code, Stdout: h.Stdout(), Stderr: h.Stderr()}
			if *code != 0 {
				return r, &CommandExitError{CommandResult: *r}
			}
			return r, nil
		}
	}
}

// Kill sends SIGKILL to the command's process group. It returns false if the
// command had already exited. The exit code becomes 137.
func (h *CommandHandle) Kill(ctx context.Context) (bool, error) {
	out, err := h.sb.c.Output(ctx, h.sb.Name, []string{"sh", "-c", killScript, "_", h.dir, "KILL", "137"}, workspaces.ExecOptions{NoDir: true, Timeout: 30 * time.Second})
	var ee *workspaces.ExitError
	if errors.As(err, &ee) && ee.ExitCode == exitNotFound {
		return false, &NotFoundError{What: "command", Name: strconv.Itoa(h.PID)}
	}
	if err != nil {
		return false, wrapErr(err)
	}
	return strings.TrimSpace(string(out)) == "1", nil
}

func (h *CommandHandle) poll(ctx context.Context, waitTenths int) (*int, error) {
	h.pollMu.Lock()
	defer h.pollMu.Unlock()
	h.mu.Lock()
	if h.exitCode != nil {
		c := h.exitCode
		h.mu.Unlock()
		return c, nil
	}
	outOff, errOff := h.outOff, h.errOff
	h.mu.Unlock()
	argv := []string{"sh", "-c", pollScript, "_", h.dir, strconv.FormatInt(outOff, 10), strconv.FormatInt(errOff, 10), strconv.Itoa(waitTenths), strconv.Itoa(maxPollChunk)}
	out, err := h.sb.c.Output(ctx, h.sb.Name, argv, workspaces.ExecOptions{NoDir: true, Timeout: 60 * time.Second})
	var ee *workspaces.ExitError
	if errors.As(err, &ee) && ee.ExitCode == exitNotFound {
		return nil, &NotFoundError{What: "command", Name: strconv.Itoa(h.PID)}
	}
	if err != nil {
		return nil, wrapErr(err)
	}
	p, err := parsePoll(out, outOff, errOff)
	if err != nil {
		return nil, err
	}
	h.mu.Lock()
	h.stdout.Write(p.stdout)
	h.stderr.Write(p.stderr)
	h.outOff, h.errOff = p.outEnd, p.errEnd
	// The exit code counts only once all output is in (a capped poll may leave more).
	if p.exit != nil && !p.truncated {
		h.exitCode = p.exit
	}
	code := h.exitCode
	h.mu.Unlock()
	return code, nil
}

type pollResult struct {
	exit           *int
	stdout, stderr []byte
	outEnd, errEnd int64
	truncated      bool
}

// parsePoll decodes pollScript output: "<exit|-> <outEnd> <errEnd>\n" + stdout bytes + stderr bytes.
func parsePoll(out []byte, outOff, errOff int64) (pollResult, error) {
	var p pollResult
	nl := bytes.IndexByte(out, '\n')
	if nl < 0 {
		return p, fmt.Errorf("sandbox: malformed poll output %q", out)
	}
	f := strings.Fields(string(out[:nl]))
	if len(f) != 3 {
		return p, fmt.Errorf("sandbox: malformed poll header %q", out[:nl])
	}
	if f[0] != "-" {
		code, err := strconv.Atoi(f[0])
		if err != nil {
			return p, fmt.Errorf("sandbox: malformed exit code %q", f[0])
		}
		p.exit = &code
	}
	so, err1 := strconv.ParseInt(f[1], 10, 64)
	se, err2 := strconv.ParseInt(f[2], 10, 64)
	if err1 != nil || err2 != nil {
		return p, fmt.Errorf("sandbox: malformed poll header %q", out[:nl])
	}
	body := out[nl+1:]
	nOut, nErr := max(so-outOff, 0), max(se-errOff, 0)
	if int64(len(body)) < nOut+nErr {
		// Output shrank or was cut short; take what arrived.
		nOut = min(nOut, int64(len(body)))
		nErr = int64(len(body)) - nOut
		p.truncated = true
	}
	p.stdout, p.stderr = body[:nOut], body[nOut:nOut+nErr]
	p.outEnd, p.errEnd = outOff+nOut, errOff+nErr
	if nOut >= maxPollChunk || nErr >= maxPollChunk {
		p.truncated = true
	}
	return p, nil
}

func randomID() string {
	b := make([]byte, 8)
	_, _ = rand.Read(b)
	return hex.EncodeToString(b)
}
