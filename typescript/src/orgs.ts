// Your Chainguard organizations, and auto-detecting the one to use.
//
// The organization ("instance") a Client works in can be given three ways, in this order:
//   1. explicitly: new Client({ instance: "<org id>" }) (or Sandbox.create({ instance }));
//   2. the environment: $CHAINCTL_SANDBOX_INSTANCE;
//   3. auto-detection: if neither is set, the client lists the organizations your identity
//      belongs to (Chainguard's account API) on first use and takes the only one. With several
//      it reports them. Turn it off with { detectInstance: false } or $CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1.
// List IDs with listOrganizations(), `chainguard-sandbox orgs` or `chainctl iam organizations list`.
import { cached, chainctlToken, type TokenSource } from "./auth";
import { browserLoginToken, PLATFORM_AUDIENCE } from "./login";

export const DEFAULT_PLATFORM_URL = "https://console-api.enforce.dev";
export const ENV_NO_DETECT_INSTANCE = "CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE";

/** A Chainguard organization (a root IAM group). */
export interface Organization {
  /** The 40-hex organization ID, used as the sessions instance. */
  id: string;
  name: string;
}

/** Your identity belongs to no Chainguard organization. */
export class NoOrganizationError extends Error {}

/** You belong to several organizations; choose one explicitly. */
export class AmbiguousOrganizationError extends Error {
  constructor(readonly organizations: Organization[]) {
    super(`chainguard-sandbox: you belong to several organizations; choose one with { instance } or $CHAINCTL_SANDBOX_INSTANCE:${organizations.map((o) => `\n  ${o.id}  ${o.name}`).join("")}`);
  }
}

/** Tokens for the account API, minted like defaultTokenSource() does for sessions. */
export function platformTokenSource(): TokenSource {
  if (process.env.CHAINGUARD_SANDBOX_AUTH === "chainctl") return cached(chainctlToken({ audience: PLATFORM_AUDIENCE }));
  return cached(
    browserLoginToken({
      audience: PLATFORM_AUDIENCE,
      login: { connection: process.env.CHAINGUARD_SANDBOX_SOCIAL_LOGIN || undefined },
      noBrowser: process.env.CHAINGUARD_SANDBOX_NO_BROWSER === "1",
    }),
  );
}

/**
 * Organizations visible to the token's identity, sorted by name. The token must be for the
 * account API (default: platformTokenSource()). Calls
 * GET https://console-api.enforce.dev/iam/v1/groups?uidp.in_root=true.
 */
export async function listOrganizations(tokenSource?: TokenSource, signal?: AbortSignal): Promise<Organization[]> {
  const tok = await (tokenSource ?? platformTokenSource()).token(signal);
  const base = (process.env.CHAINGUARD_SANDBOX_PLATFORM_URL || DEFAULT_PLATFORM_URL).replace(/\/$/, "");
  const t = AbortSignal.timeout(30_000);
  const resp = await fetch(`${base}/iam/v1/groups?uidp.in_root=true`, {
    headers: { Authorization: `Bearer ${tok}`, Accept: "application/json" },
    redirect: "manual",
    signal: signal ? AbortSignal.any([signal, t]) : t,
  });
  const body = await resp.text();
  if (!resp.ok) {
    let msg = body.trim();
    try {
      msg = JSON.parse(body).message || msg;
    } catch {}
    throw new Error(`listing organizations: HTTP ${resp.status}: ${msg}`);
  }
  const items: { id?: string; name?: string }[] = JSON.parse(body).items ?? [];
  return items
    .filter((g) => g.id && !g.id.includes("/"))
    .map((g) => ({ id: g.id!, name: g.name ?? "" }))
    .sort((a, b) => (a.name < b.name ? -1 : a.name > b.name ? 1 : 0));
}

/** The ID of the only organization the identity belongs to. */
export async function detectInstance(tokenSource?: TokenSource, signal?: AbortSignal): Promise<string> {
  const orgs = await listOrganizations(tokenSource, signal);
  if (orgs.length === 0) throw new NoOrganizationError("chainguard-sandbox: your identity belongs to no Chainguard organization");
  if (orgs.length > 1) throw new AmbiguousOrganizationError(orgs);
  return orgs[0]!.id;
}

export function noInstanceMessage(detectError?: string): string {
  let msg =
    "chainguard-sandbox: no organization configured. Set one of:\n" +
    "  - { instance } (Client or Sandbox.create) to the organization ID\n" +
    "  - $CHAINCTL_SANDBOX_INSTANCE=<organization ID>\n" +
    "  - or let it be auto-detected when you belong to exactly one organization\n" +
    "List your organization IDs with `chainguard-sandbox orgs` or `chainctl iam organizations list`.";
  if (detectError) msg += `\nAuto-detection: ${detectError}`;
  return msg;
}
