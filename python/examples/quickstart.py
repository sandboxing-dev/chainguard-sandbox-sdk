"""Quickstart: create a sandbox, run a command, write and read a file, delete it.

Before anything else: your Chainguard organization must exist and have
Workspaces turned on (one time, by an owner: `chainguard-sandbox enable`).
Otherwise create fails with "PERMISSION_DENIED: this organization is not
entitled to Chainguard Workspaces". See the Quickstart in the docs.

Before you run it:

0. Install the SDK in your project, from GitHub or from a local checkout:

       uv add "chainguard-sandbox-sdk @ git+https://github.com/sandboxing-dev/chainguard-sandbox-sdk#subdirectory=python"
       uv add --editable ../chainguard-sandbox-sdk/python

   (Or run this file from chainguard-sandbox-sdk/python/, where it's already importable.)

1. Log in once. It opens your browser; the login is saved and shared by the
   Go, Python and TypeScript SDKs:

       uv run chainguard-sandbox login          # from python/; optional: add github, google-oauth2 or gitlab

   A saved login lasts as long as it's used at least once every 20 hours. If a
   run opens your browser, the login expired: approve it and the run continues,
   or renew it ahead of time with the same login command.

2. Choose the organization (the "instance"). Any one of these works; the first
   one set wins:

   a. in code: Sandbox.create(instance="XXXXXX") (commented out below);
   b. an environment variable, for this terminal session only:
          export CHAINCTL_SANDBOX_INSTANCE=XXXXXX    # your organization ID
   c. nothing: if you belong to exactly one organization, it is detected
      automatically (turn that off with CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE=1).

   List your organization IDs with either of:

       uv run chainguard-sandbox orgs
       chainctl iam organizations list

3. Run it (from chainguard-sandbox-sdk/python/):

       uv run python examples/quickstart.py
"""
# --8<-- [start:program]
from chainguard_sandbox import Sandbox

# --8<-- [start:quickstart]
with Sandbox.create(
    packages=["bash", "coreutils", "python-3.13"],
    # instance="XXXXXX",  # your organization ID; overrides $CHAINCTL_SANDBOX_INSTANCE and auto-detection
) as sbx:  # deleted on exit
    result = sbx.commands.run("""python3 -c "print('hello from', __import__('platform').node())" """)
    print(result.stdout, end="")

    sbx.files.write("notes/hello.txt", "hi")
    print(sbx.files.read("notes/hello.txt"))
# --8<-- [end:quickstart]
# --8<-- [end:program]
