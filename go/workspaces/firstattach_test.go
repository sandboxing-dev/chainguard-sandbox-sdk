package workspaces

import (
	"bytes"
	"compress/gzip"
	"context"
	"errors"
	"io"
	"testing"
	"time"

	"google.golang.org/grpc/codes"
	"google.golang.org/grpc/status"

	"github.com/sandboxing-dev/chainguard-sandbox-sdk/go/api/sessionv1"
)

func opened(pending bool, encodings ...sessionv1.AttachRequest_WorkspaceChunk_Encoding) *sessionv1.AttachResponse {
	return &sessionv1.AttachResponse{Output: &sessionv1.AttachResponse_Opened_{Opened: &sessionv1.AttachResponse_Opened{
		SessionName: "app/quiet-harbor", WorkspacePending: pending, WorkspaceEncodings: encodings,
	}}}
}

func workspaceReady(n int) *sessionv1.AttachResponse {
	return &sessionv1.AttachResponse{Output: &sessionv1.AttachResponse_WorkspaceReady_{WorkspaceReady: &sessionv1.AttachResponse_WorkspaceReady{Bytes: int64(n)}}}
}

// uploadServer plays the plane's side of a first attach: it checks the Open,
// answers Opened, collects the workspace chunks, answers WorkspaceReady and
// then waits for the client to go away (the detach).
func uploadServer(t *testing.T, encodings []sessionv1.AttachRequest_WorkspaceChunk_Encoding, got chan<- *sessionv1.AttachRequest_WorkspaceChunk, detached chan<- struct{}) func(attachSrv) error {
	return func(s attachSrv) error {
		first, err := s.Recv()
		if err != nil {
			return err
		}
		o := first.GetOpen()
		if o.GetName() != "instances/org1/sessions/a" || o.GetResume() == nil || o.GetResume().GetIncarnation() != 0 {
			return status.Errorf(codes.InvalidArgument, "open %v", first)
		}
		if err := s.Send(opened(true, encodings...)); err != nil {
			return err
		}
		req, err := s.Recv()
		if err != nil {
			return err
		}
		c := req.GetWorkspaceChunk()
		if c == nil {
			return status.Errorf(codes.FailedPrecondition, "want a workspace chunk, got %v", req)
		}
		got <- c
		if err := s.Send(workspaceReady(len(c.GetData()))); err != nil {
			return err
		}
		// The client detaches by canceling the stream.
		<-s.Context().Done()
		close(detached)
		return nil
	}
}

func TestCompleteFirstAttachUploadsAnEmptyTree(t *testing.T) {
	for _, gz := range []bool{false, true} {
		c, fake := newTestClient(t)
		var enc []sessionv1.AttachRequest_WorkspaceChunk_Encoding
		if gz {
			enc = []sessionv1.AttachRequest_WorkspaceChunk_Encoding{sessionv1.AttachRequest_WorkspaceChunk_GZIP}
		}
		got := make(chan *sessionv1.AttachRequest_WorkspaceChunk, 1)
		detached := make(chan struct{})
		fake.attachFn = uploadServer(t, enc, got, detached)

		ctx, cancel := context.WithTimeout(context.Background(), 10*time.Second)
		if err := c.CompleteFirstAttach(ctx, "instances/org1/sessions/a"); err != nil {
			t.Fatalf("gzip=%v: %v", gz, err)
		}
		cancel()
		chunk := <-got
		if !chunk.GetLast() || chunk.GetFormat() != sessionv1.AttachRequest_WorkspaceChunk_FULL_TAR {
			t.Fatalf("gzip=%v: chunk last=%v format=%v", gz, chunk.GetLast(), chunk.GetFormat())
		}
		data := chunk.GetData()
		if gz {
			if chunk.GetEncoding() != sessionv1.AttachRequest_WorkspaceChunk_GZIP {
				t.Fatalf("encoding = %v; want GZIP when offered", chunk.GetEncoding())
			}
			zr, err := gzip.NewReader(bytes.NewReader(data))
			if err != nil {
				t.Fatal(err)
			}
			if data, err = io.ReadAll(zr); err != nil {
				t.Fatal(err)
			}
		} else if chunk.GetEncoding() != sessionv1.AttachRequest_WorkspaceChunk_ENCODING_UNSPECIFIED {
			t.Fatalf("encoding = %v; want none when GZIP isn't offered", chunk.GetEncoding())
		}
		if !bytes.Equal(data, make([]byte, 1024)) {
			t.Fatalf("gzip=%v: the tree isn't an empty tar (%d bytes)", gz, len(data))
		}
		select {
		case <-detached:
		case <-time.After(5 * time.Second):
			t.Fatalf("gzip=%v: the client didn't detach", gz)
		}
	}
}

func TestCompleteFirstAttachWithNothingPending(t *testing.T) {
	c, fake := newTestClient(t)
	sent := make(chan *sessionv1.AttachRequest, 4)
	fake.attachFn = func(s attachSrv) error {
		if _, err := s.Recv(); err != nil {
			return err
		}
		_ = s.Send(opened(false))
		for {
			req, err := s.Recv()
			if err != nil {
				close(sent)
				return nil
			}
			sent <- req
		}
	}
	if err := c.CompleteFirstAttach(context.Background(), "instances/org1/sessions/a"); err != nil {
		t.Fatal(err)
	}
	for req := range sent {
		t.Fatalf("nothing should be sent after Opened without a pending workspace; got %v", req)
	}
}

func TestCompleteFirstAttachErrors(t *testing.T) {
	c, fake := newTestClient(t)
	fake.attachFn = func(s attachSrv) error {
		return status.Error(codes.NotFound, "no such session")
	}
	if err := c.CompleteFirstAttach(context.Background(), "instances/org1/sessions/a"); status.Code(err) != codes.NotFound {
		t.Fatalf("err = %v; want the plane's NotFound", err)
	}

	fake.attachFn = func(s attachSrv) error {
		if _, err := s.Recv(); err != nil {
			return err
		}
		_ = s.Send(opened(true))
		if _, err := s.Recv(); err != nil {
			return err
		}
		return s.Send(&sessionv1.AttachResponse{Output: &sessionv1.AttachResponse_Detached_{Detached: &sessionv1.AttachResponse_Detached{}}})
	}
	if err := c.CompleteFirstAttach(context.Background(), "instances/org1/sessions/a"); !errors.Is(err, ErrDetached) {
		t.Fatalf("err = %v; want ErrDetached", err)
	}

	fake.attachFn = func(s attachSrv) error {
		if _, err := s.Recv(); err != nil {
			return err
		}
		<-s.Context().Done() // never answers
		return nil
	}
	ctx, cancel := context.WithTimeout(context.Background(), 200*time.Millisecond)
	defer cancel()
	if err := c.CompleteFirstAttach(ctx, "instances/org1/sessions/a"); status.Code(err) != codes.DeadlineExceeded {
		t.Fatalf("err = %v; want DeadlineExceeded", err)
	}
}
