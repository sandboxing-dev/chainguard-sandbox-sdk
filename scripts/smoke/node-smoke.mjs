// Checks an installed chainguard-sandbox-sdk package: both entry points load and export the API.
// With --live it also creates a sandbox, runs a command, moves a file and deletes the sandbox
// (needs a saved login and an organization with Workspaces on).
import { Sandbox, workspacesStatus, enableWorkspaces } from "chainguard-sandbox-sdk";
import { Client, browserLogin } from "chainguard-sandbox-sdk/workspaces";

for (const [name, v] of Object.entries({ Sandbox, workspacesStatus, enableWorkspaces, Client, browserLogin })) {
  if (typeof v !== "function") throw new Error(`${name} is not exported`);
}
console.log(`imports ok (${process.versions.bun ? "bun " + process.versions.bun : "node " + process.versions.node})`);

if (process.argv.includes("--live")) {
  const sbx = await Sandbox.create({ packages: ["bash", "coreutils"] });
  try {
    const r = await sbx.commands.run("echo hello from $(uname -s)");
    if (r.stdout.trim() !== "hello from Linux") throw new Error(`unexpected output: ${JSON.stringify(r.stdout)}`);
    await sbx.files.write("notes/hello.txt", "hi");
    if ((await sbx.files.read("notes/hello.txt")) !== "hi") throw new Error("file round trip failed");
    console.log("live ok: created a sandbox, ran a command, wrote and read a file");
  } finally {
    await sbx.kill();
  }
}
process.exit(0);
