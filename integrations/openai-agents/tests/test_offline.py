"""Configuration, serialization, refusals and error mapping, without the service.

The Chainguard sandbox is replaced by a small stand-in; the live behavior is in test_live.py.
"""

import io
import subprocess
import sys
import tarfile
from dataclasses import dataclass, field
from pathlib import Path

import grpc
import pytest
from agents.sandbox import Manifest
from agents.sandbox.errors import (
    ConfigurationError,
    ExecTimeoutError,
    ExecTransportError,
    WorkspaceArchiveWriteError,
    WorkspaceReadNotFoundError,
    WorkspaceWriteTypeError,
)
from agents.sandbox.session import BaseSandboxClientOptions, SandboxSessionState
from agents.sandbox.snapshot import NoopSnapshot
from agents.sandbox.types import User
from chainguard_sandbox import FileNotFoundError as SandboxFileNotFoundError
from chainguard_sandbox.workspaces import ExecResult, OutcomeUnknownError
from pydantic import ValidationError

from chainguard_sandbox_openai_agents import (
    BACKEND_ID,
    ChainguardSandboxClient,
    ChainguardSandboxClientOptions,
    ChainguardSandboxSession,
    ChainguardSandboxSessionState,
)


def state(**kw) -> ChainguardSandboxSessionState:
    return ChainguardSandboxSessionState(
        manifest=Manifest(root="/work"),
        snapshot=NoopSnapshot(id="s"),
        session_name="instances/XXXXXX/sessions/0123abcd",
        sandbox_id="0123abcd",
        **kw,
    )


# --- options and state ------------------------------------------------------------------------


def test_options_parse_from_json_and_refuse_unknown_fields():
    o = BaseSandboxClientOptions.parse({"type": "chainguard_workspaces", "packages": ["bash"], "pause_on_exit": True})
    assert isinstance(o, ChainguardSandboxClientOptions)
    assert (o.packages, o.pause_on_exit, o.egress_policy) == (["bash"], True, "deny")
    with pytest.raises(ValidationError):
        ChainguardSandboxClientOptions(pakages=["bash"])  # pyright: ignore[reportCallIssue]
    with pytest.raises(ValidationError):
        ChainguardSandboxClientOptions(egress_policy="allow")  # pyright: ignore[reportArgumentType]
    with pytest.raises(ValidationError):
        ChainguardSandboxClientOptions(start_timeout=0)


def test_state_round_trips_through_the_client():
    c = ChainguardSandboxClient()
    s = state(packages=["bash"], base_env_vars={"A": "1"}, pause_on_exit=True)
    payload = c.serialize_session_state(s)
    assert payload["type"] == BACKEND_ID
    back = c.deserialize_session_state(payload)
    assert isinstance(back, ChainguardSandboxSessionState)
    assert (back.session_name, back.packages, back.base_env_vars, back.pause_on_exit) == (
        s.session_name, ["bash"], {"A": "1"}, True)
    assert isinstance(SandboxSessionState.parse(payload), ChainguardSandboxSessionState)


def test_types_register_in_a_fresh_interpreter_once_imported():
    code = (
        "import chainguard_sandbox_openai_agents\n"
        "from agents.sandbox.session import BaseSandboxClientOptions\n"
        "o = BaseSandboxClientOptions.parse({'type': 'chainguard_workspaces', 'name': 'x'})\n"
        "print(type(o).__name__)\n"
    )
    out = subprocess.run([sys.executable, "-c", code], capture_output=True, text=True, check=True).stdout
    assert out.strip() == "ChainguardSandboxClientOptions"


# --- refused before anything is created --------------------------------------------------------


class OtherOptions(BaseSandboxClientOptions):
    type: str = "another_provider_for_tests"


async def test_create_refuses_unsupported_input_before_connecting():
    c = ChainguardSandboxClient()
    with pytest.raises(ConfigurationError):
        await c.create(options=OtherOptions())  # pyright: ignore[reportArgumentType]
    with pytest.raises(ConfigurationError, match="root"):
        await c.create(manifest=Manifest(root="/work", users=[User(name="alice")]), options=ChainguardSandboxClientOptions())
    assert c._client is None, "nothing was connected or created"


async def test_resume_refuses_another_providers_state():
    with pytest.raises(TypeError):
        await ChainguardSandboxClient().resume(object())  # pyright: ignore[reportArgumentType]


# --- the session, against a stand-in sandbox ---------------------------------------------------


@dataclass
class FakeClient:
    result: ExecResult = field(default_factory=lambda: ExecResult(0))
    stdout: bytes = b"out"
    stderr: bytes = b"err"
    raises: BaseException | None = None
    calls: list = field(default_factory=list)

    def exec(self, name, argv, **kw):
        self.calls.append((name, argv, kw))
        if self.raises:
            raise self.raises
        if kw.get("stdout"):
            kw["stdout"].write(self.stdout)
        if kw.get("stderr"):
            kw["stderr"].write(self.stderr)
        return self.result


@dataclass
class FakeFiles:
    files: dict = field(default_factory=dict)

    def read(self, path, format="text"):
        if path not in self.files:
            raise SandboxFileNotFoundError("path", path)
        return self.files[path]

    def write(self, path, data):
        self.files[path] = data


@dataclass
class FakeSandbox:
    client: FakeClient = field(default_factory=FakeClient)
    files: FakeFiles = field(default_factory=FakeFiles)
    name: str = "instances/XXXXXX/sessions/0123abcd"
    sandbox_id: str = "0123abcd"


def session(fake: FakeSandbox | None = None, **kw) -> ChainguardSandboxSession:
    return ChainguardSandboxSession(state=state(**kw), sandbox=fake or FakeSandbox())  # pyright: ignore[reportArgumentType]


async def test_exec_runs_in_the_workspace_root_with_the_environment():
    fake = FakeSandbox()
    s = session(fake, base_env_vars={"MODE": "test"})
    r = await s._exec_internal("ls", "-la", timeout=5)
    assert (r.stdout, r.stderr, r.exit_code) == (b"out", b"err", 0)
    name, argv, kw = fake.client.calls[0]
    assert argv == ["env", "MODE=test", "ls", "-la"]
    assert (kw["dir"], kw["timeout"]) == ("/work", 5.0)
    await s._exec_internal("true")
    assert fake.client.calls[1][2]["timeout"] == 0.0, "no timeout means no deadline"


async def test_exec_errors_map_to_the_sdk_types():
    fake = FakeSandbox()
    s = session(fake)
    fake.client.result = ExecResult(124, timed_out=True)
    with pytest.raises(ExecTimeoutError):
        await s._exec_internal("sleep", "9", timeout=1)
    fake.client.raises = OutcomeUnknownError()
    with pytest.raises(ExecTransportError) as e:
        await s._exec_internal("x")
    assert e.value.retryable is False, "the command may have run: not safe to retry"

    class Unavailable(grpc.RpcError):
        def code(self):
            return grpc.StatusCode.UNAVAILABLE

        def details(self):
            return "connection reset"

    fake.client.raises = Unavailable()
    with pytest.raises(ExecTransportError) as e:
        await s._exec_internal("x")
    assert e.value.retryable is True


async def test_users_and_terminals_are_refused():
    s = session()
    assert s.supports_pty() is False
    with pytest.raises(ConfigurationError):
        await s.exec("id", user="alice")
    with pytest.raises(ConfigurationError):
        await s.read(Path("/work/x"), user=User(name="alice"))
    assert s._prepare_exec_command("id", shell=False, user="root") == ["id"]


class NotBytes(io.RawIOBase):
    def read(self, size=-1):  # pyright: ignore[reportIncompatibleMethodOverride]
        return 123


async def test_files_map_errors_and_types(monkeypatch):
    fake = FakeSandbox()
    s = session(fake)

    async def no_remote_check(path, *, for_write=False):  # the path helper runs remotely; tested live
        return s.normalize_path(path, for_write=for_write)

    monkeypatch.setattr(s, "_validate_path_access", no_remote_check)
    await s.write(Path("notes/a.txt"), io.BytesIO(b"hi"))
    assert fake.files.files == {"/work/notes/a.txt": b"hi"}
    assert (await s.read(Path("notes/a.txt"))).read() == b"hi"
    with pytest.raises(WorkspaceReadNotFoundError):
        await s.read(Path("missing.txt"))
    with pytest.raises(WorkspaceWriteTypeError):
        await s.write(Path("x"), NotBytes())


async def test_hydrate_refuses_an_unsafe_archive():
    buf = io.BytesIO()
    with tarfile.open(fileobj=buf, mode="w") as tf:
        info = tarfile.TarInfo("../escape.txt")
        info.size = 1
        tf.addfile(info, io.BytesIO(b"x"))
    fake = FakeSandbox()
    with pytest.raises(WorkspaceArchiveWriteError):
        await session(fake).hydrate_workspace(io.BytesIO(buf.getvalue()))
    assert not fake.files.files and not fake.client.calls, "nothing reached the sandbox"
