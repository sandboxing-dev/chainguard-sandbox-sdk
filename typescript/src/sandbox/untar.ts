// A small tar reader for downloadDir: regular files, directories and
// symlinks (USTAR, PAX and GNU long names). Nothing is written outside the
// target directory, and no link that leads outside it is left behind.
import { lstatSync, mkdirSync, readlinkSync, realpathSync, rmSync, symlinkSync, writeFileSync } from "node:fs";
import { dirname, isAbsolute, join, relative, resolve } from "node:path";
import { SandboxError } from "./errors";

export interface TarItem {
  name: string;
  type: "file" | "dir" | "symlink" | "other";
  mode: number;
  linkname: string;
  data: Uint8Array;
}

const dec = new TextDecoder();

function field(h: Uint8Array, off: number, len: number): string {
  const b = h.subarray(off, off + len);
  const end = b.indexOf(0);
  return dec.decode(end < 0 ? b : b.subarray(0, end));
}

function octal(h: Uint8Array, off: number, len: number): number {
  const s = field(h, off, len).trim();
  return s ? parseInt(s, 8) : 0;
}

/** Parse an uncompressed tar archive. */
export function readTar(b: Uint8Array): TarItem[] {
  const items: TarItem[] = [];
  let paxPath: string | undefined;
  let paxLink: string | undefined;
  let longName: string | undefined;
  let longLink: string | undefined;
  for (let off = 0; off + 512 <= b.length; ) {
    const h = b.subarray(off, off + 512);
    if (h.every((x) => x === 0)) break;
    const size = octal(h, 124, 12);
    const flag = String.fromCharCode(h[156]!) || "0";
    const data = b.subarray(off + 512, off + 512 + size);
    off += 512 + Math.ceil(size / 512) * 512;
    if (flag === "x") {
      for (const m of dec.decode(data).matchAll(/\d+ (path|linkpath)=([^\n]*)\n/g)) {
        if (m[1] === "path") paxPath = m[2];
        else paxLink = m[2];
      }
      continue;
    }
    if (flag === "g") continue;
    if (flag === "L") {
      longName = dec.decode(data).replace(/\0+$/, "");
      continue;
    }
    if (flag === "K") {
      longLink = dec.decode(data).replace(/\0+$/, "");
      continue;
    }
    const prefix = field(h, 345, 155);
    const name = paxPath ?? longName ?? (prefix ? `${prefix}/${field(h, 0, 100)}` : field(h, 0, 100));
    const linkname = paxLink ?? longLink ?? field(h, 157, 100);
    paxPath = paxLink = longName = longLink = undefined;
    const type = flag === "0" || flag === "\0" || flag === "7" ? "file" : flag === "5" ? "dir" : flag === "2" ? "symlink" : "other";
    items.push({ name, type, mode: octal(h, 100, 8) & 0o777, linkname, data });
  }
  return items;
}

const isSymlink = (p: string) => lstatSync(p, { throwIfNoEntry: false })?.isSymbolicLink() ?? false;
const parts = (p: string) => p.split(/[\\/]+/).filter((x) => x !== "");

/**
 * Checks that every existing component of rel (a directory path under root) is a real directory,
 * not a symbolic link. Components that don't exist yet are fine: they get created as real directories.
 */
function realParents(root: string, rel: string): string | undefined {
  let cur = root;
  for (const part of parts(rel)) {
    if (part === ".") continue;
    cur = join(cur, part);
    const st = lstatSync(cur, { throwIfNoEntry: false });
    if (!st) return undefined;
    if (st.isSymbolicLink()) return `${JSON.stringify(part)} is a symbolic link`;
    if (!st.isDirectory()) return `${JSON.stringify(part)} is not a directory`;
  }
  return undefined;
}

/**
 * Whether following the symbolic link at p leaves root. It walks the link's target one component
 * at a time, as path resolution does, expanding any link it meets (so the order links were created
 * in doesn't matter) and treating missing components as plain names.
 */
function linkEscapes(root: string, p: string): boolean {
  const target = readlinkSync(p);
  if (isAbsolute(target) || target.startsWith("/")) return true;
  let cur = dirname(p);
  const todo = parts(target);
  for (let hops = 0; todo.length > 0; ) {
    const part = todo.shift()!;
    if (part === ".") continue;
    if (part === "..") {
      if (cur === root) return true;
      cur = dirname(cur);
      continue;
    }
    const next = join(cur, part);
    if (isSymlink(next)) {
      const t = readlinkSync(next);
      if (++hops > 40 || isAbsolute(t) || t.startsWith("/")) return true;
      todo.unshift(...parts(t));
      continue;
    }
    cur = next;
  }
  return false;
}

/**
 * Extract a gzip'd tar into dir. Nothing it writes can land outside dir, and no symbolic link it
 * leaves behind leads outside dir:
 *
 * - names that are absolute or contain ".." are refused;
 * - an entry is never written through a symbolic link (one from the archive or one already in
 *   dir), so a link can't redirect later entries;
 * - once everything is unpacked, every link from the archive is followed the way the kernel would
 *   follow it, link by link. One that leaves dir (also through other links, in whatever order they
 *   were unpacked) is removed and the extraction fails.
 */
export function extractTarGz(tgz: Uint8Array, dir: string): void {
  mkdirSync(resolve(dir), { recursive: true });
  // The real path, so the destination itself may be reached through a link.
  const root = realpathSync(resolve(dir));
  const inside = (p: string) => {
    const rel = relative(root, p);
    return rel === "" || (!rel.startsWith("..") && !isAbsolute(rel));
  };
  const links: string[] = [];
  let escaping: string[] = [];
  try {
    for (const it of readTar(Bun.gunzipSync(tgz as Uint8Array<ArrayBuffer>))) {
      const name = it.name.replace(/^\.\//, "");
      if (name === "" || name === "." || name === "./") continue;
      if (it.name.startsWith("/") || name.split("/").includes("..")) throw new SandboxError(`refusing archive entry ${JSON.stringify(it.name)} outside the target`);
      const target = join(root, name);
      if (!inside(target)) throw new SandboxError(`refusing archive entry ${JSON.stringify(it.name)} outside the target`);
      if (it.type === "other") continue; // hard links, devices, FIFOs: skipped
      const why = realParents(root, dirname(relative(root, target)));
      if (why) throw new SandboxError(`refusing archive entry ${JSON.stringify(it.name)}: ${why}`);
      if (it.type === "dir") {
        if (isSymlink(target)) throw new SandboxError(`refusing archive entry ${JSON.stringify(it.name)}: a symbolic link is in the way`);
        mkdirSync(target, { recursive: true });
      } else if (it.type === "file") {
        mkdirSync(dirname(target), { recursive: true });
        if (isSymlink(target)) rmSync(target); // replace the link; don't write to what it points at
        writeFileSync(target, it.data, { mode: it.mode | 0o200 });
      } else if (it.type === "symlink") {
        if (isAbsolute(it.linkname) || it.linkname.startsWith("/") || !inside(resolve(dirname(target), it.linkname))) {
          throw new SandboxError(`refusing symlink ${JSON.stringify(it.name)} -> ${JSON.stringify(it.linkname)} pointing outside the target`);
        }
        mkdirSync(dirname(target), { recursive: true });
        rmSync(target, { force: true });
        symlinkSync(it.linkname, target);
        links.push(target);
      }
    }
  } finally {
    // Also when an entry is refused partway: no escaping link stays behind.
    escaping = links.filter((l) => isSymlink(l) && linkEscapes(root, l));
    for (const l of escaping) rmSync(l, { force: true });
  }
  if (escaping.length) throw new SandboxError(`refusing symlink ${JSON.stringify(relative(root, escaping[0]!))}: following it leads outside the target`);
}
