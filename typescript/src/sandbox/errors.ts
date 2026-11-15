// Errors thrown by the sandbox API (chainguard-sandbox-sdk). All extend SandboxError.

/** Base class for sandbox errors. */
export class SandboxError extends Error {
  constructor(message: string, options?: ErrorOptions) {
    super(message, options);
    this.name = new.target.name;
  }
}

/** A finished command's output and exit code. */
export interface CommandResult {
  exitCode: number;
  stdout: string;
  stderr: string;
}

/** A command exited with a non-zero code. It also is the result. */
export class CommandExitError extends SandboxError implements CommandResult {
  constructor(readonly result: CommandResult) {
    const msg = result.stderr.trim().slice(-500);
    super(`command exited with code ${result.exitCode}${msg ? `: ${msg}` : ""}`);
  }
  get exitCode(): number {
    return this.result.exitCode;
  }
  get stdout(): string {
    return this.result.stdout;
  }
  get stderr(): string {
    return this.result.stderr;
  }
}

/** A file, directory, background command or sandbox does not exist. */
export class NotFoundError extends SandboxError {
  constructor(
    readonly what: string,
    readonly target: string,
  ) {
    super(`${what} not found: ${target}`);
  }
}

/** A path in the sandbox does not exist. */
export class FileNotFoundError extends NotFoundError {}

/** No sandbox with that ID. */
export class SandboxNotFoundError extends NotFoundError {}

/** The path exists but is the wrong kind ("is a directory" / "not a directory"). */
export class InvalidPathError extends SandboxError {
  constructor(
    readonly path: string,
    readonly reason: string,
  ) {
    super(`${path}: ${reason}`);
  }
}

/** A deadline passed. For a command, `result` holds the output received before it was stopped. */
export class TimeoutError extends SandboxError {
  constructor(
    message: string,
    readonly result?: CommandResult,
  ) {
    super(message);
  }
}

/** Another client (an interactive shell) is attached; commands run once it detaches. */
export class SandboxBusyError extends SandboxError {}

/** The organization is at its session limit (one sandbox per user and three per organization during the Workspaces beta). */
export class LimitReachedError extends SandboxError {}

/**
 * The connection ended, or failed, before the command reported an exit status; it may or may not
 * have run, in part or in full. `cause` is the transport error that cut it short, if there was one.
 */
export class OutcomeUnknownError extends SandboxError {
  constructor(message: string, cause?: unknown) {
    super(message, cause !== undefined ? { cause } : undefined);
  }
}
