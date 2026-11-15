// Minimal tar writer for file uploads, following Go's archive/tar
// (the Go SDK's workspaces/files.go): USTAR when every field fits (ASCII, name split
// into prefix+name when longer than 100 bytes), PAX extended headers
// otherwise (non-ASCII names, link targets over 100 bytes, sizes ≥ 8 GiB).
// Entries are root-owned; mtimes are rounded to the nearest second.
//
// tarChunks streams: an entry with `source` is read from disk in pieces as the
// archive is consumed, so only one piece is held at a time.
import { closeSync, openSync, readSync, type PathLike } from "node:fs";

export interface TarEntry {
  /** Path in the archive; raw bytes are written as-is (filenames need not be UTF-8). */
  name: string | Uint8Array;
  /** Permission bits as from fs.Stats.mode; defaults to 0o644 (0o755 dirs, 0o777 symlinks). */
  mode?: number;
  /** Seconds since the epoch (may be fractional; rounded); defaults to 0. */
  mtime?: number;
  type?: "file" | "dir" | "symlink";
  linkname?: string | Uint8Array;
  data?: Uint8Array;
  /**
   * A file read lazily instead of `data`, size bytes long (from lstat). It is
   * read in pieces when the entry is written; a file that is shorter than size
   * by then fails the archive, and bytes past size are not read (as Go's
   * archive/tar would reject them).
   */
  source?: { path: PathLike; size: number };
}

/** How much of a source file is read at a time. */
export const TAR_READ_SIZE = 1 << 20;

const enc = new TextEncoder();
const NAME_SIZE = 100;
const PREFIX_SIZE = 155;
const MAX_OCTAL_SIZE = 0o77777777777; // 11 octal digits

const bytes = (s: string | Uint8Array) => (typeof s === "string" ? enc.encode(s) : s);
const isASCII = (b: Uint8Array) => b.every((x) => x < 0x80);
const latin1 = (b: Uint8Array) => Buffer.from(b).toString("latin1"); // byte-preserving for ASCII

function octal(n: number, width: number): string {
  return n.toString(8).padStart(width - 1, "0") + "\0";
}

/** Truncate to at most n bytes without splitting a UTF-8 sequence. */
function truncateBytes(s: string | Uint8Array, n: number): Uint8Array {
  const b = bytes(s);
  if (b.length <= n) return b;
  let end = n;
  while (end > 0 && (b[end]! & 0xc0) === 0x80) end--; // back off continuation bytes
  return b.subarray(0, end);
}

/** Go's splitUSTARPath: ASCII names of 101..256 bytes split at a "/" into prefix (≤155) and name (≤100). */
function splitUSTARPath(nameB: Uint8Array): [string, string] | undefined {
  if (nameB.length <= NAME_SIZE || !isASCII(nameB)) return undefined;
  const name = latin1(nameB);
  let length = name.length;
  if (length > PREFIX_SIZE + 1) length = PREFIX_SIZE + 1;
  else if (name[length - 1] === "/") length--;
  const i = name.slice(0, length).lastIndexOf("/");
  const nlen = name.length - i - 1;
  if (i <= 0 || nlen > NAME_SIZE || nlen === 0 || i > PREFIX_SIZE) return undefined;
  return [name.slice(0, i), name.slice(i + 1)];
}

interface Fields {
  name: string | Uint8Array;
  prefix?: string;
  size: number;
  mode: number;
  mtime: number;
  typeflag: string;
  linkname: string | Uint8Array;
}

function header(f: Fields): Uint8Array {
  const h = new Uint8Array(512);
  const put = (off: number, s: string | Uint8Array, len: number) => h.set(truncateBytes(s, len), off);
  put(0, f.name, NAME_SIZE);
  put(100, octal(f.mode & 0o7777, 8), 8);
  put(108, octal(0, 8), 8); // uid: root
  put(116, octal(0, 8), 8); // gid: root
  put(124, octal(Math.min(f.size, MAX_OCTAL_SIZE), 12), 12);
  put(136, octal(Math.max(0, f.mtime), 12), 12);
  put(148, "        ", 8); // checksum placeholder
  put(156, f.typeflag, 1);
  put(157, f.linkname, 100);
  put(257, "ustar\0", 6);
  put(263, "00", 2);
  // uname/gname (265, 297) stay empty
  put(329, octal(0, 8), 8); // devmajor
  put(337, octal(0, 8), 8); // devminor
  if (f.prefix) put(345, f.prefix, PREFIX_SIZE);
  let sum = 0;
  for (const b of h) sum += b;
  put(148, sum.toString(8).padStart(6, "0") + "\0 ", 8);
  return h;
}

function padding(size: number): Uint8Array[] {
  const rem = size % 512;
  return rem === 0 ? [] : [new Uint8Array(512 - rem)];
}

function padded(data: Uint8Array): Uint8Array[] {
  return [data, ...padding(data.length)];
}

/** The size bytes of a source file, in pieces of at most TAR_READ_SIZE. */
function* readSource(src: { path: PathLike; size: number }): Generator<Uint8Array> {
  const fd = openSync(src.path, "r");
  try {
    let left = src.size;
    while (left > 0) {
      const buf = new Uint8Array(Math.min(left, TAR_READ_SIZE));
      const n = readSync(fd, buf, 0, buf.length, src.size - left);
      if (n === 0) throw new Error(`${String(src.path)}: file shrank while being archived (${src.size - left} of ${src.size} bytes)`);
      left -= n;
      yield n === buf.length ? buf : buf.subarray(0, n);
    }
  } finally {
    closeSync(fd);
  }
}

/** One PAX record: "<len> <key>=<value>\n", where <len> counts the bytes of the whole record. */
export function paxRecord(key: string, value: string | Uint8Array): Uint8Array {
  const v = bytes(value);
  const bodyBytes = enc.encode(` ${key}=`).length + v.length + 1;
  let len = bodyBytes + 1;
  while (String(len).length + bodyBytes !== len) len = String(len).length + bodyBytes;
  return Buffer.concat([enc.encode(`${len} ${key}=`), v, enc.encode("\n")]);
}

/** ASCII-only rendering (Go writes PAX-covered fields this way in the USTAR block). */
function toASCII(b: Uint8Array): Uint8Array {
  return b.filter((x) => x < 0x80);
}

/** Serialize entries into an uncompressed tar archive. */
export function tarBytes(entries: Iterable<TarEntry>): Uint8Array<ArrayBuffer> {
  const parts = [...tarChunks(entries)];
  let total = 0;
  for (const p of parts) total += p.length;
  const out = new Uint8Array(total);
  let off = 0;
  for (const p of parts) {
    out.set(p, off);
    off += p.length;
  }
  return out;
}

/**
 * The archive as a sequence of byte pieces, produced as they are pulled:
 * entries (which may be a lazy generator) are consumed one at a time and
 * `source` files are read in pieces, so memory stays bounded by the piece size.
 */
export function* tarChunks(entries: Iterable<TarEntry>): Generator<Uint8Array> {
  for (const e of entries) {
    const type = e.type ?? "file";
    const data = type === "file" && !e.source ? (e.data ?? new Uint8Array()) : new Uint8Array();
    const size = type === "file" && e.source ? e.source.size : data.length;
    const typeflag = type === "dir" ? "5" : type === "symlink" ? "2" : "0";
    let nameB = bytes(e.name);
    if (type === "dir" && nameB[nameB.length - 1] !== 0x2f) nameB = Buffer.concat([nameB, enc.encode("/")]);
    const linkB = bytes(e.linkname ?? "");
    const mode = e.mode ?? (type === "dir" ? 0o755 : type === "symlink" ? 0o777 : 0o644);
    const mtime = Math.round(e.mtime ?? 0);

    const pax: Record<string, Uint8Array> = {};
    let ustarName: Uint8Array = nameB;
    let prefix: string | undefined;
    if (!isASCII(nameB) || nameB.length > NAME_SIZE) {
      const split = splitUSTARPath(nameB);
      if (split) {
        prefix = split[0];
        ustarName = enc.encode(split[1]);
      } else {
        pax.path = nameB;
        ustarName = toASCII(nameB);
      }
    }
    let ustarLink: Uint8Array = linkB;
    if (!isASCII(linkB) || linkB.length > NAME_SIZE) {
      pax.linkpath = linkB;
      ustarLink = toASCII(linkB);
    }
    if (size > MAX_OCTAL_SIZE) pax.size = enc.encode(String(size));

    const keys = Object.keys(pax).sort();
    if (keys.length) {
      const records = Buffer.concat(keys.map((k) => paxRecord(k, pax[k]!)));
      const trimmed = nameB[nameB.length - 1] === 0x2f ? nameB.subarray(0, nameB.length - 1) : nameB;
      const slash = trimmed.lastIndexOf(0x2f);
      const dir = trimmed.subarray(0, slash + 1);
      const file = trimmed.subarray(slash + 1);
      const paxName = toASCII(Buffer.concat([dir, enc.encode("PaxHeaders.0/"), file]));
      yield header({ name: paxName, size: records.length, mode: 0, mtime: 0, typeflag: "x", linkname: "" });
      yield* padded(records);
    }
    yield header({ name: ustarName, prefix, size, mode, mtime, typeflag, linkname: ustarLink });
    if (type === "file" && e.source) yield* readSource(e.source);
    else if (data.length) yield data;
    yield* padding(size);
  }
  yield new Uint8Array(1024);
}
