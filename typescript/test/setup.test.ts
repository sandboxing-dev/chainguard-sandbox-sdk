// Turning Workspaces on: status, terms and the enable call, against a local stand-in for the
// platform API (no network). It checks only what this package sends; it says nothing about the
// real service.
import { afterAll, beforeAll, beforeEach, expect, test } from "bun:test";
import { Code, ConnectError } from "@connectrpc/connect";
import { ENABLE_TERMS, GITHUB_APP_INSTALL_URL, PlatformError, WORKSPACES_TERMS, staticToken } from "../src/index.js";
import { GitHubAppNotInstalledError, GitHubLinkRequiredError, TermsNotAcceptedError, WorkspacesNotEnabledError, enableWorkspaces, workspacesStatus } from "../src/sandbox/index.js";

const ORG = "0123456789abcdef0123456789abcdef01234567";
let accepted = new Set<string>();
let enabled = false;
let needForEnable: string[] = []; // documents the switch insists on beyond Workspaces' own
let posts: string[] = [];
let github: string[] = []; // linked GitHub accounts
let appInstalled = true; // whether a link attempt succeeds
let linkBody: Record<string, string> = {};
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
      if (path === `/guardener/v1alpha1/githubAssociations/${ORG}` && req.method === "GET") return json({ organizations: github.map((organization) => ({ organization, installationId: "1" })) });
      if (path === "/guardener/v1alpha1/githubAssociations:begin" && req.method === "GET") return json({ clientId: "client-1", state: "signed-state" });
      if (path === `/guardener/v1alpha1/githubAssociations/${ORG}:link` && req.method === "POST") {
        linkBody = (await req.json()) as Record<string, string>;
        posts.push("link");
        if (!appInstalled) return json({ code: 9, message: "Precondition failed: the guardener GitHub App must be installed on the GitHub account (organization or user) before it can be linked" }, 400);
        github.push(linkBody.organization || "the-authorizing-user");
        return json({ organization: github.at(-1), installationId: "1" });
      }
      if (path === `/entitlements/v1alpha1/features/${ORG}:enableSandbox` && req.method === "POST") {
        posts.push("enable");
        // The service's real answer, as seen on 2026-11-15.
        if (github.length === 0) return json({ code: 9, message: "Precondition failed: the organization must link a GitHub organization through the Chainguard GitHub App first" }, 400);
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
  github = ["already-linked"];
  appInstalled = true;
  linkBody = {};
});

test("status and enable", async () => {
  let st = await workspacesStatus(opts);
  expect([st.enabled, st.ready, st.plan, st.maxSandboxes, st.maxSandboxesPerUser]).toEqual([false, false, "Starter", 3, 1]);
  expect(st.missingTerms.map((d) => d.id)).toEqual([...ENABLE_TERMS]);
  expect(st.githubAccounts).toEqual(["already-linked"]);
  expect(st.missingTerms.every((d) => d.label && d.url)).toBe(true);

  // Terms are never accepted without being asked to.
  const refused = await enableWorkspaces(opts).catch((e) => e);
  expect(refused).toBeInstanceOf(TermsNotAcceptedError);
  expect((refused as TermsNotAcceptedError).missing.length).toBe(ENABLE_TERMS.length);
  expect(posts).toEqual([]);

  st = await enableWorkspaces({ ...opts, acceptTerms: true });
  expect(st.ready).toBe(true);
  expect(posts).toEqual(["terms:" + ENABLE_TERMS.join(","), "enable"]);

  posts = []; // already on: nothing is sent
  expect((await enableWorkspaces({ ...opts, acceptTerms: true })).ready).toBe(true);
  expect(posts).toEqual([]);
});

test("enable accepts the terms the switch asks for", async () => {
  needForEnable = ["some-new-terms.v1"]; // a document this package doesn't know about yet
  for (const id of ENABLE_TERMS) accepted.add(id);
  const refused = await enableWorkspaces(opts).catch((e) => e);
  expect(refused).toBeInstanceOf(TermsNotAcceptedError);
  expect((refused as TermsNotAcceptedError).missing.map((d) => d.id)).toEqual(["some-new-terms.v1"]);
  expect((refused as TermsNotAcceptedError).missing[0]!.url).not.toBe("");
  expect(enabled).toBe(false);
  posts = [];
  expect((await enableWorkspaces({ ...opts, acceptTerms: true })).ready).toBe(true);
  expect(posts).toEqual(["enable", "terms:some-new-terms.v1", "enable"]);
});

test("enable links a GitHub account first when none is linked", async () => {
  github = [];
  // Without a browser it says what is needed.
  expect(await enableWorkspaces({ ...opts, acceptTerms: true, openBrowser: false }).catch((e) => e)).toBeInstanceOf(GitHubLinkRequiredError);
  expect(enabled).toBe(false);

  const probe = Bun.serve({ port: 0, hostname: "127.0.0.1", fetch: () => new Response("") });
  const port = probe.port!;
  await probe.stop(true);
  let seen = new URLSearchParams();
  // Stands in for the person and GitHub: follow the redirect_uri.
  const personAndGitHub = (url: string) => {
    seen = new URL(url).searchParams;
    void fetch(`${seen.get("redirect_uri")}?code=gh-code&state=${seen.get("state")}`).then((r) => r.text());
  };
  const run = { ...opts, acceptTerms: true, githubPort: port, openUrl: personAndGitHub, out: () => {} };

  appInstalled = false;
  const missingApp = await enableWorkspaces(run).catch((e) => e);
  expect(missingApp).toBeInstanceOf(GitHubAppNotInstalledError);
  expect((missingApp as GitHubAppNotInstalledError).installUrl).toBe(GITHUB_APP_INSTALL_URL);
  expect(enabled).toBe(false);
  expect([seen.get("client_id"), seen.get("state"), seen.get("code_challenge_method"), seen.get("prompt")]).toEqual(["client-1", "signed-state", "S256", "select_account"]);
  expect(seen.get("code_challenge")).toBeTruthy();
  expect(seen.get("redirect_uri")).toBe(`http://127.0.0.1:${port}/callback`);
  expect([linkBody.code, linkBody.state, linkBody.redirectUri]).toEqual(["gh-code", "signed-state", seen.get("redirect_uri")!]);
  expect(linkBody.codeVerifier).toBeTruthy();

  appInstalled = true; // the app is installed now: the next attempt links and turns it on
  posts = [];
  const st = await enableWorkspaces({ ...run, githubAccount: "my-org" });
  expect(st.ready).toBe(true);
  expect(st.githubAccounts).toEqual(["my-org"]);
  expect(posts).toEqual(["link", "enable"]);
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
