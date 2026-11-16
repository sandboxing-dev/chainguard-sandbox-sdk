// Turning Workspaces on for an organization.
//
// Sessions can be created only in an organization that is entitled to Workspaces.
// `chainctl develop init` gets an organization there in these steps, each a call to
// Chainguard's platform API (DEFAULT_PLATFORM_URL):
//
//   1. Find out which legal documents the organization has accepted:
//        GET /iam/v1/terms/{org}                                     (acceptedTerms)
//   2. Accept the ones turning it on needs (ENABLE_TERMS):
//        POST /iam/v1/terms/{org}  {"documentIds": [...]}             (acceptTerms)
//   3. Link a GitHub account (an organization, or your own user) to the organization. The service
//      refuses step 4 without it. The Chainguard GitHub App must be installed on that account
//      (GITHUB_APP_INSTALL_URL).
//        GET  /guardener/v1alpha1/githubAssociations/{org}            (linkedGitHubOrganizations)
//        GET  /guardener/v1alpha1/githubAssociations:begin            (beginGitHubOAuth)
//        browser: GitHub's authorize page, back to a loopback port    (authorizeGitHub)
//        POST /guardener/v1alpha1/githubAssociations/{org}:link       (linkGitHubOrganization)
//   4. Turn the feature on, with the plan's allowance:
//        POST /entitlements/v1alpha1/features/{org}:enableSandbox     (enableSandbox)
//   5. Read the result:
//        GET /entitlements/v2beta1/organizationEntitlements?parent={org}   (workspacesEntitlement)
//
// Creating the organization itself (sign-up) is not covered: use the Chainguard Console or
// `chainctl develop init`.
//
// All of these need a token for the platform API (platformTokenSource()), and steps 2 to 4 need
// the owner role in the organization. Step 3 also needs you to own the GitHub account.
import { createHash, randomBytes } from "node:crypto";
import { setTimeout as sleep } from "node:timers/promises";
import { type TokenSource } from "./auth.js";
import { openBrowser } from "./login.js";
import { serveLoopback, type LoopbackServer } from "./loopback.js";
import { DEFAULT_PLATFORM_URL, platformTokenSource } from "./orgs.js";

/** A legal document an organization accepts. */
export interface TermsDocument {
  id: string;
  label: string;
  url: string;
}

/**
 * The documents an organization must accept to use Workspaces: the Secured Platform Terms and the
 * Master Service and License Agreement. (Source: chainguard.dev/sdk, proto/chainguard/platform/iam/terms.)
 */
export const WORKSPACES_TERMS: readonly string[] = ["secure-platform-terms.v1", "msla.v1"];

/**
 * The documents an organization accepts when Workspaces is turned on for it the way
 * `chainctl develop init` does: the Workspaces terms, the Code Analysis Terms (the same switch turns
 * on Chainguard Checks), and the Terms of Service and Data Privacy Agreement that linking a GitHub
 * account requires.
 */
export const ENABLE_TERMS: readonly string[] = ["secure-platform-terms.v1", "msla.v1", "code-analysis-terms.v1", "guardener-tos.v1", "sfdpa.v1"];

/** Where the Chainguard GitHub App is installed on a GitHub organization or user. Linking that account fails until it is. */
export const GITHUB_APP_INSTALL_URL = "https://github.com/apps/chainguard/installations/new";

/** The loopback port the GitHub authorization redirects to (the one chainctl uses). Another free port works too. */
export const DEFAULT_GITHUB_OAUTH_PORT = 8989;

const KNOWN_TERMS: Record<string, [label: string, url: string]> = {
  "secure-platform-terms.v1": ["Secured Platform Terms", "https://www.chainguard.dev/legal/secure-platform-terms"],
  "msla.v1": ["Master Service and License Agreement", "https://www.chainguard.dev/legal/master-service-and-license-agreement"],
  "code-analysis-terms.v1": ["Code Analysis Terms", "https://www.chainguard.dev/legal/code-analysis-terms"],
  "guardener-tos.v1": ["Terms of Service", "https://www.chainguard.dev/legal/guardener"],
  "sfdpa.v1": ["Data Privacy Agreement", "https://www.chainguard.dev/legal/supplemental-dpa"],
};

/** The label and URL of a known document ID, or a document with only the ID. */
export function termsDocument(id: string): TermsDocument {
  const [label, url] = KNOWN_TERMS[id] ?? ["", ""];
  return { id, label, url };
}

/**
 * A non-2xx answer from the platform API. `code` is the gRPC status code the gateway reports
 * (7 = permission denied, 9 = failed precondition, ...). `missingTerms` is set when the call was
 * refused because the organization has not accepted those documents.
 */
export class PlatformError extends Error {
  constructor(
    what: string,
    readonly httpStatus: number,
    readonly code: number,
    readonly detail: string,
    readonly missingTerms: TermsDocument[],
    /** The machine-readable reason, when the service gives one. */
    readonly reason = "",
  ) {
    super(`${what}: platform API: HTTP ${httpStatus}: ${detail}`);
    this.name = "PlatformError";
  }
  /** The organization has to link a GitHub account first. */
  get githubLinkRequired(): boolean {
    return this.reason === "GUARDENER_GITHUB_LINK_REQUIRED" || this.detail.includes("must link a GitHub organization");
  }
  /** The Chainguard GitHub App is not installed on the GitHub account. */
  get githubAppNotInstalled(): boolean {
    return this.reason.endsWith("GITHUB_APP_NOT_INSTALLED") || this.detail.includes("must be installed on the GitHub");
  }
  /** The GitHub account is linked to another Chainguard organization. */
  get githubAlreadyLinked(): boolean {
    return this.reason === "GUARDENER_GITHUB_ALREADY_LINKED" || this.detail.includes("already linked to a different");
  }
}

export interface SetupCallOptions {
  /** Mints tokens for the platform API (default: platformTokenSource()). */
  tokenSource?: TokenSource;
  signal?: AbortSignal;
}

async function call(what: string, method: "GET" | "POST", path: string, body: unknown, o: SetupCallOptions): Promise<Record<string, unknown>> {
  const tok = await (o.tokenSource ?? platformTokenSource()).token(o.signal);
  const base = (process.env.CHAINGUARD_SANDBOX_PLATFORM_URL || DEFAULT_PLATFORM_URL).replace(/\/$/, "");
  const t = AbortSignal.timeout(30_000);
  const headers: Record<string, string> = { Authorization: `Bearer ${tok}`, Accept: "application/json" };
  if (body !== undefined) headers["Content-Type"] = "application/json";
  const resp = await fetch(base + path, {
    method,
    headers,
    body: body === undefined ? undefined : JSON.stringify(body),
    redirect: "manual",
    signal: o.signal ? AbortSignal.any([o.signal, t]) : t,
  });
  const text = await resp.text();
  let parsed: Record<string, unknown> = {};
  try {
    const v = JSON.parse(text);
    if (v && typeof v === "object" && !Array.isArray(v)) parsed = v;
  } catch {}
  if (!resp.ok) {
    // A refusal for unaccepted terms carries a detail listing the missing documents.
    const missing: TermsDocument[] = [];
    let reason = "";
    for (const d of (parsed.details as { reason?: string; missing?: { id?: string; label?: string; url?: string }[] }[] | undefined) ?? []) {
      reason ||= d?.reason ?? "";
      for (const m of d?.missing ?? []) {
        const doc = termsDocument(m.id ?? "");
        missing.push({ id: doc.id, label: m.label || doc.label, url: m.url || doc.url });
      }
    }
    throw new PlatformError(what, resp.status, Number(parsed.code ?? 0), (parsed.message as string) || text.trim(), missing, reason);
  }
  return parsed;
}

function orgPath(org: string): string {
  if (!org || /[?#]/.test(org)) throw new Error(`chainguard-sandbox: invalid organization ID ${JSON.stringify(org)}`);
  return encodeURIComponent(org);
}

/** IDs of the legal documents the organization has accepted. */
export async function acceptedTerms(org: string, o: SetupCallOptions = {}): Promise<string[]> {
  const out = await call("listing accepted terms", "GET", `/iam/v1/terms/${orgPath(org)}`, undefined, o);
  return ((out.items as { docId?: string }[] | undefined) ?? []).map((i) => i.docId ?? "");
}

/** The documents Workspaces needs that the organization has not accepted yet. */
export async function missingWorkspacesTerms(org: string, o: SetupCallOptions = {}): Promise<TermsDocument[]> {
  const have = new Set(await acceptedTerms(org, o));
  return WORKSPACES_TERMS.filter((id) => !have.has(id)).map(termsDocument);
}

/**
 * Record that the organization accepts these documents. This is a legal act on the organization's
 * behalf: call it only when the person running your program has agreed to the documents. Needs the
 * owner role; accepting a document again changes nothing.
 */
export async function acceptTerms(org: string, documentIds: readonly string[], o: SetupCallOptions = {}): Promise<void> {
  if (documentIds.length === 0) throw new Error("chainguard-sandbox: no documents to accept");
  await call("accepting terms", "POST", `/iam/v1/terms/${orgPath(org)}`, { documentIds: [...documentIds] }, o);
}

/**
 * Turn on Checks and Workspaces for the organization, with its plan's monthly allowance (step 5 of
 * `chainctl develop init`). An allowance Chainguard already granted is kept. Needs the owner role,
 * and the organization must have accepted the terms first: otherwise PlatformError's
 * `missingTerms` lists the documents.
 */
export async function enableSandbox(org: string, o: SetupCallOptions = {}): Promise<void> {
  await call("turning on Workspaces", "POST", `/entitlements/v1alpha1/features/${orgPath(org)}:enableSandbox`, {}, o);
}

/** An organization's Workspaces entitlement. */
export interface Entitlement {
  /** Plan ID ("starter", "team", ...) and display name. */
  plan: string;
  planName: string;
  /** Whether Workspaces is turned on. */
  active: boolean;
  /** The raw state ("STATE_ACTIVE", "STATE_NOT_INCLUDED", or "" when the product isn't listed). */
  state: string;
  /** Concurrent-session limits (0: not reported). */
  maxSandboxes: number;
  maxSandboxesPerUser: number;
}

/** Whether Workspaces is turned on for the organization, its plan, and its session limits. */
export async function workspacesEntitlement(org: string, o: SetupCallOptions = {}): Promise<Entitlement> {
  orgPath(org);
  const q = new URLSearchParams({ parent: org, products: "PRODUCT_SANDBOX_WORKSPACES" });
  const out = await call("reading the organization's entitlements", "GET", `/entitlements/v2beta1/organizationEntitlements?${q}`, undefined, o);
  const plan = (out.plan as { id?: string; displayName?: string } | undefined) ?? {};
  const e: Entitlement = { plan: plan.id ?? "", planName: plan.displayName ?? "", active: false, state: "", maxSandboxes: 0, maxSandboxesPerUser: 0 };
  for (const p of (out.products as { product?: string; state?: string; limits?: { displayName?: string; max?: string }[] }[] | undefined) ?? []) {
    if (p.product !== "PRODUCT_SANDBOX_WORKSPACES") continue;
    e.state = p.state ?? "";
    e.active = p.state === "STATE_ACTIVE";
    for (const l of p.limits ?? []) {
      const n = Number(l.max);
      if (!Number.isFinite(n)) continue;
      if (l.displayName === "concurrent workspaces") e.maxSandboxes = n;
      if (l.displayName === "concurrent workspaces per user") e.maxSandboxesPerUser = n;
    }
  }
  return e;
}

// ---------------------------------------------------------------- linking a GitHub account

/** The GitHub logins (organizations or users) linked to the organization. */
export async function linkedGitHubOrganizations(org: string, o: SetupCallOptions = {}): Promise<string[]> {
  const out = await call("listing linked GitHub accounts", "GET", `/guardener/v1alpha1/githubAssociations/${orgPath(org)}`, undefined, o);
  return ((out.organizations as { organization?: string }[] | undefined) ?? []).map((g) => g.organization ?? "");
}

/**
 * What the service hands out to start a GitHub authorization: the Chainguard GitHub App's OAuth
 * client ID, and a state value the service signed and checks again when the account is linked.
 */
export interface GitHubOAuthStart {
  clientId: string;
  state: string;
}

/** Start a GitHub authorization. */
export async function beginGitHubOAuth(o: SetupCallOptions = {}): Promise<GitHubOAuthStart> {
  const out = await call("starting the GitHub authorization", "GET", "/guardener/v1alpha1/githubAssociations:begin", undefined, o);
  if (!out.clientId || !out.state) throw new PlatformError("starting the GitHub authorization", 200, 0, "the service returned no client ID or state", []);
  return { clientId: out.clientId as string, state: out.state as string };
}

/** The result of authorizeGitHub. It holds no GitHub token: the service exchanges the code itself when the account is linked. */
export interface GitHubAuthorization {
  code: string;
  redirectUri: string;
  verifier: string;
  state: string;
}

export interface GitHubAuthorizeOptions {
  /** The loopback port to receive GitHub's redirect on (default DEFAULT_GITHUB_OAUTH_PORT). */
  port?: number;
  /** How long to wait for the person (default 10 minutes). */
  timeoutMs?: number;
  /** Opens the page (default openBrowser). Resolve false to have the URL printed only. */
  openUrl?: (url: string) => boolean | void | Promise<boolean | void>;
  /** Receives the instructions (default: written to stderr). */
  out?: (message: string) => void;
}

/**
 * Send the person to GitHub to authorize the Chainguard GitHub App and wait for GitHub's redirect
 * to a loopback port. It is the flow chainctl uses (chainguard.dev/sdk/githubflow): PKCE with S256,
 * the account picker forced, and the state checked on return.
 */
export async function authorizeGitHub(start: GitHubOAuthStart, o: GitHubAuthorizeOptions = {}): Promise<GitHubAuthorization> {
  const port = o.port || DEFAULT_GITHUB_OAUTH_PORT;
  const say = o.out ?? ((m: string) => void process.stderr.write(m + "\n"));
  const redirectUri = `http://127.0.0.1:${port}/callback`;
  const verifier = randomBytes(32).toString("base64url");
  const challenge = createHash("sha256").update(verifier).digest("base64url");
  const url =
    "https://github.com/login/oauth/authorize?" +
    new URLSearchParams({
      client_id: start.clientId,
      redirect_uri: redirectUri,
      response_type: "code",
      state: start.state,
      code_challenge: challenge,
      code_challenge_method: "S256",
      prompt: "select_account",
      allow_signup: "false",
    });
  let settle!: (r: { code?: string; error?: string }) => void;
  const got = new Promise<{ code?: string; error?: string }>((r) => (settle = r));
  let server: LoopbackServer;
  try {
    server = await serveLoopback(port, (_method, u) => {
      if (u.pathname !== "/callback") return { status: 404, body: "not found" };
      const q = u.searchParams;
      let status = 400;
      let page: string;
      if (q.get("error")) {
        page = "Authorization was denied. You can close this window.";
        settle({ error: `GitHub refused the authorization: ${q.get("error")}: ${q.get("error_description") ?? ""}` });
      } else if (q.get("state") !== start.state) {
        page = "Authorization state mismatch. You can close this window.";
        settle({ error: "GitHub's redirect carried the wrong state" });
      } else if (!q.get("code")) {
        page = "Missing authorization code. You can close this window.";
        settle({ error: "GitHub's redirect carried no authorization code" });
      } else {
        status = 200;
        page = "Authorization complete. You can close this window and return to the terminal.";
        settle({ code: q.get("code")! });
      }
      // "Connection: close": the browser must not keep this connection for a later attempt, which
      // gets a new server on the same port.
      return { status, headers: { "Content-Type": "text/plain; charset=utf-8", Connection: "close" }, body: page };
    });
  } catch (e) {
    throw new Error(`chainguard-sandbox: listening on loopback port ${port} for GitHub's redirect (choose another port if it is taken): ${e}`);
  }
  try {
    let opened = false;
    try {
      opened = (await (o.openUrl ?? openBrowser)(url)) !== false;
    } catch {}
    say(opened ? `Opened a browser to authorize with GitHub. If it didn't open, visit:\n  ${url}` : `Open this URL in a browser to authorize with GitHub:\n  ${url}`);
    const timeoutMs = o.timeoutMs ?? 600_000;
    const res = await Promise.race([got, sleep(timeoutMs).then(() => ({ error: `the GitHub authorization was not finished within ${timeoutMs} ms` }) as { code?: string; error?: string })]);
    if (res.error || !res.code) throw new Error(`chainguard-sandbox: ${res.error}`);
    await sleep(100); // let the browser finish loading the result page
    return { code: res.code, redirectUri, verifier, state: start.state };
  } finally {
    await server.stop(true); // close kept-alive connections too, so the port is free for another attempt
  }
}

/**
 * Link a GitHub account to the Chainguard organization, using an authorization from
 * authorizeGitHub. `githubLogin` is the account to link, an organization or a user; empty means the
 * GitHub user who authorized. Resolves the linked login.
 *
 * The Chainguard GitHub App must be installed on the account (otherwise the PlatformError has
 * `githubAppNotInstalled`), and you must own it. An account can be linked to one Chainguard
 * organization at a time.
 */
export async function linkGitHubOrganization(org: string, a: GitHubAuthorization, githubLogin = "", o: SetupCallOptions = {}): Promise<string> {
  const body: Record<string, string> = { code: a.code, redirectUri: a.redirectUri, codeVerifier: a.verifier, state: a.state };
  if (githubLogin) body.organization = githubLogin;
  const out = await call("linking the GitHub account", "POST", `/guardener/v1alpha1/githubAssociations/${orgPath(org)}:link`, body, o);
  return (out.organization as string) ?? "";
}

