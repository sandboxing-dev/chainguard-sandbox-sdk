// File helpers over Exec, and local tar builders. Mirrors the Go SDK's workspaces/files.go.
import { lstatSync, readdirSync, readlinkSync } from "node:fs";
import { gzipSync } from "node:zlib";
import type { Client } from "./client.js";
import { guestPath, shell } from "./exec.js";
import { tarBytes, type TarEntry } from "./tar.js";

/** A filesystem path for dir + raw relative path bytes. */
const rawPath = (dir: string, rel: Uint8Array) => Buffer.concat([Buffer.from(dir.replace(/\/$/, "") + "/"), Buffer.from(rel)]);

/**
 * The shell program behind writeFile (exported for tests). It refuses an
 * existing directory with exit 21: plain `mv tmp dir` would move the temp file
 * into the directory and report success.
 */
export const WRITE_FILE_SCRIPT =
  'p=$1; if [ -d "$p" ]; then echo "$p: is a directory" >&2; cat >/dev/null; exit 21; fi; ' +
  'd=$(dirname -- "$p") && mkdir -p -- "$d" && t="$d/.chainguard-sandbox-write.$$" && cat > "$t" && chmod "$2" "$t" && mv -f -- "$t" "$p" || { rm -f -- "$t"; exit 1; }';

/** Atomically replace a file in the guest, creating parent directories. Refuses an empty path and an existing directory. */
export async function writeFile(c: Client, name: string, path: string, data: Uint8Array | string, perm = 0o644): Promise<void> {
  if (path.trim() === "") throw new Error("chainguard-sandbox: writeFile needs a file path");
  await shell(c, name, WRITE_FILE_SCRIPT, { stdin: data }, guestPath(path), (perm & 0o7777).toString(8));
}

type RawSkip = (rel: Uint8Array, relStr: string, isDir: boolean) => boolean;

/**
 * Walk dir with raw (byte) names, so filenames that aren't UTF-8 survive
 * intact. Lazy: directories are read and files stat'ed as entries are pulled,
 * and file contents are left to the tar writer (TarEntry.source).
 */
function* walkEntries(dir: string, skip?: RawSkip): Generator<TarEntry> {
  const slash = Buffer.from("/");
  const walk = function* (rel: Buffer | undefined): Generator<TarEntry> {
    // Bun returns Uint8Array here (not Buffer); normalize so toString/concat behave.
    const names = readdirSync(rel ? rawPath(dir, rel) : dir, { encoding: "buffer" })
      .map((n) => Buffer.from(n))
      .sort(Buffer.compare);
    for (const name of names) {
      const r = rel ? Buffer.concat([rel, slash, name]) : name;
      const p = rawPath(dir, r);
      const st = lstatSync(p);
      if (skip?.(r, r.toString("utf8"), st.isDirectory())) continue;
      const mode = st.mode & 0o7777;
      const mtime = st.mtimeMs / 1000; // rounded by the tar writer, like Go
      if (st.isSymbolicLink()) yield { name: r, type: "symlink", linkname: readlinkSync(p, { encoding: "buffer" }), mode, mtime };
      else if (st.isDirectory()) {
        yield { name: r, type: "dir", mode, mtime };
        yield* walk(r);
      } else if (st.isFile()) yield { name: r, type: "file", mode, mtime, source: { path: p, size: st.size } };
      // sockets, devices, FIFOs are skipped
    }
  };
  yield* walk(undefined);
}

/** Tar dir (symlinks as links; sockets/devices/FIFOs skipped). skip excludes a path and, for a dir, its contents. */
export function tarDir(dir: string, skip?: (rel: string, isDir: boolean) => boolean): Uint8Array<ArrayBuffer> {
  return tarBytes(walkEntries(dir, skip && ((_b, rel, isDir) => skip(rel, isDir))));
}

/** gzip'd tarDir. */
export function tarGzDir(dir: string, skip?: (rel: string, isDir: boolean) => boolean): Uint8Array {
  return gzipSync(tarDir(dir, skip));
}
