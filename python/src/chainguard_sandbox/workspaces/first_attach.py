"""The first attach: the one AttachSession exchange the simple sandbox API needs.

A session created with a workspace environment expects its files on the first
interactive attach (Session.workspace_pending), and the service refuses Exec
until then. complete_first_attach opens a shell attach, uploads an empty tree,
waits for WorkspaceReady and detaches (cancels the stream).
"""

from __future__ import annotations

import gzip
import queue
from typing import TYPE_CHECKING, Iterator

import grpc

from chainguard_sandbox.workspaces.pb import session as spb

if TYPE_CHECKING:
    from chainguard_sandbox.workspaces.client import Client

#: An empty tar archive: the 1 KiB end-of-archive marker.
EMPTY_TAR = b"\0" * 1024

_Chunk = spb.AttachRequest.WorkspaceChunk


class FirstAttachError(Exception):
    """The attach ended before the workspace upload was acknowledged."""


def complete_first_attach(client: "Client", name: str, timeout: float = 120.0) -> None:
    """Complete the session's first attach with an empty workspace.

    Sends Open{name, resume:{}}; if the service answers Opened with
    workspace_pending, sends one FULL_TAR chunk holding an empty tar (gzip'd
    when GZIP is among Opened.workspace_encodings) and waits for
    WorkspaceReady. Returns at once if no upload is pending. Raises
    TimeoutError after timeout seconds, FirstAttachError if the stream ends
    (or another client takes over) first, and grpc.RpcError for service
    errors."""
    requests: queue.Queue[spb.AttachRequest | None] = queue.Queue()

    def send() -> Iterator[spb.AttachRequest]:
        while (req := requests.get()) is not None:
            yield req

    requests.put(spb.AttachRequest(open=spb.AttachRequest.Open(name=name, resume=spb.AttachRequest.Resume())))
    call = client.raw.AttachSession(send(), timeout=timeout)
    try:
        for resp in call:
            kind = resp.WhichOneof("output")
            if kind == "opened":
                if not resp.opened.workspace_pending:
                    return
                data, enc = EMPTY_TAR, _Chunk.ENCODING_UNSPECIFIED
                if _Chunk.GZIP in resp.opened.workspace_encodings:
                    data, enc = gzip.compress(EMPTY_TAR), _Chunk.GZIP
                requests.put(spb.AttachRequest(workspace_chunk=_Chunk(data=data, last=True, format=_Chunk.FULL_TAR, encoding=enc)))
            elif kind == "workspace_ready":
                return
            elif kind == "detached":
                raise FirstAttachError("another client took over the session during the first attach")
        raise FirstAttachError("the attach ended before the workspace was ready")
    except grpc.RpcError as e:
        if e.code() == grpc.StatusCode.DEADLINE_EXCEEDED:
            raise TimeoutError(f"the first attach did not complete within {timeout:g}s") from e
        raise
    finally:
        requests.put(None)  # ends the request iterator
        call.cancel()  # the detach; a no-op if the stream already ended
