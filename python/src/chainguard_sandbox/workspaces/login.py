"""Browser login, without chainctl.

A port of the Go library's login.go, which is adapted from
chainguard.dev/sdk/auth/login (Copyright 2023 Chainguard, Inc., Apache-2.0):
a callback server on a random localhost port, and the issuer's /oauth page
opened in the browser with redirect=<callback>. The issuer redirects back with
?token=...&refresh_token=.... The parameters match chainctl 0.2.398's.

The saved login (a rotating refresh token) lives in the same file the Go and
TypeScript libraries use, so one login serves all three.
"""

from __future__ import annotations

import base64
import contextlib
import datetime as _dt
import json
import os
import sys
import tempfile
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
import webbrowser
from dataclasses import dataclass, field
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from typing import Callable, Iterator, Protocol, TextIO

from chainguard_sandbox.workspaces.auth import DEFAULT_AUDIENCE, DEFAULT_ISSUER, AuthError, _urlopen


class AccountNotFoundError(AuthError):
    """No Chainguard account for the person who logged in (register first)."""


class LoginExpiredError(AuthError):
    """The saved login was refused; log in again."""


class LoginRequiredError(AuthError):
    """No saved login, and the token source may not open a browser."""


@dataclass
class LoginResult:
    token: str
    refresh_token: str = ""


def browser_login(
    issuer: str = DEFAULT_ISSUER,
    audience: list[str] | None = None,
    connection: str = "",
    client_id: str = "",
    identity_provider: str = "",
    org_name: str = "",
    identity: str = "",
    refresh_token: bool = True,
    open_url: Callable[[str], bool] | None = None,
    out: TextIO | None = None,
    timeout: float = 600.0,
) -> LoginResult:
    """Open the issuer's login page in a browser and wait (up to timeout
    seconds) for the redirect back to a localhost callback.

    connection preselects a social login ("github", "google-oauth2",
    "gitlab"); identity_provider / org_name use an organization's custom
    identity provider instead. open_url returns False (or raises) to have the
    URL printed instead; it defaults to webbrowser.open."""
    if identity_provider and org_name:
        raise ValueError("set at most one of identity_provider and org_name")
    out = out or sys.stderr
    open_url = open_url or webbrowser.open
    srv = _LoginServer()
    try:
        p: list[tuple[str, str]] = []
        if identity_provider:
            p.append(("idp_id", identity_provider))
        elif org_name:
            p.append(("idp_id", org_name))  # the issuer takes a verified org name here too
        else:
            p.append(("client_id", client_id or "auth0"))
            if connection:
                p.append(("connection", connection))
        p.append(("audience", ",".join(audience or default_login_audiences())))
        if refresh_token:
            p += [("create_refresh_token", "true"), ("refresh_token_rotation", "true")]
        if identity:
            p.append(("identity", identity))
        p += [("skip_registration", "true"), ("exit", "redirect"), ("redirect", srv.callback_url)]
        url = f"{issuer.rstrip('/')}/oauth?{urllib.parse.urlencode(sorted(p))}"
        try:
            opened = open_url(url)
        except Exception:
            opened = False
        if opened is False:
            print(f"Open this URL in a browser to log in to Chainguard:\n  {url}", file=out)
        else:
            print(f"Opened a browser to log in to Chainguard. If it didn't open, visit:\n  {url}", file=out)
        if not srv.done.wait(timeout):
            raise TimeoutError(f"login: no answer from the browser within {timeout:g}s")
        if srv.error:
            raise srv.error
        # Keep serving until the browser has loaded the success page.
        srv.page_served.wait(5)
        return srv.result  # type: ignore[return-value]
    finally:
        srv.close()


_DONE_PAGE = (b'<!doctype html><html><head><meta charset="utf-8"><title>Login complete</title>'
              b"<style>:root{color-scheme:light dark;font-family:system-ui,sans-serif}"
              b"body{display:grid;place-items:center;height:100vh;margin:0}</style></head>"
              b"<body><div><h1>Chainguard login complete</h1>"
              b"<p>You can close this tab and return to your program.</p></div></body></html>")


class _LoginServer:
    def __init__(self):
        self.result: LoginResult | None = None
        self.error: Exception | None = None
        self.done = threading.Event()
        self.page_served = threading.Event()
        self._lock = threading.Lock()
        outer = self

        class Handler(BaseHTTPRequestHandler):
            def log_message(self, format, *args):  # quiet
                pass

            def _send(self, code: int, body: bytes, ctype: str = "text/plain; charset=utf-8", headers=()):
                self.send_response(code)
                self.send_header("Cache-Control", "no-store")
                self.send_header("Referrer-Policy", "no-referrer")
                self.send_header("Content-Type", ctype)
                self.send_header("Content-Length", str(len(body)))
                for k, v in headers:
                    self.send_header(k, v)
                self.end_headers()
                self.wfile.write(body)

            def do_POST(self):
                self._send(405, b"method not allowed")

            do_PUT = do_DELETE = do_PATCH = do_POST

            def do_GET(self):
                u = urllib.parse.urlsplit(self.path)
                if u.path == "/callback":
                    q = dict(urllib.parse.parse_qsl(u.query))
                    tok = q.get("token", "")
                    if not tok:
                        sc = q.get("status_code", "")
                        code = int(sc) if sc.isdigit() else 0
                        # Respond first, then report: browser_login closes the
                        # server once it has the answer.
                        if code == 404:
                            self._send(404, b"No Chainguard account was found for this login.")
                            return outer._finish(error=AccountNotFoundError(
                                "login: no Chainguard account for this login (sign up for Workspaces at "
                                "https://console.chainguard.dev/workspaces/signup, or register with `chainctl auth login`)"))
                        msg = q.get("error") or "login failed"
                        self._send(400, f"Login failed: {msg}".encode())
                        return outer._finish(error=AuthError(f"login: the issuer reported an error (status {code}): {msg}"))
                    outer._finish(result=LoginResult(tok, q.get("refresh_token", "")))
                    # Redirect so the tokens leave the address bar and history entry.
                    return self._send(303, b"", headers=[("Location", "/")])
                if u.path == "/":
                    with outer._lock:
                        ok = outer.result is not None
                    if not ok:
                        return self._send(404, b"not found")
                    self._send(200, _DONE_PAGE, "text/html; charset=utf-8")
                    outer.page_served.set()
                    return
                self._send(404, b"not found")

        self._httpd = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self._httpd.daemon_threads = True
        port = self._httpd.server_address[1]
        # The issuer only redirects to localhost; the host name is what chainctl sends.
        self.callback_url = f"http://localhost:{port}/callback?token=true&error=true"
        threading.Thread(target=self._httpd.serve_forever, name="chainguard-sandbox-login", daemon=True).start()

    def _finish(self, result: LoginResult | None = None, error: Exception | None = None):
        with self._lock:
            if self.done.is_set():
                return  # the first answer wins
            self.result, self.error = result, error
            self.done.set()

    def close(self):
        self._httpd.shutdown()
        self._httpd.server_close()


# --- refresh tokens -------------------------------------------------------------


#: Chainguard's account (IAM) API, used to list your organizations.
PLATFORM_AUDIENCE = "https://console-api.enforce.dev"


def default_login_audiences() -> list[str]:
    """What a browser login asks for: the sessions service and the account API.
    $CHAINGUARD_SANDBOX_LOGIN_AUDIENCES (comma-separated) overrides it; set it to the sessions
    audience alone to keep logins from reaching the account API."""
    v = os.environ.get("CHAINGUARD_SANDBOX_LOGIN_AUDIENCES", "").strip()
    if v:
        return [a.strip() for a in v.split(",") if a.strip()]
    return [DEFAULT_AUDIENCE, PLATFORM_AUDIENCE]


def _refresh_claims(rt: str) -> tuple[float | None, list[str]]:
    """A refresh token's (unverified) expiry and audiences; it is base64 JSON."""
    try:
        c = json.loads(base64.b64decode(rt + "=" * (-len(rt) % 4)))
        aud = c.get("aud") or []
        return float(c["exp"]), [aud] if isinstance(aud, str) else list(aud)
    except (ValueError, KeyError, TypeError):
        return None, []


def refresh_login(refresh_token: str, issuer: str = DEFAULT_ISSUER, audience: str = DEFAULT_AUDIENCE,
                  timeout: float = 30.0) -> tuple[str, str]:
    """Trade a refresh token for a token for audience. Returns (token,
    new_refresh_token); the second is "" if the issuer didn't rotate it.
    SecurityTokenService/ExchangeRefreshToken over its HTTP binding, which
    takes the audience as a query parameter."""
    q = urllib.parse.urlencode({"aud": audience})
    req = urllib.request.Request(f"{issuer.rstrip('/')}/sts/exchange_refresh_token?{q}", method="POST", data=b"",
                                 headers={"Authorization": f"Bearer {refresh_token}", "Accept": "application/json"})
    try:
        with _urlopen(req, timeout) as resp:  # redirects refused: the token is never re-sent elsewhere
            body = resp.read(1 << 20)
    except urllib.error.HTTPError as e:
        raw = e.read(1 << 20) if e.fp else b""
        msg = raw.decode(errors="replace").strip()
        with contextlib.suppress(ValueError):
            v = json.loads(raw)
            if isinstance(v, dict) and v.get("message"):
                msg = str(v["message"])
        # The issuer refuses a spent/expired refresh token with 401/403, or with
        # 400 "invalid refresh token: refresh token expired at …" (observed).
        refused = e.code in (401, 403) or (e.code == 400 and "refresh token" in msg.lower())
        cls = LoginExpiredError if refused else AuthError
        raise cls(f"refreshing the login: HTTP {e.code}: {msg}") from e
    except urllib.error.URLError as e:
        raise AuthError(f"refreshing the login: {e.reason}") from e
    try:
        v = json.loads(body)
    except ValueError as e:
        raise AuthError("refreshing the login: response is not JSON") from e
    tok = ((v.get("token") or {}).get("token") if isinstance(v, dict) else "") or ""
    if not isinstance(tok, str) or not tok:
        raise AuthError("refreshing the login: response has no token")
    rt = v.get("refreshToken") or v.get("refresh_token") or {}
    return tok, (rt.get("token", "") if isinstance(rt, dict) else "") or ""


# --- the saved login --------------------------------------------------------------


def default_login_file() -> str:
    """<user config dir>/chainguard-sandbox/login.json, the same file Go's
    os.UserConfigDir() gives (macOS: ~/Library/Application Support, Linux:
    $XDG_CONFIG_HOME or ~/.config, Windows: %AppData%). $CHAINGUARD_SANDBOX_LOGIN_FILE overrides it."""
    if p := os.environ.get("CHAINGUARD_SANDBOX_LOGIN_FILE"):
        return p
    if sys.platform == "darwin":
        base = os.path.expanduser("~/Library/Application Support")
    elif os.name == "nt":
        base = os.environ.get("AppData") or os.path.expanduser("~")
    else:
        base = os.environ.get("XDG_CONFIG_HOME") or os.path.expanduser("~/.config")
    return os.path.join(base, "chainguard-sandbox", "login.json")


def _norm_issuer(s: str) -> str:
    return (s or DEFAULT_ISSUER).rstrip("/")


class LoginStore(Protocol):
    def load(self, issuer: str) -> str: ...
    def save(self, issuer: str, refresh_token: str) -> None: ...  # "" deletes
    def lock(self, timeout: float = 30.0) -> contextlib.AbstractContextManager: ...


@dataclass
class FileLoginStore:
    """Logins in a JSON file (0600, in a 0700 directory), keyed by issuer."""

    path: str = field(default_factory=default_login_file)

    def _read(self) -> dict:
        try:
            with open(self.path) as f:
                v = json.load(f)
        except FileNotFoundError:
            return {"version": 1, "logins": {}}
        if not isinstance(v, dict) or not isinstance(v.get("logins"), dict):
            raise AuthError(f"{self.path}: not a login file")
        return v

    def load(self, issuer: str = "") -> str:
        return str((self._read()["logins"].get(_norm_issuer(issuer)) or {}).get("refresh_token", ""))

    def save(self, issuer: str, refresh_token: str) -> None:
        try:
            v = self._read()
        except (AuthError, ValueError):
            v: dict = {"logins": {}}  # a corrupt file is replaced
        v["version"] = 1
        if refresh_token:
            v["logins"][_norm_issuer(issuer)] = {
                "refresh_token": refresh_token,
                "updated": _dt.datetime.now(_dt.timezone.utc).strftime("%Y-%m-%dT%H:%M:%S.%fZ"),
            }
        else:
            v["logins"].pop(_norm_issuer(issuer), None)
        d = os.path.dirname(self.path)
        os.makedirs(d, mode=0o700, exist_ok=True)
        fd, tmp = tempfile.mkstemp(dir=d, prefix=".login-", suffix=".json")  # created 0600
        try:
            with os.fdopen(fd, "w") as f:
                json.dump(v, f, indent=2)
                f.write("\n")
            os.replace(tmp, self.path)
        except BaseException:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(tmp)
            raise

    @contextlib.contextmanager
    def lock(self, timeout: float = 30.0) -> Iterator[None]:
        """<file>.lock, created exclusively; older than two minutes counts as
        left over from a crashed process. Same scheme as Go and TypeScript."""
        p = self.path + ".lock"
        os.makedirs(os.path.dirname(p), mode=0o700, exist_ok=True)
        deadline = time.monotonic() + timeout
        while True:
            try:
                fd = os.open(p, os.O_CREAT | os.O_EXCL | os.O_WRONLY, 0o600)
                os.write(fd, f"{os.getpid()}\n".encode())
                os.close(fd)
                break
            except FileExistsError:
                with contextlib.suppress(FileNotFoundError):
                    if time.time() - os.stat(p).st_mtime > 120:
                        os.unlink(p)
                        continue
                if time.monotonic() > deadline:
                    raise TimeoutError(f"waiting for {p}")
                time.sleep(0.1)
        try:
            yield
        finally:
            with contextlib.suppress(FileNotFoundError):
                os.unlink(p)


def logout(store: LoginStore | None = None, issuer: str = "") -> None:
    """Forget the saved login for issuer."""
    (store or FileLoginStore()).save(issuer, "")


# --- the token source ------------------------------------------------------------------


def token_claims(tok: str) -> dict:
    """A JWT's unverified claims (for display)."""
    parts = tok.split(".")
    if len(parts) != 3:
        raise ValueError("not a JWT")
    v = json.loads(base64.urlsafe_b64decode(parts[1] + "=" * (-len(parts[1]) % 4)))
    if not isinstance(v, dict):
        raise ValueError("JWT claims are not an object")
    return v


def _has_audience(tok: str, aud: str) -> bool:
    try:
        a = token_claims(tok).get("aud")
    except ValueError:
        return False
    return aud == a if isinstance(a, str) else isinstance(a, list) and aud in a


@dataclass
class BrowserLoginToken:
    """A TokenSource backed by the library's own login: tokens are minted from
    the saved refresh token; when there is none (or it has expired) a browser
    login runs, unless no_browser is set. Wrap in Cached."""

    issuer: str = DEFAULT_ISSUER
    audience: str = DEFAULT_AUDIENCE
    connection: str = ""
    store: LoginStore = field(default_factory=FileLoginStore)
    no_browser: bool = False
    login_timeout: float = 600.0
    #: Extra keyword arguments for browser_login (identity_provider, open_url, out, ...).
    login_options: dict = field(default_factory=dict)

    def token(self) -> str:
        tok = self._from_saved()
        if tok:
            return tok
        if self.no_browser:
            raise LoginRequiredError("not logged in (run `chainguard-sandbox login`, or allow a browser login)")
        # No lock while the person logs in (that can take minutes).
        auds = default_login_audiences()
        if self.audience not in auds:
            auds.append(self.audience)
        res = browser_login(issuer=self.issuer, audience=auds, connection=self.connection,
                            timeout=self.login_timeout, **self.login_options)
        with self.store.lock():
            if res.refresh_token:
                self.store.save(self.issuer, res.refresh_token)
            if _has_audience(res.token, self.audience):
                return res.token
            if not res.refresh_token:
                raise AuthError(f"login: the token isn't for {self.audience} and there's no refresh token to mint one")
            tok, nrt = refresh_login(res.refresh_token, self.issuer, self.audience)
            if nrt:
                self.store.save(self.issuer, nrt)
            return tok

    def _from_saved(self) -> str:
        with self.store.lock():
            rt = self.store.load(self.issuer)
            if not rt:
                return ""
            try:
                tok, nrt = refresh_login(rt, self.issuer, self.audience)
            except LoginExpiredError:
                # Forget the login only if it really is unusable: expired, or
                # refused for an audience it covers. A login made without this
                # audience is still good for its own audiences, so it's kept.
                exp, auds = _refresh_claims(rt)
                if exp is not None and exp > time.time() and self.audience not in auds:
                    if self.no_browser:
                        raise LoginRequiredError(f"the saved login doesn't cover {self.audience}; log in again "
                                                 "(chainguard-sandbox login) to add it") from None
                    return ""  # a browser login asks for it, then replaces the saved one
                self.store.save(self.issuer, "")
                return ""
            if nrt and nrt != rt:
                self.store.save(self.issuer, nrt)
            return tok
