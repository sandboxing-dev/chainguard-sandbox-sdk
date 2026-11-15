// The lower-level client against a local fake Sessions server (no network): sessions, Exec, the first
// attach, token sources, URL normalization and the tar writer.
import { afterAll, beforeAll, expect, test } from "bun:test";
import * as http2 from "node:http2";
import { execFileSync, spawnSync } from "node:child_process";
import { existsSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { type MessageInitShape } from "@bufbuild/protobuf";
import { timestampNow } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type ConnectRouter } from "@connectrpc/connect";
import { connectNodeAdapter } from "@connectrpc/connect-node";
import {
  CD_SCRIPT,
  Client,
  DEFAULT_ENVIRONMENT,
  EMPTY_TAR,
  ExitError,
  FirstAttachError,
  OutcomeUnknownError,
  WRITE_FILE_SCRIPT,
  cached,
  completeFirstAttach,
  exec,
  identityExchange,
  jwtExpiry,
  listSessions,
  normalizeGitHubUrl,
  output,
  parkSession,
  repositorySession,
  repositoryUrl,
  resolveSession,
  staticToken,
  tarBytes,
  writeFile,
  pb,
} from "../src/index";
import { CommandHandle, OutcomeUnknownError as SandboxOutcomeUnknown, Sandbox, SandboxError, TimeoutError } from "../src/sandbox";
import { timings } from "../src/sandbox/timings";

type SessionInit = MessageInitShape<typeof pb.SessionSchema>;

const te = new TextEncoder();
const td = new TextDecoder();
const A = "instances/org1/sessions/a";

// ---- the fake service ----

const execs: pb.ExecRequest[] = [];
const updates: pb.UpdateSessionRequest[] = [];
let busyHits = 0;
let execAborted = false;
/** Sessions by name; listSessions pages through them two at a time. */
let sessions = new Map<string, SessionInit>();
let created = 0;
/** How attachSession behaves: "gzip"/"plain" upload, "ready" (nothing pending), "detached", "end". */
let attachMode = "gzip";
let uploads: { data: Uint8Array; encoding: pb.AttachRequest_WorkspaceChunk_Encoding; last: boolean }[] = [];
let opens: pb.AttachRequest_Open[] = [];
/** When set, these script getSession and exec; deletes records every deleteSession. */
let getHook: ((name: string) => SessionInit | Promise<SessionInit>) | undefined;
let execHook: ((req: pb.ExecRequest) => AsyncGenerator<MessageInitShape<typeof pb.ExecResponseSchema>>) | undefined;
let deletes: string[] = [];

function resetSessions(list: SessionInit[] = []) {
  sessions = new Map(list.map((s) => [s.name!, s]));
}

function routes(router: ConnectRouter) {
  router.service(pb.Sessions, {
    async createSession(req) {
      if (req.parent !== "instances/org1") throw new ConnectError(`parent ${req.parent}`, Code.InvalidArgument);
      const name = `instances/org1/sessions/new${++created}`;
      const s: SessionInit = {
        environment: req.session?.environment,
        idle: req.session?.idle,
        name,
        shortId: `new${created}`,
        displayName: `sandbox/new-${created}`,
        phase: pb.Phase.CREATING,
      };
      sessions.set(name, s);
      return s;
    },
    async getSession(req) {
      if (getHook) return getHook(req.name);
      const s = sessions.get(req.name);
      if (!s) throw new ConnectError(`session ${req.name} not found`, Code.NotFound);
      // The guest "boots" at once; a workspace session waits for its first upload.
      if (s.phase === pb.Phase.CREATING) {
        s.phase = pb.Phase.IDLE;
        s.guestStartedTime = timestampNow();
        s.workspacePending = s.environment?.case === "workspace";
      }
      return s;
    },
    async listSessions(req, ctx) {
      if (ctx.requestHeader.get("authorization") !== "Bearer test-token") throw new ConnectError("auth", Code.Unauthenticated);
      if (req.parent !== "instances/org1") throw new ConnectError(`parent ${req.parent}`, Code.InvalidArgument);
      const all = [...sessions.values()];
      const start = req.pageToken ? Number(req.pageToken) : 0;
      const next = start + 2 < all.length ? String(start + 2) : "";
      return { sessions: all.slice(start, start + 2), nextPageToken: next };
    },
    async updateSession(req) {
      updates.push(req);
      return req.session!;
    },
    async deleteSession(req) {
      deletes.push(req.name);
      sessions.delete(req.name);
      return {};
    },
    async heartbeatSession(req) {
      return sessions.get(req.name) ?? { name: req.name };
    },
    async *exec(req, ctx) {
      execs.push(req);
      if (execHook) {
        yield* execHook(req);
        return;
      }
      const s = sessions.get(req.name);
      if (s?.workspacePending) throw new ConnectError("attach with a shell first to upload the workspace", Code.FailedPrecondition);
      let argv = req.argv;
      if (argv.length > 5 && argv[0] === "sh" && argv[1] === "-c" && argv[3] === "_") argv = argv.slice(5);
      if (argv[0] === "busy-once") {
        if (++busyHits === 1) throw new ConnectError("exec: reattaching guest: microvm: instance held by another controller: supervisor: Channel already in use", Code.FailedPrecondition);
        yield { event: { case: "exited", value: { code: 0 } } };
        return;
      }
      if (argv[0] === "cat") {
        yield { event: { case: "stdout", value: te.encode(`contents of ${argv.at(-1)}`) } };
        yield { event: { case: "exited", value: { code: 0 } } };
      } else if (argv[0] === "false") {
        yield { event: { case: "stderr", value: te.encode("nope") } };
        yield { event: { case: "exited", value: { code: 3 } } };
      } else if (argv[0] === "vanish") {
        return;
      } else if (argv[0] === "yes") {
        // Streams output until the client goes away; records cancellation.
        for (let i = 0; i < 500; i++) {
          if (ctx.signal.aborted) {
            execAborted = true;
            return;
          }
          yield { event: { case: "stdout", value: te.encode("y\n") } };
          await Bun.sleep(5);
        }
        yield { event: { case: "exited", value: { code: 0 } } };
      } else {
        // Anything else echoes its last argument (for a sandbox command: the bash -lc string).
        yield { event: { case: "stdout", value: te.encode(argv.at(-1) ?? "") } };
        yield { event: { case: "exited", value: { code: 0 } } };
      }
    },
    async *attachSession(reqs): AsyncGenerator<MessageInitShape<typeof pb.AttachResponseSchema>> {
      const it = reqs[Symbol.asyncIterator]();
      const first = (await it.next()).value as pb.AttachRequest;
      if (first.input.case !== "open") throw new ConnectError("want open", Code.InvalidArgument);
      opens.push(first.input.value);
      const s = sessions.get(first.input.value.name);
      const pending = attachMode !== "ready" && (s?.workspacePending ?? true);
      if (attachMode === "end") {
        yield { output: { case: "opened", value: { sessionName: "x", workspacePending: true } } };
        return; // the stream ends before any upload
      }
      const encodings = attachMode === "gzip" ? [pb.AttachRequest_WorkspaceChunk_Encoding.GZIP] : [];
      yield { output: { case: "opened", value: { sessionName: s?.displayName ?? "x", workspacePending: pending, workspaceEncodings: encodings } } };
      if (!pending) {
        // Hold the stream open until the client cancels it.
        for (;;) if ((await it.next()).done) return;
      }
      if (attachMode === "detached") {
        yield { output: { case: "detached", value: {} } };
        return;
      }
      for (;;) {
        const n = await it.next();
        if (n.done) return;
        const r = n.value as pb.AttachRequest;
        if (r.input.case !== "workspaceChunk") throw new ConnectError("want a workspace chunk", Code.FailedPrecondition);
        expect(r.input.value.format).toBe(pb.AttachRequest_WorkspaceChunk_Format.FULL_TAR);
        uploads.push({ data: r.input.value.data, encoding: r.input.value.encoding, last: r.input.value.last });
        if (r.input.value.last) break;
      }
      if (s) s.workspacePending = false;
      yield { output: { case: "workspaceReady", value: { bytes: 1024n } } };
      for (;;) if ((await it.next()).done) return;
    },
  });
}

let server: http2.Http2Server;
let c: Client;

beforeAll(async () => {
  server = http2.createServer(connectNodeAdapter({ routes }));
  await new Promise<void>((r) => server.listen(0, "127.0.0.1", r));
  const port = (server.address() as { port: number }).port;
  c = new Client({ url: `http://127.0.0.1:${port}`, instance: "org1", tokenSource: staticToken("test-token") });
});
afterAll(() => server.close());

// ---- sessions ----

test("list follows pages and resolve matches chainctl's spellings", async () => {
  resetSessions([
    { name: A, shortId: "3f9a2c1e", displayName: "app/quiet-harbor" },
    { name: "instances/org1/sessions/b", shortId: "77aa0011", displayName: "app/scratch" },
    { name: "instances/org1/sessions/c", shortId: "cccccccc", displayName: "other" },
  ]);
  expect((await listSessions(c)).length).toBe(3);
  for (const [ref, want] of [
    ["3f9a2c1e", A],
    ["quiet-harbor", A],
    ["app/scratch", "instances/org1/sessions/b"],
    ["acme/app/scratch", "instances/org1/sessions/b"],
    [A, A],
  ] as const) {
    expect((await resolveSession(c, ref)).name).toBe(want);
  }
  expect(await resolveSession(c, "nothing").catch((e) => e)).toBeInstanceOf(Error);
});

test("resolveSession: exact matches win over suffix matches", async () => {
  resetSessions([
    { name: "instances/org1/sessions/a", shortId: "aaaaaaaa", displayName: "app/x" },
    { name: "instances/org1/sessions/b", shortId: "bbbbbbbb", displayName: "x" },
  ]);
  expect((await resolveSession(c, "x")).shortId).toBe("bbbbbbbb"); // exact display name, not ambiguous
  expect((await resolveSession(c, "app/x")).shortId).toBe("aaaaaaaa");
  expect((await resolveSession(c, "acme/app/x")).shortId).toBe("aaaaaaaa"); // chainctl's owner-qualified form
  expect(await resolveSession(c, "z/y/app/x").catch((e) => e)).toBeInstanceOf(Error);
});

test("park sends park_request with a field mask", async () => {
  await parkSession(c, A);
  expect(updates.at(-1)!.updateMask!.paths).toEqual(["park_request"]);
  expect(updates.at(-1)!.session!.parkRequest).toBe(pb.Session_ParkRequest.PARK);
});

test("repositorySession matches chainctl's request shape", () => {
  for (const repo of ["github.com/acme/app", "https://github.com/acme/app.git", "git@github.com:acme/app.git"]) {
    const s = repositorySession(repo);
    expect(s.environment.case).toBe("source");
    if (s.environment.case !== "source") return;
    expect(s.environment.value.repository).toBe("https://github.com/acme/app");
    expect(s.environment.value.fallbackDocument).toEqual(DEFAULT_ENVIRONMENT);
    expect(s.idle).toBe(pb.IdlePolicy.AUTO_HOLD);
  }
});

test("normalizeGitHubUrl / repositoryUrl handle credentials, ports, schemes and non-GitHub remotes", () => {
  const gh = "https://github.com/o/r";
  for (const u of [
    "git@github.com:o/r.git",
    "ssh://git@github.com/o/r.git",
    "ssh://git@github.com:22/o/r",
    "git://github.com/o/r.git",
    "https://x-access-token:SECRET@GitHub.com/o/r.git",
    "https://github.com/o/r/",
    "github.com/o/r",
  ]) {
    expect(normalizeGitHubUrl(u)).toBe(gh);
    expect(repositoryUrl(u)).toBe(gh);
  }
  expect(normalizeGitHubUrl("https://user:pw@gitlab.com/o/r.git")).toBe("https://gitlab.com/o/r");
  expect(repositoryUrl("git@gitlab.com:o/r")).toBe("git@gitlab.com:o/r");
});

// ---- Exec ----

test("exec wraps argv into /work, reports exit codes and unknown outcomes", async () => {
  resetSessions([{ name: A }]);
  expect(td.decode(await output(c, A, ["cat", "notes.md"]))).toBe("contents of notes.md");
  const err = await output(c, A, ["false"]).catch((e) => e);
  expect(err).toBeInstanceOf(ExitError);
  expect((err as ExitError).result.exitCode).toBe(3);
  // Note: Bun's expect(promise).rejects interferes with the call; catch explicitly.
  expect(await exec(c, A, ["vanish"]).catch((e) => e)).toBeInstanceOf(OutcomeUnknownError);
  await exec(c, A, ["go", "test"], { dir: "sub", timeoutMs: 60_000 });
  expect(execs.at(-1)!.argv).toEqual(["sh", "-c", CD_SCRIPT, "_", "/work/sub", "go", "test"]);
  expect(Number(execs.at(-1)!.deadline!.seconds)).toBe(60);
  await exec(c, A, ["true"], { noDir: true });
  expect(execs.at(-1)!.argv).toEqual(["true"]);
});

test("exec retries while another controller briefly holds the guest", async () => {
  resetSessions([{ name: A }]);
  busyHits = 0;
  const t = Date.now();
  expect((await exec(c, A, ["busy-once"])).exitCode).toBe(0);
  expect(busyHits).toBe(2);
  expect(Date.now() - t).toBeGreaterThanOrEqual(2000);
}, 15000);

test("exec: a throwing stdout callback cancels the server stream", async () => {
  resetSessions([{ name: A }]);
  execAborted = false;
  const err = await exec(c, A, ["yes"], {
    stdout: () => {
      throw new Error("EPIPE");
    },
  }).catch((e) => e);
  expect(String(err)).toContain("EPIPE");
  for (let i = 0; i < 100 && !execAborted; i++) await Bun.sleep(10);
  expect(execAborted).toBe(true);
});

test("writeFile refuses an empty path and sends the directory-refusing script", async () => {
  resetSessions([{ name: A }]);
  const e = await writeFile(c, A, "  ", "x").catch((x) => x);
  expect(String(e)).toMatch(/needs a file path/);
  execs.length = 0;
  await writeFile(c, A, "notes.md", "x");
  expect(execs[0]!.argv).toContain(WRITE_FILE_SCRIPT);
  expect(execs[0]!.argv).toContain("/work/notes.md");
});

test("the writeFile script refuses a directory (exit 21) and replaces a file", () => {
  const tmp = mkdtempSync(join(tmpdir(), "chainguard-sandbox-wf-"));
  mkdirSync(join(tmp, "adir"));
  const run = (path: string) => spawnSync("sh", ["-c", WRITE_FILE_SCRIPT, "_", path, "644"], { input: "payload" });
  expect(run(join(tmp, "adir")).status).toBe(21);
  expect(readdirSync(join(tmp, "adir"))).toEqual([]); // nothing moved inside
  writeFileSync(join(tmp, "f.txt"), "old");
  expect(run(join(tmp, "f.txt")).status).toBe(0);
  expect(readFileSync(join(tmp, "f.txt"), "utf8")).toBe("payload");
  expect(run(join(tmp, "new", "deep.txt")).status).toBe(0);
  expect(existsSync(join(tmp, "new", "deep.txt"))).toBe(true);
  rmSync(tmp, { recursive: true });
});

// ---- the first attach ----

test("completeFirstAttach uploads one empty FULL_TAR chunk, gzip'd when offered", async () => {
  for (const mode of ["gzip", "plain"]) {
    resetSessions([{ name: A, workspacePending: true }]);
    attachMode = mode;
    uploads = [];
    opens = [];
    await completeFirstAttach(c, A, { timeoutMs: 5000 });
    expect(opens[0]!.name).toBe(A);
    expect(opens[0]!.resume).toBeDefined();
    expect(uploads.length).toBe(1);
    expect(uploads[0]!.last).toBe(true);
    if (mode === "gzip") {
      expect(uploads[0]!.encoding).toBe(pb.AttachRequest_WorkspaceChunk_Encoding.GZIP);
      expect(Bun.gunzipSync(uploads[0]!.data as Uint8Array<ArrayBuffer>)).toEqual(EMPTY_TAR);
    } else {
      expect(uploads[0]!.encoding).toBe(pb.AttachRequest_WorkspaceChunk_Encoding.ENCODING_UNSPECIFIED);
      expect(uploads[0]!.data).toEqual(EMPTY_TAR);
    }
    expect(sessions.get(A)!.workspacePending).toBe(false);
  }
});

test("completeFirstAttach returns at once when nothing is pending, and fails when the stream ends first", async () => {
  resetSessions([{ name: A }]);
  attachMode = "ready";
  uploads = [];
  await completeFirstAttach(c, A, { timeoutMs: 5000 });
  expect(uploads.length).toBe(0);
  for (const mode of ["detached", "end"]) {
    attachMode = mode;
    expect(await completeFirstAttach(c, A, { timeoutMs: 5000 }).catch((e) => e)).toBeInstanceOf(FirstAttachError);
  }
  attachMode = "gzip";
});

test("Sandbox.create completes the first attach before running commands, and kill deletes it", async () => {
  resetSessions();
  attachMode = "gzip";
  uploads = [];
  const sbx = await Sandbox.create({ client: c, name: "unit", packages: ["bash"] });
  expect(uploads.length).toBe(1); // exec would fail with FailedPrecondition otherwise
  const r = await sbx.commands.run("echo hi");
  expect(r.exitCode).toBe(0);
  expect(r.stdout).toBe("echo hi");
  const info = (await Sandbox.list({ client: c })).map((i) => i.sandboxId);
  expect(info).toEqual([sbx.sandboxId]);
  expect(await sbx.kill()).toBe(true);
  expect(sessions.size).toBe(0);
});

// ---- review findings: cleanup, deadlines, interrupted commands, output ----

const stdoutEvent = (b: Uint8Array | string) => ({ event: { case: "stdout" as const, value: typeof b === "string" ? te.encode(b) : b } });
const exitedEvent = { event: { case: "exited" as const, value: { code: 0 } } };

/** A handle on the fake without the create flow, with the hooks reset afterwards. */
async function withHandle(fn: (sbx: Sandbox) => Promise<void>) {
  resetSessions([{ name: A, shortId: "a1" }]);
  deletes = [];
  const saved = { ...timings };
  try {
    await fn(await Sandbox.connect(A, { client: c }));
  } finally {
    getHook = execHook = undefined;
    Object.assign(timings, saved);
  }
}

test("Sandbox.kill by ID deletes without attaching", async () => {
  resetSessions([{ name: A, shortId: "a1", workspacePending: true }]);
  attachMode = "detached"; // the guest can't be attached to
  opens = [];
  deletes = [];
  expect(await Sandbox.kill(A, { client: c })).toBe(true);
  expect(deletes).toEqual([A]);
  expect(opens.length).toBe(0);
  expect(await Sandbox.kill(A, { client: c })).toBe(false); // already gone
  attachMode = "gzip";
});

test("the start timeout cuts off a stalled call, and a late start is not a success", async () => {
  resetSessions();
  deletes = [];
  getHook = async (name) => {
    await Bun.sleep(500); // answers "started", but only after the limit
    return { name, guestStartedTime: timestampNow() };
  };
  try {
    const t = Date.now();
    const err = await Sandbox.create({ client: c, startTimeoutMs: 50 }).catch((e) => e);
    expect(err).toBeInstanceOf(TimeoutError);
    expect(String(err)).toContain("did not start within");
    expect(Date.now() - t).toBeLessThan(400);
    expect(deletes.length).toBe(1); // the sandbox that didn't start in time is deleted
  } finally {
    getHook = undefined;
  }
});

test("kill does not report success while the sandbox still exists", async () => {
  await withHandle(async (sbx) => {
    timings.killWaitMs = 300;
    timings.killPollMs = 20;
    getHook = (name) => ({ name }); // never goes away
    let err = await sbx.kill().catch((e) => e);
    expect(err).toBeInstanceOf(TimeoutError);
    expect(String(err)).toContain("still exists");
    expect(deletes).toEqual([A]);
    getHook = () => {
      throw new ConnectError("boom", Code.Internal); // checks that fail are not taken as "gone" either
    };
    err = await sbx.kill().catch((e) => e);
    expect(err).toBeInstanceOf(TimeoutError);
    expect(String(err)).toContain("boom");
    getHook = undefined; // the fake now answers NotFound for the deleted name
    resetSessions([{ name: A }]);
    expect(await sbx.kill()).toBe(true);
  });
});

test("a connection that fails after output leaves the command's outcome unknown", async () => {
  await withHandle(async (sbx) => {
    execHook = async function* () {
      yield stdoutEvent("partial");
      throw new ConnectError("connection lost", Code.Unavailable);
    };
    const lib = await exec(c, A, ["x"]).catch((e) => e);
    expect(lib).toBeInstanceOf(OutcomeUnknownError);
    expect((lib.cause as ConnectError).code).toBe(Code.Unavailable);
    const err = await sbx.commands.run("do-something").catch((e) => e);
    expect(err).toBeInstanceOf(SandboxOutcomeUnknown);
    expect(String(err)).toContain("connection lost");
    expect((err.cause as ConnectError).code).toBe(Code.Unavailable); // the transport error is preserved
    execHook = async function* () {
      throw new ConnectError("bad argv", Code.InvalidArgument); // an answer from the plane: the command did not run
    };
    const refused = await sbx.commands.run("x").catch((e) => e);
    expect(refused).toBeInstanceOf(SandboxError);
    expect(refused).not.toBeInstanceOf(SandboxOutcomeUnknown);
    expect(String(refused)).toContain("InvalidArgument");
  });
});

test("output callbacks get whole characters", async () => {
  await withHandle(async (sbx) => {
    const euro = te.encode("€"); // three bytes
    execHook = async function* () {
      yield stdoutEvent(new Uint8Array([97, euro[0]!]));
      yield stdoutEvent(euro.subarray(1, 2));
      yield stdoutEvent(new Uint8Array([euro[2]!, 98]));
      yield { event: { case: "stderr" as const, value: euro.subarray(0, 2) } }; // incomplete at the very end
      yield exitedEvent;
    };
    const out: string[] = [];
    const err: string[] = [];
    const r = await sbx.commands.run("x", { onStdout: (s) => out.push(s), onStderr: (s) => err.push(s) });
    expect(r.stdout).toBe("a€b");
    expect(out).toEqual(["a", "€b"]); // the euro sign arrives whole
    expect(err).toEqual(["\ufffd"]); // an incomplete character at the end is still delivered
  });
});

/** Answers the poll script (argv: sh -c <script> _ <dir> <outOff> <errOff> <wait> <max>) from content. */
function pollFrom(content: () => Uint8Array, exit: () => string, delayMs = 0) {
  return async function* (req: pb.ExecRequest) {
    const off = Number(req.argv[5]);
    await Bun.sleep(delayMs);
    const body = content();
    yield stdoutEvent(Buffer.concat([te.encode(`${exit()} ${body.length} 0\n`), body.subarray(off)]));
    yield exitedEvent;
  };
}
const handle = (sbx: Sandbox) => new (CommandHandle as unknown as new (sbx: Sandbox, pid: number, id: string, dir: string) => CommandHandle)(sbx, 7, "x", "/tmp/x");

test("concurrent polls do not duplicate output", async () => {
  await withHandle(async (sbx) => {
    execHook = pollFrom(() => te.encode("abc"), () => "-", 100); // long enough for the polls to overlap
    const h = handle(sbx);
    await Promise.all([h.poll(), h.poll(), h.poll(), h.poll()]);
    expect(h.stdout).toBe("abc");
  });
});

test("wait callbacks get earlier output first and whole characters", async () => {
  await withHandle(async (sbx) => {
    const all = te.encode("é€");
    let content = all.subarray(0, 3); // "é" and the first byte of "€" so far
    let exit = "-";
    execHook = pollFrom(() => content, () => exit);
    const h = handle(sbx);
    expect(await h.poll()).toBeUndefined();
    content = all;
    exit = "0";
    const out: string[] = [];
    expect((await h.wait({ onStdout: (s) => out.push(s) })).stdout).toBe("é€");
    expect(out).toEqual(["é", "€"]);
  });
});

// ---- tokens ----

test("jwtExpiry reads exp", () => {
  expect(jwtExpiry("eyJhbGciOiJub25lIn0.eyJleHAiOjIwMDAwMDAwMDB9.x")?.getTime()).toBe(2000000000 * 1000);
});

test("identityExchange posts to /sts/exchange with the external token", async () => {
  let seen: URL | undefined;
  const sts = Bun.serve({
    port: 0,
    fetch(req) {
      seen = new URL(req.url);
      if (req.headers.get("authorization") !== "Bearer external-oidc") return Response.json({ code: 16, message: "bad token" }, { status: 401 });
      return Response.json({ token: "chainguard-token" });
    },
  });
  const ts = identityExchange({ identity: "org/abc123", issuer: `http://127.0.0.1:${sts.port}`, idToken: async () => "external-oidc" });
  expect(await ts.token()).toBe("chainguard-token");
  expect(seen?.pathname).toBe("/sts/exchange");
  expect(seen?.searchParams.get("identity")).toBe("org/abc123");
  expect(seen?.searchParams.get("aud")).toBe("https://sessions.chainbox.dev");
  const bad = identityExchange({ identity: "x", issuer: `http://127.0.0.1:${sts.port}`, idToken: async () => "wrong" });
  expect(String(await bad.token().catch((e) => e))).toMatch(/bad token/);
  sts.stop();
});

test("cached: recovers after a source that throws synchronously", async () => {
  let n = 0;
  const ts = cached({
    token() {
      if (n++ === 0) throw new Error("boom");
      return Promise.resolve("ok");
    },
  });
  expect(String(await ts.token().catch((e) => e))).toContain("boom");
  expect(await ts.token()).toBe("ok");
});

test("cached: one caller aborting doesn't fail the others", async () => {
  let release!: (v: string) => void;
  const ts = cached({ token: () => new Promise<string>((r) => (release = r)) });
  const ac = new AbortController();
  const p1 = ts.token(ac.signal).catch((e) => e);
  const p2 = ts.token();
  await Bun.sleep(0); // the shared fetch starts on the next tick
  ac.abort(new Error("caller 1 gave up"));
  release("tok");
  expect(String(await p1)).toContain("gave up");
  expect(await p2).toBe("tok");
});

// ---- tar ----

test("tar: long/non-ASCII names and link targets are readable by bsdtar and python", () => {
  const names = [
    "dir/" + "é".repeat(60) + ".txt", // 124 bytes, non-ASCII → PAX
    "a/" + "b".repeat(90) + "/" + "c".repeat(60) + ".txt", // ASCII > 100 → USTAR prefix split
    "d/" + "e".repeat(300), // ASCII, unsplittable → PAX
    "日本語/ファイル.txt", // short non-ASCII → PAX (like Go)
  ];
  const link = "ターゲット/" + "t".repeat(120);
  const tar = tarBytes([
    ...names.map((name) => ({ name, data: te.encode("hello"), mtime: 1000.7 })),
    { name: "link", type: "symlink" as const, linkname: link },
  ]);
  const dir = mkdtempSync(join(tmpdir(), "tar-"));
  const f = join(dir, "t.tar");
  writeFileSync(f, tar);
  // macOS bsdtar prints names NFD-normalized; compare normalized (python checks exact bytes).
  const bsd = execFileSync("tar", ["-tf", f]).toString().trim().split("\n").map((n) => n.normalize("NFC"));
  expect(bsd).toEqual([...names, "link"]);
  const py = JSON.parse(
    execFileSync("python3", ["-c", `import tarfile,json,sys; t=tarfile.open(sys.argv[1]); print(json.dumps([[m.name, m.linkname, m.mtime, m.uid, m.gid] for m in t.getmembers()]))`, f]).toString(),
  ) as [string, string, number, number, number][];
  expect(py.map((m) => m[0])).toEqual([...names, "link"]);
  expect(py.at(-1)![1]).toBe(link);
  expect(py[0]![2]).toBe(1001); // rounded to the nearest second, like Go
  expect(py.every((m) => m[3] === 0 && m[4] === 0)).toBe(true);
  rmSync(dir, { recursive: true });
});
