// Browser login, without chainctl. A port of the Go library's login.go, which
// is adapted from chainguard.dev/sdk/auth/login (Copyright 2023 Chainguard,
// Inc., Apache-2.0): a callback server on a random localhost port, and the
// issuer's /oauth page opened in the browser with redirect=<callback>. The
// issuer redirects back with ?token=…&refresh_token=…. The parameters match
// chainctl 0.2.398's.
//
// The saved login (a rotating refresh token) lives in the same file the Go and
// Python libraries use, so one login serves all three.
import { closeSync, mkdirSync, openSync, readFileSync, renameSync, statSync, unlinkSync, writeSync } from "node:fs";
import { homedir } from "node:os";
import { dirname, join } from "node:path";
import { randomBytes } from "node:crypto";
import { spawn } from "node:child_process";
import { setTimeout as sleep } from "node:timers/promises";
import { serveLoopback } from "./loopback.js";
import { DEFAULT_AUDIENCE, DEFAULT_ISSUER, type TokenSource } from "./auth.js";

export class AccountNotFoundError extends Error {}

/** Chainguard's account (IAM) API, used to list your organizations. */
export const PLATFORM_AUDIENCE = "https://console-api.enforce.dev";

/**
 * What a browser login asks for: the sessions service and the account API.
 * $CHAINGUARD_SANDBOX_LOGIN_AUDIENCES (comma-separated) overrides it; set it to the sessions
 * audience alone to keep logins from reaching the account API.
 */
export function defaultLoginAudiences(): string[] {
  const v = (process.env.CHAINGUARD_SANDBOX_LOGIN_AUDIENCES ?? "").trim();
  if (v) return v.split(",").map((a) => a.trim()).filter(Boolean);
  return [DEFAULT_AUDIENCE, PLATFORM_AUDIENCE];
}

/** A refresh token's (unverified) expiry and audiences; it is base64 JSON. */
function refreshClaims(rt: string): { exp: number; aud: string[] } | undefined {
  try {
    const c = JSON.parse(Buffer.from(rt, "base64").toString("utf8"));
    if (typeof c.exp !== "number") return undefined;
    return { exp: c.exp, aud: Array.isArray(c.aud) ? c.aud : c.aud ? [c.aud] : [] };
  } catch {
    return undefined;
  }
}
/** The saved login was refused; log in again. */
export class LoginExpiredError extends Error {}
/** No saved login, and the token source may not open a browser. */
export class LoginRequiredError extends Error {}

export interface LoginOptions {
  /** Defaults to DEFAULT_ISSUER. */
  issuer?: string;
  /** What the login's tokens may be minted for; defaults to [DEFAULT_AUDIENCE]. */
  audience?: string[];
  /** Preselects a social login: "github", "google-oauth2" or "gitlab". */
  connection?: string;
  /** Defaults to "auth0", as chainctl sends. Ignored with identityProvider / orgName. */
  clientId?: string;
  /** An organization's custom identity provider ID, or the name of an org that has one (at most one). */
  identityProvider?: string;
  orgName?: string;
  /** An assumable identity to log in as. */
  identity?: string;
  /** Skip asking for a refresh token (every later token then needs another login). */
  noRefreshToken?: boolean;
  /** Opens the login page; defaults to openBrowser. Resolve false (or throw) to have the URL printed. */
  openUrl?: (url: string) => Promise<boolean> | boolean;
  /** Receives the "open this URL" message; defaults to stderr. */
  out?: (msg: string) => void;
  signal?: AbortSignal;
  /** Milliseconds to wait for the browser (default 10 minutes). */
  timeoutMs?: number;
}

export interface LoginResult {
  token: string;
  refreshToken: string;
}

const DONE_PAGE =
  '<!doctype html><html><head><meta charset="utf-8"><title>Login complete</title>' +
  "<style>:root{color-scheme:light dark;font-family:system-ui,sans-serif}body{display:grid;place-items:center;height:100vh;margin:0}</style>" +
  "</head><body><div><h1>Chainguard login complete</h1><p>You can close this tab and return to your program.</p></div></body></html>";

/** Open the issuer's login page in a browser and wait for the redirect back to a localhost callback. */
export async function browserLogin(o: LoginOptions = {}): Promise<LoginResult> {
  if (o.identityProvider && o.orgName) throw new Error("chainguard-sandbox: login: set at most one of identityProvider and orgName");
  const issuer = (o.issuer ?? DEFAULT_ISSUER).replace(/\/$/, "");
  const out = o.out ?? ((m: string) => process.stderr.write(m + "\n"));
  const srv = await startLoginServer();
  try {
    const p: [string, string][] = [];
    if (o.identityProvider) p.push(["idp_id", o.identityProvider]);
    else if (o.orgName) p.push(["idp_id", o.orgName]); // the issuer takes a verified org name here too
    else {
      p.push(["client_id", o.clientId || "auth0"]);
      if (o.connection) p.push(["connection", o.connection]);
    }
    p.push(["audience", (o.audience?.length ? o.audience : defaultLoginAudiences()).join(",")]);
    if (!o.noRefreshToken) p.push(["create_refresh_token", "true"], ["refresh_token_rotation", "true"]);
    if (o.identity) p.push(["identity", o.identity]);
    p.push(["skip_registration", "true"], ["exit", "redirect"], ["redirect", srv.callbackUrl]);
    p.sort((a, b) => (a[0] < b[0] ? -1 : a[0] > b[0] ? 1 : 0));
    const url = `${issuer}/oauth?${new URLSearchParams(p)}`;
    let opened = false;
    try {
      opened = (await (o.openUrl ?? openBrowser)(url)) !== false;
    } catch {
      opened = false;
    }
    out(opened ? `Opened a browser to log in to Chainguard. If it didn't open, visit:\n  ${url}` : `Open this URL in a browser to log in to Chainguard:\n  ${url}`);

    const timeout = AbortSignal.timeout(o.timeoutMs ?? 600_000);
    const stop = o.signal ? AbortSignal.any([o.signal, timeout]) : timeout;
    const outcome = await new Promise<{ res?: LoginResult; err?: unknown }>((resolve) => {
      if (stop.aborted) return resolve({ err: new Error("chainguard-sandbox: login: no answer from the browser") });
      stop.addEventListener("abort", () => resolve({ err: new Error("chainguard-sandbox: login: no answer from the browser in time") }), { once: true });
      srv.result.then(resolve);
    });
    if (outcome.err) throw outcome.err;
    // Keep serving until the browser has loaded the success page.
    await Promise.race([srv.pageServed, sleep(5000), new Promise((r) => stop.addEventListener("abort", r, { once: true }))]);
    return outcome.res!;
  } finally {
    srv.stop();
  }
}

interface LoginServer {
  callbackUrl: string;
  result: Promise<{ res?: LoginResult; err?: unknown }>;
  pageServed: Promise<void>;
  stop(): void;
}

/** Exported for tests. */
export async function startLoginServer(): Promise<LoginServer> {
  let finish!: (o: { res?: LoginResult; err?: unknown }) => void;
  let done = false;
  let gotToken = false;
  const result = new Promise<{ res?: LoginResult; err?: unknown }>((r) => (finish = r));
  let served!: () => void;
  const pageServed = new Promise<void>((r) => (served = r));
  const once = (o: { res?: LoginResult; err?: unknown }) => {
    if (done) return; // the first answer wins
    done = true;
    gotToken = !!o.res;
    finish(o);
  };
  const headers = { "Cache-Control": "no-store", "Referrer-Policy": "no-referrer" };
  const server = await serveLoopback(0, (method, u) => {
    if (method !== "GET") return { status: 405, headers, body: "method not allowed" };
    if (u.pathname === "/callback") {
      const tok = u.searchParams.get("token") ?? "";
      if (!tok) {
        const code = Number(u.searchParams.get("status_code") ?? 0) || 0;
        if (code === 404) {
          once({ err: new AccountNotFoundError("chainguard-sandbox: login: no Chainguard account for this login (sign up for Workspaces at https://console.chainguard.dev/workspaces/signup, or register with `chainctl auth login`)") });
          return { status: 404, headers, body: "No Chainguard account was found for this login." };
        }
        const msg = u.searchParams.get("error") || "login failed";
        once({ err: new Error(`chainguard-sandbox: login: the issuer reported an error (status ${code}): ${msg}`) });
        return { status: 400, headers, body: `Login failed: ${msg}` };
      }
      once({ res: { token: tok, refreshToken: u.searchParams.get("refresh_token") ?? "" } });
      // Redirect so the tokens leave the address bar and history entry.
      return { status: 303, headers: { ...headers, Location: "/" } };
    }
    if (u.pathname === "/") {
      if (!gotToken) return { status: 404, headers, body: "not found" };
      queueMicrotask(served);
      return { status: 200, headers: { ...headers, "Content-Type": "text/html; charset=utf-8" }, body: DONE_PAGE };
    }
    return { status: 404, headers, body: "not found" };
  });
  // The issuer only redirects to localhost; the host name is what chainctl sends.
  // stop() lets requests in flight (the success page) finish; new ones are refused.
  return { callbackUrl: `http://localhost:${server.port}/callback?token=true&error=true`, result, pageServed, stop: () => void server.stop() };
}

/** Open url in the default browser (open / xdg-open / the Windows URL handler); no shell involved. */
export async function openBrowser(url: string): Promise<boolean> {
  const argv =
    process.platform === "darwin" ? ["open", url] : process.platform === "win32" ? ["rundll32", "url.dll,FileProtocolHandler", url] : ["xdg-open", url];
  try {
    return await new Promise<boolean>((resolve) => {
      const p = spawn(argv[0]!, argv.slice(1), { stdio: "ignore" });
      p.on("error", () => resolve(false));
      p.on("exit", (code) => resolve(code === 0));
    });
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------- refresh tokens

/**
 * Trade a refresh token for a token for audience. Returns the token and the
 * rotated refresh token ("" if it didn't rotate). The HTTP binding takes the
 * audience as a query parameter.
 */
export async function refreshLogin(
  refreshToken: string,
  o: { issuer?: string; audience?: string; signal?: AbortSignal; fetch?: typeof fetch } = {},
): Promise<{ token: string; refreshToken: string }> {
  const issuer = (o.issuer ?? DEFAULT_ISSUER).replace(/\/$/, "");
  const q = new URLSearchParams({ aud: o.audience ?? DEFAULT_AUDIENCE });
  const t = AbortSignal.timeout(30_000);
  const resp = await (o.fetch ?? fetch)(`${issuer}/sts/exchange_refresh_token?${q}`, {
    method: "POST",
    headers: { Authorization: `Bearer ${refreshToken}`, Accept: "application/json" },
    redirect: "manual", // never re-send the refresh token elsewhere
    signal: o.signal ? AbortSignal.any([o.signal, t]) : t,
  });
  const body = await resp.text();
  if (!resp.ok) {
    let msg = body.trim();
    try {
      msg = JSON.parse(body).message || msg;
    } catch {}
    // The issuer refuses a spent/expired refresh token with 401/403, or with
    // 400 "invalid refresh token: refresh token expired at …" (observed).
    const refused = resp.status === 401 || resp.status === 403 || (resp.status === 400 && /refresh token/i.test(msg));
    const E = refused ? LoginExpiredError : Error;
    throw new E(`refreshing the login: HTTP ${resp.status}: ${msg}`);
  }
  let v: { token?: { token?: string }; refreshToken?: { token?: string }; refresh_token?: { token?: string } } = {};
  try {
    v = JSON.parse(body);
  } catch {}
  const tok = v.token?.token ?? "";
  if (!tok) throw new Error("refreshing the login: response has no token");
  return { token: tok, refreshToken: v.refreshToken?.token ?? v.refresh_token?.token ?? "" };
}

// ---------------------------------------------------------------- the saved login

/**
 * <user config dir>/chainguard-sandbox/login.json, the same file Go's
 * os.UserConfigDir() gives (macOS: ~/Library/Application Support, Linux:
 * $XDG_CONFIG_HOME or ~/.config, Windows: %AppData%). $CHAINGUARD_SANDBOX_LOGIN_FILE overrides it.
 */
export function defaultLoginFile(): string {
  if (process.env.CHAINGUARD_SANDBOX_LOGIN_FILE) return process.env.CHAINGUARD_SANDBOX_LOGIN_FILE;
  const base =
    process.platform === "darwin"
      ? join(homedir(), "Library", "Application Support")
      : process.platform === "win32"
        ? (process.env.AppData ?? homedir())
        : (process.env.XDG_CONFIG_HOME || join(homedir(), ".config"));
  return join(base, "chainguard-sandbox", "login.json");
}

const normIssuer = (s?: string) => (s || DEFAULT_ISSUER).replace(/\/$/, "");

export interface LoginStore {
  load(issuer?: string): string;
  /** "" deletes. */
  save(issuer: string | undefined, refreshToken: string): void;
  /** Serializes refresh-and-save across processes (refresh tokens rotate). */
  lock(signal?: AbortSignal): Promise<() => void>;
}

type LoginFile = { version: number; logins: Record<string, { refresh_token: string; updated: string }> };

/** Logins in a JSON file (0600, in a 0700 directory), keyed by issuer. */
export class FileLoginStore implements LoginStore {
  constructor(readonly path: string = defaultLoginFile()) {}

  private read(): LoginFile {
    let raw: string;
    try {
      raw = readFileSync(this.path, "utf8");
    } catch (e) {
      if ((e as NodeJS.ErrnoException).code === "ENOENT") return { version: 1, logins: {} };
      throw e;
    }
    const v = JSON.parse(raw);
    if (!v || typeof v !== "object" || typeof v.logins !== "object" || !v.logins) throw new Error(`${this.path}: not a login file`);
    return v;
  }

  load(issuer?: string): string {
    return this.read().logins[normIssuer(issuer)]?.refresh_token ?? "";
  }

  save(issuer: string | undefined, refreshToken: string): void {
    let v: LoginFile;
    try {
      v = this.read();
    } catch {
      v = { version: 1, logins: {} }; // a corrupt file is replaced
    }
    v.version = 1;
    if (refreshToken) v.logins[normIssuer(issuer)] = { refresh_token: refreshToken, updated: new Date().toISOString() };
    else delete v.logins[normIssuer(issuer)];
    const dir = dirname(this.path);
    mkdirSync(dir, { recursive: true, mode: 0o700 });
    const tmp = join(dir, `.login-${randomBytes(6).toString("hex")}.json`);
    const fd = openSync(tmp, "wx", 0o600);
    try {
      writeSync(fd, JSON.stringify(v, null, 2) + "\n");
    } finally {
      closeSync(fd);
    }
    try {
      renameSync(tmp, this.path);
    } catch (e) {
      try {
        unlinkSync(tmp);
      } catch {}
      throw e;
    }
  }

  /** <file>.lock, created exclusively; older than two minutes counts as left over. Same scheme as Go and Python. */
  async lock(signal?: AbortSignal): Promise<() => void> {
    const p = this.path + ".lock";
    mkdirSync(dirname(p), { recursive: true, mode: 0o700 });
    const deadline = Date.now() + 30_000;
    for (;;) {
      try {
        const fd = openSync(p, "wx", 0o600);
        writeSync(fd, `${process.pid}\n`);
        closeSync(fd);
        return () => {
          try {
            unlinkSync(p);
          } catch {}
        };
      } catch (e) {
        if ((e as NodeJS.ErrnoException).code !== "EEXIST") throw e;
      }
      try {
        if (Date.now() - statSync(p).mtimeMs > 120_000) {
          unlinkSync(p);
          continue;
        }
      } catch {}
      if (signal?.aborted) throw signal.reason;
      if (Date.now() > deadline) throw new Error(`waiting for ${p}`);
      await sleep(100);
    }
  }
}

/** Forget the saved login for issuer. */
export function logout(store: LoginStore = new FileLoginStore(), issuer?: string): void {
  store.save(issuer, "");
}

// ---------------------------------------------------------------- the token source

/** A JWT's unverified claims (for display). */
export function tokenClaims(tok: string): { sub?: string; iss?: string; aud?: string | string[]; exp?: number } {
  const parts = tok.split(".");
  if (parts.length !== 3) throw new Error("not a JWT");
  return JSON.parse(Buffer.from(parts[1]!, "base64url").toString("utf8"));
}

function hasAudience(tok: string, aud: string): boolean {
  try {
    const a = tokenClaims(tok).aud;
    return Array.isArray(a) ? a.includes(aud) : a === aud;
  } catch {
    return false;
  }
}

export interface BrowserLoginTokenOptions {
  /** Options for the browser login (issuer, connection, openUrl, ...). */
  login?: LoginOptions;
  /** Audience of the returned tokens; defaults to DEFAULT_AUDIENCE. */
  audience?: string;
  /** Defaults to new FileLoginStore() (defaultLoginFile()). */
  store?: LoginStore;
  /** Fail with LoginRequiredError instead of opening a browser. */
  noBrowser?: boolean;
  fetch?: typeof fetch;
}

/**
 * A TokenSource backed by the library's own login: tokens are minted from the
 * saved refresh token; when there is none (or it has expired) a browser login
 * runs, unless noBrowser. Wrap in cached().
 */
export function browserLoginToken(o: BrowserLoginTokenOptions = {}): TokenSource {
  const aud = o.audience ?? DEFAULT_AUDIENCE;
  const store = o.store ?? new FileLoginStore();
  const issuer = o.login?.issuer;
  const refresh = (rt: string, signal?: AbortSignal) => refreshLogin(rt, { issuer, audience: aud, signal, fetch: o.fetch });

  const fromSaved = async (signal?: AbortSignal): Promise<string> => {
    const unlock = await store.lock(signal);
    try {
      const rt = store.load(issuer);
      if (!rt) return "";
      let r;
      try {
        r = await refresh(rt, signal);
      } catch (e) {
        if (e instanceof LoginExpiredError) {
          // Forget the login only if it really is unusable: expired, or refused for an
          // audience it covers. A login made without this audience is still good for its
          // own audiences, so it's kept.
          const c = refreshClaims(rt);
          if (c && c.exp > Date.now() / 1000 && !c.aud.includes(aud)) {
            if (o.noBrowser) throw new LoginRequiredError(`chainguard-sandbox: the saved login doesn't cover ${aud}; log in again (chainguard-sandbox login) to add it`);
            return ""; // a browser login asks for it, then replaces the saved one
          }
          store.save(issuer, "");
          return "";
        }
        throw e;
      }
      if (r.refreshToken && r.refreshToken !== rt) store.save(issuer, r.refreshToken);
      return r.token;
    } finally {
      unlock();
    }
  };

  return {
    async token(signal) {
      const saved = await fromSaved(signal);
      if (saved) return saved;
      if (o.noBrowser) throw new LoginRequiredError("chainguard-sandbox: not logged in (run `chainguard-sandbox login`, or allow a browser login)");
      // No lock while the person logs in (that can take minutes).
      const auds = o.login?.audience ?? defaultLoginAudiences();
      const res = await browserLogin({ ...o.login, audience: auds.includes(aud) ? auds : [...auds, aud], signal: o.login?.signal ?? signal });
      const unlock = await store.lock(signal);
      try {
        if (res.refreshToken) store.save(issuer, res.refreshToken);
        if (hasAudience(res.token, aud)) return res.token;
        if (!res.refreshToken) throw new Error(`chainguard-sandbox: login: the token isn't for ${aud} and there's no refresh token to mint one`);
        const r = await refresh(res.refreshToken, signal);
        if (r.refreshToken) store.save(issuer, r.refreshToken);
        return r.token;
      } finally {
        unlock();
      }
    },
  };
}
