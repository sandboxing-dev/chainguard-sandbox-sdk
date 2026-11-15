"""Turning Workspaces on: status, terms and the enable call, against a local
stand-in for the platform API (no network). It checks only what this package
sends; it says nothing about the real service."""

from __future__ import annotations

import http.server
import json
import threading
import urllib.parse

import pytest

from chainguard_sandbox import TermsNotAcceptedError, enable_workspaces, workspaces_status
from chainguard_sandbox.workspaces import StaticToken
from chainguard_sandbox.workspaces.setup import ENABLE_TERMS, GITHUB_APP_INSTALL_URL, WORKSPACES_TERMS, PlatformError

ORG = "0123456789abcdef0123456789abcdef01234567"


class FakePlatform:
    def __init__(self):
        self.accepted: set[str] = set()
        self.enabled = False
        self.need_for_enable: list[str] = []  # documents the switch insists on beyond Workspaces' own
        self.posts: list[str] = []
        self.github: list[str] = ["already-linked"]  # linked GitHub accounts
        self.app_installed = True  # whether a link attempt succeeds
        self.link_body: dict = {}

    def handle(self, method: str, path: str, query: dict, body: dict, auth: str):
        if auth != "Bearer platform-token":
            return 401, {"code": 16, "message": "unauthenticated"}
        if path == f"/iam/v1/terms/{ORG}" and method == "GET":
            return 200, {"items": [{"docId": i} for i in sorted(self.accepted)]}
        if path == f"/iam/v1/terms/{ORG}" and method == "POST":
            self.posts.append("terms:" + ",".join(body["documentIds"]))
            self.accepted |= set(body["documentIds"])
            return 200, {"group": ORG, "documentIds": body["documentIds"]}
        if path == f"/guardener/v1alpha1/githubAssociations/{ORG}" and method == "GET":
            return 200, {"organizations": [{"organization": g, "installationId": "1"} for g in self.github]}
        if path == "/guardener/v1alpha1/githubAssociations:begin" and method == "GET":
            return 200, {"clientId": "client-1", "state": "signed-state"}
        if path == f"/guardener/v1alpha1/githubAssociations/{ORG}:link" and method == "POST":
            self.link_body = body
            self.posts.append("link")
            if not self.app_installed:
                return 400, {"code": 9, "message": "Precondition failed: the guardener GitHub App must be installed on the GitHub account (organization or user) before it can be linked"}
            self.github.append(body.get("organization") or "the-authorizing-user")
            return 200, {"organization": self.github[-1], "installationId": "1"}
        if path == f"/entitlements/v1alpha1/features/{ORG}:enableSandbox" and method == "POST":
            self.posts.append("enable")
            if not self.github:  # the service's real answer, as seen on 2026-11-15
                return 400, {"code": 9, "message": "Precondition failed: the organization must link a GitHub organization through the Chainguard GitHub App first"}
            missing = [{"id": i, "label": f"Label of {i}", "url": f"https://example.test/{i}"}
                       for i in [*WORKSPACES_TERMS, *self.need_for_enable] if i not in self.accepted]
            if missing:
                return 400, {"code": 9, "message": "terms not accepted", "details": [{"@type": "x/TermsNotAcceptedDetail", "missing": missing}]}
            self.enabled = True
            return 200, {"featureEntitlements": []}
        if path == "/entitlements/v2beta1/organizationEntitlements" and query.get("parent") == [ORG]:
            return 200, {"plan": {"id": "starter", "displayName": "Starter"}, "products": [{
                "product": "PRODUCT_SANDBOX_WORKSPACES", "state": "STATE_ACTIVE" if self.enabled else "STATE_NOT_INCLUDED",
                "limits": [{"displayName": "concurrent workspaces", "max": "3"}, {"displayName": "concurrent workspaces per user", "max": "1"}]}]}
        return 404, {"code": 5, "message": "no such route"}


@pytest.fixture()
def platform(monkeypatch):
    fake = FakePlatform()

    class Handler(http.server.BaseHTTPRequestHandler):
        def _serve(self):
            u = urllib.parse.urlparse(self.path)
            n = int(self.headers.get("Content-Length") or 0)
            body = json.loads(self.rfile.read(n) or b"{}") if n else {}
            status, out = fake.handle(self.command, urllib.parse.unquote(u.path), urllib.parse.parse_qs(u.query), body,
                                      self.headers.get("Authorization", ""))
            data = json.dumps(out).encode()
            self.send_response(status)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        do_GET = do_POST = _serve

        def log_message(self, *a):
            pass

    srv = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    threading.Thread(target=srv.serve_forever, daemon=True).start()
    monkeypatch.setenv("CHAINGUARD_SANDBOX_PLATFORM_URL", f"http://127.0.0.1:{srv.server_address[1]}")
    yield fake, {"instance": ORG, "token_source": StaticToken("platform-token")}
    srv.shutdown()


def test_status_and_enable(platform):
    fake, kw = platform
    st = workspaces_status(**kw)
    assert (st.enabled, st.ready, st.plan, st.max_sandboxes, st.max_sandboxes_per_user) == (False, False, "Starter", 3, 1)
    assert [d.id for d in st.missing_terms] == list(ENABLE_TERMS) and all(d.label and d.url for d in st.missing_terms)
    assert st.github_accounts == ["already-linked"]

    with pytest.raises(TermsNotAcceptedError) as e:  # terms are never accepted without being asked to
        enable_workspaces(**kw)
    assert len(e.value.missing) == len(ENABLE_TERMS) and fake.posts == []

    st = enable_workspaces(accept_terms=True, **kw)
    assert st.ready and fake.posts == ["terms:" + ",".join(ENABLE_TERMS), "enable"]

    fake.posts.clear()  # already on: nothing is sent
    assert enable_workspaces(accept_terms=True, **kw).ready and fake.posts == []


def test_enable_accepts_terms_the_switch_asks_for(platform):
    fake, kw = platform
    fake.need_for_enable = ["some-new-terms.v1"]  # a document this package doesn't know about yet
    fake.accepted |= set(ENABLE_TERMS)
    with pytest.raises(TermsNotAcceptedError) as e:
        enable_workspaces(**kw)
    assert [d.id for d in e.value.missing] == ["some-new-terms.v1"] and e.value.missing[0].url and not fake.enabled
    fake.posts.clear()
    assert enable_workspaces(accept_terms=True, **kw).ready
    assert fake.posts == ["enable", "terms:some-new-terms.v1", "enable"]


def test_enable_links_github_first(platform):
    """No GitHub account linked: enable authorizes with GitHub through a loopback
    redirect, links the account, and only then turns Workspaces on."""
    import io
    import socket
    import urllib.request

    from chainguard_sandbox import GitHubAppNotInstalledError, GitHubLinkRequiredError

    fake, kw = platform
    fake.github = []
    with pytest.raises(GitHubLinkRequiredError):  # without a browser it says what is needed
        enable_workspaces(accept_terms=True, open_browser=False, **kw)
    assert not fake.enabled

    with socket.socket() as s:
        s.bind(("127.0.0.1", 0))
        port = s.getsockname()[1]
    seen = {}

    def person_and_github(url):  # stands in for the person and GitHub: follow the redirect_uri
        q = {k: v[0] for k, v in urllib.parse.parse_qs(urllib.parse.urlparse(url).query).items()}
        seen.update(q)
        threading.Thread(target=lambda: urllib.request.urlopen(f"{q['redirect_uri']}?code=gh-code&state={q['state']}").read()).start()

    fake.app_installed = False
    with pytest.raises(GitHubAppNotInstalledError) as e:
        enable_workspaces(accept_terms=True, github_port=port, open_url=person_and_github, out=io.StringIO(), **kw)
    assert e.value.install_url == GITHUB_APP_INSTALL_URL and not fake.enabled
    assert (seen["client_id"], seen["state"], seen["code_challenge_method"], seen["prompt"]) == ("client-1", "signed-state", "S256", "select_account")
    assert seen["code_challenge"] and seen["redirect_uri"] == f"http://127.0.0.1:{port}/callback"
    assert fake.link_body["code"] == "gh-code" and fake.link_body["state"] == "signed-state"
    assert fake.link_body["codeVerifier"] and fake.link_body["redirectUri"] == seen["redirect_uri"]

    fake.app_installed = True  # the app is installed now: the next attempt links and turns it on
    fake.posts.clear()
    st = enable_workspaces(accept_terms=True, github_account="my-org", github_port=port, open_url=person_and_github, out=io.StringIO(), **kw)
    assert st.ready and st.github_accounts == ["my-org"] and fake.posts == ["link", "enable"]


def test_setup_errors_keep_the_service_message(platform):
    _, kw = platform
    with pytest.raises(PlatformError, match="unauthenticated") as e:
        workspaces_status(instance=ORG, token_source=StaticToken("wrong"))
    assert e.value.http_status == 401


def test_create_without_workspaces_is_a_clear_error():
    import grpc

    from chainguard_sandbox import WorkspacesNotEnabledError
    from chainguard_sandbox._sandbox import _wrap

    class Refused(grpc.RpcError):
        def __init__(self, msg):
            self._msg = msg

        def code(self):
            return grpc.StatusCode.PERMISSION_DENIED

        def details(self):
            return self._msg

    e = _wrap(Refused("this organization is not entitled to Chainguard Workspaces; an organization owner can turn it on with chainctl develop init --parent X"))
    assert isinstance(e, WorkspacesNotEnabledError) and "not entitled" in str(e) and "enable_workspaces" in str(e)
    assert not isinstance(_wrap(Refused("caller lacks sessions.create")), WorkspacesNotEnabledError)
