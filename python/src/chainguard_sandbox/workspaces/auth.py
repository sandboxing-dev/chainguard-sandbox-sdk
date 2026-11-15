"""Token sources for the sandbox plane (audience https://sessions.chainbox.dev)."""

from __future__ import annotations

import base64
import json
import os
import subprocess
import threading
import time
import urllib.error
import urllib.parse
import urllib.request
from dataclasses import dataclass, field
from typing import Callable, Protocol

DEFAULT_AUDIENCE = "https://sessions.chainbox.dev"
DEFAULT_ISSUER = "https://issuer.enforce.dev"
ENV_AUTH_TOKEN = "CHAINCTL_AUTH_TOKEN"


class TokenSource(Protocol):
    """Returns a Chainguard bearer token whose audience includes DEFAULT_AUDIENCE."""

    def token(self) -> str: ...


class AuthError(Exception):
    pass


class _NoRedirect(urllib.request.HTTPRedirectHandler):
    """Refuse redirects: urllib would re-send our Authorization header (an
    OIDC or Chainguard token) to whatever host the redirect names."""

    def redirect_request(self, req, fp, code, msg, headers, newurl):
        return None


def _urlopen(req: urllib.request.Request, timeout: float):
    return urllib.request.build_opener(_NoRedirect).open(req, timeout=timeout)


def _json_object(body: bytes, what: str) -> dict:
    try:
        v = json.loads(body)
    except ValueError as e:
        raise AuthError(f"{what}: response is not JSON") from e
    if not isinstance(v, dict):
        raise AuthError(f"{what}: response is not a JSON object")
    return v


@dataclass
class StaticToken:
    value: str

    def token(self) -> str:
        if not self.value:
            raise AuthError("empty token")
        return self.value


@dataclass
class ChainctlToken:
    """Mints tokens with `chainctl auth token --audience https://sessions.chainbox.dev`.

    Reuses chainctl's saved login (`chainctl auth login` / `chainctl develop init`).
    """

    path: str = "chainctl"
    audience: str = DEFAULT_AUDIENCE
    #: chainctl can block waiting for an interactive login; give up after this.
    timeout: float = 60.0

    def token(self) -> str:
        try:
            p = subprocess.run(
                [self.path, "auth", "token", "--audience", self.audience],
                capture_output=True, text=True, check=False, timeout=self.timeout,
                stdin=subprocess.DEVNULL,
            )
        except FileNotFoundError as e:
            raise AuthError(f"{self.path} not found: {e}") from e
        except subprocess.TimeoutExpired as e:
            raise AuthError(f"chainctl auth token timed out after {self.timeout:g}s "
                            f"(log in first: chainctl auth login --audience {self.audience})") from e
        if p.returncode != 0:
            raise AuthError(f"chainctl auth token --audience {self.audience}: exit {p.returncode}: {p.stderr.strip()}")
        tok = p.stdout.strip()
        if not tok:
            raise AuthError("chainctl auth token returned no token")
        return tok


def jwt_expiry(tok: str) -> float | None:
    """The unverified `exp` claim of a JWT, as a Unix timestamp."""
    parts = tok.split(".")
    if len(parts) != 3:
        return None
    try:
        payload = parts[1] + "=" * (-len(parts[1]) % 4)
        claims = json.loads(base64.urlsafe_b64decode(payload))
        if not isinstance(claims, dict):
            return None
        exp = claims.get("exp")
        return float(exp) if exp else None
    except (ValueError, TypeError):
        return None


@dataclass
class Cached:
    """Reuses a token until a minute before its JWT `exp` (5 minutes if unknown)."""

    src: TokenSource
    _tok: str = field(default="", init=False, repr=False)
    _exp: float = field(default=0.0, init=False, repr=False)
    _lock: threading.Lock = field(default_factory=threading.Lock, init=False, repr=False)

    def token(self) -> str:
        with self._lock:
            if self._tok and self._exp - time.time() > 60:
                return self._tok
            tok = self.src.token()
            self._tok, self._exp = tok, jwt_expiry(tok) or time.time() + 300
            return tok


def default_token_source() -> TokenSource:
    """$CHAINCTL_AUTH_TOKEN when set, otherwise the library's own login (a
    cached BrowserLoginToken): tokens are minted from the saved login, and a
    browser login runs when there is none. chainctl isn't used; set
    $CHAINGUARD_SANDBOX_AUTH=chainctl to use it instead. $CHAINGUARD_SANDBOX_SOCIAL_LOGIN preselects the
    social provider (github, google-oauth2, gitlab); $CHAINGUARD_SANDBOX_NO_BROWSER=1 makes
    a missing login an error instead of opening a browser."""
    if t := os.environ.get(ENV_AUTH_TOKEN):
        return StaticToken(t)
    if os.environ.get("CHAINGUARD_SANDBOX_AUTH") == "chainctl":
        return Cached(ChainctlToken())
    from chainguard_sandbox.workspaces.login import BrowserLoginToken

    return Cached(BrowserLoginToken(connection=os.environ.get("CHAINGUARD_SANDBOX_SOCIAL_LOGIN", ""),
                                    no_browser=os.environ.get("CHAINGUARD_SANDBOX_NO_BROWSER") == "1"))


@dataclass
class IdentityExchange:
    """For machines without chainctl (CI jobs, servers, pods).

    Trades a token from an external OIDC issuer (GitHub Actions, GitLab, a
    Kubernetes service account, ...) for a Chainguard token for the sessions
    audience, acting as a Chainguard *assumable identity* that trusts that
    issuer and subject. One-time setup by an organization owner:

        chainctl iam identities create <name> --parent <org> \\
          --identity-issuer <issuer URL> --subject <subject> --role sandbox.user

    Calls SecurityTokenService/Exchange over its HTTP binding (POST /sts/exchange).
    Wrap in Cached to reuse tokens.
    """

    identity: str
    id_token: Callable[[], str]
    issuer: str = DEFAULT_ISSUER
    audience: str = DEFAULT_AUDIENCE
    timeout: float = 30.0

    def token(self) -> str:
        if not self.identity or not self.id_token:
            raise AuthError("IdentityExchange needs identity and id_token")
        idt = self.id_token()
        q = urllib.parse.urlencode({"aud": self.audience, "identity": self.identity})
        req = urllib.request.Request(
            f"{self.issuer.rstrip('/')}/sts/exchange?{q}", method="POST", data=b"",
            headers={"Authorization": f"Bearer {idt}", "Accept": "application/json"},
        )
        try:
            with _urlopen(req, self.timeout) as resp:
                body = resp.read(1 << 20)
        except urllib.error.HTTPError as e:
            raw = e.read(1 << 20) if e.fp else b""
            msg = raw.decode(errors="replace").strip()
            try:
                v = json.loads(raw)
                if isinstance(v, dict) and v.get("message"):
                    msg = str(v["message"])
            except ValueError:
                pass
            raise AuthError(f"STS exchange for identity {self.identity}: HTTP {e.code}: {msg}") from e
        except urllib.error.URLError as e:
            raise AuthError(f"STS exchange: {e.reason}") from e
        tok = _json_object(body, "STS exchange").get("token", "")
        if not tok or not isinstance(tok, str):
            raise AuthError("STS exchange: response has no token")
        return tok


def github_actions_id_token(issuer: str = DEFAULT_ISSUER) -> Callable[[], str]:
    """A GitHub Actions OIDC token whose audience is the Chainguard issuer
    (the workflow needs `permissions: id-token: write`)."""

    def fetch() -> str:
        url, tok = os.environ.get("ACTIONS_ID_TOKEN_REQUEST_URL"), os.environ.get("ACTIONS_ID_TOKEN_REQUEST_TOKEN")
        if not url or not tok:
            raise AuthError("not in a GitHub Actions job with `permissions: id-token: write`")
        sep = "&" if "?" in url else "?"
        req = urllib.request.Request(f"{url}{sep}{urllib.parse.urlencode({'audience': issuer})}",
                                     headers={"Authorization": f"Bearer {tok}"})
        try:
            with _urlopen(req, 30) as resp:
                body = resp.read(1 << 20)
        except urllib.error.URLError as e:
            raise AuthError(f"GitHub Actions OIDC token request: {getattr(e, 'reason', e)}") from e
        value = _json_object(body, "GitHub Actions OIDC token request").get("value", "")
        if not value or not isinstance(value, str):
            raise AuthError("GitHub Actions OIDC token request returned no value")
        return value

    return fetch


def env_id_token(name: str) -> Callable[[], str]:
    """Reads the OIDC token from an environment variable (e.g. GitLab `id_tokens:`)."""

    def read() -> str:
        if v := os.environ.get(name, "").strip():
            return v
        raise AuthError(f"${name} is empty")

    return read


def file_id_token(path: str) -> Callable[[], str]:
    """Reads the OIDC token from a file on every call (e.g. a projected
    Kubernetes service-account token)."""

    def read() -> str:
        with open(path) as f:
            if v := f.read().strip():
                return v
        raise AuthError(f"{path} is empty")

    return read
