"""A minimal tar writer for workspace uploads, matching Go's archive/tar.

Headers follow what the Go library writes (Writer.WriteHeader with an
unspecified format): USTAR when the entry fits (ASCII names, long names split
into the 155-byte prefix the way Go splits them), PAX otherwise. mtime is
rounded to the nearest second, owners are root (uid/gid 0, no names), and the
archive ends with two zero blocks and no record padding. Every path is written
as a regular file, directory or symlink: never as a hard link.
"""

from __future__ import annotations

import os
import stat
import tarfile
from typing import Protocol

_BLOCK = 512
_NAME, _PREFIX, _LINK = 100, 155, 100


def round_mtime(t: float) -> int:
    """Nearest second, halves away from zero (Go's time.Round)."""
    return int(t + 0.5) if t >= 0 else -int(-t + 0.5)


def _octal(n: int, width: int) -> bytes:
    return b"%0*o" % (width - 1, n) + b"\0"


def _split_ustar(name: bytes) -> tuple[bytes, bytes] | None:
    """Go's splitUSTARPath: the longest prefix (<= 155 bytes) ending at "/"
    whose remainder fits the 100-byte name field."""
    length = len(name)
    if length > _PREFIX + 1:
        length = _PREFIX + 1
    elif name.endswith(b"/"):
        length -= 1
    i = name.rfind(b"/", 0, length)
    nlen, plen = len(name) - i - 1, i
    if i <= 0 or nlen > _NAME or nlen == 0 or plen > _PREFIX:
        return None
    return name[:i], name[i + 1:]


def _block(name: bytes, mode: int, size: int, mtime: int, typeflag: bytes, linkname: bytes = b"",
           prefix: bytes = b"", dev: bool = True) -> bytes:
    """One ustar-magic header block, fields formatted like Go's archive/tar."""
    h = bytearray(_BLOCK)

    def put(off: int, b: bytes) -> None:
        h[off:off + len(b)] = b

    put(0, name[:_NAME])
    put(100, _octal(mode & 0o7777, 8))
    put(108, _octal(0, 8))
    put(116, _octal(0, 8))
    put(124, _octal(size, 12))
    put(136, _octal(mtime, 12))
    put(148, b" " * 8)
    put(156, typeflag)
    put(157, linkname[:_LINK])
    put(257, b"ustar\x0000")
    if dev:
        put(329, _octal(0, 8))
        put(337, _octal(0, 8))
    put(345, prefix)
    put(148, b"%06o\0" % sum(h))  # byte 155 stays " ", as in Go
    return bytes(h)


def _to_ascii(b: bytes) -> bytes:
    """Go's toASCII: drop non-ASCII (and NUL) characters."""
    return bytes(c for c in b.decode("utf-8", "surrogateescape").encode("ascii", "ignore") if c)


def _pax_record(k: bytes, v: bytes) -> bytes:
    """Go's formatPAXRecord: "<len> <key>=<value>\n", len counting itself."""
    size = len(k) + len(v) + 3
    size += len(str(size))
    rec = b"%d %s=%s\n" % (size, k, v)
    if len(rec) != size:
        rec = b"%d %s=%s\n" % (len(rec), k, v)
    return rec


def _ustar_header(name: bytes, mode: int, size: int, mtime: int, typeflag: bytes, linkname: bytes) -> bytes | None:
    if not (name.isascii() and linkname.isascii()) or len(linkname) > _LINK:
        return None
    if size >= 8 ** 11 or not 0 <= mtime < 8 ** 11:
        return None
    prefix = b""
    if len(name) > _NAME:
        split = _split_ustar(name)
        if split is None:
            return None
        prefix, name = split
    return _block(name, mode, size, mtime, typeflag, linkname, prefix)


def _pax_headers(name: bytes, mode: int, size: int, mtime: int, typeflag: bytes, linkname: bytes) -> bytes:
    """Go's writePAXHeader: a "<dir>/PaxHeaders.0/<file>" extended header with
    sorted records, then the main header with ASCII-only, truncated names."""
    recs: dict[bytes, bytes] = {}
    if not name.isascii() or (len(name) > _NAME and _split_ustar(name) is None):
        recs[b"path"] = name
    if not linkname.isascii() or len(linkname) > _LINK:
        recs[b"linkpath"] = linkname
    if size >= 8 ** 11:
        recs[b"size"] = b"%d" % size
    data = b"".join(_pax_record(k, recs[k]) for k in sorted(recs))
    d, _, f = name.rpartition(b"/")
    xname = b"/".join(p for p in (d, b"PaxHeaders.0", f) if p)
    xname = _to_ascii(xname)[:_NAME].rstrip(b"/")
    out = _block(xname, 0, len(data), 0, tarfile.XHDTYPE, dev=False) + data
    if len(data) % _BLOCK:
        out += b"\0" * (_BLOCK - len(data) % _BLOCK)
    return out + _block(_to_ascii(name), mode, size if size < 8 ** 11 else 0, mtime, typeflag, _to_ascii(linkname))


class Writer(Protocol):
    """Anything with write(bytes): a file, BytesIO, GzipFile."""

    def write(self, b: bytes, /) -> object: ...


class TarWriter:
    """Writes entries to a binary stream; call close() to finish the archive."""

    def __init__(self, w: Writer):
        self._w = w

    def _header(self, name: str, mode: int, size: int, mtime: int, typeflag: bytes, linkname: str = "") -> None:
        nb = name.encode("utf-8", "surrogateescape")
        lb = linkname.encode("utf-8", "surrogateescape")
        h = _ustar_header(nb, mode, size, mtime, typeflag, lb)
        self._w.write(h if h is not None else _pax_headers(nb, mode, size, mtime, typeflag, lb))

    def add_bytes(self, name: str, data: bytes, mode: int = 0o644, mtime: int = 0) -> None:
        self._header(name, mode, len(data), mtime, tarfile.REGTYPE)
        self._w.write(data)
        self._pad(len(data))

    def add_path(self, name: str, path: str) -> bool:
        """Add the file, directory or symlink at path (lstat; never a hard
        link). Sockets, devices and FIFOs are skipped (returns False)."""
        st = os.lstat(path)
        mode, mtime = stat.S_IMODE(st.st_mode), round_mtime(st.st_mtime)
        if stat.S_ISDIR(st.st_mode):
            self._header(name if name.endswith("/") else name + "/", mode, 0, mtime, tarfile.DIRTYPE)
        elif stat.S_ISLNK(st.st_mode):
            self._header(name, mode, 0, mtime, tarfile.SYMTYPE, os.readlink(path))
        elif stat.S_ISREG(st.st_mode):
            size = st.st_size
            self._header(name, mode, size, mtime, tarfile.REGTYPE)
            # Exactly size bytes even if the file changes while we read it
            # (a growing log must not corrupt the archive).
            left = size
            with open(path, "rb") as f:
                while left > 0:
                    b = f.read(min(left, 1 << 20))
                    if not b:
                        break
                    self._w.write(b)
                    left -= len(b)
            if left > 0:
                self._w.write(b"\0" * left)
            self._pad(size)
        else:
            return False
        return True

    def _pad(self, n: int) -> None:
        if n % _BLOCK:
            self._w.write(b"\0" * (_BLOCK - n % _BLOCK))

    def close(self) -> None:
        self._w.write(b"\0" * (2 * _BLOCK))
