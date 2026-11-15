package workspaces

import (
	"bytes"
	"compress/gzip"
	"context"
	"errors"
	"io"
	"slices"
	"time"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

// FirstAttachTimeout bounds CompleteFirstAttach when ctx has no earlier
// deadline.
const FirstAttachTimeout = 2 * time.Minute

// ErrDetached means another client attached to the session and took it over
// while CompleteFirstAttach was running.
var ErrDetached = errors.New("workspaces: another client took over the session")

// CompleteFirstAttach completes a new workspace session's first attach, which
// the plane requires before it allows Exec (until then Exec fails with
// FailedPrecondition "attach with a shell first to upload the workspace").
//
// It opens one AttachSession stream with a shell, and if the plane reports
// workspace_pending, uploads an empty tree: a single FULL_TAR chunk holding an
// empty tar archive (gzip'd when the plane offers GZIP). It waits for
// WorkspaceReady and then detaches by canceling the stream. A session that
// isn't pending (already attached once, or started from a repository) is left
// as it is.
//
// gRPC errors from the plane are returned as they are (status codes intact).
func (c *Client) CompleteFirstAttach(ctx context.Context, name string) error {
	if _, ok := ctx.Deadline(); !ok {
		var cancel context.CancelFunc
		ctx, cancel = context.WithTimeout(ctx, FirstAttachTimeout)
		defer cancel()
	}
	// Canceling the stream's context is the detach.
	ctx, detach := context.WithCancel(ctx)
	defer detach()

	stream, err := c.raw.AttachSession(ctx)
	if err != nil {
		return err
	}
	open := &sessionv1.AttachRequest{Input: &sessionv1.AttachRequest_Open_{Open: &sessionv1.AttachRequest_Open{
		Name:   name,
		Resume: &sessionv1.AttachRequest_Resume{},
	}}}
	if err := stream.Send(open); err != nil {
		return recvErr(stream, err)
	}
	for {
		resp, err := stream.Recv()
		if err != nil {
			if errors.Is(err, io.EOF) {
				return errors.New("workspaces: the attach ended before the workspace was ready")
			}
			return err
		}
		switch out := resp.GetOutput().(type) {
		case *sessionv1.AttachResponse_Opened_:
			if !out.Opened.GetWorkspacePending() {
				return nil
			}
			chunk, err := emptyWorkspaceChunk(out.Opened.GetWorkspaceEncodings())
			if err != nil {
				return err
			}
			req := &sessionv1.AttachRequest{Input: &sessionv1.AttachRequest_WorkspaceChunk_{WorkspaceChunk: chunk}}
			if err := stream.Send(req); err != nil {
				return recvErr(stream, err)
			}
		case *sessionv1.AttachResponse_WorkspaceReady_:
			return nil
		case *sessionv1.AttachResponse_Detached_:
			return ErrDetached
		}
		// Anything else the plane sends on a shell attach (terminal output,
		// for one) isn't in this SDK's protocol subset and arrives with no
		// output set: it's skipped.
	}
}

// emptyTar is an empty tar archive: the 1 KiB end-of-archive marker.
var emptyTar = make([]byte, 1024)

func emptyWorkspaceChunk(encodings []sessionv1.AttachRequest_WorkspaceChunk_Encoding) (*sessionv1.AttachRequest_WorkspaceChunk, error) {
	chunk := &sessionv1.AttachRequest_WorkspaceChunk{
		Data:   emptyTar,
		Last:   true,
		Format: sessionv1.AttachRequest_WorkspaceChunk_FULL_TAR,
	}
	if slices.Contains(encodings, sessionv1.AttachRequest_WorkspaceChunk_GZIP) {
		var buf bytes.Buffer
		zw := gzip.NewWriter(&buf)
		if _, err := zw.Write(emptyTar); err != nil {
			return nil, err
		}
		if err := zw.Close(); err != nil {
			return nil, err
		}
		chunk.Data = buf.Bytes()
		chunk.Encoding = sessionv1.AttachRequest_WorkspaceChunk_GZIP
	}
	return chunk, nil
}

// recvErr turns a failed Send into the stream's real status: gRPC reports a
// closed stream as io.EOF on Send, and the cause on the next Recv.
func recvErr(stream sessionv1.Sessions_AttachSessionClient, sendErr error) error {
	if !errors.Is(sendErr, io.EOF) {
		return sendErr
	}
	for {
		if _, err := stream.Recv(); err != nil {
			if errors.Is(err, io.EOF) {
				return errors.New("workspaces: the attach ended before the workspace was ready")
			}
			return err
		}
	}
}
