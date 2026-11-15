package sandbox

import (
	"errors"
	"fmt"
	"strings"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/workspaces"
)

// Errors are typed so callers can match them with errors.As / errors.Is:
//
//	var exit *sandbox.CommandExitError   // a command exited non-zero
//	var nf *sandbox.NotFoundError        // file, directory, command or sandbox missing
//	errors.Is(err, sandbox.ErrBusy)      // the sandbox is attached elsewhere
//	var to *sandbox.TimeoutError         // a command or start deadline passed

// ErrBusy: the sandbox is held by another controller, typically an
// interactive shell attached with chainctl or the attach API. Commands can't
// run until it detaches.
var ErrBusy = errors.New("sandbox: busy: another client is attached to this sandbox (an interactive shell); commands run once it detaches")

// ErrLimitReached: the organization is at its session limit (the beta allows
// one sandbox per user and three per organization). Kill a sandbox first.
var ErrLimitReached = errors.New("sandbox: session limit reached (one sandbox per user during the Workspaces beta); kill an existing sandbox first")

// ErrOutcomeUnknown: the connection ended, or failed, before the command
// reported an exit status. It may or may not have run, in part or in full.
// When a transport error cut it short, that error is wrapped too, so
// status.Code(err) still reports its gRPC code.
var ErrOutcomeUnknown = errors.New("sandbox: the command's outcome is unknown (the connection ended before it reported an exit status)")

// CommandExitError is returned by Commands.Run and CommandHandle.Wait when
// the command exits with a non-zero code. It carries the full result.
type CommandExitError struct {
	CommandResult
}

func (e *CommandExitError) Error() string {
	msg := strings.TrimSpace(e.Stderr)
	if len(msg) > 500 {
		msg = msg[len(msg)-500:]
	}
	if msg == "" {
		return fmt.Sprintf("sandbox: command exited with code %d", e.ExitCode)
	}
	return fmt.Sprintf("sandbox: command exited with code %d: %s", e.ExitCode, msg)
}

// NotFoundError: a file, directory, background command or sandbox does not exist.
type NotFoundError struct {
	// What is "file", "directory", "path", "command" or "sandbox".
	What string
	Name string
}

func (e *NotFoundError) Error() string {
	return fmt.Sprintf("sandbox: %s not found: %s", e.What, e.Name)
}

// InvalidPathError: the path exists but is the wrong kind (a directory where a
// file was expected, or the reverse).
type InvalidPathError struct {
	Path   string
	Reason string // "is a directory" or "not a directory"
}

func (e *InvalidPathError) Error() string { return fmt.Sprintf("sandbox: %s: %s", e.Path, e.Reason) }

// TimeoutError: a deadline passed. For a command, Result holds the output
// received before it was stopped.
type TimeoutError struct {
	Op     string // "command", "start" or "kill"
	Detail string
	Result *CommandResult
}

func (e *TimeoutError) Error() string {
	if e.Detail != "" {
		return "sandbox: timeout: " + e.Detail
	}
	return "sandbox: " + e.Op + " timed out"
}

// wrapErr maps library errors to this package's error types.
func wrapErr(err error) error {
	switch {
	case err == nil:
		return nil
	case errors.Is(err, workspaces.ErrOutcomeUnknown):
		var ou *workspaces.OutcomeUnknownError
		if errors.As(err, &ou) {
			return fmt.Errorf("%w: %w", ErrOutcomeUnknown, ou.Cause)
		}
		return ErrOutcomeUnknown
	case workspaces.IsGuestHeld(err):
		return fmt.Errorf("%w (%v)", ErrBusy, err)
	case status.Code(err) == codes.ResourceExhausted:
		return fmt.Errorf("%w (%s)", ErrLimitReached, status.Convert(err).Message())
	}
	return err
}

// scriptError maps a script's reserved exit codes to typed errors.
func scriptError(code int, p string, stderr []byte) error {
	switch code {
	case exitNotFound:
		return &NotFoundError{What: "path", Name: p}
	case exitIsDir:
		return &InvalidPathError{Path: p, Reason: "is a directory"}
	case exitNotDir:
		return &InvalidPathError{Path: p, Reason: "not a directory"}
	}
	msg := strings.TrimSpace(string(stderr))
	if msg == "" {
		msg = fmt.Sprintf("exit %d", code)
	}
	return fmt.Errorf("sandbox: %s: %s", p, msg)
}
