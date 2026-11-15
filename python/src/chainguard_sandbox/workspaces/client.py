"""Client for the Chainguard Workspaces Sessions service: the calls the simple sandbox API uses."""

from __future__ import annotations

import errno
import os
import threading
import time
import urllib.parse
from typing import BinaryIO, Iterable

import grpc
from google.protobuf import duration_pb2, field_mask_pb2

from chainguard_sandbox.workspaces.auth import TokenSource, default_token_source
from chainguard_sandbox.workspaces.exec import (
    MAX_EXEC_STDIN,
    ExecResult,
    ExitError,
    OutcomeUnknownError,
    guest_path,
    outcome_unknown,
    with_dir,
    DEFAULT_WORK_DIR,
)
from chainguard_sandbox.workspaces.pb import session as spb
from chainguard_sandbox.workspaces.pb import session_grpc
from chainguard_sandbox.workspaces.sessions import instance_name, is_session_name, resolve_matches

DEFAULT_URL = "https://sessions.chainbox.dev"
ENV_SESSIONS_URL = "CHAINCTL_SESSIONS_URL"
ENV_INSTANCE = "CHAINCTL_SANDBOX_INSTANCE"


# The write_file program (same as the Go library's): exit 21 for a directory.
WRITE_FILE_SCRIPT = (
    'p=$1; if [ -d "$p" ]; then echo "$p: is a directory" >&2; cat >/dev/null; exit 21; fi; '
    'd=$(dirname -- "$p") && mkdir -p -- "$d" && t="$d/.chainguard-sandbox-write.$$" && cat > "$t" '
    '&& chmod "$2" "$t" && mv -f -- "$t" "$p" || { rm -f -- "$t"; exit 1; }'
)


def _controller_busy(msg: str) -> bool:
    return "held by another controller" in msg or "Channel already in use" in msg


class NotFoundError(LookupError):
    pass


class AuthTimeoutError(TimeoutError):
    """The token source didn't produce a token within the call's timeout."""


class AmbiguousError(LookupError):
    pass


class _Discard:
    def write(self, b: bytes) -> int:
        return len(b)


class _BearerInterceptor(
    grpc.UnaryUnaryClientInterceptor,
    grpc.UnaryStreamClientInterceptor,
    grpc.StreamUnaryClientInterceptor,
    grpc.StreamStreamClientInterceptor,
):
    """Adds `authorization: Bearer <token>` to every call."""

    def __init__(self, ts: TokenSource):
        self._ts = ts

    def _token(self, timeout: float | None) -> str:
        """The token source has no deadline of its own, so a call with a
        timeout fetches it on a helper thread and waits at most that long."""
        if timeout is None:
            return self._ts.token()
        box: dict = {}

        def run():
            try:
                box["tok"] = self._ts.token()
            except BaseException as e:  # re-raised on the caller's thread
                box["err"] = e

        t = threading.Thread(target=run, name="chainguard-sandbox-token", daemon=True)
        t.start()
        t.join(timeout)
        if t.is_alive():
            raise AuthTimeoutError(f"no token within {timeout:g}s")
        if "err" in box:
            raise box["err"]
        return box["tok"]

    def _details(self, d):
        start = time.monotonic()
        tok = self._token(d.timeout)
        md = [*(d.metadata or []), ("authorization", "Bearer " + tok)]
        # The token's wait counts against the call's timeout.
        timeout = None if d.timeout is None else max(d.timeout - (time.monotonic() - start), 0.001)
        if hasattr(d, "_replace"):
            return d._replace(metadata=md, timeout=timeout)
        return _CallDetails(d, md, timeout)

    def intercept_unary_unary(self, continuation, client_call_details, request):
        return continuation(self._details(client_call_details), request)

    def intercept_unary_stream(self, continuation, client_call_details, request):
        return continuation(self._details(client_call_details), request)

    def intercept_stream_unary(self, continuation, client_call_details, request_iterator):
        return continuation(self._details(client_call_details), request_iterator)

    def intercept_stream_stream(self, continuation, client_call_details, request_iterator):
        return continuation(self._details(client_call_details), request_iterator)


class _CallDetails(grpc.ClientCallDetails):
    def __init__(self, d, md, timeout):
        self.method, self.timeout, self.metadata = d.method, timeout, md
        self.credentials, self.wait_for_ready, self.compression = d.credentials, d.wait_for_ready, d.compression


def _parse_url(raw: str) -> tuple[str, bool]:
    if "://" not in raw:
        raw = "https://" + raw
    u = urllib.parse.urlparse(raw)
    if u.scheme not in ("https", "http"):
        raise ValueError(f"sessions URL {raw!r}: unsupported scheme {u.scheme!r}")
    if not u.hostname:
        raise ValueError(f"sessions URL {raw!r}: no host")
    secure = u.scheme == "https"
    host = f"[{u.hostname}]" if ":" in u.hostname else u.hostname  # IPv6 literals keep their brackets
    return f"{host}:{u.port or (443 if secure else 80)}", secure


class Client:
    """Talks to the sandbox plane.

    >>> c = Client(instance="<org id>")
    >>> for s in c.list(): print(s.display_name, s.short_id)
    """

    def __init__(
        self,
        url: str | None = None,
        instance: str | None = None,
        token_source: TokenSource | None = None,
        user_agent: str = "chainguard-sandbox-sdk-py",
        options: Iterable[tuple[str, object]] = (),
        channel: grpc.Channel | None = None,
        detect_instance: bool = True,
        platform_token_source: TokenSource | None = None,
    ):
        """instance: the organization ID. If None, $CHAINCTL_SANDBOX_INSTANCE is
        used; if that is empty too, the organization is auto-detected (the only
        one you belong to) unless detect_instance=False or
        $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1. Detection needs an account-API token:
        platform_token_source, by default the saved login (when token_source
        is not given and $CHAINCTL_AUTH_TOKEN is unset)."""
        self.instance_id = instance if instance is not None else os.environ.get(ENV_INSTANCE, "")
        self._detect_error: str | None = None
        if not self.instance_id and channel is None:
            from chainguard_sandbox.workspaces.auth import ENV_AUTH_TOKEN
            from chainguard_sandbox.workspaces.orgs import ENV_NO_DETECT_INSTANCE, detect_instance as _detect, platform_token_source as _pts

            if not detect_instance or os.environ.get(ENV_NO_DETECT_INSTANCE) == "1":
                self._detect_error = "turned off"
            else:
                pts = platform_token_source
                if pts is None and token_source is None and not os.environ.get(ENV_AUTH_TOKEN):
                    pts = _pts()
                if pts is None:
                    self._detect_error = f"needs platform_token_source when a custom token source or ${ENV_AUTH_TOKEN} is used"
                else:
                    try:
                        self.instance_id = _detect(pts)
                    except Exception as e:  # noqa: BLE001 - reported when an instance is needed
                        self._detect_error = str(e)
        self._owns_channel = channel is None
        if channel is None:
            target, secure = _parse_url(url or os.environ.get(ENV_SESSIONS_URL) or DEFAULT_URL)
            opts = [("grpc.primary_user_agent", user_agent), *options]
            channel = (grpc.secure_channel(target, grpc.ssl_channel_credentials(), options=opts)
                       if secure else grpc.insecure_channel(target, options=opts))
            channel = grpc.intercept_channel(channel, _BearerInterceptor(token_source or default_token_source()))
        self.channel = channel
        self.raw = session_grpc.SessionsStub(channel)

    @classmethod
    def from_channel(cls, channel: grpc.Channel, instance: str = "") -> "Client":
        """A client on an existing channel (which must carry its own credentials)."""
        return cls(instance=instance, channel=channel)

    def close(self) -> None:
        if self._owns_channel:
            self.channel.close()

    def __enter__(self):
        return self

    def __exit__(self, *exc):
        self.close()

    @property
    def instance(self) -> str:
        """'instances/{id}', the parent of sessions."""
        return instance_name(self.instance_id)

    def _parent(self) -> str:
        if not self.instance_id:
            from chainguard_sandbox.workspaces.orgs import no_instance_message

            raise ValueError(no_instance_message(self._detect_error))
        return self.instance

    # --- sessions -----------------------------------------------------------

    def create(self, s: spb.Session) -> spb.Session:
        return self.raw.CreateSession(spb.CreateSessionRequest(parent=self._parent(), session=s))

    def get(self, name: str, timeout: float | None = None) -> spb.Session:
        """The session. timeout (seconds) bounds the call; it then fails with
        DEADLINE_EXCEEDED (or AuthTimeoutError if no token arrived in time)."""
        return self.raw.GetSession(spb.GetSessionRequest(name=name), timeout=timeout)

    def list(self, owner: str = "", page_size: int = 0) -> list[spb.Session]:
        out: list[spb.Session] = []
        req = spb.ListSessionsRequest(parent=self._parent(), page_size=page_size, owner=owner)
        while True:
            resp = self.raw.ListSessions(req)
            out.extend(resp.sessions)
            if not resp.next_page_token:
                return out
            req.page_token = resp.next_page_token

    def resolve(self, ref: str) -> spb.Session:
        """A full resource name, short ID or display name ("quiet-harbor",
        "app/quiet-harbor", "acme/app/quiet-harbor")."""
        if is_session_name(ref):
            return self.get(ref)
        hits = resolve_matches(self.list(), ref)
        if not hits:
            raise NotFoundError(f"no matching session: {ref!r}")
        if len(hits) > 1:
            raise AmbiguousError(f"reference matches more than one session: {ref!r}")
        return hits[0]

    def delete(self, name: str) -> None:
        self.raw.DeleteSession(spb.DeleteSessionRequest(name=name))

    def update(self, s: spb.Session, *paths: str) -> spb.Session:
        return self.raw.UpdateSession(spb.UpdateSessionRequest(session=s, update_mask=field_mask_pb2.FieldMask(paths=list(paths))))

    def park(self, name: str) -> spb.Session:
        """Snapshot and stop the VM ("Park now")."""
        return self.update(spb.Session(name=name, park_request=spb.Session.PARK_REQUEST_PARK), "park_request")

    def heartbeat(self, name: str, hold_for: float = 0) -> spb.Session:
        req = spb.HeartbeatSessionRequest(name=name)
        if hold_for > 0:
            req.hold_for.CopyFrom(duration_pb2.Duration(seconds=int(hold_for), nanos=int((hold_for % 1) * 1e9)))
        return self.raw.HeartbeatSession(req)

    # --- exec ---------------------------------------------------------------

    def exec(
        self,
        name: str,
        argv: list[str],
        *,
        dir: str = "",
        no_dir: bool = False,
        stdin: bytes = b"",
        timeout: float = 0,
        stdout: BinaryIO | None = None,
        stderr: BinaryIO | None = None,
    ) -> ExecResult:
        """Run argv in the session (no shell). dir defaults to /work, as with
        `chainctl develop exec`; no_dir passes argv through unwrapped (the plane
        itself starts commands in /). Refused while a shell attach holds the
        guest (see is_guest_held). Raises OutcomeUnknownError if the stream ends
        without an exit status, or the connection fails once the command may
        have started (the transport error is its ``cause``)."""
        if not argv:
            raise ValueError("empty argv")
        if len(stdin) > MAX_EXEC_STDIN:
            raise ValueError(f"stdin is {len(stdin)} bytes; the limit is {MAX_EXEC_STDIN}")
        if not no_dir:
            argv = with_dir(dir or DEFAULT_WORK_DIR, argv)
        req = spb.ExecRequest(name=name, argv=argv, stdin=stdin)
        if timeout > 0:
            req.deadline.CopyFrom(duration_pb2.Duration(seconds=int(timeout), nanos=int((timeout % 1) * 1e9)))
        out, err = stdout or _Discard(), stderr or _Discard()
        events = self._exec_stream(req)
        started = False  # an event arrived: the command was running
        try:
            for ev in events:
                started = True
                kind = ev.WhichOneof("event")
                if kind == "stdout":
                    out.write(ev.stdout)
                elif kind == "stderr":
                    err.write(ev.stderr)
                elif kind == "exited":
                    return ExecResult(ev.exited.code, ev.exited.timed_out)
        except grpc.RpcError as e:
            if outcome_unknown(e, started):
                raise OutcomeUnknownError(cause=e) from e
            raise
        finally:
            events.close()  # cancels the stream (also when a writer raised)
        raise OutcomeUnknownError()

    def _exec_stream(self, req: spb.ExecRequest):
        """Exec responses. While the plane answers FAILED_PRECONDITION "instance
        held by another controller" (briefly, right after another client's
        attach or exec; nothing ran), retry with backoff for up to ~30s."""
        for delay in (2, 4, 8, 8, 8, None):
            stream = self.raw.Exec(req)
            try:
                try:
                    first = next(stream)
                except StopIteration:
                    return
                except grpc.RpcError as e:
                    if delay is None or e.code() != grpc.StatusCode.FAILED_PRECONDITION or not _controller_busy(e.details() or ""):
                        raise
                    time.sleep(delay)
                    continue
                yield first
                yield from stream
                return
            finally:
                stream.cancel()  # no-op once finished; stops the server stream on early exit

    def output(self, name: str, argv: list[str], **kw) -> bytes:
        """stdout of argv; raises ExitError on a non-zero exit."""
        import io

        out, err = io.BytesIO(), io.BytesIO()
        res = self.exec(name, argv, stdout=out, stderr=err, **kw)
        if res.exit_code != 0 or res.timed_out:
            raise ExitError(res, out.getvalue(), err.getvalue())
        return out.getvalue()

    def shell(self, name: str, script: str, *args: str, **kw) -> bytes:
        """`sh -c script` with args as $1, $2, ..."""
        return self.output(name, ["sh", "-c", script, "_", *args], **kw)

    # --- files (shell programs over exec, like chainctl's MCP tools) --------

    def write_file(self, name: str, path: str, data: bytes, mode: int = 0o644) -> None:
        """Atomically replace path (creating parent directories). Refuses an
        empty path (ValueError) and an existing directory (IsADirectoryError):
        `mv tmp dir` would otherwise move the temp file inside it and "succeed"."""
        if not path.strip():
            raise ValueError("write_file needs a file path")
        try:
            self.shell(name, WRITE_FILE_SCRIPT, guest_path(path), f"{mode & 0o7777:o}", stdin=data)
        except ExitError as e:
            if e.exit_code == 21:
                raise IsADirectoryError(errno.EISDIR, "is a directory", guest_path(path)) from e
            raise

    # --- first attach -------------------------------------------------------

    def first_attach(self, name: str, timeout: float = 120.0) -> None:
        """Complete the session's first attach with an empty workspace (see
        chainguard_sandbox.workspaces.first_attach.complete_first_attach)."""
        from chainguard_sandbox.workspaces.first_attach import complete_first_attach

        complete_first_attach(self, name, timeout)
