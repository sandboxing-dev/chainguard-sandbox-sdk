// Quickstart: create a sandbox, run a command, write and read a file, delete it.
//
// Before you run it:
//
//  0. Get the SDK and install its dependencies:
//
//        git clone https://github.com/sandboxing-dev/chainguard-sandbox-sdk
//        cd chainguard-sandbox-sdk/typescript && bun install
//
//     (In your own project, add it with: bun add ../chainguard-sandbox-sdk/typescript)
//
//  1. Log in once, from chainguard-sandbox-sdk/typescript. It opens your browser; the login is saved and
//     shared by the Go, Python and TypeScript SDKs:
//
//        bun bin/chainguard-sandbox.ts login     # optional: add github, google-oauth2 or gitlab
//
//     A saved login lasts as long as it's used at least once every 20 hours. If a run opens your
//     browser, the login expired: approve it and the run continues, or renew it ahead of time with
//     the same login command.
//
//  2. Choose the organization (the "instance"). Any one of these works; the first one set wins:
//
//     a. in code: Sandbox.create({ instance: "XXXXXX" }) (commented out below);
//     b. an environment variable, for this terminal session only:
//          export CHAINCTL_SANDBOX_INSTANCE=XXXXXX    # your organization ID
//     c. nothing: if you belong to exactly one organization, it is detected automatically
//        (turn that off with CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1).
//
//     List your organization IDs with either of:
//
//        bun bin/chainguard-sandbox.ts orgs
//        chainctl iam organizations list
//
//  3. Run it, from chainguard-sandbox-sdk/typescript:
//
//        bun examples/quickstart.ts
import { Sandbox } from "chainguard-sandbox-sdk";

{
  // --8<-- [start:quickstart]
  await using sbx = await Sandbox.create({
    packages: ["bash", "coreutils", "python-3.13"],
    // instance: "XXXXXX", // your organization ID; overrides $CHAINCTL_SANDBOX_INSTANCE and auto-detection
  }); // deleted at the end of the block

  const result = await sbx.commands.run(`python3 -c "print('hello from', __import__('platform').node())"`);
  console.log(result.stdout.trimEnd());

  await sbx.files.write("notes/hello.txt", "hi");
  console.log(await sbx.files.read("notes/hello.txt"));
  // --8<-- [end:quickstart]
}

// connect-node keeps its HTTP/2 session open, so exit explicitly, after the block has disposed the sandbox.
process.exit(0);
