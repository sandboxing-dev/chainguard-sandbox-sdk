// Browser login, refresh rotation and the shared login file (local fakes only).
import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync, readFileSync, statSync, utimesSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import {
  AccountNotFoundError,
  DEFAULT_AUDIENCE,
  FileLoginStore,
  LoginExpiredError,
  LoginRequiredError,
  browserLogin,
  browserLoginToken,
  logout,
  refreshLogin,
  startLoginServer,
  tokenClaims,
} from "../src/index.js";

const b64 = (s: string) => Buffer.from(s).toString("base64url");
const fakeJwt = (...aud: string[]) => `${b64("{}")}.${b64(JSON.stringify({ sub: "user1", aud, exp: Math.floor(Date.now() / 1000) + 3600 }))}.sig`;
const hasAud = (t: string, a: string) => [tokenClaims(t).aud].flat().includes(a);

/** /sts/exchange_refresh_token with rotating refresh tokens. */
const issuer = { valid: new Set<string>(), n: 0, lastAud: "" };
let issuerUrl = "";
let srv: ReturnType<typeof Bun.serve>;
beforeAll(() => {
  srv = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(req) {
      const u = new URL(req.url);
      if (u.pathname !== "/sts/exchange_refresh_token" || req.method !== "POST") return new Response("", { status: 404 });
      const aud = u.searchParams.get("aud");
      if (!aud) return Response.json({ code: 3, message: "Audience must be specified" }, { status: 400 });
      const rt = (req.headers.get("authorization") ?? "").replace(/^Bearer /, "");
      // What the real issuer sends for an expired refresh token.
      if (rt.startsWith("expired"))
        return Response.json({ code: 3, message: "invalid refresh token: refresh token expired at 2026-11-10 23:10:16 +0000 UTC" }, { status: 400 });
      if (!issuer.valid.has(rt)) return Response.json({ code: 16, message: "invalid refresh token" }, { status: 401 });
      issuer.valid.delete(rt);
      const next = `rt-${++issuer.n}`;
      issuer.valid.add(next);
      issuer.lastAud = aud;
      return Response.json({ token: { token: fakeJwt(aud) }, refreshToken: { token: next } });
    },
  });
  issuerUrl = `http://127.0.0.1:${srv.port}`;
});
afterAll(() => srv.stop(true));

function reset(valid: string[] = []) {
  issuer.valid = new Set(valid);
  issuer.n = 0;
  issuer.lastAud = "";
}

/** Plays the browser: follow the redirect to the callback (with query), then load "/". */
function complete(query: Record<string, string>, seen?: URLSearchParams[], pages?: { body: string; url: string }[]) {
  return (url: string) => {
    const q = new URL(url).searchParams;
    seen?.push(q);
    const cb = new URL(q.get("redirect")!.replace("localhost", "127.0.0.1"));
    cb.search = new URLSearchParams(query).toString();
    void fetch(cb).then(async (r) => pages?.push({ body: await r.text(), url: r.url }));
    return true;
  };
}

const quiet = () => {};

test("browserLogin sends chainctl's parameters and strips the tokens from the URL", async () => {
  const seen: URLSearchParams[] = [];
  const pages: { body: string; url: string }[] = [];
  const res = await browserLogin({ connection: "github", openUrl: complete({ token: "tok", refresh_token: "rt" }, seen, pages), out: quiet, timeoutMs: 5000 });
  expect(res).toEqual({ token: "tok", refreshToken: "rt" });
  const q = seen[0]!;
  for (const [k, v] of Object.entries({
    client_id: "auth0",
    connection: "github",
    audience: `${DEFAULT_AUDIENCE},https://console-api.enforce.dev`,
    create_refresh_token: "true",
    refresh_token_rotation: "true",
    skip_registration: "true",
    exit: "redirect",
  }))
    expect(q.get(k)).toBe(v);
  expect(q.get("redirect")!).toMatch(/^http:\/\/localhost:\d+\/callback\?token=true&error=true$/);
  for (let i = 0; i < 100 && !pages.length; i++) await Bun.sleep(10);
  expect(pages[0]!.body).toContain("login complete");
  expect(pages[0]!.url).not.toContain("token=");
});

test("browserLogin errors, and prints the URL when no browser opens", async () => {
  const e1 = await browserLogin({ openUrl: complete({ status_code: "404" }), out: quiet, timeoutMs: 5000 }).catch((e) => e);
  expect(e1).toBeInstanceOf(AccountNotFoundError);
  const e2 = await browserLogin({ openUrl: complete({ status_code: "500", error: "boom" }), out: quiet, timeoutMs: 5000 }).catch((e) => e);
  expect(String(e2)).toContain("boom");
  let printed = "";
  const e3 = await browserLogin({ openUrl: () => false, out: (m) => (printed += m), timeoutMs: 200 }).catch((e) => e);
  expect(String(e3)).toMatch(/no answer/);
  expect(printed).toContain("https://issuer.enforce.dev/oauth?");
});

test("the callback server ignores stray requests and keeps the first answer", async () => {
  const s = await startLoginServer();
  try {
    const base = s.callbackUrl.split("/callback")[0]!.replace("localhost", "127.0.0.1");
    expect((await fetch(`${base}/`)).status).toBe(404);
    expect((await fetch(`${base}/callback?token=x`, { method: "POST" })).status).toBe(405);
    expect((await fetch(`${base}/other`)).status).toBe(404);
    await fetch(`${base}/callback?token=a`, { redirect: "manual" });
    await fetch(`${base}/callback?token=b`, { redirect: "manual" });
    expect((await s.result).res?.token).toBe("a");
  } finally {
    s.stop();
  }
});

test("browserLoginToken logs in, refreshes, rotates, and logs in again after revocation", async () => {
  reset();
  const dir = mkdtempSync(join(tmpdir(), "chainguard-sandbox-login-"));
  const store = new FileLoginStore(join(dir, "sub", "login.json"));
  let logins = 0;
  const login = {
    issuer: issuerUrl,
    out: quiet,
    timeoutMs: 5000,
    openUrl: (u: string) => {
      logins++;
      issuer.valid.add("rt-login");
      // The login's own token is for another audience, so a refresh follows.
      return complete({ token: fakeJwt("https://console-api.enforce.dev"), refresh_token: "rt-login" })(u);
    },
  };
  const src = browserLoginToken({ store, login });
  const tok = await src.token();
  expect(hasAud(tok, DEFAULT_AUDIENCE)).toBe(true);
  expect(logins).toBe(1);
  expect(issuer.lastAud).toBe(DEFAULT_AUDIENCE);
  expect(store.load(issuerUrl)).toBe("rt-1");
  expect(statSync(store.path).mode & 0o777).toBe(0o600);
  expect(statSync(dirname(store.path)).mode & 0o777).toBe(0o700);

  await src.token(); // the saved login: no browser, rotated again
  expect(store.load(issuerUrl)).toBe("rt-2");
  expect(logins).toBe(1);

  issuer.valid.clear(); // revoked
  const e = await browserLoginToken({ store, login, noBrowser: true })
    .token()
    .catch((x) => x);
  expect(e).toBeInstanceOf(LoginRequiredError);
  expect(store.load(issuerUrl)).toBe(""); // the revoked login is dropped
  await src.token();
  expect(logins).toBe(2);
});

test("concurrent refreshes never spend the same refresh token twice", async () => {
  reset(["rt-0"]);
  const path = join(mkdtempSync(join(tmpdir(), "chainguard-sandbox-login-")), "login.json");
  new FileLoginStore(path).save(issuerUrl, "rt-0");
  await Promise.all(
    Array.from({ length: 8 }, () => browserLoginToken({ store: new FileLoginStore(path), noBrowser: true, login: { issuer: issuerUrl } }).token()),
  );
  expect(new FileLoginStore(path).load(issuerUrl)).toBe("rt-8");
});

test("a stale lock is taken over; logout keeps other issuers; the file matches Go's", async () => {
  const store = new FileLoginStore(join(mkdtempSync(join(tmpdir(), "chainguard-sandbox-login-")), "login.json"));
  writeFileSync(store.path + ".lock", "1\n");
  const old = new Date(Date.now() - 180_000);
  utimesSync(store.path + ".lock", old, old);
  (await store.lock())();
  store.save(undefined, "rt");
  store.save("https://other.example/", "rt2");
  const v = JSON.parse(readFileSync(store.path, "utf8"));
  expect(v.version).toBe(1);
  expect(v.logins["https://issuer.enforce.dev"].refresh_token).toBe("rt");
  logout(store);
  expect(store.load()).toBe("");
  expect(store.load("https://other.example")).toBe("rt2");
});

test("refreshLogin sends the audience as a query parameter", async () => {
  reset(["r"]);
  const r = await refreshLogin("r", { issuer: issuerUrl });
  expect(hasAud(r.token, DEFAULT_AUDIENCE)).toBe(true);
  expect(r.refreshToken).toBe("rt-1");
  const e = await refreshLogin("r", { issuer: issuerUrl }).catch((x) => x);
  expect(e).toBeInstanceOf(LoginExpiredError);
});

test("an expired refresh token answered with 400 counts as a login expiry", async () => {
  const e = await refreshLogin("expired-1", { issuer: issuerUrl }).catch((x) => x);
  expect(e).toBeInstanceOf(LoginExpiredError);
  const store = new FileLoginStore(join(mkdtempSync(join(tmpdir(), "chainguard-sandbox-login-")), "login.json"));
  store.save(issuerUrl, "expired-1");
  const e2 = await browserLoginToken({ store, noBrowser: true, login: { issuer: issuerUrl } })
    .token()
    .catch((x) => x);
  expect(e2).toBeInstanceOf(LoginRequiredError);
  expect(store.load(issuerUrl)).toBe("");
});
