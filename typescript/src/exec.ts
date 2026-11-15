// Exec: run one process in a session. Mirrors the Go SDK's workspaces/exec.go.
import { Code, ConnectError } from "@connectrpc/connect";
import { durationFromMs } from "@bufbuild/protobuf/wkt";
import type { ExecResponse } from "./gen/sandbox/runtime/session/v1/session_pb";
import { DEFAULT_WORK_DIR, MAX_EXEC_STDIN, type Client } from "./client";

/**
 * The exec stream ended, or failed, without an exit status (chainctl reports
 * 255): the command may have run, in part or in full. `cause` is the transport
 * error that cut it short, if there was one.
 */
export class OutcomeUnknownError extends Error {
  constructor(cause?: unknown) {
    const why = cause instanceof ConnectError ? `: ${Code[cause.code]}: ${cause.rawMessage}` : cause !== undefined ? `: ${String(cause)}` : "";
    super(`chainguard-sandbox: exec ended without an exit status${why}`, cause !== undefined ? { cause } : undefined);
  }
}

/** Failures of the connection itself: the request may still have reached the plane. */
const TRANSPORT_CODES = new Set([Code.Unavailable, Code.Internal, Code.Unknown, Code.DataLoss, Code.DeadlineExceeded, Code.Canceled]);

/**
 * Classifies an error from the exec stream. Once the stream has delivered an
 * event the command was running, so any failure leaves its outcome unknown.
 * Before that only a failure of the connection does; an answer from the plane
 * (not found, held, invalid, over the limit, ...) means the command did not
 * run. An error caused by the caller's own abort is returned as it is.
 */
function interrupted(e: unknown, started: boolean, signal?: AbortSignal): unknown {
  if (signal?.aborted) return e;
  if (started || (e instanceof ConnectError && TRANSPORT_CODES.has(e.code))) return new OutcomeUnknownError(e);
  return e;
}

/**
 * The plane refused because another controller holds the guest: an attached
 * shell (Aborted), or FailedPrecondition "instance held by another
 * controller" briefly after another client's attach or exec ended.
 */
export function isGuestHeld(err: unknown): boolean {
  if (!(err instanceof ConnectError)) return false;
  if (err.code === Code.Aborted) return err.rawMessage.includes("held");
  return err.code === Code.FailedPrecondition && controllerBusy(err.rawMessage);
}

function controllerBusy(msg: string): boolean {
  return msg.includes("held by another controller") || msg.includes("Channel already in use");
}

export interface ExecOptions {
  /** Working directory, absolute or relative to /work. Default /work (the plane itself starts in /). */
  dir?: string;
  /** Pass argv through unwrapped (runs in /). */
  noDir?: boolean;
  /** At most MAX_EXEC_STDIN bytes. */
  stdin?: Uint8Array | string;
  timeoutMs?: number;
  stdout?: (chunk: Uint8Array) => void;
  stderr?: (chunk: Uint8Array) => void;
  signal?: AbortSignal;
}

export interface ExecResult {
  exitCode: number;
  timedOut: boolean;
}

export class ExitError extends Error {
  constructor(
    readonly result: ExecResult,
    readonly stdout: Uint8Array,
    readonly stderr: Uint8Array,
  ) {
    const msg = new TextDecoder().decode(stderr).trim();
    super(result.timedOut ? `command timed out: ${msg}` : `command exited ${result.exitCode}: ${msg}`);
  }
  get exitCode() {
    return this.result.exitCode;
  }
}

/** How chainctl implements `exec --cwd`. */
export const CD_SCRIPT = 'cd "$1" && shift && exec "$@"';

export function guestPath(p: string): string {
  const joined = p.startsWith("/") ? p : `${DEFAULT_WORK_DIR}/${p}`;
  const out: string[] = [];
  for (const seg of joined.split("/")) {
    if (seg === "" || seg === ".") continue;
    if (seg === "..") out.pop();
    else out.push(seg);
  }
  return "/" + out.join("/");
}

export function withDir(dir: string, argv: string[]): string[] {
  return ["sh", "-c", CD_SCRIPT, "_", guestPath(dir), ...argv];
}

/** Sleep that ends early (rejecting with the abort reason) when signal aborts. */
export function abortableSleep(ms: number, signal?: AbortSignal): Promise<void> {
  if (signal?.aborted) return Promise.reject(signal.reason);
  return new Promise((resolve, reject) => {
    const t = setTimeout(() => {
      signal?.removeEventListener("abort", onAbort);
      resolve();
    }, ms);
    const onAbort = () => {
      clearTimeout(t);
      reject(signal!.reason);
    };
    signal?.addEventListener("abort", onAbort, { once: true });
  });
}

/**
 * Runs argv (no shell) in the session. A non-zero exit is reported in the result, not thrown.
 * Rejects with OutcomeUnknownError if the stream ends without an exit status, or the connection
 * fails once the command may have started (the transport error is its `cause`).
 */
export async function exec(c: Client, name: string, argv: string[], o: ExecOptions = {}): Promise<ExecResult> {
  if (argv.length === 0) throw new Error("chainguard-sandbox: empty argv");
  const stdin = typeof o.stdin === "string" ? new TextEncoder().encode(o.stdin) : (o.stdin ?? new Uint8Array());
  if (stdin.length > MAX_EXEC_STDIN) throw new Error(`chainguard-sandbox: stdin is ${stdin.length} bytes; the limit is ${MAX_EXEC_STDIN}`);
  if (!o.noDir) argv = withDir(o.dir || DEFAULT_WORK_DIR, argv);
  const req = { name, argv, stdin, deadline: o.timeoutMs ? durationFromMs(o.timeoutMs) : undefined };
  // Our own controller, linked to the caller's signal, cancels the server
  // stream on every way out (result, error, or a throwing stdout/stderr callback).
  const ac = new AbortController();
  const onAbort = () => ac.abort(o.signal!.reason);
  if (o.signal?.aborted) throw o.signal.reason;
  o.signal?.addEventListener("abort", onAbort, { once: true });
  try {
    // While the plane answers FailedPrecondition "instance held by another
    // controller" (briefly, right after another client's attach or exec;
    // nothing ran), retry with backoff for up to ~30s.
    let it!: AsyncIterator<ExecResponse>;
    let r!: IteratorResult<ExecResponse>;
    for (const delay of [2000, 4000, 8000, 8000, 8000, 0]) {
      it = c.raw.exec(req, { signal: ac.signal })[Symbol.asyncIterator]();
      try {
        r = await it.next();
        break;
      } catch (e) {
        if (!delay || !(e instanceof ConnectError && e.code === Code.FailedPrecondition && controllerBusy(e.rawMessage))) throw interrupted(e, false, o.signal);
        await abortableSleep(delay, ac.signal);
      }
    }
    // An event arrived: the command was running, so a failure from here on leaves its outcome unknown.
    const next = () =>
      it.next().catch((e) => {
        throw interrupted(e, true, o.signal);
      });
    for (; !r.done; r = await next()) {
      const ev = r.value;
      switch (ev.event.case) {
        case "stdout":
          o.stdout?.(ev.event.value);
          break;
        case "stderr":
          o.stderr?.(ev.event.value);
          break;
        case "exited":
          return { exitCode: ev.event.value.code, timedOut: ev.event.value.timedOut };
      }
    }
    throw new OutcomeUnknownError();
  } finally {
    o.signal?.removeEventListener("abort", onAbort);
    ac.abort(); // no-op if the stream already ended; otherwise cancels it on the server
  }
}

function concat(chunks: Uint8Array[]): Uint8Array {
  const out = new Uint8Array(chunks.reduce((n, c) => n + c.length, 0));
  let off = 0;
  for (const c of chunks) {
    out.set(c, off);
    off += c.length;
  }
  return out;
}

/** Runs argv and returns stdout; throws ExitError on a non-zero exit or timeout. */
export async function output(c: Client, name: string, argv: string[], o: ExecOptions = {}): Promise<Uint8Array> {
  const out: Uint8Array[] = [];
  const err: Uint8Array[] = [];
  const res = await exec(c, name, argv, { ...o, stdout: (b) => out.push(b), stderr: (b) => err.push(b) });
  if (res.exitCode !== 0 || res.timedOut) throw new ExitError(res, concat(out), concat(err));
  return concat(out);
}

/** Runs script with `sh -c`; extra args become $1, $2, ... */
export async function shell(c: Client, name: string, script: string, o: ExecOptions = {}, ...args: string[]): Promise<Uint8Array> {
  return output(c, name, ["sh", "-c", script, "_", ...args], o);
}
