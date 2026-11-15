"""Browser login, refresh rotation and the shared login file (local fakes only)."""

from __future__ import annotations

import base64
import io
import json
import os
import stat
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from concurrent import futures
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

import pytest

from chainguard_sandbox.workspaces.auth import DEFAULT_AUDIENCE
from chainguard_sandbox.workspaces.login import (
    AccountNotFoundError,
    BrowserLoginToken,
    FileLoginStore,
    LoginExpiredError,
    LoginRequiredError,
    _has_audience,
    _LoginServer,
    browser_login,
    logout,
    refresh_login,
)


def fake_jwt(*aud: str) -> str:
    enc = lambda b: base64.urlsafe_b64encode(b).rstrip(b"=").decode()  # noqa: E731
    cl = json.dumps({"sub": "user1", "aud": list(aud), "exp": int(time.time()) + 3600}).encode()
    return f"{enc(b'{}')}.{enc(cl)}.sig"


class FakeIssuer:
    """/sts/exchange_refresh_token with rotating refresh tokens."""

    def __init__(self, valid=()):
        self.valid = set(valid)
        self.n = 0
        self.last_aud = ""
        self.lock = threading.Lock()
        outer = self

        class H(BaseHTTPRequestHandler):
            def log_message(self, *a):
                pass

            def do_POST(self):
                u = urllib.parse.urlsplit(self.path)
                q = dict(urllib.parse.parse_qsl(u.query))
                with outer.lock:
                    if u.path != "/sts/exchange_refresh_token":
                        return self._send(404, {})
                    if not q.get("aud"):
                        return self._send(400, {"code": 3, "message": "Audience must be specified"})
                    rt = self.headers.get("Authorization", "").removeprefix("Bearer ")
                    if rt.startswith("expired"):  # what the real issuer sends for an expired refresh token
                        return self._send(400, {"code": 3, "message": "invalid refresh token: refresh token expired at 2026-11-10 23:10:16 +0000 UTC"})
                    if rt not in outer.valid:
                        return self._send(401, {"code": 16, "message": "invalid refresh token"})
                    outer.valid.discard(rt)
                    outer.n += 1
                    nxt = f"rt-{outer.n}"
                    outer.valid.add(nxt)
                    outer.last_aud = q["aud"]
                    self._send(200, {"token": {"token": fake_jwt(q["aud"])}, "refreshToken": {"token": nxt}})

            def _send(self, code, body):
                b = json.dumps(body).encode()
                self.send_response(code)
                self.send_header("Content-Length", str(len(b)))
                self.end_headers()
                self.wfile.write(b)

        self.httpd = ThreadingHTTPServer(("127.0.0.1", 0), H)
        self.url = f"http://127.0.0.1:{self.httpd.server_address[1]}"
        threading.Thread(target=self.httpd.serve_forever, daemon=True).start()

    def close(self):
        self.httpd.shutdown()
        self.httpd.server_close()


@pytest.fixture()
def issuer():
    f = FakeIssuer()
    yield f
    f.close()


def complete(query: dict, seen: dict | None = None, check: list | None = None):
    """Plays the browser: follow the issuer's redirect to the callback, then load '/'."""

    def open_url(url: str) -> bool:
        q = dict(urllib.parse.parse_qsl(urllib.parse.urlsplit(url).query))
        if seen is not None:
            seen.update(q)
        cb = urllib.parse.urlsplit(q["redirect"])
        target = urllib.parse.urlunsplit((cb.scheme, cb.netloc.replace("localhost", "127.0.0.1"), cb.path,
                                          urllib.parse.urlencode(query), ""))

        def go():
            try:
                with urllib.request.urlopen(target, timeout=5) as r:
                    body, final = r.read(), r.geturl()
            except urllib.error.HTTPError as e:
                body, final = e.read(), e.geturl()
            if check is not None:
                check.append((body, final))

        threading.Thread(target=go, daemon=True).start()
        return True

    return open_url


def test_browser_login_sends_chainctls_parameters():
    seen: dict = {}
    check: list = []
    res = browser_login(connection="github", open_url=complete({"token": "tok", "refresh_token": "rt"}, seen, check),
                        out=io.StringIO(), timeout=5)
    assert (res.token, res.refresh_token) == ("tok", "rt")
    for k, v in {"client_id": "auth0", "connection": "github", "audience": DEFAULT_AUDIENCE + ",https://console-api.enforce.dev",
                 "create_refresh_token": "true", "refresh_token_rotation": "true",
                 "skip_registration": "true", "exit": "redirect"}.items():
        assert seen[k] == v, k
    assert seen["redirect"].startswith("http://localhost:") and seen["redirect"].endswith("/callback?token=true&error=true")
    body, final = check[0]
    assert b"login complete" in body and "token=" not in final  # tokens stripped from the URL


def test_browser_login_errors_and_timeout():
    with pytest.raises(AccountNotFoundError):
        browser_login(open_url=complete({"status_code": "404"}), out=io.StringIO(), timeout=5)
    with pytest.raises(Exception, match="boom"):
        browser_login(open_url=complete({"status_code": "500", "error": "boom"}), out=io.StringIO(), timeout=5)
    out = io.StringIO()
    with pytest.raises(TimeoutError):
        browser_login(open_url=lambda u: False, out=out, timeout=0.2)
    assert "https://issuer.enforce.dev/oauth?" in out.getvalue()  # printed when no browser opens


def test_login_server_ignores_stray_requests():
    s = _LoginServer()
    try:
        base = s.callback_url.split("/callback")[0].replace("localhost", "127.0.0.1")
        for method, path, code in [("GET", "/", 404), ("POST", "/callback?token=x", 405), ("GET", "/other", 404)]:
            req = urllib.request.Request(base + path, method=method, data=b"" if method == "POST" else None)
            with pytest.raises(urllib.error.HTTPError) as e:
                urllib.request.urlopen(req, timeout=5)
            assert e.value.code == code
        assert not s.done.is_set()
        for tok in ("a", "b"):  # the first callback wins
            urllib.request.urlopen(f"{base}/callback?token={tok}", timeout=5).read()
        assert s.result.token == "a"
    finally:
        s.close()


def test_token_source_logs_in_refreshes_rotates_and_relogs(issuer, tmp_path):
    store = FileLoginStore(str(tmp_path / "sub" / "login.json"))
    logins = []

    def open_url(u):
        logins.append(u)
        with issuer.lock:
            issuer.valid.add("rt-login")
        # The login's own token is for another audience, so a refresh follows.
        return complete({"token": fake_jwt("https://console-api.enforce.dev"), "refresh_token": "rt-login"})(u)

    src = BrowserLoginToken(issuer=issuer.url, store=store, login_options={"open_url": open_url, "out": io.StringIO()})
    tok = src.token()
    assert _has_audience(tok, DEFAULT_AUDIENCE) and len(logins) == 1 and issuer.last_aud == DEFAULT_AUDIENCE
    assert store.load(issuer.url) == "rt-1"
    assert stat.S_IMODE(os.stat(store.path).st_mode) == 0o600
    assert stat.S_IMODE(os.stat(os.path.dirname(store.path)).st_mode) == 0o700

    src.token()  # saved login: no browser, rotated again
    assert store.load(issuer.url) == "rt-2" and len(logins) == 1

    with issuer.lock:
        issuer.valid.clear()  # revoked
    with pytest.raises(LoginRequiredError):
        BrowserLoginToken(issuer=issuer.url, store=store, no_browser=True).token()
    assert store.load(issuer.url) == ""  # the revoked login is dropped
    src.token()
    assert len(logins) == 2


def test_concurrent_refreshes_never_reuse_a_spent_token(issuer, tmp_path):
    issuer.valid.add("rt-0")
    path = str(tmp_path / "login.json")
    FileLoginStore(path).save(issuer.url, "rt-0")
    with futures.ThreadPoolExecutor(8) as ex:
        list(ex.map(lambda _: BrowserLoginToken(issuer=issuer.url, store=FileLoginStore(path), no_browser=True).token(),
                    range(8)))
    assert FileLoginStore(path).load(issuer.url) == "rt-8"


def test_stale_lock_logout_and_go_compatible_file(tmp_path):
    store = FileLoginStore(str(tmp_path / "login.json"))
    lock = store.path + ".lock"
    with open(lock, "w") as f:
        f.write("1\n")
    old = time.time() - 180
    os.utime(lock, (old, old))
    with store.lock(timeout=1):
        pass
    store.save("", "rt")
    store.save("https://other.example/", "rt2")
    v = json.load(open(store.path))
    assert v["version"] == 1 and v["logins"]["https://issuer.enforce.dev"]["refresh_token"] == "rt"
    assert v["logins"]["https://issuer.enforce.dev"]["updated"].endswith("Z")
    logout(store)
    assert store.load("") == "" and store.load("https://other.example") == "rt2"


def test_refresh_login_sends_audience_as_query(issuer):
    issuer.valid.add("r")
    tok, nxt = refresh_login("r", issuer.url)
    assert _has_audience(tok, DEFAULT_AUDIENCE) and nxt == "rt-1"
    with pytest.raises(LoginExpiredError):
        refresh_login("r", issuer.url)


def test_expired_refresh_token_answered_400_is_a_login_expiry(issuer, tmp_path):
    with pytest.raises(LoginExpiredError):
        refresh_login("expired-1", issuer.url)
    store = FileLoginStore(str(tmp_path / "login.json"))
    store.save(issuer.url, "expired-1")
    with pytest.raises(LoginRequiredError):
        BrowserLoginToken(issuer=issuer.url, store=store, no_browser=True).token()
    assert store.load(issuer.url) == ""
