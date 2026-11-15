"""Exec helpers and errors."""

from __future__ import annotations

import posixpath
from dataclasses import dataclass

import grpc

DEFAULT_WORK_DIR = "/work"
MAX_EXEC_STDIN = 16 << 20
#: How chainctl implements `exec --cwd`.
CD_SCRIPT = 'cd "$1" && shift && exec "$@"'


@dataclass
class ExecResult:
    exit_code: int
    timed_out: bool = False


class OutcomeUnknownError(Exception):
    """The exec stream ended, or failed, without an exit status (chainctl
    reports 255): the command may have run, in part or in full. ``cause`` is
    the transport error that cut it short, if there was one."""

    def __init__(self, result: ExecResult | None = None, cause: grpc.RpcError | None = None):
        msg = "exec ended without an exit status"
        if cause is not None:
            msg += f": {cause.code().name}: {cause.details()}"  # pyright: ignore[reportAttributeAccessIssue]
        super().__init__(msg)
        self.result = result or ExecResult(255)
        self.cause = cause


#: Failures of the connection itself: the request may still have reached the plane.
_TRANSPORT_CODES = frozenset({
    grpc.StatusCode.UNAVAILABLE, grpc.StatusCode.INTERNAL, grpc.StatusCode.UNKNOWN,
    grpc.StatusCode.DATA_LOSS, grpc.StatusCode.DEADLINE_EXCEEDED, grpc.StatusCode.CANCELLED,
})


def outcome_unknown(err: grpc.RpcError, started: bool) -> bool:
    """Whether an error from the exec stream leaves the command's outcome
    unknown. Once the stream has delivered an event the command was running,
    so any failure does. Before that only a failure of the connection does; an
    answer from the plane (not found, held, invalid, over the limit, ...)
    means the command did not run."""
    return started or err.code() in _TRANSPORT_CODES  # pyright: ignore[reportAttributeAccessIssue]


class ExitError(Exception):
    """A non-zero exit from output()/shell()."""

    def __init__(self, result: ExecResult, stdout: bytes, stderr: bytes):
        msg = stderr.decode(errors="replace").strip()
        super().__init__(f"command timed out: {msg}" if result.timed_out else f"command exited {result.exit_code}: {msg}")
        self.result, self.stdout, self.stderr = result, stdout, stderr

    @property
    def exit_code(self) -> int:
        return self.result.exit_code


def guest_path(p: str) -> str:
    """p resolved against /work."""
    return posixpath.normpath(p) if posixpath.isabs(p) else posixpath.join(DEFAULT_WORK_DIR, p)


def with_dir(dir: str, argv: list[str]) -> list[str]:
    return ["sh", "-c", CD_SCRIPT, "_", guest_path(dir), *argv]


def is_guest_held(err: BaseException) -> bool:
    """Whether err is the plane refusing a call because another controller
    holds the guest: an attached shell still heartbeating (ABORTED), or
    FAILED_PRECONDITION "instance held by another controller" right after
    another client's attach or exec ended."""
    if not isinstance(err, grpc.RpcError):
        return False
    msg = err.details() or ""
    if err.code() == grpc.StatusCode.ABORTED:
        return "held" in msg
    # Briefly after another client's attach or exec ends (observed live).
    return err.code() == grpc.StatusCode.FAILED_PRECONDITION and (
        "held by another controller" in msg or "Channel already in use" in msg)
