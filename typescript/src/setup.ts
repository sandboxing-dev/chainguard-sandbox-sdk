// Turning Workspaces on for an organization.
//
// Sessions can be created only in an organization that is entitled to Workspaces.
// `chainctl develop init` gets an organization there in these steps, each a call to
// Chainguard's platform API (DEFAULT_PLATFORM_URL):
//
//   1. Find out which legal documents the organization has accepted:
//        GET /iam/v1/terms/{org}                                     (acceptedTerms)
//   2. Accept the ones Workspaces needs (WORKSPACES_TERMS):
//        POST /iam/v1/terms/{org}  {"documentIds": [...]}             (acceptTerms)
//   3. Turn the feature on, with the plan's allowance:
//        POST /entitlements/v1alpha1/features/{org}:enableSandbox     (enableSandbox)
//   4. Read the result:
//        GET /entitlements/v2beta1/organizationEntitlements?parent={org}   (workspacesEntitlement)
//
// chainctl also links a GitHub account to the organization between steps 2 and 3. That is for
// Checks and for sessions that act on GitHub; this module does not do it. Creating the
// organization itself (sign-up) is not covered either: use the Chainguard Console or
// `chainctl develop init`.
//
// All of these need a token for the platform API (platformTokenSource()), and steps 2 and 3
// need the owner role in the organization.
import { type TokenSource } from "./auth";
import { DEFAULT_PLATFORM_URL, platformTokenSource } from "./orgs";

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
  ) {
    super(`${what}: platform API: HTTP ${httpStatus}: ${detail}`);
    this.name = "PlatformError";
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
    for (const d of (parsed.details as { missing?: { id?: string; label?: string; url?: string }[] }[] | undefined) ?? []) {
      for (const m of d?.missing ?? []) {
        const doc = termsDocument(m.id ?? "");
        missing.push({ id: doc.id, label: m.label || doc.label, url: m.url || doc.url });
      }
    }
    throw new PlatformError(what, resp.status, Number(parsed.code ?? 0), (parsed.message as string) || text.trim(), missing);
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
