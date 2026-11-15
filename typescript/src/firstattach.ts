// The first attach: the one AttachSession exchange the simple sandbox API needs.
//
// A session created with a workspace environment refuses Exec ("attach with a shell first to upload
// the workspace") until a shell attach has uploaded its files. completeFirstAttach opens a shell
// attach, uploads an empty tree (an empty tar archive) when the service asks for one, waits for
// WorkspaceReady, and detaches by canceling the stream. The session keeps running.
import { create } from "@bufbuild/protobuf";
import {
  AttachRequestSchema,
  AttachRequest_WorkspaceChunk_Encoding,
  AttachRequest_WorkspaceChunk_Format,
  type AttachRequest,
  type AttachResponse,
} from "./gen/sandbox/runtime/session/v1/session_pb";
import type { Client } from "./client";

/** An empty tar archive: the end-of-archive marker, two 512-byte zero blocks. */
export const EMPTY_TAR = new Uint8Array(1024);

export interface FirstAttachOptions {
  signal?: AbortSignal;
  /** Gives up after this long (default 120 s). */
  timeoutMs?: number;
  /** Called with each response, for tests and diagnostics. */
  onEvent?: (r: AttachResponse) => void;
}

/** The first attach ended without the upload being accepted. */
export class FirstAttachError extends Error {}

/** The requests for the stream: pushed one at a time, ended by close(). */
class Requests implements AsyncIterable<AttachRequest> {
  private items: AttachRequest[] = [];
  private wake?: () => void;
  private closed = false;

  push(r: AttachRequest) {
    this.items.push(r);
    this.wake?.();
  }

  close() {
    this.closed = true;
    this.wake?.();
  }

  async *[Symbol.asyncIterator]() {
    for (;;) {
      if (this.items.length) {
        yield this.items.shift()!;
        continue;
      }
      if (this.closed) return;
      await new Promise<void>((r) => (this.wake = r));
      this.wake = undefined;
    }
  }
}

/**
 * Completes a new session's first attach with an empty tree, so Exec is allowed. Resolves once the
 * service has accepted the upload (WorkspaceReady), or at once if it doesn't ask for one
 * (Opened.workspacePending is false). Rejects with FirstAttachError if the stream ends first, or with
 * the RPC error.
 */
export async function completeFirstAttach(c: Client, name: string, o: FirstAttachOptions = {}): Promise<void> {
  const ac = new AbortController();
  const timeout = AbortSignal.timeout(o.timeoutMs ?? 120_000);
  const signal = AbortSignal.any([ac.signal, timeout, ...(o.signal ? [o.signal] : [])]);
  const reqs = new Requests();
  reqs.push(create(AttachRequestSchema, { input: { case: "open", value: { name, resume: {} } } }));
  try {
    for await (const r of c.raw.attachSession(reqs, { signal })) {
      o.onEvent?.(r);
      switch (r.output.case) {
        case "opened": {
          if (!r.output.value.workspacePending) return;
          const gzip = r.output.value.workspaceEncodings.includes(AttachRequest_WorkspaceChunk_Encoding.GZIP);
          reqs.push(
            create(AttachRequestSchema, {
              input: {
                case: "workspaceChunk",
                value: {
                  data: gzip ? Bun.gzipSync(EMPTY_TAR) : EMPTY_TAR,
                  last: true,
                  format: AttachRequest_WorkspaceChunk_Format.FULL_TAR,
                  encoding: gzip ? AttachRequest_WorkspaceChunk_Encoding.GZIP : AttachRequest_WorkspaceChunk_Encoding.ENCODING_UNSPECIFIED,
                },
              },
            }),
          );
          break;
        }
        case "workspaceReady":
          return;
        case "detached":
          throw new FirstAttachError("chainguard-sandbox: another client took over the session during the first attach");
      }
    }
    throw new FirstAttachError("chainguard-sandbox: the first attach ended before the upload was accepted");
  } catch (e) {
    if (timeout.aborted && !o.signal?.aborted) throw new FirstAttachError("chainguard-sandbox: the first attach did not complete in time");
    throw e;
  } finally {
    reqs.close();
    ac.abort(); // detach: cancel the stream; the session keeps running
  }
}
