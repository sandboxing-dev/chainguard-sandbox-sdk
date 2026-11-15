"""chainguard_sandbox: unit tests, plus a live test (CHAINGUARD_SANDBOX_LIVE=1)."""

from __future__ import annotations

import re
from pathlib import Path

import pytest

from chainguard_sandbox import (
    CommandExitError,
    FileNotFoundError,
    InvalidPathError,
    Sandbox,
    TimeoutError,
    environment_document,
)
from chainguard_sandbox import _scripts as S
from chainguard_sandbox._sandbox import _guest_path, parse_entries, parse_poll

GO_SCRIPTS = Path(__file__).resolve().parents[2] / "go" / "sandbox" / "scripts.go"


def test_scripts_match_the_go_package():
    if not GO_SCRIPTS.exists():
        pytest.skip("Go package not alongside")
    go = GO_SCRIPTS.read_text()
    stat_fn = re.search(r"\bstatFn\s*=\s*`([^`]*)`", go).group(1)
    for name, value in vars(S).items():
        if not name.endswith("_SCRIPT") and name != "STAT_FN":
            continue
        go_name = re.sub(r"_([a-z])", lambda m: m.group(1).upper(), name.lower())
        m = re.search(r"\b" + go_name + r"\s*=\s*(statFn\s*\+\s*)?`([^`]*)`", go)
        assert m, go_name
        assert value == (stat_fn if m.group(1) else "") + m.group(2), name
    assert f'stateDir = "{S.STATE_DIR}"' in go


def test_environment_document():
    d = environment_document(["python-3.13", 'we"ird'], ["example.com"], "destroy").decode()
    assert '    - "python-3.13"\n    - "we\\"ird"\n' in d
    assert '      egress:\n        - "example.com"\n' in d and '      policy: "destroy"\n' in d
    default = environment_document().decode()
    assert '- "python-3.13"' in default and '- "*"' in default and 'policy: "deny"' in default
    assert "egress: []" in environment_document(egress=[]).decode()


def test_guest_path():
    assert _guest_path(None) == "/work"
    assert _guest_path("a/b") == "/work/a/b"
    assert _guest_path("/tmp/../etc") == "/etc"


def test_parse_poll():
    assert parse_poll(b"- 5 3\nhelloerr", 0, 0) == (None, b"hello", b"err", False)
    assert parse_poll(b"7 8 3\nxyz", 5, 3) == (7, b"xyz", b"", False)
    code, out, err, truncated = parse_poll(b"0 10 0\nabc", 0, 0)
    assert (out, truncated) == (b"abc", True)
    with pytest.raises(Exception):
        parse_poll(b"garbage", 0, 0)


def test_parse_entries():
    out = (b"file\t12 644 root root 1700000000\t\tnotes.txt\n"
           b"dir\t4096 755 root root 1700000001\t\tsub dir\n"
           b"symlink\t9 777 root root 1700000002\tnotes.txt\tlink\n")
    es = parse_entries(out, "/work")
    assert [e.path for e in es] == ["/work/notes.txt", "/work/sub dir", "/work/link"]
    assert es[0].permissions == "rw-r--r--" and es[0].size == 12 and es[0].modified_time.timestamp() == 1700000000
    assert es[1].type == "dir" and es[2].symlink_target == "notes.txt"


def test_kill_works_on_class_and_instance():
    assert callable(Sandbox.kill)
    s = Sandbox.__new__(Sandbox)
    assert callable(s.kill)
