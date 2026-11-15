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
from chainguard_sandbox.workspaces.setup import WORKSPACES_TERMS, PlatformError

ORG = "0123456789abcdef0123456789abcdef01234567"


class FakePlatform:
    def __init__(self):
        self.accepted: set[str] = set()
        self.enabled = False
        self.need_for_enable: list[str] = []  # documents the switch insists on beyond Workspaces' own
        self.posts: list[str] = []

    def handle(self, method: str, path: str, query: dict, body: dict, auth: str):
        if auth != "Bearer platform-token":
            return 401, {"code": 16, "message": "unauthenticated"}
        if path == f"/iam/v1/terms/{ORG}" and method == "GET":
            return 200, {"items": [{"docId": i} for i in sorted(self.accepted)]}
        if path == f"/iam/v1/terms/{ORG}" and method == "POST":
            self.posts.append("terms:" + ",".join(body["documentIds"]))
            self.accepted |= set(body["documentIds"])
            return 200, {"group": ORG, "documentIds": body["documentIds"]}
        if path == f"/entitlements/v1alpha1/features/{ORG}:enableSandbox" and method == "POST":
            self.posts.append("enable")
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
    assert [d.id for d in st.missing_terms] == list(WORKSPACES_TERMS) and all(d.label and d.url for d in st.missing_terms)

    with pytest.raises(TermsNotAcceptedError) as e:  # terms are never accepted without being asked to
        enable_workspaces(**kw)
    assert len(e.value.missing) == 2 and fake.posts == []

    st = enable_workspaces(accept_terms=True, **kw)
    assert st.ready and fake.posts == ["terms:secure-platform-terms.v1,msla.v1", "enable"]

    fake.posts.clear()  # already on: nothing is sent
    assert enable_workspaces(accept_terms=True, **kw).ready and fake.posts == []


def test_enable_accepts_terms_the_switch_asks_for(platform):
    fake, kw = platform
    fake.need_for_enable = ["code-analysis-terms.v1"]  # the switch also turns on Checks
    fake.accepted |= set(WORKSPACES_TERMS)
    with pytest.raises(TermsNotAcceptedError) as e:
        enable_workspaces(**kw)
    assert [d.id for d in e.value.missing] == ["code-analysis-terms.v1"] and e.value.missing[0].url and not fake.enabled
    fake.posts.clear()
    assert enable_workspaces(accept_terms=True, **kw).ready
    assert fake.posts == ["enable", "terms:code-analysis-terms.v1", "enable"]


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
