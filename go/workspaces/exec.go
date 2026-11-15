package workspaces

import (
	"bytes"
	"context"
	"errors"
	"fmt"
	"io"
	"path"
	"strings"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"
	"google.golang.org/protobuf/types/known/durationpb"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

// ErrOutcomeUnknown means the exec stream ended without an exit status
// (chainctl reports this as exit code 255).
var ErrOutcomeUnknown = errors.New("workspaces: exec ended without an exit status")

// OutcomeUnknownError is ErrOutcomeUnknown with the transport error that cut
// the exec short: the command may have run, in part or in full. It matches
// errors.Is(err, ErrOutcomeUnknown), and Cause stays reachable, so
// status.Code(err) still reports the gRPC code (for example Unavailable).
type OutcomeUnknownError struct{ Cause error }

func (e *OutcomeUnknownError) Error() string {
	return ErrOutcomeUnknown.Error() + ": " + e.Cause.Error()
}
func (e *OutcomeUnknownError) Is(target error) bool { return target == ErrOutcomeUnknown }
func (e *OutcomeUnknownError) Unwrap() error        { return e.Cause }

// interrupted classifies an error from the exec stream. Once the stream has
// delivered an event the command was running, so any failure leaves its
// outcome unknown. Before that, a failure of the connection itself does too
// (the request may have arrived); an answer from the plane (not found, held,
// invalid, over the limit, ...) means the command did not run. An error caused
// by the caller's own context is returned as it is.
func interrupted(ctx context.Context, err error, started bool) error {
	if ctx.Err() != nil {
		return err
	}
	if started {
		return &OutcomeUnknownError{Cause: err}
	}
	switch status.Code(err) {
	case codes.Unavailable, codes.Internal, codes.Unknown, codes.DataLoss, codes.DeadlineExceeded, codes.Canceled:
		return &OutcomeUnknownError{Cause: err}
	}
	return err
}

// IsGuestHeld reports whether err is the plane refusing a call because
// another controller holds the guest (e.g. an attached shell that is still
// heartbeating; the plane waits ~10s for it to let go, then runs nothing).
//
// It also matches FailedPrecondition "… instance held by another controller …
// Channel already in use", which the plane returns for a few seconds after
// another client's attach or exec ends.
func IsGuestHeld(err error) bool {
	st, ok := status.FromError(err)
	if !ok {
		return false
	}
	switch st.Code() {
	case codes.Aborted:
		return strings.Contains(st.Message(), "held")
	case codes.FailedPrecondition:
		return controllerBusy(st.Message())
	}
	return false
}

func controllerBusy(msg string) bool {
	return strings.Contains(msg, "held by another controller") || strings.Contains(msg, "Channel already in use")
}

// ExecOptions configures Exec.
type ExecOptions struct {
	// Dir is the working directory, absolute or relative to /work. Empty
	// means /work, as with `chainctl develop exec` (the plane itself starts
	// commands in /). Set NoDir to pass argv through unwrapped.
	Dir   string
	NoDir bool
	// Stdin is passed to the command (at most MaxExecStdin bytes).
	Stdin []byte
	// Timeout stops the command; the result then has TimedOut set (the exit
	// code is whatever the plane reports; chainctl itself exits 124).
	Timeout time.Duration
	// Stdout and Stderr receive output as it streams; nil discards it.
	Stdout, Stderr io.Writer
}

// ExecResult is a finished command's status.
type ExecResult struct {
	ExitCode int
	TimedOut bool
}

// Exec runs argv in the session (no shell; use Shell for that). A parked
// session is resumed by the plane first. Exec is refused while a shell attach
// holds the guest (see IsGuestHeld). The returned error is non-nil only
// when the call itself failed; a non-zero exit is reported in ExecResult.
// If the connection fails once the command may have started, the error
// matches ErrOutcomeUnknown and wraps the transport error (OutcomeUnknownError).
func (c *Client) Exec(ctx context.Context, name string, argv []string, o ExecOptions) (ExecResult, error) {
	if len(argv) == 0 {
		return ExecResult{}, errors.New("workspaces: empty argv")
	}
	if len(o.Stdin) > MaxExecStdin {
		return ExecResult{}, fmt.Errorf("workspaces: stdin is %d bytes; the limit is %d", len(o.Stdin), MaxExecStdin)
	}
	if !o.NoDir {
		if o.Dir == "" {
			o.Dir = DefaultWorkDir
		}
		argv = withDir(o.Dir, argv)
	}
	req := &sessionv1.ExecRequest{Name: name, Argv: argv, Stdin: o.Stdin}
	if o.Timeout > 0 {
		req.Deadline = durationpb.New(o.Timeout)
	}
	// Canceling on every return releases the stream (and stops the remote
	// command if we bail out early, e.g. on a writer error).
	caller := ctx
	ctx, cancel := context.WithCancel(ctx)
	defer cancel()
	stdout, stderr := orDiscard(o.Stdout), orDiscard(o.Stderr)
	stream, first, err := c.startExec(ctx, req)
	if err != nil {
		return ExecResult{ExitCode: 255}, interrupted(caller, err, false)
	}
	for {
		ev, err := first, error(nil)
		if ev == nil {
			ev, err = stream.Recv()
		}
		first = nil
		if errors.Is(err, io.EOF) {
			return ExecResult{ExitCode: 255}, ErrOutcomeUnknown
		}
		if err != nil {
			// The stream delivered at least one event: the command was running.
			return ExecResult{ExitCode: 255}, interrupted(caller, err, true)
		}
		switch e := ev.GetEvent().(type) {
		case *sessionv1.ExecResponse_Stdout:
			if _, err := stdout.Write(e.Stdout); err != nil {
				return ExecResult{ExitCode: 255}, err
			}
		case *sessionv1.ExecResponse_Stderr:
			if _, err := stderr.Write(e.Stderr); err != nil {
				return ExecResult{ExitCode: 255}, err
			}
		case *sessionv1.ExecResponse_Exited_:
			return ExecResult{ExitCode: int(e.Exited.GetCode()), TimedOut: e.Exited.GetTimedOut()}, nil
		}
	}
}

// startExec opens the Exec stream and reads its first event. While the plane
// answers FailedPrecondition "instance held by another controller" (briefly,
// right after another client's attach or exec; nothing ran), it retries with
// backoff for up to ~30s.
func (c *Client) startExec(ctx context.Context, req *sessionv1.ExecRequest) (sessionv1.Sessions_ExecClient, *sessionv1.ExecResponse, error) {
	delays := []time.Duration{2 * time.Second, 4 * time.Second, 8 * time.Second, 8 * time.Second, 8 * time.Second}
	for i := 0; ; i++ {
		stream, err := c.raw.Exec(ctx, req)
		if err != nil {
			return nil, nil, err
		}
		first, err := stream.Recv()
		if errors.Is(err, io.EOF) {
			return nil, nil, ErrOutcomeUnknown
		}
		st, _ := status.FromError(err)
		if err == nil || st.Code() != codes.FailedPrecondition || !controllerBusy(st.Message()) || i >= len(delays) {
			return stream, first, err
		}
		select {
		case <-ctx.Done():
			return nil, nil, ctx.Err()
		case <-time.After(delays[i]):
		}
	}
}

// ExitError is returned by Output and Shell helpers for a non-zero exit.
type ExitError struct {
	ExecResult
	Stderr []byte
}

func (e *ExitError) Error() string {
	msg := strings.TrimSpace(string(e.Stderr))
	if e.TimedOut {
		return "command timed out: " + msg
	}
	return fmt.Sprintf("command exited %d: %s", e.ExitCode, msg)
}

// Output runs argv and returns its stdout, or an *ExitError on a non-zero exit.
func (c *Client) Output(ctx context.Context, name string, argv []string, o ExecOptions) ([]byte, error) {
	var stdout, stderr bytes.Buffer
	o.Stdout, o.Stderr = &stdout, &stderr
	res, err := c.Exec(ctx, name, argv, o)
	if err != nil {
		return nil, err
	}
	if res.ExitCode != 0 || res.TimedOut {
		return stdout.Bytes(), &ExitError{ExecResult: res, Stderr: stderr.Bytes()}
	}
	return stdout.Bytes(), nil
}

// Shell runs script with `sh -c`; extra args become $1, $2, ...
func (c *Client) Shell(ctx context.Context, name, script string, o ExecOptions, args ...string) ([]byte, error) {
	return c.Output(ctx, name, append([]string{"sh", "-c", script, "_"}, args...), o)
}

// cdScript is how chainctl implements `exec --cwd`.
const cdScript = `cd "$1" && shift && exec "$@"`

func withDir(dir string, argv []string) []string {
	return append([]string{"sh", "-c", cdScript, "_", guestPath(dir)}, argv...)
}

// guestPath resolves p against /work.
func guestPath(p string) string {
	if path.IsAbs(p) {
		return path.Clean(p)
	}
	return path.Join(DefaultWorkDir, p)
}

func orDiscard(w io.Writer) io.Writer {
	if w == nil {
		return io.Discard
	}
	return w
}
