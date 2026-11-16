// Token sources for the sandbox plane. Mirrors the Go SDK's workspaces/auth.go
// and exchange.go.

export const DEFAULT_AUDIENCE = "https://sessions.chainbox.dev";
export const DEFAULT_ISSUER = "https://issuer.enforce.dev";
export const ENV_AUTH_TOKEN = "CHAINCTL_AUTH_TOKEN";

import { execFile } from "node:child_process";
import { readFile } from "node:fs/promises";
import { browserLoginToken } from "./login.js";

/** Returns a Chainguard bearer token whose audience includes DEFAULT_AUDIENCE. */
export interface TokenSource {
  token(signal?: AbortSignal): Promise<string>;
}

/** Adapts a function to TokenSource. */
export function tokenFunc(f: (signal?: AbortSignal) => Promise<string>): TokenSource {
  return { token: f };
}

/** Always returns the same token. */
export function staticToken(tok: string): TokenSource {
  return {
    async token() {
      if (!tok) throw new Error("chainguard-sandbox: empty token");
      return tok;
    },
  };
}

/**
 * Mints tokens with `chainctl auth token --audience https://sessions.chainbox.dev`,
 * reusing the login chainctl saved (`chainctl auth login` / `chainctl develop init`).
 */
export function chainctlToken(opts: { path?: string; audience?: string } = {}): TokenSource {
  const path = opts.path ?? "chainctl";
  const aud = opts.audience ?? DEFAULT_AUDIENCE;
  return {
    async token(signal) {
      const { out, err, code } = await new Promise<{ out: string; err: string; code: number | string }>((resolve, reject) => {
        execFile(path, ["auth", "token", "--audience", aud], { signal, encoding: "utf8" }, (e, stdout, stderr) => {
          if (e && signal?.aborted) return reject(signal.reason);
          if (e && typeof e.code !== "number") return reject(e); // not started (e.g. chainctl isn't installed)
          resolve({ out: stdout, err: stderr, code: e ? e.code! : 0 });
        });
      });
      if (code !== 0) throw new Error(`chainctl auth token --audience ${aud}: exit ${code}: ${err.trim()}`);
      const tok = out.trim();
      if (!tok) throw new Error("chainctl auth token returned no token");
      return tok;
    },
  };
}

/**
 * $CHAINCTL_AUTH_TOKEN when set, otherwise the library's own login (a cached
 * browserLoginToken): tokens are minted from the saved login, and a browser
 * login runs when there is none. chainctl isn't used; set $CHAINGUARD_SANDBOX_AUTH=chainctl
 * to use it instead. $CHAINGUARD_SANDBOX_SOCIAL_LOGIN preselects the social provider
 * (github, google-oauth2, gitlab); $CHAINGUARD_SANDBOX_NO_BROWSER=1 makes a missing login
 * an error instead of opening a browser.
 */
export function defaultTokenSource(): TokenSource {
  const t = process.env[ENV_AUTH_TOKEN];
  if (t) return staticToken(t);
  if (process.env.CHAINGUARD_SANDBOX_AUTH === "chainctl") return cached(chainctlToken());
  return cached(
    browserLoginToken({
      login: { connection: process.env.CHAINGUARD_SANDBOX_SOCIAL_LOGIN || undefined },
      noBrowser: process.env.CHAINGUARD_SANDBOX_NO_BROWSER === "1",
    }),
  );
}

/** Reads the unverified "exp" claim of a JWT. */
export function jwtExpiry(tok: string): Date | undefined {
  const parts = tok.split(".");
  if (parts.length !== 3) return undefined;
  try {
    const claims = JSON.parse(Buffer.from(parts[1]!, "base64url").toString("utf8"));
    return typeof claims.exp === "number" && claims.exp > 0 ? new Date(claims.exp * 1000) : undefined;
  } catch {
    return undefined;
  }
}

/** HTTP timeout for token exchanges; applies even when the caller passes a signal. */
const HTTP_TIMEOUT_MS = 30_000;

function withTimeout(signal?: AbortSignal): AbortSignal {
  const t = AbortSignal.timeout(HTTP_TIMEOUT_MS);
  return signal ? AbortSignal.any([signal, t]) : t;
}

/** Resolves like p, but rejects early if signal aborts (p keeps running for other waiters). */
function raceAbort<T>(p: Promise<T>, signal?: AbortSignal): Promise<T> {
  if (!signal) return p;
  if (signal.aborted) return Promise.reject(signal.reason);
  return new Promise<T>((resolve, reject) => {
    const onAbort = () => reject(signal.reason);
    signal.addEventListener("abort", onAbort, { once: true });
    p.then(
      (v) => (signal.removeEventListener("abort", onAbort), resolve(v)),
      (e) => (signal.removeEventListener("abort", onAbort), reject(e)),
    );
  });
}

/**
 * Reuses a token until a minute before its JWT expiry (5 minutes if unreadable).
 * Concurrent callers share one fetch, which runs without any one caller's
 * signal (each caller can still give up on its own signal); a failed fetch is
 * never cached.
 */
export function cached(src: TokenSource): TokenSource {
  let tok = "";
  let exp = 0;
  let inflight: Promise<string> | undefined;
  return {
    async token(signal) {
      if (tok && exp - Date.now() > 60_000) return tok;
      if (!inflight) {
        // Defer the call so a synchronous throw becomes a rejection after
        // inflight is assigned; clear inflight only if it's still ours.
        const p: Promise<string> = Promise.resolve()
          .then(() => src.token())
          .then((t) => {
            tok = t;
            exp = jwtExpiry(t)?.getTime() ?? Date.now() + 5 * 60_000;
            return t;
          });
        inflight = p;
        p.then(
          () => inflight === p && (inflight = undefined),
          () => inflight === p && (inflight = undefined),
        );
      }
      return raceAbort(inflight, signal);
    },
  };
}

/**
 * Trades an external OIDC token (GitHub Actions, GitLab, a Kubernetes service
 * account, ...) for a Chainguard token, acting as an assumable identity that
 * trusts that issuer and subject. For machines without chainctl.
 *
 * Setup (an organization owner, once):
 *   chainctl iam identities create NAME --parent ORG \
 *     --identity-issuer ISSUER --subject SUBJECT --role sandbox.user
 */
export function identityExchange(opts: {
  identity: string;
  idToken: (signal?: AbortSignal) => Promise<string>;
  issuer?: string;
  audience?: string;
  fetch?: typeof fetch;
}): TokenSource {
  const issuer = (opts.issuer ?? DEFAULT_ISSUER).replace(/\/$/, "");
  const aud = opts.audience ?? DEFAULT_AUDIENCE;
  const f = opts.fetch ?? fetch;
  return {
    async token(signal) {
      if (!opts.identity) throw new Error("chainguard-sandbox: identityExchange needs identity");
      const idt = await opts.idToken(signal);
      const q = new URLSearchParams({ aud, identity: opts.identity });
      const resp = await f(`${issuer}/sts/exchange?${q}`, {
        method: "POST",
        headers: { Authorization: `Bearer ${idt}`, Accept: "application/json" },
        signal: withTimeout(signal),
      });
      const body = await resp.text();
      if (!resp.ok) {
        let msg = body.trim();
        try {
          msg = JSON.parse(body).message || msg;
        } catch {}
        throw new Error(`STS exchange for identity ${opts.identity}: HTTP ${resp.status}: ${msg}`);
      }
      let tok = "";
      try {
        tok = JSON.parse(body).token ?? "";
      } catch {}
      if (!tok) throw new Error("STS exchange: response has no token");
      return tok;
    },
  };
}

/** A GitHub Actions OIDC token with the Chainguard issuer as audience (needs `id-token: write`). */
export function githubActionsIdToken(issuer = DEFAULT_ISSUER): (signal?: AbortSignal) => Promise<string> {
  return async (signal) => {
    const reqUrl = process.env.ACTIONS_ID_TOKEN_REQUEST_URL;
    const reqTok = process.env.ACTIONS_ID_TOKEN_REQUEST_TOKEN;
    if (!reqUrl || !reqTok) throw new Error("not in a GitHub Actions job with `permissions: id-token: write`");
    const u = new URL(reqUrl);
    u.searchParams.set("audience", issuer);
    const resp = await fetch(u, { headers: { Authorization: `Bearer ${reqTok}` }, signal: withTimeout(signal) });
    const out = resp.ok ? ((await resp.json()) as { value?: string }) : {};
    if (!out.value) throw new Error(`GitHub Actions OIDC token request failed: HTTP ${resp.status}`);
    return out.value;
  };
}

/** Reads the OIDC token from an environment variable (e.g. a GitLab `id_tokens:` variable). */
export function envIdToken(name: string): () => Promise<string> {
  return async () => {
    const v = (process.env[name] ?? "").trim();
    if (!v) throw new Error(`$${name} is empty`);
    return v;
  };
}

/** Reads the OIDC token from a file on every call (e.g. a projected service-account token). */
export function fileIdToken(path: string): () => Promise<string> {
  return async () => {
    const v = (await readFile(path, "utf8")).trim();
    if (!v) throw new Error(`${path} is empty`);
    return v;
  };
}
