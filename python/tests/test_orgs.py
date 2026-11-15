"""Organization listing / auto-detection, and login audiences (local fakes only)."""

from __future__ import annotations

import base64
import json
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from chainguard_sandbox.workspaces import Client, StaticToken
from chainguard_sandbox.workspaces.login import (
    DEFAULT_AUDIENCE,
    PLATFORM_AUDIENCE,
    BrowserLoginToken,
    FileLoginStore,
    LoginRequiredError,
    default_login_audiences,
)
from chainguard_sandbox.workspaces.orgs import AmbiguousOrganizationError, NoOrganizationError, detect_instance, list_organizations


@pytest.fixture()
def iam(monkeypatch):
    state = {"groups": []}

    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_GET(self):
            ok = self.path == "/iam/v1/groups?uidp.in_root=true" and self.headers.get("Authorization") == "Bearer platform-tok"
            body = json.dumps({"items": state["groups"]} if ok else {"code": 7, "message": "nope"}).encode()
            self.send_response(200 if ok else 403)
            self.send_header("Content-Length", str(len(body)))
            self.end_headers()
            self.wfile.write(body)

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    monkeypatch.setenv("CHAINGUARD_SANDBOX_PLATFORM_URL", f"http://127.0.0.1:{srv.server_address[1]}")
    yield state
    srv.shutdown()


def test_list_and_detect(iam):
    ts = StaticToken("platform-tok")
    iam["groups"] = [{"id": "b2", "name": "zeta"}, {"id": "a1", "name": "alpha"}, {"id": "a1/child", "name": "sub"}]
    assert [(o.id, o.name) for o in list_organizations(ts)] == [("a1", "alpha"), ("b2", "zeta")]
    with pytest.raises(AmbiguousOrganizationError, match="a1  alpha"):
        detect_instance(ts)
    iam["groups"] = [{"id": "only1", "name": "my-org"}]
    assert detect_instance(ts) == "only1"
    iam["groups"] = []
    with pytest.raises(NoOrganizationError):
        detect_instance(ts)
    with pytest.raises(Exception, match="HTTP 403"):
        list_organizations(StaticToken("wrong"))


def test_client_instance_precedence(iam, monkeypatch):
    iam["groups"] = [{"id": "only1", "name": "my-org"}]
    monkeypatch.delenv("CHAINCTL_SANDBOX_INSTANCE", raising=False)
    monkeypatch.delenv("CHAINGUARD_SANDBOX_NO_DETECT_INSTANCE", raising=False)
    kw = dict(url="http://127.0.0.1:1", token_source=StaticToken("x"), platform_token_source=StaticToken("platform-tok"))
    assert Client(**kw).instance_id == "only1"
    monkeypatch.setenv("CHAINCTL_SANDBOX_INSTANCE", "fromenv")
    assert Client(**kw).instance_id == "fromenv"
    assert Client(instance="explicit", **kw).instance_id == "explicit"
    monkeypatch.delenv("CHAINCTL_SANDBOX_INSTANCE")
    for c in (Client(url="http://127.0.0.1:1", token_source=StaticToken("x"), detect_instance=False),
              Client(url="http://127.0.0.1:1", token_source=StaticToken("x"))):
        with pytest.raises(ValueError, match=r"CHAINCTL_SANDBOX_INSTANCE[\s\S]*chainctl iam organizations list"):
            c.list()


def _refresh_tok(exp: float, *aud: str) -> str:
    return base64.b64encode(json.dumps({"exp": int(exp), "aud": list(aud)}).encode()).decode()


def test_saved_login_kept_when_it_only_lacks_an_audience(tmp_path, monkeypatch):
    # An issuer that refuses every refresh with 401.
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):
            pass

        def do_POST(self):
            self.send_response(401)
            self.send_header("Content-Length", "2")
            self.end_headers()
            self.wfile.write(b"{}")

    srv = ThreadingHTTPServer(("127.0.0.1", 0), H)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    issuer = f"http://127.0.0.1:{srv.server_address[1]}"
    store = FileLoginStore(str(tmp_path / "login.json"))
    rt = _refresh_tok(time.time() + 3600, DEFAULT_AUDIENCE)
    store.save(issuer, rt)
    with pytest.raises(LoginRequiredError, match="doesn't cover"):
        BrowserLoginToken(issuer=issuer, audience=PLATFORM_AUDIENCE, store=store, no_browser=True).token()
    assert store.load(issuer) == rt
    with pytest.raises(LoginRequiredError):
        BrowserLoginToken(issuer=issuer, audience=DEFAULT_AUDIENCE, store=store, no_browser=True).token()
    assert store.load(issuer) == ""
    srv.shutdown()


def test_default_login_audiences(monkeypatch):
    monkeypatch.delenv("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", raising=False)
    assert default_login_audiences() == [DEFAULT_AUDIENCE, PLATFORM_AUDIENCE]
    monkeypatch.setenv("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", DEFAULT_AUDIENCE)
    assert default_login_audiences() == [DEFAULT_AUDIENCE]
