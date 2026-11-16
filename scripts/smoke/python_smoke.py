"""Checks an installed chainguard-sandbox-sdk package: the API imports.

With --live it also creates a sandbox, runs a command, moves a file and deletes the sandbox
(needs a saved login and an organization with Workspaces on).
"""

import sys
from importlib.metadata import version

from chainguard_sandbox import Sandbox, enable_workspaces, workspaces_status

assert callable(enable_workspaces) and callable(workspaces_status)
print(f"imports ok (chainguard-sandbox-sdk {version('chainguard-sandbox-sdk')}, python {sys.version.split()[0]})")

if "--live" in sys.argv:
    with Sandbox.create(packages=["bash", "coreutils"]) as sbx:
        r = sbx.commands.run("echo hello from $(uname -s)")
        assert r.stdout.strip() == "hello from Linux", r.stdout
        sbx.files.write("notes/hello.txt", "hi")
        assert sbx.files.read("notes/hello.txt") == "hi"
    print("live ok: created a sandbox, ran a command, wrote and read a file")
