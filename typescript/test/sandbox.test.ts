// The sandbox API (chainguard-sandbox-sdk): unit tests (no network).
import { expect, test } from "bun:test";
import { existsSync, lstatSync, mkdirSync, mkdtempSync, readFileSync, readdirSync, symlinkSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { tarBytes } from "../src/tar.js";
import { environmentDocument, parseEntries, parsePoll } from "../src/sandbox/index.js";
import * as S from "../src/sandbox/scripts.js";
import { extractTarGz } from "../src/sandbox/untar.js";

const enc = new TextEncoder();
// The Go SDK in this repository is the source of the scripts.
const GO_SCRIPTS = join(import.meta.dir, "..", "..", "go", "sandbox", "scripts.go");

test("scripts match the Go package", () => {
  if (!existsSync(GO_SCRIPTS)) return;
  const go = readFileSync(GO_SCRIPTS, "utf8");
  const statFn = /\bstatFn\s*=\s*`([^`]*)`/.exec(go)![1]!;
  let n = 0;
  for (const [name, value] of Object.entries(S)) {
    if (!name.endsWith("_SCRIPT") && name !== "STAT_FN") continue;
    const goName = name.toLowerCase().replace(/_([a-z])/g, (_, c) => c.toUpperCase());
    const m = new RegExp(`\\b${goName}\\s*=\\s*(statFn\\s*\\+\\s*)?\`([^\`]*)\``).exec(go);
    expect(m).not.toBeNull();
    expect(value).toBe((m![1] ? statFn : "") + m![2]);
    n++;
  }
  expect(n).toBe(18);
});

test("environmentDocument", () => {
  const d = new TextDecoder().decode(environmentDocument({ packages: ["python-3.13", 'we"ird'], egress: ["example.com"], egressPolicy: "destroy" }));
  expect(d).toContain('    - "python-3.13"\n    - "we\\"ird"\n');
  expect(d).toContain('      egress:\n        - "example.com"\n');
  expect(d).toContain('      policy: "destroy"\n');
  const def = new TextDecoder().decode(environmentDocument());
  expect(def).toContain('- "python-3.13"');
  expect(def).toContain('- "*"');
  expect(new TextDecoder().decode(environmentDocument({ egress: [] }))).toContain("egress: []");
});

test("parsePoll", () => {
  let p = parsePoll(enc.encode("- 5 3\nhelloerr"), 0, 0);
  expect([p.exitCode, new TextDecoder().decode(p.stdout), new TextDecoder().decode(p.stderr), p.truncated]).toEqual([undefined, "hello", "err", false]);
  p = parsePoll(enc.encode("7 8 3\nxyz"), 5, 3);
  expect([p.exitCode, new TextDecoder().decode(p.stdout), p.stderr.length]).toEqual([7, "xyz", 0]);
  p = parsePoll(enc.encode("0 10 0\nabc"), 0, 0);
  expect(p.truncated).toBe(true);
  expect(() => parsePoll(enc.encode("garbage"), 0, 0)).toThrow();
});

test("parseEntries", () => {
  const es = parseEntries(
    enc.encode("file\t12 644 root root 1700000000\t\tnotes.txt\ndir\t4096 755 root root 1700000001\t\tsub dir\nsymlink\t9 777 root root 1700000002\tnotes.txt\tlink\n"),
    "/work",
  );
  expect(es.map((e) => e.path)).toEqual(["/work/notes.txt", "/work/sub dir", "/work/link"]);
  expect(es[0]!.permissions).toBe("rw-r--r--");
  expect(es[0]!.modifiedTime.getTime()).toBe(1700000000_000);
  expect(es[2]!.symlinkTarget).toBe("notes.txt");
});

test("extractTarGz refuses entries outside the target", () => {
  const gz = (entries: Parameters<typeof tarBytes>[0]) => Bun.gzipSync(tarBytes(entries));
  const dir = mkdtempSync(join(tmpdir(), "chainguard-sandbox-untar-"));
  extractTarGz(gz([{ name: "a/", type: "dir" }, { name: "a/f.txt", data: enc.encode("x") }, { name: "a/l", type: "symlink", linkname: "f.txt" }]), dir);
  expect(readFileSync(join(dir, "a", "l"), "utf8")).toBe("x");
  for (const bad of [
    [{ name: "../evil", data: enc.encode("x") }],
    [{ name: "/etc/evil", data: enc.encode("x") }],
    [{ name: "l", type: "symlink" as const, linkname: "../../outside" }],
    [{ name: "l", type: "symlink" as const, linkname: "/etc/passwd" }],
  ]) {
    expect(() => extractTarGz(gz(bad), mkdtempSync(join(tmpdir(), "chainguard-sandbox-untar-")))).toThrow();
  }
});

test("extractTarGz: symbolic links cannot lead outside the target", () => {
  const gz = (entries: Parameters<typeof tarBytes>[0]) => Bun.gzipSync(tarBytes(entries));
  const link = (name: string, linkname: string) => ({ name, type: "symlink" as const, linkname });
  const file = (name: string) => ({ name, data: enc.encode("x") });
  const exists = (p: string) => lstatSync(p, { throwIfNoEntry: false }) !== undefined;
  const cases = {
    // Lexically "a/../outside" stays inside; followed, a is "." and it is a sibling of the target.
    "write through a link chain": [link("a", "."), link("b", "a/../outside"), file("b/proof")],
    "link chain only": [link("a", "."), link("b", "a/../outside")],
    "link chain, other order": [link("b", "a/../outside"), link("a", ".")],
    "file under a link": [{ name: "real/", type: "dir" as const }, link("l", "real"), file("l/f")],
  };
  for (const [name, entries] of Object.entries(cases)) {
    const parent = mkdtempSync(join(tmpdir(), "chainguard-sandbox-untar-"));
    const dir = join(parent, "target");
    mkdirSync(join(parent, "outside"));
    expect(() => extractTarGz(gz(entries), dir), name).toThrow();
    expect(readdirSync(join(parent, "outside")), name).toEqual([]);
    expect(exists(join(parent, "proof")), name).toBe(false);
    expect(exists(join(dir, "b")), name).toBe(false); // the escaping link is not left in place
  }

  // A link already in the destination is not written through either.
  const parent = mkdtempSync(join(tmpdir(), "chainguard-sandbox-untar-"));
  const dir = join(parent, "target");
  const elsewhere = join(parent, "elsewhere");
  mkdirSync(dir);
  mkdirSync(elsewhere);
  symlinkSync(elsewhere, join(dir, "sub"));
  expect(() => extractTarGz(gz([file("sub/f")]), dir)).toThrow();
  expect(readdirSync(elsewhere)).toEqual([]);

  // Links that stay inside are kept, including ones through other links.
  const ok = mkdtempSync(join(tmpdir(), "chainguard-sandbox-untar-"));
  extractTarGz(gz([{ name: "a/", type: "dir" }, file("a/f.txt"), link("a/up", ".."), link("l", "a/up/a/f.txt"), link("dangling", "a/missing")]), ok);
  expect(readFileSync(join(ok, "l"), "utf8")).toBe("x");
});
