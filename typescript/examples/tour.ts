// Tour: every part of the API, one snippet per docs section. It's type-checked but not meant to be
// run as-is (it refers to local files like ./my-project).
//
// Setup is the same as for the quickstart (see quickstart.ts), from chainguard-sandbox-sdk/typescript:
//   - log in once:  bun bin/chainguard-sandbox.ts login
//     (if a run opens your browser, the saved login expired: approve it);
//   - choose the organization: Sandbox.create({ instance: "XXXXXX" }) in code, or
//     export CHAINCTL_SANDBOX_INSTANCE=XXXXXX for this terminal session, or
//     nothing if you belong to exactly one organization (auto-detected);
//   - list organization IDs:  bun bin/chainguard-sandbox.ts orgs
//     or chainctl iam organizations list.
import { CommandExitError, FileNotFoundError, Sandbox, TimeoutError } from "chainguard-sandbox-sdk";

// --8<-- [start:create]
const sbx = await Sandbox.create({
  name: "data-job", // display name "data-job/<word-word>"
  packages: ["bash", "coreutils", "python-3.12"], // Chainguard package names
  envs: { MODE: "test" }, // for every command
  egress: ["pypi.org", "files.pythonhosted.org"],
  // instance: "XXXXXX", // organization ID; else $CHAINCTL_SANDBOX_INSTANCE, else auto-detected
});
console.log("sandbox", sbx.sandboxId);
// ... and await sbx.kill() when done (or `await using sbx = ...`)
// --8<-- [end:create]

// --8<-- [start:run]
let r = await sbx.commands.run("echo $MODE && ls /work", {
  cwd: "/work",
  envs: { EXTRA: "1" },
  timeoutMs: 30_000, // 0 = no limit
  onStdout: (s) => process.stdout.write(`live: ${s}`),
});
console.log(r.exitCode, r.stdout);

// An array runs without a shell.
r = await sbx.commands.run(["python3", "-c", "import sys; print(sys.version)"]);
// --8<-- [end:run]

// --8<-- [start:errors]
try {
  await sbx.commands.run("exit 3");
} catch (e) {
  if (e instanceof CommandExitError) console.log("exit code", e.exitCode, "stderr", e.stderr);
}
try {
  await sbx.commands.run("sleep 10", { timeoutMs: 1000 });
} catch (e) {
  if (e instanceof TimeoutError) console.log("timed out; partial output:", e.result?.stdout);
}
// --8<-- [end:errors]

// --8<-- [start:background]
const server = await sbx.commands.run("python3 -m http.server 8000", { background: true });
console.log("pid", server.pid);
console.log("running:", (await server.poll()) === undefined);
console.log("killed:", await server.kill());

const job = await sbx.commands.run("for i in 1 2 3; do echo $i; sleep 1; done", { background: true });
const result = await job.wait({ onStdout: (s) => process.stdout.write(s) });
// --8<-- [end:background]
void result;

// --8<-- [start:files]
await sbx.files.write("data/input.csv", "a,b\n1,2\n");
await sbx.files.write([
  { path: "app/main.py", data: "print('hi')\n" },
  { path: "app/run.sh", data: "python3 app/main.py\n", mode: 0o755 },
]);
const text = await sbx.files.read("data/input.csv"); // string
const raw = await sbx.files.read("data/input.csv", { format: "bytes" }); // Uint8Array
for (const e of await sbx.files.list("app")) console.log(e.name, e.type, e.size, e.permissions);
const exists = await sbx.files.exists("app/main.py");
const created = await sbx.files.makeDir("out/reports");
await sbx.files.rename("data/input.csv", "data/archive/input.csv");
await sbx.files.remove("out");
try {
  await sbx.files.read("missing.txt");
} catch (e) {
  if (e instanceof FileNotFoundError) console.log("not found:", e.target);
}
// --8<-- [end:files]
void [text, raw, exists, created];

// --8<-- [start:transfer]
await sbx.files.upload("./local/model.bin", "models/model.bin");
await sbx.files.download("results/out.json", "./out/out.json");
await sbx.files.uploadDir("./my-project", "/work/project");
await sbx.files.downloadDir("/work/project/build", "./build");
// --8<-- [end:transfer]

// --8<-- [start:lifecycle]
const info = await sbx.getInfo();
console.log(info.state, info.region, info.cpuCount, info.memoryMB);
await sbx.keepAlive(30 * 60_000); // don't park while idle for 30 min
await sbx.pause(); // snapshot memory+disk; resumes on next call

const again = await Sandbox.connect(sbx.sandboxId); // from anywhere, later
console.log((await Sandbox.list()).length, "sandboxes");
await Sandbox.kill(sbx.sandboxId); // by ID (or sbx.kill())
// --8<-- [end:lifecycle]
void again;
process.exit(0);
