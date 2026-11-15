// Turning Workspaces on: status, terms and the enable call, against a local stand-in for the
// platform API (no network). It checks only what this package sends; it says nothing about the
// real service.
import { afterAll, beforeAll, beforeEach, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";
import { PlatformError, WORKSPACES_TERMS, staticToken } from "../src/index";
import { TermsNotAcceptedError, WorkspacesNotEnabledError, enableWorkspaces, workspacesStatus } from "../src/sandbox";

const ORG = "0123456789abcdef0123456789abcdef01234567";
let accepted = new Set<string>();
let enabled = false;
let needForEnable: string[] = []; // documents the switch insists on beyond Workspaces' own
let posts: string[] = [];
let server: ReturnType<typeof Bun.serve>;
const opts = { instance: ORG, tokenSource: staticToken("platform-token") };

beforeAll(() => {
  server = Bun.serve({
    port: 0,
    hostname: "127.0.0.1",
    async fetch(req) {
      const u = new URL(req.url);
      const path = decodeURIComponent(u.pathname);
      const json = (v: unknown, status = 200) => new Response(JSON.stringify(v), { status, headers: { "Content-Type": "application/json" } });
      if (req.headers.get("authorization") !== "Bearer platform-token") return json({ code: 16, message: "unauthenticated" }, 401);
      if (path === `/iam/v1/terms/${ORG}` && req.method === "GET") return json({ items: [...accepted].map((docId) => ({ docId })) });
      if (path === `/iam/v1/terms/${ORG}` && req.method === "POST") {
        const body = (await req.json()) as { documentIds: string[] };
        posts.push("terms:" + body.documentIds.join(","));
        for (const id of body.documentIds) accepted.add(id);
        return json({ group: ORG, documentIds: body.documentIds });
      }
      if (path === `/entitlements/v1alpha1/features/${ORG}:enableSandbox` && req.method === "POST") {
        posts.push("enable");
        const missing = [...WORKSPACES_TERMS, ...needForEnable].filter((id) => !accepted.has(id)).map((id) => ({ id, label: `Label of ${id}`, url: `https://example.test/${id}` }));
        if (missing.length) return json({ code: 9, message: "terms not accepted", details: [{ "@type": "x/TermsNotAcceptedDetail", missing }] }, 400);
        enabled = true;
        return json({ featureEntitlements: [] });
      }
      if (path === "/entitlements/v2beta1/organizationEntitlements" && u.searchParams.get("parent") === ORG) {
        return json({
          plan: { id: "starter", displayName: "Starter" },
          products: [{ product: "PRODUCT_SANDBOX_WORKSPACES", state: enabled ? "STATE_ACTIVE" : "STATE_NOT_INCLUDED", limits: [{ displayName: "concurrent workspaces", max: "3" }, { displayName: "concurrent workspaces per user", max: "1" }] }],
        });
      }
      return json({ code: 5, message: "no such route" }, 404);
    },
  });
  process.env.CHAINGUARD_SANDBOX_PLATFORM_URL = `http://127.0.0.1:${server.port}`;
});
afterAll(() => {
  delete process.env.CHAINGUARD_SANDBOX_PLATFORM_URL;
  server.stop(true);
});
beforeEach(() => {
  accepted = new Set();
  enabled = false;
  needForEnable = [];
  posts = [];
});

test("status and enable", async () => {
  let st = await workspacesStatus(opts);
  expect([st.enabled, st.ready, st.plan, st.maxSandboxes, st.maxSandboxesPerUser]).toEqual([false, false, "Starter", 3, 1]);
  expect(st.missingTerms.map((d) => d.id)).toEqual([...WORKSPACES_TERMS]);
  expect(st.missingTerms.every((d) => d.label && d.url)).toBe(true);

  // Terms are never accepted without being asked to.
  const refused = await enableWorkspaces(opts).catch((e) => e);
  expect(refused).toBeInstanceOf(TermsNotAcceptedError);
  expect((refused as TermsNotAcceptedError).missing.length).toBe(2);
  expect(posts).toEqual([]);

  st = await enableWorkspaces({ ...opts, acceptTerms: true });
  expect(st.ready).toBe(true);
  expect(posts).toEqual(["terms:secure-platform-terms.v1,msla.v1", "enable"]);

  posts = []; // already on: nothing is sent
  expect((await enableWorkspaces({ ...opts, acceptTerms: true })).ready).toBe(true);
  expect(posts).toEqual([]);
});

test("enable accepts the terms the switch asks for", async () => {
  needForEnable = ["code-analysis-terms.v1"]; // the switch also turns on Checks
  for (const id of WORKSPACES_TERMS) accepted.add(id);
  const refused = await enableWorkspaces(opts).catch((e) => e);
  expect(refused).toBeInstanceOf(TermsNotAcceptedError);
  expect((refused as TermsNotAcceptedError).missing.map((d) => d.id)).toEqual(["code-analysis-terms.v1"]);
  expect((refused as TermsNotAcceptedError).missing[0]!.url).not.toBe("");
  expect(enabled).toBe(false);
  posts = [];
  expect((await enableWorkspaces({ ...opts, acceptTerms: true })).ready).toBe(true);
  expect(posts).toEqual(["enable", "terms:code-analysis-terms.v1", "enable"]);
});

test("setup errors keep the service's message", async () => {
  const err = await workspacesStatus({ instance: ORG, tokenSource: staticToken("wrong") }).catch((e) => e);
  expect(err).toBeInstanceOf(PlatformError);
  expect((err as PlatformError).httpStatus).toBe(401);
  expect(String(err)).toContain("unauthenticated");
});

test("WorkspacesNotEnabledError is exported for the not-entitled refusal", () => {
  expect(new WorkspacesNotEnabledError("x")).toBeInstanceOf(Error);
  expect(new ConnectError("this organization is not entitled to Chainguard Workspaces", Code.PermissionDenied).rawMessage).toContain("not entitled");
});
