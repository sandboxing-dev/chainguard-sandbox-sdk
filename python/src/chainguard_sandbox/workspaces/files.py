"""Tar a local directory for upload into a sandbox."""

from __future__ import annotations

import gzip
import io
import os
from typing import Callable

from chainguard_sandbox.workspaces.tarw import TarWriter, Writer

SkipFn = Callable[[str, os.DirEntry], bool]


def _walk(root: str, skip: SkipFn | None, rel: str = ""):
    """Lexical-order walk like Go's filepath.WalkDir; yields (rel, entry).
    A directory that skip() rejects is not descended into (Go's SkipDir), so
    an unreadable ignored directory is never opened. Symlinked directories are
    never descended into."""
    with os.scandir(os.path.join(root, rel) if rel else root) as it:
        entries = sorted(it, key=lambda e: os.fsencode(e.name))
    for e in entries:
        r = f"{rel}/{e.name}" if rel else e.name
        if skip and skip(r, e):
            continue
        yield r, e
        if e.is_dir(follow_symlinks=False):
            yield from _walk(root, skip, r)


def tar_dir(w: Writer, dir: str, skip: SkipFn | None = None) -> None:
    """dir as an uncompressed tar: symlinks as links (never followed);
    sockets, devices and FIFOs skipped; hard-linked files written as regular
    files. skip(rel, entry) excludes a path and, for a directory, everything
    under it. Entries are root-owned, like chainctl's uploads."""
    tw = TarWriter(w)
    for rel, _e in _walk(dir, skip):
        tw.add_path(rel, os.path.join(dir, rel))
    tw.close()


def tar_gz_dir(dir: str, skip: SkipFn | None = None) -> bytes:
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb") as zw:
        tar_dir(zw, dir, skip)
    return buf.getvalue()
