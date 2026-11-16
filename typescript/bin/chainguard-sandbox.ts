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
import { createInterface } from "node:readline/promises";
import { FileLoginStore, browserLogin, browserLoginToken, defaultLoginFile, listOrganizations, logout, tokenClaims } from "../src/index.js";
import { GitHubAppNotInstalledError, Sandbox, TermsNotAcceptedError, enableWorkspaces, workspacesStatus, type WorkspacesStatus } from "../src/sandbox/index.js";
import { GITHUB_APP_INSTALL_URL, openBrowser } from "../src/index.js";

/**
 * Ordered stdout/stderr writers. A process.exit can drop output still queued for a pipe, so each
 * write is awaited (its callback runs once the data is handed to the system) before exiting.
 */
let outChain: Promise<unknown> = Promise.resolve();
let errChain: Promise<unknown> = Promise.resolve();
const write = (s: NodeJS.WriteStream, b: Uint8Array | string) => new Promise<void>((r) => void s.write(b, () => r()));
const out = (b: Uint8Array | string) => void (outChain = outChain.then(() => write(process.stdout, b)));
const err = (b: Uint8Array | string) => void (errChain = errChain.then(() => write(process.stderr, b)));
/** Ask a question on the terminal and read one line; "" when input is closed. */
async function ask(question: string): Promise<string> {
  const rl = createInterface({ input: process.stdin, output: process.stderr });
  try {
    return await rl.question(question + " ");
  } catch {
    return "";
  } finally {
    rl.close();
  }
}
const println = (s: string) => out(s + "\n");

function usage(): never {
  console.error("usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | status [ORG] | enable [--accept-terms] [--github ACCOUNT] [--port N] [ORG] | ls | rm SANDBOX_ID");
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
  if (st.githubAccounts.length) println(`github:       linked to ${st.githubAccounts.join(", ")}`);
  else if (!st.enabled) println("github:       no account linked yet");
  if (st.missingTerms.length === 0) println("terms:        accepted");
  else {
    println("terms:        not accepted yet:");
    for (const d of st.missingTerms) println(`  - ${d.label}: ${d.url}`);
  }
}

/** status [ORG] | enable [--accept-terms] [--github ACCOUNT] [--port N] [ORG] */
async function setupCmd(cmd: string, args: string[]): Promise<number | void> {
  let accept = false;
  let instance: string | undefined;
  let githubAccount: string | undefined;
  let githubPort: number | undefined;
  for (let i = 0; i < args.length; i++) {
    const a = args[i]!;
    if (a === "--accept-terms" || a === "--yes") accept = true;
    else if (a === "--github" && i + 1 < args.length) githubAccount = args[++i];
    else if (a === "--port" && i + 1 < args.length) githubPort = Number(args[++i]);
    else if (a.startsWith("-") || instance) usage();
    else instance = a;
  }
  if (cmd === "status" && (accept || githubAccount || githubPort)) usage();
  if (githubPort !== undefined && !Number.isInteger(githubPort)) usage();
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
    const answer = await ask('\nDo you accept them on behalf of the organization? Type "yes" to accept:');
    if (answer.trim().toLowerCase() !== "yes") {
      err("chainguard-sandbox: not accepted; nothing was changed\n");
      return 1;
    }
    accept = true;
  }
  if (st.githubAccounts.length === 0) {
    println(
      "\nNext, a GitHub account is linked to the organization: Chainguard requires one before it turns\n" +
        "Workspaces on. Your browser opens GitHub, where you choose the account (your own user, unless you\n" +
        `passed --github ORG). The Chainguard GitHub App has to be installed on that account:\n  ${GITHUB_APP_INSTALL_URL}`,
    );
  }
  const organization = st.organization;
  for (let attempt = 1; ; attempt++) {
    try {
      st = await enableWorkspaces({ instance: organization, acceptTerms: accept, githubAccount, githubPort });
      break;
    } catch (e) {
      if (e instanceof TermsNotAcceptedError) {
        err(`chainguard-sandbox: ${e.message}\nRe-run with --accept-terms once you have read them\n`);
        return 1;
      }
      // The app isn't on the account yet: install it, then authorize again.
      if (!(e instanceof GitHubAppNotInstalledError) || attempt >= 3) throw e;
      println(`\nThe Chainguard GitHub App is not installed on that GitHub account yet.\nInstall it here (opening it in your browser), then come back:\n  ${e.installUrl}`);
      await outChain;
      await openBrowser(e.installUrl);
      await ask("\nPress Enter once the app is installed, to authorize again:");
    }
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
