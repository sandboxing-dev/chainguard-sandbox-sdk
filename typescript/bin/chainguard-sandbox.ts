#!/usr/bin/env bun
// chainguard-sandbox: a small CLI for the Simple Chainguard Sandbox SDK (TypeScript).
//
//   bun bin/chainguard-sandbox.ts login [github|google-oauth2|gitlab]   # browser login, saved for later runs
//   bun bin/chainguard-sandbox.ts whoami                                # the saved login
//   bun bin/chainguard-sandbox.ts logout
//   bun bin/chainguard-sandbox.ts orgs                                  # your organization IDs
//   export CHAINCTL_SANDBOX_INSTANCE=XXXXXX                   # only needed with several
//   bun bin/chainguard-sandbox.ts status                                # is Workspaces turned on for the organization?
//   bun bin/chainguard-sandbox.ts enable                                # turn it on (asks before accepting the terms)
//   bun bin/chainguard-sandbox.ts ls                                    # your sandboxes
//   bun bin/chainguard-sandbox.ts rm SANDBOX_ID                         # delete one
import { FileLoginStore, browserLogin, browserLoginToken, defaultLoginFile, listOrganizations, logout, tokenClaims } from "../src/index";
import { Sandbox, TermsNotAcceptedError, enableWorkspaces, workspacesStatus, type WorkspacesStatus } from "../src/sandbox/index";

/**
 * Ordered stdout/stderr writers. Bun's process.stdout.write can drop data still
 * queued for a pipe when the process exits, so output goes through Bun.write
 * (which resolves once written) and is awaited before exiting.
 */
let outChain: Promise<unknown> = Promise.resolve();
let errChain: Promise<unknown> = Promise.resolve();
const out = (b: Uint8Array | string) => void (outChain = outChain.then(() => Bun.write(Bun.stdout, b)));
const err = (b: Uint8Array | string) => void (errChain = errChain.then(() => Bun.write(Bun.stderr, b)));
const println = (s: string) => out(s + "\n");

function usage(): never {
  console.error("usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | status [ORG] | enable [--accept-terms] [ORG] | ls | rm SANDBOX_ID");
  process.exit(2);
}

function table(rows: string[][]) {
  const w = rows[0]!.map((_, i) => Math.max(...rows.map((r) => r[i]!.length)));
  for (const r of rows) println(r.map((v, i) => v.padEnd(w[i]!)).join("  ").trimEnd());
}

async function loginCmd(cmd: string, args: string[]): Promise<void> {
  if (cmd === "logout") {
    if (args.length) usage();
    logout();
    println(`logged out: ${defaultLoginFile()}`);
    return;
  }
  if (cmd === "login") {
    if (args.length > 1) usage();
    const res = await browserLogin({ connection: args[0] ?? (process.env.CHAINGUARD_SANDBOX_SOCIAL_LOGIN || undefined), out: (m) => err(m + "\n") });
    if (!res.refreshToken) throw new Error("the issuer returned no refresh token, so the login can't be saved");
    new FileLoginStore().save(undefined, res.refreshToken);
    println(`logged in; saved to ${defaultLoginFile()}`);
  } else if (args.length) usage();
  const c = tokenClaims(await browserLoginToken({ noBrowser: true }).token());
  const aud = Array.isArray(c.aud) ? c.aud : [c.aud];
  println(`subject:  ${c.sub}\nissuer:   ${c.iss}\naudience: ${JSON.stringify(aud)}\nexpires:  ${new Date((c.exp ?? 0) * 1000).toLocaleString()}`);
}

function printStatus(st: WorkspacesStatus) {
  println(`organization: ${st.organization}\nplan:         ${st.plan}\nworkspaces:   ${st.enabled ? "on" : "off"}`);
  if (st.maxSandboxes || st.maxSandboxesPerUser) println(`sandboxes:    up to ${st.maxSandboxesPerUser} per user, ${st.maxSandboxes} in the organization`);
  if (st.missingTerms.length === 0) println("terms:        accepted");
  else {
    println("terms:        not accepted yet:");
    for (const d of st.missingTerms) println(`  - ${d.label}: ${d.url}`);
  }
}

/** status [ORG] | enable [--accept-terms] [ORG] */
async function setupCmd(cmd: string, args: string[]): Promise<number | void> {
  let accept = false;
  let instance: string | undefined;
  for (const a of args) {
    if (a === "--accept-terms" || a === "--yes") accept = true;
    else if (a.startsWith("-") || instance) usage();
    else instance = a;
  }
  if (accept && cmd === "status") usage();
  let st = await workspacesStatus({ instance });
  if (cmd === "status") {
    printStatus(st);
    if (!st.ready) {
      println("\nSandboxes can't be created yet. An organization owner can turn Workspaces on with: chainguard-sandbox enable");
      return 1;
    }
    return;
  }
  if (st.ready) {
    printStatus(st);
    println("\nWorkspaces is already on.");
    return;
  }
  println(`Turning on Chainguard Workspaces (and Checks) for organization ${st.organization}.`);
  if (st.missingTerms.length && !accept) {
    println("\nThe organization has to accept these documents first:");
    for (const d of st.missingTerms) println(`  - ${d.label}: ${d.url}`);
    await outChain;
    const answer = prompt('\nDo you accept them on behalf of the organization? Type "yes" to accept:');
    if ((answer ?? "").trim().toLowerCase() !== "yes") {
      err("chainguard-sandbox: not accepted; nothing was changed\n");
      return 1;
    }
    accept = true;
  }
  try {
    st = await enableWorkspaces({ instance: st.organization, acceptTerms: accept });
  } catch (e) {
    if (!(e instanceof TermsNotAcceptedError)) throw e;
    err(`chainguard-sandbox: ${e.message}\nRe-run with --accept-terms once you have read them\n`);
    return 1;
  }
  println("");
  printStatus(st);
  println("\nWorkspaces is on. Create a sandbox with the SDK, or list them with: chainguard-sandbox ls");
}

async function main(): Promise<number | void> {
  const [cmd, ...args] = process.argv.slice(2);
  if (!cmd) usage();
  switch (cmd) {
    case "login":
    case "logout":
    case "whoami":
      return loginCmd(cmd, args);
    case "orgs": {
      if (args.length) usage();
      const orgs = await listOrganizations();
      table([["ID", "NAME"], ...orgs.map((o) => [o.id, o.name])]);
      return;
    }
    case "status":
    case "enable":
      return setupCmd(cmd, args);
    case "ls": {
      if (args.length) usage();
      const rows = (await Sandbox.list()).map((s) => [s.sandboxId, s.displayName, s.state, s.region, s.createdAt?.toLocaleString() ?? "-"]);
      table([["ID", "NAME", "STATE", "REGION", "CREATED"], ...rows]);
      return;
    }
    case "rm": {
      if (args.length !== 1) usage();
      if (!(await Sandbox.kill(args[0]!))) {
        err(`chainguard-sandbox: no sandbox ${args[0]}\n`);
        return 1;
      }
      return;
    }
    default:
      usage();
  }
}

/** connect-node keeps its HTTP/2 session open, so exit explicitly, once all output is written. */
async function exitAfterDrain(code: number): Promise<never> {
  await Promise.allSettled([outChain, errChain]);
  process.exit(code);
}

main().then(
  (code) => exitAfterDrain(typeof code === "number" ? code : 0),
  (e) => {
    err(`chainguard-sandbox: ${e instanceof Error ? e.message : e}\n`);
    return exitAfterDrain(1);
  },
);
