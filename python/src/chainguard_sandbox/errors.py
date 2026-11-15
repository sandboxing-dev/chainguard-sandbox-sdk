"""Exceptions raised by chainguard_sandbox. All derive from SandboxError."""

from __future__ import annotations

from dataclasses import dataclass


class SandboxError(Exception):
    """Base class for sandbox errors."""


@dataclass
class CommandResult:
    """A finished command's output and exit code."""

    exit_code: int
    stdout: str
    stderr: str


class CommandExitError(SandboxError):
    """A command exited with a non-zero code. Carries the full result."""

    def __init__(self, result: CommandResult):
        msg = result.stderr.strip()[-500:]
        super().__init__(f"command exited with code {result.exit_code}" + (f": {msg}" if msg else ""))
        self.result = result

    @property
    def exit_code(self) -> int:
        return self.result.exit_code

    @property
    def stdout(self) -> str:
        return self.result.stdout

    @property
    def stderr(self) -> str:
        return self.result.stderr


class NotFoundError(SandboxError):
    """A file, directory, background command or sandbox does not exist."""

    def __init__(self, what: str, name: str):
        super().__init__(f"{what} not found: {name}")
        self.what, self.name = what, name


class FileNotFoundError(NotFoundError):  # noqa: A001 - mirrors E2B's FileNotFoundException naming
    """A path in the sandbox does not exist."""


class SandboxNotFoundError(NotFoundError):
    """No sandbox with that ID."""


class InvalidPathError(SandboxError):
    """The path exists but is the wrong kind (a directory where a file was expected, or the reverse)."""

    def __init__(self, path: str, reason: str):
        super().__init__(f"{path}: {reason}")
        self.path, self.reason = path, reason


class TimeoutError(SandboxError):  # noqa: A001 - a sandbox timeout, not the builtin
    """A deadline passed. For a command, ``result`` holds the output received before it was stopped."""

    def __init__(self, message: str, result: CommandResult | None = None):
        super().__init__(message)
        self.result = result


class SandboxBusyError(SandboxError):
    """Another client (an interactive shell) is attached; commands run once it detaches."""


class LimitReachedError(SandboxError):
    """The organization is at its session limit (one sandbox per user and three per organization during the Workspaces beta)."""


class OutcomeUnknownError(SandboxError):
    """The connection ended, or failed, before the command reported an exit
    status; it may or may not have run, in part or in full. ``cause`` is the
    transport error (a grpc.RpcError) that cut it short, if there was one."""

    def __init__(self, message: str, cause: BaseException | None = None):
        super().__init__(message)
        self.cause = cause
