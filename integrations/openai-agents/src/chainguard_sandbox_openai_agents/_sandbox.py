"""Chainguard Workspaces as a sandbox provider for the OpenAI Agents SDK.

A sandbox here is one Chainguard Workspaces session (a Linux microVM), created and driven through the
Simple Chainguard Sandbox SDK. The Agents SDK's runner calls this client to create, resume and delete
sessions, and the session to run commands, read and write files, and snapshot the workspace.

The Chainguard SDK is synchronous (gRPC); every call runs in a worker thread so the event loop
stays free.
"""

from __future__ import annotations

import asyncio
import gzip
import io
import shlex
import uuid
from pathlib import Path
from typing import Literal

import grpc
from agents.sandbox.errors import (
    ConfigurationError,
    ErrorCode,
    ExecTimeoutError,
    ExecTransportError,
    MountConfigError,
    WorkspaceArchiveReadError,
    WorkspaceArchiveWriteError,
    WorkspaceReadNotFoundError,
    WorkspaceStartError,
    WorkspaceWriteTypeError,
)
from agents.sandbox.manifest import Manifest
from agents.sandbox.session import SandboxSession, SandboxSessionState
from agents.sandbox.session.base_sandbox_session import BaseSandboxSession
from agents.sandbox.session.dependencies import Dependencies
from agents.sandbox.session.manager import Instrumentation
from agents.sandbox.session.runtime_helpers import RESOLVE_WORKSPACE_PATH_HELPER, RuntimeHelperScript
from agents.sandbox.session.sandbox_client import BaseSandboxClient, BaseSandboxClientOptions
from agents.sandbox.session.tar_workspace import shell_tar_exclude_args
from agents.sandbox.snapshot import SnapshotBase, SnapshotSpec, resolve_snapshot
from agents.sandbox.types import ExecResult, User
from agents.sandbox.util.tar_utils import UnsafeTarMemberError, validate_tar_bytes
from pydantic import ConfigDict, Field

from chainguard_sandbox import FileNotFoundError as SandboxFileNotFoundError
from chainguard_sandbox import Sandbox, SandboxError, SandboxNotFoundError
from chainguard_sandbox.workspaces import Client, OutcomeUnknownError

#: The backend ID and the type name of the options and the saved state.
BACKEND_ID = "chainguard_workspaces"

#: Where the agent's workspace lives in the sandbox (the Workspaces working directory).
DEFAULT_CHAINGUARD_WORKSPACE_ROOT = "/work"

_BACKEND = {"backend": BACKEND_ID}


def _config_error(message: str, op: str = "start", **context: object) -> ConfigurationError:
    return ConfigurationError(
        message=message,
        error_code=ErrorCode.SANDBOX_CONFIG_INVALID,
        op=op,  # pyright: ignore[reportArgumentType]
        context={**_BACKEND, **context},
    )


class ChainguardSandboxClientOptions(BaseSandboxClientOptions):
    """How to create a Chainguard Workspaces sandbox.

    packages: Chainguard (Wolfi) package names, e.g. ``["python-3.13", "nodejs-22", "git"]``.
        None means the Chainguard SDK's defaults (bash, coreutils, curl, git, jq, Python, Node.js).
    egress / egress_policy: the outbound allowlist (hostnames, ``"*"`` for all; None means open)
        and what happens to a connection outside it: "deny" (refused) or "destroy" (the sandbox
        is torn down).
    env_vars: environment variables for every command (the manifest's environment is added on top).
    name: a label; the sandbox's display name becomes ``<name>/<word-word>``.
    source / source_revision: start from a GitHub repository instead of an empty workspace.
    start_timeout: seconds to wait for the sandbox to start.
    pause_on_exit: pause (snapshot and stop) the sandbox when the runner is done with it, instead
        of deleting it. A paused sandbox resumes on the next run that resumes this session, and is
        discarded by Chainguard after 5 days.
    """

    # Unknown fields are refused, so a misspelled setting can't be silently dropped.
    model_config = ConfigDict(**{**BaseSandboxClientOptions.model_config, "extra": "forbid"})

    type: Literal["chainguard_workspaces"] = "chainguard_workspaces"  # pyright: ignore[reportIncompatibleVariableOverride]
    packages: list[str] | None = None
    egress: list[str] | None = None
    egress_policy: Literal["deny", "destroy"] = "deny"
    env_vars: dict[str, str] | None = None
    name: str = "agents"
    source: str | None = None
    source_revision: str = ""
    start_timeout: float = Field(default=180.0, gt=0)
    pause_on_exit: bool = False


class ChainguardSandboxSessionState(SandboxSessionState):
    """What resuming a Chainguard sandbox needs: its session's resource name, and how to recreate
    it if it is gone."""

    type: Literal["chainguard_workspaces"] = "chainguard_workspaces"  # pyright: ignore[reportIncompatibleVariableOverride]
    session_name: str
    sandbox_id: str = ""
    packages: list[str] | None = None
    egress: list[str] | None = None
    egress_policy: Literal["deny", "destroy"] = "deny"
    base_env_vars: dict[str, str] = Field(default_factory=dict)
    name: str = "agents"
    source: str | None = None
    source_revision: str = ""
    start_timeout: float = 180.0
    pause_on_exit: bool = False


async def _thread(fn, /, *args, **kwargs):
    return await asyncio.to_thread(fn, *args, **kwargs)


def _reject_user(user: str | User | None, op: str) -> None:
    name = user.name if isinstance(user, User) else user
    if name not in (None, "", "root"):
        raise _config_error(
            "Chainguard Workspaces sandboxes run everything as root; "
            f"`{op}` can't run as user {name!r}",
            op=op,
            user=name,
        )


class ChainguardSandboxSession(BaseSandboxSession):
    """One Chainguard Workspaces sandbox, as an Agents SDK sandbox session."""

    state: ChainguardSandboxSessionState  # pyright: ignore[reportIncompatibleVariableOverride]
    _sbx: Sandbox

    def __init__(self, *, state: ChainguardSandboxSessionState, sandbox: Sandbox) -> None:
        self.state = state  # pyright: ignore[reportIncompatibleVariableOverride]
        self._sbx = sandbox

    @property
    def sandbox(self) -> Sandbox:
        """The Chainguard SDK sandbox behind this session."""
        return self._sbx

    @property
    def _client(self) -> Client:
        return self._sbx.client

    # --- capabilities -----------------------------------------------------------------------

    def supports_pty(self) -> bool:
        # The SDK's Exec sends stdin up front; there is no interactive terminal to attach to.
        return False

    def _prepare_exec_command(self, *command, shell, user) -> list[str]:
        _reject_user(user, "exec")
        return super()._prepare_exec_command(*command, shell=shell, user=None)

    def _runtime_helpers(self) -> tuple[RuntimeHelperScript, ...]:
        return (RESOLVE_WORKSPACE_PATH_HELPER,)

    async def _validate_path_access(self, path: Path | str, *, for_write: bool = False) -> Path:
        return await self._validate_remote_path_access(path, for_write=for_write)

    # --- lifecycle --------------------------------------------------------------------------

    async def _prepare_backend_workspace(self) -> None:
        root = self.state.manifest.root
        try:
            out = await _thread(
                self._client.exec, self.state.session_name, ["mkdir", "-p", "--", root], no_dir=True, timeout=60
            )
        except (grpc.RpcError, OutcomeUnknownError) as e:
            raise WorkspaceStartError(
                path=Path(root), context={**_BACKEND, "reason": "workspace_root_setup_failed"}, cause=e
            ) from e
        if out.exit_code != 0:
            raise WorkspaceStartError(
                path=Path(root),
                context={**_BACKEND, "reason": "workspace_root_nonzero_exit", "exit_code": out.exit_code},
            )

    async def running(self) -> bool:
        try:
            return await _thread(lambda: self._sbx.get_info().state in ("running", "paused"))
        except Exception:
            return False

    async def _shutdown_backend(self) -> None:
        try:
            if self.state.pause_on_exit:
                await _thread(self._sbx.pause)
            else:
                await _thread(self._sbx.kill)
        except Exception:
            pass

    # --- commands ---------------------------------------------------------------------------

    async def _resolved_envs(self) -> dict[str, str]:
        manifest_envs = await self.state.manifest.environment.resolve()
        return {**self.state.base_env_vars, **manifest_envs}

    async def _exec_internal(self, *command: str | Path, timeout: float | None = None) -> ExecResult:
        argv = [str(c) for c in command]
        envs = await self._resolved_envs()
        if envs:
            argv = ["env", *(f"{k}={v}" for k, v in envs.items()), *argv]
        out, err = io.BytesIO(), io.BytesIO()
        plane_timeout = 0.0 if timeout is None else max(float(timeout), 0.001)
        try:
            res = await _thread(
                self._client.exec,
                self.state.session_name,
                argv,
                dir=self.state.manifest.root,
                timeout=plane_timeout,
                stdout=out,
                stderr=err,
            )
        except OutcomeUnknownError as e:
            raise ExecTransportError(
                command=command,
                context={**_BACKEND, "reason": "outcome_unknown"},
                cause=e,
                message="the connection ended before the command reported an exit status; it may have run",
                retryable=False,
            ) from e
        except grpc.RpcError as e:
            code = e.code()  # pyright: ignore[reportAttributeAccessIssue]
            raise ExecTransportError(
                command=command,
                context={**_BACKEND, "grpc_code": code.name if code else None, "detail": e.details()},  # pyright: ignore[reportAttributeAccessIssue]
                cause=e,
                retryable=code in (grpc.StatusCode.UNAVAILABLE, grpc.StatusCode.FAILED_PRECONDITION),
            ) from e
        if res.timed_out:
            raise ExecTimeoutError(command=command, timeout_s=timeout, context=_BACKEND)
        return ExecResult(stdout=out.getvalue(), stderr=err.getvalue(), exit_code=res.exit_code)

    # --- files ------------------------------------------------------------------------------

    async def read(self, path: Path | str, *, user: str | User | None = None) -> io.IOBase:
        _reject_user(user, "read")
        workspace_path = await self._validate_path_access(path)
        try:
            data = await _thread(self._sbx.files.read, workspace_path.as_posix(), format="bytes")
        except SandboxFileNotFoundError as e:
            raise WorkspaceReadNotFoundError(path=workspace_path, cause=e) from e
        except (SandboxError, grpc.RpcError, OutcomeUnknownError) as e:
            raise WorkspaceArchiveReadError(path=workspace_path, context=_BACKEND, cause=e) from e
        return io.BytesIO(data)

    async def write(self, path: Path | str, data: io.IOBase, *, user: str | User | None = None) -> None:
        _reject_user(user, "write")
        payload = data.read()
        if isinstance(payload, str):
            payload = payload.encode("utf-8")
        if not isinstance(payload, bytes | bytearray):
            raise WorkspaceWriteTypeError(path=Path(str(path)), actual_type=type(payload).__name__)
        workspace_path = await self._validate_path_access(path, for_write=True)
        try:
            await _thread(self._sbx.files.write, workspace_path.as_posix(), bytes(payload))
        except (SandboxError, grpc.RpcError, OutcomeUnknownError) as e:
            raise WorkspaceArchiveWriteError(path=workspace_path, context=_BACKEND, cause=e) from e

    # --- workspace snapshots ----------------------------------------------------------------

    async def persist_workspace(self) -> io.IOBase:
        root = self._workspace_root_path()
        excludes = " ".join(shell_tar_exclude_args(self._persist_workspace_skip_relpaths()))
        script = f"tar {excludes} -C {shlex.quote(root.as_posix())} -cf - .".replace("  ", " ")
        try:
            raw = await _thread(self._client.output, self.state.session_name, ["sh", "-c", script], no_dir=True)
        except Exception as e:
            raise WorkspaceArchiveReadError(path=root, context={**_BACKEND, "reason": "tar_failed"}, cause=e) from e
        return io.BytesIO(raw)

    async def hydrate_workspace(self, data: io.IOBase) -> None:
        root = self._workspace_root_path()
        payload = data.read()
        if isinstance(payload, str):
            payload = payload.encode("utf-8")
        if not isinstance(payload, bytes | bytearray):
            raise WorkspaceWriteTypeError(path=root, actual_type=type(payload).__name__)
        try:
            validate_tar_bytes(bytes(payload), allow_external_symlink_targets=False)
        except UnsafeTarMemberError as e:
            raise WorkspaceArchiveWriteError(
                path=root,
                context={**_BACKEND, "reason": "unsafe_or_invalid_tar", "member": e.member, "detail": str(e)},
                cause=e,
            ) from e
        # Staged as a file (the SDK splits large uploads), then unpacked in place.
        tmp = f"/tmp/.chainguard-sandbox/agents-hydrate-{uuid.uuid4().hex}.tar.gz"
        script = 'mkdir -p -- "$2" && tar -xzf "$1" -C "$2"; s=$?; rm -f -- "$1"; exit $s'
        try:
            await _thread(self._sbx.files.write, tmp, gzip.compress(bytes(payload)))
            await _thread(
                self._client.output, self.state.session_name, ["sh", "-c", script, "_", tmp, root.as_posix()], no_dir=True
            )
        except Exception as e:
            raise WorkspaceArchiveWriteError(path=root, context={**_BACKEND, "reason": "tar_extract_failed"}, cause=e) from e


def _validate_manifest(manifest: Manifest) -> None:
    if manifest.mount_targets():
        raise MountConfigError(
            message="Chainguard Workspaces sandboxes don't support mounts; remove the mount entries from the manifest",
            context=_BACKEND,
        )
    if manifest.users or manifest.groups:
        raise _config_error(
            "Chainguard Workspaces sandboxes run everything as root; the manifest can't declare users or groups"
        )


class ChainguardSandboxClient(BaseSandboxClient[ChainguardSandboxClientOptions]):
    """Creates, resumes and deletes Chainguard Workspaces sandboxes for the OpenAI Agents SDK.

    >>> from agents import RunConfig
    >>> from agents.sandbox import SandboxRunConfig
    >>> run_config = RunConfig(sandbox=SandboxRunConfig(
    ...     client=ChainguardSandboxClient(),
    ...     options=ChainguardSandboxClientOptions(packages=["bash", "coreutils", "python-3.13"]),
    ... ))

    instance: the Chainguard organization ID. If None, ``$CHAINCTL_SANDBOX_INSTANCE`` is used, and
        failing that the only organization your login belongs to.
    client: a Chainguard SDK client to use instead (for a custom token source, for example). The
        caller then owns it and closes it.

    Logging in works as in the Chainguard SDK: the saved login (``chainguard-sandbox login``),
    ``$CHAINCTL_AUTH_TOKEN``, or a browser login on first use.
    """

    backend_id = BACKEND_ID

    def __init__(
        self,
        *,
        instance: str | None = None,
        client: Client | None = None,
        instrumentation: Instrumentation | None = None,
        dependencies: Dependencies | None = None,
    ) -> None:
        self._instance = instance
        self._client = client
        self._owns_client = client is None
        self._client_lock = asyncio.Lock()
        self._instrumentation = instrumentation if instrumentation is not None else Instrumentation()
        self._dependencies = dependencies

    async def _get_client(self) -> Client:
        async with self._client_lock:
            if self._client is None:
                # Connecting may detect the organization (one API call) or log in.
                self._client = await _thread(Client, instance=self._instance)
            return self._client

    async def close(self) -> None:
        """Close the connection this client opened (not one passed in)."""
        if self._owns_client and self._client is not None:
            c, self._client = self._client, None
            await _thread(c.close)

    async def __aenter__(self) -> ChainguardSandboxClient:
        return self

    async def __aexit__(self, *_: object) -> None:
        await self.close()

    async def _create_sandbox(self, state: ChainguardSandboxSessionState) -> Sandbox:
        c = await self._get_client()
        return await _thread(
            Sandbox.create,
            packages=state.packages,
            egress=state.egress,
            egress_policy=state.egress_policy,
            name=state.name,
            source=state.source,
            source_revision=state.source_revision,
            start_timeout=state.start_timeout,
            client=c,
        )

    async def create(
        self,
        *,
        snapshot: SnapshotSpec | SnapshotBase | None = None,
        manifest: Manifest | None = None,
        options: ChainguardSandboxClientOptions,
    ) -> SandboxSession:
        if not isinstance(options, ChainguardSandboxClientOptions):
            raise _config_error(f"ChainguardSandboxClient needs ChainguardSandboxClientOptions, not {type(options).__name__}")
        if manifest is None:
            manifest = Manifest(root=DEFAULT_CHAINGUARD_WORKSPACE_ROOT)
        self._validate_manifest_for_create(manifest)
        _validate_manifest(manifest)

        session_id = uuid.uuid4()
        state = ChainguardSandboxSessionState(
            session_id=session_id,
            manifest=manifest,
            snapshot=resolve_snapshot(snapshot, str(session_id)),
            session_name="",
            packages=options.packages,
            egress=options.egress,
            egress_policy=options.egress_policy,
            base_env_vars=dict(options.env_vars or {}),
            name=options.name,
            source=options.source,
            source_revision=options.source_revision,
            start_timeout=options.start_timeout,
            pause_on_exit=options.pause_on_exit,
        )
        sbx = await self._create_sandbox(state)
        state.session_name, state.sandbox_id = sbx.name, sbx.sandbox_id
        inner = ChainguardSandboxSession(state=state, sandbox=sbx)
        return self._wrap_session(inner, instrumentation=self._instrumentation)

    async def delete(self, session: SandboxSession) -> SandboxSession:
        inner = session._inner
        if not isinstance(inner, ChainguardSandboxSession):
            raise TypeError("ChainguardSandboxClient.delete expects a ChainguardSandboxSession")
        try:
            await inner.shutdown()
        except Exception:
            pass
        return session

    async def resume(self, state: SandboxSessionState) -> SandboxSession:
        if not isinstance(state, ChainguardSandboxSessionState):
            raise TypeError("ChainguardSandboxClient.resume expects a ChainguardSandboxSessionState")
        state.assert_path_grants_rebound()
        _validate_manifest(state.manifest)
        c = await self._get_client()
        sbx: Sandbox | None = None
        if state.session_name:
            try:
                # A paused sandbox resumes on its first command.
                sbx = await _thread(Sandbox.connect, state.session_name, client=c)
            except SandboxNotFoundError:
                sbx = None
        reconnected = sbx is not None
        if sbx is None:
            sbx = await self._create_sandbox(state)
            state.session_name, state.sandbox_id = sbx.name, sbx.sandbox_id
            state.workspace_root_ready = False
        inner = ChainguardSandboxSession(state=state, sandbox=sbx)
        inner._set_start_state_preserved(reconnected, system=reconnected)
        return self._wrap_session(inner, instrumentation=self._instrumentation)

    def deserialize_session_state(self, payload: dict[str, object]) -> SandboxSessionState:
        return self._deserialize_session_state_payload(payload, ChainguardSandboxSessionState)
