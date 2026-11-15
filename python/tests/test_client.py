"""Client, exec, first-attach and sandbox-flow tests against an in-process fake
Sessions server (no network)."""

from __future__ import annotations

import gzip
import http.server
import io
import os
import subprocess
import tarfile
import threading
import time
from concurrent import futures

import grpc
import pytest
from google.protobuf import timestamp_pb2

from chainguard_sandbox.workspaces import (
    Client,
    ExitError,
    FirstAttachError,
    IdentityExchange,
    NotFoundError,
    OutcomeUnknownError,
    StaticToken,
    complete_first_attach,
    file_id_token,
    jwt_expiry,
    normalize_github_url,
    tar_dir,
)
from chainguard_sandbox.workspaces.auth import AuthError
from chainguard_sandbox.workspaces.client import WRITE_FILE_SCRIPT, AuthTimeoutError, _parse_url
from chainguard_sandbox.workspaces.exec import CD_SCRIPT
from chainguard_sandbox.workspaces.first_attach import EMPTY_TAR
from chainguard_sandbox.workspaces.pb import session as spb
from chainguard_sandbox.workspaces.pb import session_grpc
from chainguard_sandbox.workspaces.sessions import repository_url, resolve_matches
from chainguard_sandbox.workspaces.tarw import TarWriter, round_mtime

NAME = "instances/org1/sessions/a"
_Chunk = spb.AttachRequest.WorkspaceChunk


class FakeSessions(session_grpc.SessionsServicer):
    """Sessions, exec and the first-attach exchange. attach_mode selects the
    attach behavior: "gzip" (pending, GZIP offered), "plain" (pending, no
    encodings), "ready" (nothing pending), "eof" (ends after Opened),
    "detached" (another client takes over), "hang" (never answers)."""

    def __init__(self):
        self.execs: list[spb.ExecRequest] = []
        self.updates: list[spb.UpdateSessionRequest] = []
        self.heartbeats: list[spb.HeartbeatSessionRequest] = []
        self.created: list[spb.Session] = []
        self.deleted: list[str] = []
        self.opens: list[spb.AttachRequest.Open] = []
        self.chunks: list[spb.AttachRequest.WorkspaceChunk] = []
        self.attach_canceled = threading.Event()
        self.busy_hits = 0
        self.attach_mode = "gzip"
        self.pending = True
        self.exec_flow = None
        self.get_flow = None  # scripts GetSession when set
        self.url = ""

    def _auth(self, ctx):
        md = dict(ctx.invocation_metadata())
        if md.get("authorization") != "Bearer test-token":
            ctx.abort(grpc.StatusCode.UNAUTHENTICATED, f"authorization={md.get('authorization')}")

    def ListSessions(self, req, ctx):
        self._auth(ctx)
        if req.parent != "instances/org1":
            ctx.abort(grpc.StatusCode.INVALID_ARGUMENT, req.parent)
        if not req.page_token:
            return spb.ListSessionsResponse(
                sessions=[spb.Session(name=NAME, short_id="3f9a2c1e", display_name="app/quiet-harbor")],
                next_page_token="p2",
            )
        return spb.ListSessionsResponse(sessions=[spb.Session(name="instances/org1/sessions/b", short_id="77aa0011", display_name="app/scratch")])

    def CreateSession(self, req, ctx):
        self._auth(ctx)
        self.created.append(req.session)
        s = spb.Session()
        s.CopyFrom(req.session)
        s.name, s.short_id, s.display_name, s.phase = NAME, "3f9a2c1e", "sandbox/quiet-harbor", spb.CREATING
        return s

    def GetSession(self, req, ctx):
        self._auth(ctx)
        if self.get_flow:
            return self.get_flow(req, ctx)
        if req.name in self.deleted:
            ctx.abort(grpc.StatusCode.NOT_FOUND, f"session {req.name} not found")
        return spb.Session(name=req.name, short_id="3f9a2c1e", display_name="sandbox/quiet-harbor", phase=spb.IDLE,
                           guest_started_time=timestamp_pb2.Timestamp(seconds=1), workspace_pending=self.pending)

    def DeleteSession(self, req, ctx):
        self.deleted.append(req.name)
        from google.protobuf import empty_pb2

        return empty_pb2.Empty()

    def UpdateSession(self, req, ctx):
        self.updates.append(req)
        return req.session

    def HeartbeatSession(self, req, ctx):
        self.heartbeats.append(req)
        return spb.Session(name=req.name)

    def Exec(self, req, ctx):
        self._auth(ctx)
        self.execs.append(req)
        if self.exec_flow:
            yield from self.exec_flow(req, ctx)
            return
        argv = list(req.argv)
        if len(argv) > 5 and argv[0] == "sh" and argv[2] == CD_SCRIPT:
            argv = argv[5:]
        if argv[0] == "busy-once":
            self.busy_hits += 1
            if self.busy_hits == 1:
                ctx.abort(grpc.StatusCode.FAILED_PRECONDITION,
                          "exec: reattaching guest: microvm: instance held by another controller: supervisor: Channel already in use")
            yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=0))
            return
        if argv[0] == "cat":
            yield spb.ExecResponse(stdout=f"contents of {argv[-1]}".encode())
            yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=0))
        elif argv[0] == "false":
            yield spb.ExecResponse(stderr=b"nope")
            yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=3))
        elif argv[0] == "vanish":
            return
        else:
            yield spb.ExecResponse(stdout=b"hi\n")
            yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=0))

    def AttachSession(self, reqs, ctx):
        ctx.add_callback(self.attach_canceled.set)
        first = next(reqs)
        assert first.WhichOneof("input") == "open", first
        self.opens.append(first.open)
        mode = self.attach_mode
        if mode == "hang":
            while ctx.is_active():
                time.sleep(0.02)
            return
        if mode == "ready":
            yield spb.AttachResponse(opened=spb.AttachResponse.Opened(session_name="sandbox/quiet-harbor"))
            while ctx.is_active():
                time.sleep(0.02)
            return
        enc = [_Chunk.GZIP] if mode == "gzip" else []
        yield spb.AttachResponse(opened=spb.AttachResponse.Opened(
            session_name="sandbox/quiet-harbor", workspace_pending=True, workspace_encodings=enc))
        if mode == "eof":
            return
        if mode == "detached":
            yield spb.AttachResponse(detached=spb.AttachResponse.Detached())
            return
        for r in reqs:
            assert r.WhichOneof("input") == "workspace_chunk", r
            self.chunks.append(r.workspace_chunk)
            if r.workspace_chunk.last:
                break
        self.pending = False
        yield spb.AttachResponse(workspace_ready=spb.AttachResponse.WorkspaceReady(bytes=1024))
        while ctx.is_active():  # a real shell attach stays open until the client detaches
            time.sleep(0.02)


@pytest.fixture()
def fake():
    srv = grpc.server(futures.ThreadPoolExecutor(max_workers=8))
    f = FakeSessions()
    session_grpc.add_SessionsServicer_to_server(f, srv)
    port = srv.add_insecure_port("127.0.0.1:0")
    srv.start()
    f.url = f"http://127.0.0.1:{port}"
    c = Client(url=f.url, instance="org1", token_source=StaticToken("test-token"))
    yield c, f
    c.close()
    srv.stop(None)


# --- sessions ---------------------------------------------------------------


def test_list_and_resolve(fake):
    c, _ = fake
    assert len(c.list()) == 2
    for ref, want in [("3f9a2c1e", "a"), ("quiet-harbor", "a"), ("app/scratch", "b"), ("acme/app/scratch", "b")]:
        assert c.resolve(ref).name == f"instances/org1/sessions/{want}"
    with pytest.raises(NotFoundError):
        c.resolve("nothing")


def test_park_field_mask(fake):
    c, f = fake
    c.park(NAME)
    assert list(f.updates[-1].update_mask.paths) == ["park_request"]
    assert f.updates[-1].session.park_request == spb.Session.PARK_REQUEST_PARK


def test_heartbeat_hold_for(fake):
    c, f = fake
    c.heartbeat(NAME, 90.5)
    assert (f.heartbeats[-1].hold_for.seconds, f.heartbeats[-1].hold_for.nanos) == (90, 500_000_000)
    c.heartbeat(NAME)
    assert not f.heartbeats[-1].HasField("hold_for")


def test_repository_session():
    from chainguard_sandbox.workspaces import DEFAULT_ENVIRONMENT, repository_session
    for repo in ("github.com/owner/app", "https://github.com/owner/app.git", "git@github.com:owner/app.git"):
        s = repository_session(repo)
        assert s.source.repository == "https://github.com/owner/app"
        assert s.source.fallback_document == DEFAULT_ENVIRONMENT and s.idle == spb.AUTO_HOLD


def test_resolve_precedence():
    a = spb.Session(name="instances/o/sessions/a", short_id="aaaa", display_name="harbor")
    b = spb.Session(name="instances/o/sessions/b", short_id="bbbb", display_name="x/harbor")
    c = spb.Session(name="instances/o/sessions/c", short_id="cccc", display_name="app/scratch")
    assert resolve_matches([a, b], "harbor") == [a]  # exact beats suffix
    assert resolve_matches([a, b, c], "acme/app/scratch") == [c]  # owner-qualified, as chainctl prints
    assert resolve_matches([a, b, c], "y/z/app/scratch") == []  # not arbitrary prefixes
    assert resolve_matches([a, b, c], "y/z/harbor") == []


@pytest.mark.parametrize("raw,want", [
    ("github.com/owner/app", "https://github.com/owner/app"),
    ("https://x-access-token:T@github.com/owner/app.git", "https://github.com/owner/app"),
    ("ssh://git@github.com:22/owner/app.git", "https://github.com/owner/app"),
    ("git://github.com/owner/app", "https://github.com/owner/app"),
    ("https://GitHub.COM/owner/app/", "https://github.com/owner/app"),
    ("git@github.com:owner/app.git", "https://github.com/owner/app"),
    ("git@gitlab.com:o/r.git", "git@gitlab.com:o/r.git"),
    ("https://tok@gitlab.com/o/r.git", "https://gitlab.com/o/r"),
])
def test_repository_url_normalization(raw, want):
    assert repository_url(raw) == want
    if "gitlab" not in raw:
        assert normalize_github_url(raw) == want


def test_parse_url_ipv6():
    assert _parse_url("http://[::1]:8080") == ("[::1]:8080", False)
    assert _parse_url("https://sessions.chainbox.dev") == ("sessions.chainbox.dev:443", True)


# --- exec -------------------------------------------------------------------


def test_exec(fake):
    c, f = fake
    assert c.output(NAME, ["cat", "/work/notes.md"]) == b"contents of /work/notes.md"
    with pytest.raises(ExitError) as ei:
        c.output(NAME, ["false"])
    assert ei.value.result.exit_code == 3
    with pytest.raises(OutcomeUnknownError):
        c.exec(NAME, ["vanish"])
    c.exec(NAME, ["go", "test"], dir="sub", timeout=60)
    assert list(f.execs[-1].argv) == ["sh", "-c", CD_SCRIPT, "_", "/work/sub", "go", "test"]
    assert f.execs[-1].deadline.seconds == 60
    c.exec(NAME, ["true"], no_dir=True)
    assert list(f.execs[-1].argv) == ["true"]


def test_exec_retries_while_controller_busy(fake):
    c, f = fake
    t = time.time()
    assert c.exec(NAME, ["busy-once"]).exit_code == 0
    assert f.busy_hits == 2 and time.time() - t >= 2


def test_exec_cancels_stream_when_writer_fails(fake):
    c, f = fake
    canceled = threading.Event()

    def flow(req, ctx):
        ctx.add_callback(canceled.set)
        while ctx.is_active():
            yield spb.ExecResponse(stdout=b"x" * 1024)
            time.sleep(0.01)

    f.exec_flow = flow

    class Broken:
        def write(self, b):
            raise BrokenPipeError("consumer went away")

    with pytest.raises(BrokenPipeError):
        c.exec(NAME, ["yes"], stdout=Broken())
    assert canceled.wait(3), "the server stream must be canceled"


def test_write_file_script_refuses_directories(tmp_path):
    target = tmp_path / "dir"
    target.mkdir()
    p = subprocess.run(["sh", "-c", WRITE_FILE_SCRIPT, "_", str(target), "644"], input=b"data", capture_output=True)
    assert p.returncode == 21 and b"is a directory" in p.stderr
    assert list(target.iterdir()) == []  # no .chainguard-sandbox-write.<pid> left inside
    ok = tmp_path / "new" / "file.txt"
    p = subprocess.run(["sh", "-c", WRITE_FILE_SCRIPT, "_", str(ok), "600"], input=b"data", capture_output=True)
    assert p.returncode == 0 and ok.read_bytes() == b"data" and (ok.stat().st_mode & 0o777) == 0o600


def test_write_file_raises_for_directory_and_empty_path(fake):
    c, f = fake

    def flow(req, ctx):
        yield spb.ExecResponse(stderr=b"/work/dir: is a directory\n")
        yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=21))

    f.exec_flow = flow
    with pytest.raises(IsADirectoryError):
        c.write_file(NAME, "dir", b"x")
    for bad in ("", "   "):
        with pytest.raises(ValueError):
            c.write_file(NAME, bad, b"x")


# --- first attach -------------------------------------------------------------


def test_first_attach_uploads_a_gzipd_empty_tree(fake):
    c, f = fake
    complete_first_attach(c, NAME, timeout=10)
    assert [o.name for o in f.opens] == [NAME] and f.opens[0].HasField("resume")
    assert len(f.chunks) == 1
    ch = f.chunks[0]
    assert (ch.last, ch.format, ch.encoding) == (True, _Chunk.FULL_TAR, _Chunk.GZIP)
    assert gzip.decompress(ch.data) == EMPTY_TAR
    assert f.attach_canceled.wait(3), "the client must detach (cancel the stream)"


def test_first_attach_without_gzip_sends_the_raw_tar(fake):
    c, f = fake
    f.attach_mode = "plain"
    c.first_attach(NAME, timeout=10)
    assert len(f.chunks) == 1 and f.chunks[0].data == EMPTY_TAR
    assert f.chunks[0].encoding == _Chunk.ENCODING_UNSPECIFIED


def test_first_attach_returns_when_nothing_is_pending(fake):
    c, f = fake
    f.attach_mode = "ready"
    complete_first_attach(c, NAME, timeout=10)
    assert f.chunks == [] and f.attach_canceled.wait(3)


@pytest.mark.parametrize("mode,match", [("eof", "ended before"), ("detached", "took over")])
def test_first_attach_fails_when_the_stream_ends_early(fake, mode, match):
    c, f = fake
    f.attach_mode = mode
    with pytest.raises(FirstAttachError, match=match):
        complete_first_attach(c, NAME, timeout=10)


def test_first_attach_times_out(fake):
    c, f = fake
    f.attach_mode = "hang"
    t = time.monotonic()
    with pytest.raises(TimeoutError, match="did not complete"):
        complete_first_attach(c, NAME, timeout=0.5)
    assert time.monotonic() - t < 3


# --- the sandbox flow over the fake -------------------------------------------


def test_sandbox_create_run_kill(fake):
    from chainguard_sandbox import Sandbox

    c, f = fake
    sbx = Sandbox.create(packages=["bash"], client=c)
    assert f.created[0].workspace.label == "/chainguard-sandbox/sandbox"
    assert b'- "bash"' in f.created[0].workspace.document
    assert f.created[0].idle == spb.AUTO_HOLD
    assert len(f.chunks) == 1, "create completes the first attach"
    assert sbx.sandbox_id == "3f9a2c1e"
    assert sbx.commands.run("echo hi").stdout == "hi\n"
    assert sbx.get_info().state == "running"
    assert sbx.kill() is True
    assert f.deleted == [NAME]


def test_sandbox_first_attach_errors_are_sandbox_errors(fake):
    from chainguard_sandbox import Sandbox, SandboxError

    c, f = fake
    f.attach_mode = "detached"
    with pytest.raises(SandboxError, match="completing the first attach"):
        Sandbox.create(client=c)
    assert f.deleted == [NAME], "a failed create leaves nothing behind"


# --- tar ----------------------------------------------------------------------


def test_hard_links_are_written_as_regular_files(tmp_path):
    (tmp_path / "one.txt").write_text("hello")
    os.link(tmp_path / "one.txt", tmp_path / "two.txt")
    buf = io.BytesIO()
    tar_dir(buf, str(tmp_path))
    members = tarfile.open(fileobj=io.BytesIO(buf.getvalue())).getmembers()
    assert {m.name: (m.type, m.size) for m in members} == {"one.txt": (tarfile.REGTYPE, 5), "two.txt": (tarfile.REGTYPE, 5)}


def test_symlinked_directories_are_not_followed(tmp_path):
    d = tmp_path / "w"
    d.mkdir()
    outside = tmp_path / "outside"
    outside.mkdir()
    (outside / "f.txt").write_text("OUTSIDE-MARKER\n")
    os.symlink(str(outside), d / "sub")
    buf = io.BytesIO()
    tar_dir(buf, str(d))
    tf = tarfile.open(fileobj=io.BytesIO(buf.getvalue()))
    assert tf.getnames() == ["sub"] and tf.getmember("sub").issym()


def test_tar_headers_match_go_conventions(tmp_path):
    long_dir = "d" * 60
    name = f"{long_dir}/{'n' * 60}.txt"  # 125 bytes: USTAR prefix split, not PAX
    buf = io.BytesIO()
    tw = TarWriter(buf)
    tw.add_bytes(name, b"x", mtime=round_mtime(1000.7))
    tw.close()
    raw = buf.getvalue()
    assert len(raw) == 512 + 512 + 1024, "no PAX header, no record padding"
    assert raw[345:345 + 60] == long_dir.encode() and raw[0:65] == ("n" * 60 + ".txt").encode() + b"\0"
    assert raw[136:148] == b"%011o\0" % 1001  # mtime rounded to the nearest second
    m = tarfile.open(fileobj=io.BytesIO(raw)).getmembers()[0]
    assert (m.name, m.uid, m.gid, m.uname, m.gname) == (name, 0, 0, "", "")


def test_non_ascii_long_names_round_trip(tmp_path):
    name = "dir/" + "é" * 60 + ".txt"
    buf = io.BytesIO()
    tw = TarWriter(buf)
    tw.add_bytes(name, b"data")
    tw.close()
    tf = tarfile.open(fileobj=io.BytesIO(buf.getvalue()))
    assert tf.getnames() == [name] and tf.extractfile(name).read() == b"data"


# --- auth ---------------------------------------------------------------------


def test_jwt_expiry():
    assert jwt_expiry("eyJhbGciOiJub25lIn0.eyJleHAiOjIwMDAwMDAwMDB9.x") == 2000000000


def test_identity_exchange(tmp_path):
    seen = {}

    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            seen["path"], seen["auth"] = self.path, self.headers.get("Authorization")
            ok = seen["auth"] == "Bearer external-oidc"
            body = b'{"token":"chainguard-token"}' if ok else b'{"code":16,"message":"bad token"}'
            self.send_response(200 if ok else 401)
            self.send_header("Content-Type", "application/json")
            self.end_headers()
            self.wfile.write(body)

        def log_message(self, *a):
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    issuer = f"http://127.0.0.1:{srv.server_port}"
    x = IdentityExchange(identity="org/abc123", id_token=lambda: "external-oidc", issuer=issuer)
    assert x.token() == "chainguard-token"
    assert seen["path"].startswith("/sts/exchange?") and "identity=org%2Fabc123" in seen["path"]
    assert "aud=https%3A%2F%2Fsessions.chainbox.dev" in seen["path"]
    with pytest.raises(Exception, match="bad token"):
        IdentityExchange(identity="x", id_token=lambda: "wrong", issuer=issuer).token()
    srv.shutdown()
    p = tmp_path / "tok"
    p.write_text("from-file\n")
    assert file_id_token(str(p))() == "from-file"


def test_auth_rejects_non_object_json_and_redirects():
    assert jwt_expiry("a." + "WzFd" + ".c") is None  # payload "[1]"
    hits = []

    class H(http.server.BaseHTTPRequestHandler):
        def do_POST(self):
            hits.append((self.path, self.headers.get("Authorization")))
            if self.path.startswith("/sts/exchange"):
                self.send_response(302)
                self.send_header("Location", f"http://127.0.0.1:{self.server.server_port}/elsewhere")
                self.end_headers()
                return
            self.send_response(200)
            self.end_headers()
            self.wfile.write(b'["not an object"]')

        def log_message(self, *a):
            pass

    srv = http.server.HTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    try:
        with pytest.raises(AuthError):
            IdentityExchange(identity="i", id_token=lambda: "secret", issuer=f"http://127.0.0.1:{srv.server_port}").token()
        assert [p for p, _ in hits] == ["/sts/exchange?aud=https%3A%2F%2Fsessions.chainbox.dev&identity=i"], "redirect not followed"
    finally:
        srv.shutdown()


def test_calls_without_timeout_fetch_on_the_caller_thread(fake):
    _, f = fake
    seen = []

    class T:
        def token(self):
            seen.append(threading.current_thread())
            return "test-token"

    c2 = Client(url=f.url, instance="org1", token_source=T())
    c2.raw.GetSession(spb.GetSessionRequest(name=NAME))
    assert seen == [threading.current_thread()]
    c2.close()


def test_slow_token_is_bounded_by_the_call_timeout(fake):
    _, f = fake

    class Slow:
        def token(self):
            time.sleep(5)
            return "test-token"

    c2 = Client(url=f.url, instance="org1", token_source=Slow())
    t = time.monotonic()
    with pytest.raises(AuthTimeoutError):
        c2.raw.GetSession(spb.GetSessionRequest(name=NAME), timeout=0.4)
    assert time.monotonic() - t < 2
    c2.close()


# --- review findings: cleanup, deadlines, interrupted commands, output ---------


def _handle(c):
    """A sandbox handle on the fake without the create flow."""
    from chainguard_sandbox import Sandbox

    return Sandbox(c, spb.Session(name=NAME, short_id="3f9a2c1e"), None, False)


def test_kill_by_id_deletes_without_attaching(fake):
    from chainguard_sandbox import Sandbox

    c, f = fake
    f.attach_mode = "detached"  # the guest can't be attached to
    assert f.pending
    assert Sandbox.kill(NAME, client=c) is True
    assert f.deleted == [NAME] and f.opens == [], "one delete, no attach"
    assert Sandbox.kill(NAME, client=c) is False, "already gone"


def test_start_timeout_bounds_each_call(fake):
    from chainguard_sandbox import Sandbox, TimeoutError

    c, f = fake

    def slow_start(req, ctx):  # answers "started", but only after the limit
        time.sleep(0.5)
        return spb.Session(name=req.name, guest_started_time=timestamp_pb2.Timestamp(seconds=1))

    f.get_flow = slow_start
    t = time.monotonic()
    with pytest.raises(TimeoutError, match="did not start within"):
        Sandbox.create(client=c, start_timeout=0.05)
    assert time.monotonic() - t < 0.4, "the stalled call was cut off at the deadline"
    assert f.deleted == [NAME], "the sandbox that didn't start in time is deleted"


def test_kill_reports_unconfirmed_deletion(fake, monkeypatch):
    from chainguard_sandbox import TimeoutError, _sandbox

    monkeypatch.setattr(_sandbox, "_KILL_WAIT", 0.3)
    monkeypatch.setattr(_sandbox, "_KILL_POLL", 0.02)
    c, f = fake
    f.get_flow = lambda req, ctx: spb.Session(name=req.name)  # never goes away
    with pytest.raises(TimeoutError, match="still exists"):
        _handle(c).kill()
    assert f.deleted == [NAME]

    def failing(req, ctx):
        ctx.abort(grpc.StatusCode.INTERNAL, "boom")

    f.get_flow = failing  # checks that fail are not taken as "gone" either
    with pytest.raises(TimeoutError, match="boom"):
        _handle(c).kill()
    f.get_flow = None  # the fake now answers NOT_FOUND for the deleted name
    assert _handle(c).kill() is True


def test_context_manager_keeps_the_blocks_error(fake, monkeypatch):
    from chainguard_sandbox import _sandbox

    monkeypatch.setattr(_sandbox, "_KILL_WAIT", 0.1)
    monkeypatch.setattr(_sandbox, "_KILL_POLL", 0.02)
    c, f = fake
    f.get_flow = lambda req, ctx: spb.Session(name=req.name)
    with pytest.raises(ZeroDivisionError):
        with _handle(c):
            1 / 0
    assert f.deleted == [NAME]


def test_interrupted_command_is_outcome_unknown(fake):
    from chainguard_sandbox import OutcomeUnknownError as SandboxOutcomeUnknown
    from chainguard_sandbox import SandboxError

    c, f = fake

    def dropped(req, ctx):
        yield spb.ExecResponse(stdout=b"partial")
        ctx.abort(grpc.StatusCode.UNAVAILABLE, "connection lost")

    f.exec_flow = dropped
    with pytest.raises(OutcomeUnknownError) as lib:
        c.exec(NAME, ["x"])
    assert lib.value.cause.code() == grpc.StatusCode.UNAVAILABLE
    with pytest.raises(SandboxOutcomeUnknown, match="connection lost") as sb:
        _handle(c).commands.run("do-something")
    assert sb.value.cause.code() == grpc.StatusCode.UNAVAILABLE, "the transport error is preserved"

    def refused(req, ctx):  # an answer from the plane: the command did not run
        ctx.abort(grpc.StatusCode.INVALID_ARGUMENT, "bad argv")
        yield

    f.exec_flow = refused
    with pytest.raises(SandboxError) as e:
        _handle(c).commands.run("x")
    assert not isinstance(e.value, SandboxOutcomeUnknown) and "INVALID_ARGUMENT" in str(e.value)


def test_callbacks_get_whole_characters(fake):
    c, f = fake
    euro = "€".encode()  # three bytes

    def split(req, ctx):
        yield spb.ExecResponse(stdout=b"a" + euro[:1])
        yield spb.ExecResponse(stdout=euro[1:2])
        yield spb.ExecResponse(stdout=euro[2:] + b"b")
        yield spb.ExecResponse(stderr=euro[:2])  # incomplete at the very end
        yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=0))

    f.exec_flow = split
    out, err = [], []
    r = _handle(c).commands.run("x", on_stdout=out.append, on_stderr=err.append)
    assert r.stdout == "a€b"
    assert out == ["a", "€b"], "the euro sign arrives whole"
    assert err == ["\ufffd"], "an incomplete character at the end is still delivered"


def _background(c, f, content: bytes, exit_code: str = "-", delay: float = 0.0):
    """A background-command handle whose poll script the fake answers from content."""
    from chainguard_sandbox._sandbox import CommandHandle

    def poll(req, ctx):  # argv: sh -c <poll script> _ <dir> <outOff> <errOff> <wait> <max>
        off = int(req.argv[5])
        time.sleep(delay)
        yield spb.ExecResponse(stdout=f"{exit_code} {len(content)} 0\n".encode() + content[off:])
        yield spb.ExecResponse(exited=spb.ExecResponse.Exited(code=0))

    f.exec_flow = poll
    return CommandHandle(_handle(c), 7, "x", "/tmp/x")


def test_concurrent_polls_do_not_duplicate_output(fake):
    c, f = fake
    h = _background(c, f, b"abc", delay=0.1)  # long enough for the polls to overlap
    threads = [threading.Thread(target=h.poll) for _ in range(4)]
    for t in threads:
        t.start()
    for t in threads:
        t.join()
    assert h.stdout == "abc"


def test_wait_callbacks_get_whole_characters_and_earlier_output(fake):
    c, f = fake
    h = _background(c, f, "é€".encode()[:3])  # "é" and the first byte of "€" so far
    assert h.poll() is None
    h2 = _background(c, f, "é€".encode(), exit_code="0")
    h2._out, h2._err = h._out, h._err  # the same command, now finished
    out = []
    assert h2.wait(on_stdout=out.append).stdout == "é€"
    assert out == ["é", "€"], "earlier output first, and no split character"
