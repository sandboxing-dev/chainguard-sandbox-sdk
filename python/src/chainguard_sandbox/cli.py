"""chainguard-sandbox: log in and manage sandboxes from the command line.

    chainguard-sandbox login                  # browser login, saved for later runs (no chainctl needed)
    chainguard-sandbox orgs                   # your organizations (auto-detected if you have one)
    export CHAINCTL_SANDBOX_INSTANCE=XXXXXX   # only needed with several
    chainguard-sandbox ls                     # this SDK's sandboxes
    chainguard-sandbox rm SANDBOX_ID          # delete one
"""

from __future__ import annotations

import os
import sys
from datetime import datetime

USAGE = "usage: chainguard-sandbox login [github|google-oauth2|gitlab] | logout | whoami | orgs | ls | rm SANDBOX_ID"


def main(argv: list[str] | None = None) -> int:
    args = sys.argv[1:] if argv is None else argv
    if not args:
        print(USAGE, file=sys.stderr)
        return 2
    cmd, rest = args[0], args[1:]
    try:
        if cmd in ("login", "logout", "whoami"):
            return _login_cmd(cmd, rest)
        if cmd == "orgs" and not rest:
            return _orgs()
        if cmd == "ls" and not rest:
            return _ls()
        if cmd == "rm" and len(rest) == 1:
            from chainguard_sandbox import Sandbox

            if not Sandbox.kill(rest[0]):
                print(f"chainguard-sandbox: no sandbox {rest[0]!r}", file=sys.stderr)
                return 1
            return 0
    except Exception as e:  # noqa: BLE001 - CLI boundary
        print(f"chainguard-sandbox: {e}", file=sys.stderr)
        return 1
    print(USAGE, file=sys.stderr)
    return 2


def _orgs() -> int:
    from chainguard_sandbox.workspaces.orgs import list_organizations

    orgs = list_organizations()
    w = max([len("ID")] + [len(o.id) for o in orgs])
    print(f"{'ID'.ljust(w)}  NAME")
    for o in orgs:
        print(f"{o.id.ljust(w)}  {o.name}")
    return 0


def _ls() -> int:
    from chainguard_sandbox import Sandbox

    rows = [["ID", "NAME", "STATE", "REGION", "CREATED"]]
    for i in Sandbox.list():
        created = i.created_at.astimezone().isoformat(" ", "seconds") if i.created_at else "-"
        rows.append([i.sandbox_id, i.display_name, i.state, i.region, created])
    w = [max(len(r[c]) for r in rows) for c in range(len(rows[0]))]
    for r in rows:
        print("  ".join(v.ljust(w[c]) for c, v in enumerate(r)).rstrip())
    return 0


def _login_cmd(cmd: str, rest: list[str]) -> int:
    from chainguard_sandbox.workspaces.login import BrowserLoginToken, FileLoginStore, browser_login, default_login_file, logout, token_claims

    if cmd == "logout" and not rest:
        logout()
        print("logged out:", default_login_file())
        return 0
    if cmd == "login" and len(rest) <= 1:
        res = browser_login(connection=rest[0] if rest else os.environ.get("CHAINGUARD_SANDBOX_SOCIAL_LOGIN", ""))
        if not res.refresh_token:
            raise RuntimeError("the issuer returned no refresh token, so the login can't be saved")
        FileLoginStore().save("", res.refresh_token)
        print("logged in; saved to", default_login_file())
    elif not (cmd == "whoami" and not rest):
        print(USAGE, file=sys.stderr)
        return 2
    c = token_claims(BrowserLoginToken(no_browser=True).token())
    aud = c.get("aud")
    print(f"subject:  {c.get('sub')}\nissuer:   {c.get('iss')}\naudience: {aud if isinstance(aud, list) else [aud]}\n"
          f"expires:  {datetime.fromtimestamp(c.get('exp', 0)).isoformat(' ')}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
