// Organization listing / auto-detection, and login audiences (local fakes only).
import { afterAll, beforeAll, expect, test } from "bun:test";
import { mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import {
  AmbiguousOrganizationError,
  Client,
  DEFAULT_AUDIENCE,
  FileLoginStore,
  LoginRequiredError,
  NoOrganizationError,
  PLATFORM_AUDIENCE,
  browserLoginToken,
  defaultLoginAudiences,
  detectInstance,
  listOrganizations,
  staticToken,
} from "../src/index.js";

let groups: { id: string; name: string }[] = [];
let srv: ReturnType<typeof Bun.serve>;
let issuer: ReturnType<typeof Bun.serve>;
beforeAll(() => {
  srv = Bun.serve({
    hostname: "127.0.0.1",
    port: 0,
    fetch(req) {
      const u = new URL(req.url);
      if (u.pathname !== "/iam/v1/groups" || u.searchParams.get("uidp.in_root") !== "true" || req.headers.get("authorization") !== "Bearer platform-tok")
        return Response.json({ code: 7, message: "nope" }, { status: 403 });
      return Response.json({ items: groups });
    },
  });
  process.env.CHAINGUARD_SANDBOX_PLATFORM_URL = `http://127.0.0.1:${srv.port}`;
  // An issuer that refuses every refresh.
  issuer = Bun.serve({ hostname: "127.0.0.1", port: 0, fetch: () => Response.json({}, { status: 401 }) });
});
afterAll(() => {
  srv.stop(true);
  issuer.stop(true);
  delete process.env.CHAINGUARD_SANDBOX_PLATFORM_URL;
});

const ts = staticToken("platform-tok");

test("listOrganizations and detectInstance", async () => {
  groups = [
    { id: "b2", name: "zeta" },
    { id: "a1", name: "alpha" },
    { id: "a1/child", name: "sub" },
  ];
  expect(await listOrganizations(ts)).toEqual([
    { id: "a1", name: "alpha" },
    { id: "b2", name: "zeta" },
  ]);
  const e = await detectInstance(ts).catch((x) => x);
  expect(e).toBeInstanceOf(AmbiguousOrganizationError);
  expect(e.message).toContain("a1  alpha");
  groups = [{ id: "only1", name: "my-org" }];
  expect(await detectInstance(ts)).toBe("only1");
  groups = [];
  expect(await detectInstance(ts).catch((x) => x)).toBeInstanceOf(NoOrganizationError);
  expect(String(await listOrganizations(staticToken("wrong")).catch((x) => x))).toContain("HTTP 403");
});

test("client instance precedence: explicit, environment, detection", async () => {
  groups = [{ id: "only1", name: "my-org" }];
  const saved = process.env.CHAINCTL_SANDBOX_INSTANCE;
  delete process.env.CHAINCTL_SANDBOX_INSTANCE;
  try {
    const base = { url: "http://127.0.0.1:1", tokenSource: staticToken("x"), platformTokenSource: ts };
    expect(await new Client(base).parentName()).toBe("instances/only1");
    process.env.CHAINCTL_SANDBOX_INSTANCE = "fromenv";
    expect(await new Client(base).parentName()).toBe("instances/fromenv");
    expect(await new Client({ ...base, instance: "explicit" }).parentName()).toBe("instances/explicit");
    delete process.env.CHAINCTL_SANDBOX_INSTANCE;
    for (const c of [new Client({ url: "http://127.0.0.1:1", tokenSource: staticToken("x"), detectInstance: false }), new Client({ url: "http://127.0.0.1:1", tokenSource: staticToken("x") })]) {
      const e = String(await c.parentName().catch((x) => x));
      expect(e).toContain("$CHAINCTL_SANDBOX_INSTANCE");
      expect(e).toContain("chainctl iam organizations list");
    }
  } finally {
    if (saved !== undefined) process.env.CHAINCTL_SANDBOX_INSTANCE = saved;
  }
});

test("a saved login that only lacks an audience is kept", async () => {
  const store = new FileLoginStore(join(mkdtempSync(join(tmpdir(), "chainguard-sandbox-orgs-")), "login.json"));
  const iss = `http://127.0.0.1:${issuer.port}`;
  const rt = Buffer.from(JSON.stringify({ exp: Math.floor(Date.now() / 1000) + 3600, aud: [DEFAULT_AUDIENCE] })).toString("base64");
  store.save(iss, rt);
  const e = await browserLoginToken({ store, noBrowser: true, audience: PLATFORM_AUDIENCE, login: { issuer: iss } })
    .token()
    .catch((x) => x);
  expect(e).toBeInstanceOf(LoginRequiredError);
  expect(e.message).toContain("doesn't cover");
  expect(store.load(iss)).toBe(rt);
  expect(await browserLoginToken({ store, noBrowser: true, login: { issuer: iss } }).token().catch((x) => x)).toBeInstanceOf(LoginRequiredError);
  expect(store.load(iss)).toBe("");
});

test("defaultLoginAudiences", () => {
  delete process.env.CHAINGUARD_SANDBOX_LOGIN_AUDIENCES;
  expect(defaultLoginAudiences()).toEqual([DEFAULT_AUDIENCE, PLATFORM_AUDIENCE]);
  process.env.CHAINGUARD_SANDBOX_LOGIN_AUDIENCES = DEFAULT_AUDIENCE;
  expect(defaultLoginAudiences()).toEqual([DEFAULT_AUDIENCE]);
  delete process.env.CHAINGUARD_SANDBOX_LOGIN_AUDIENCES;
});
