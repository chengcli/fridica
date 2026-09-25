import asyncio

from aiohttp.test_utils import TestClient, TestServer

from fridica.control.client import ControlError, DaemonUnavailable
from fridica.dashboard.server import create_app


class FakeClient:
    def __init__(self, error=None):
        self.calls = []
        self.error = error

    async def request(self, method, path, body=None):
        self.calls.append((method, path, body))
        if self.error:
            raise self.error
        return {"ok": True, "path": path}


def scenario(config, body, client=None):
    fake = client or FakeClient()

    async def run():
        async with TestClient(TestServer(create_app(config, "secret-key", fake))) as http:
            await body(http)
    asyncio.run(run())
    return fake


def local(http, **headers):
    return {"Host": f"127.0.0.1:{http.port}", **headers}


def test_static_files_and_security_headers(config):
    async def body(http):
        page = await http.get("/", headers=local(http))
        assert page.status == 200 and "Fridica" in await page.text()
        assert "default-src 'self'" in page.headers["Content-Security-Policy"]
        assert (await http.get("/app.js", headers=local(http))).status == 200
        assert (await http.get("/secrets.txt", headers=local(http))).status == 404
        assert (await http.get("/", headers={"Host": "evil.example"})).status == 403
        assert (await http.get("/", headers=local(http, Origin="http://evil.example"))).status == 403
    scenario(config, body)


def test_api_requires_the_key_and_an_allowed_route(config):
    async def body(http):
        assert (await http.get("/api/status", headers=local(http))).status == 401
        auth = local(http, Authorization="Bearer secret-key")
        response = await http.get("/api/threads?control=active", headers=auth)
        assert response.status == 200 and (await response.json())["path"] == "/threads?control=active"
        assert (await http.get("/api/../../etc", headers=auth)).status == 404
        assert (await http.delete("/api/threads/x", headers=auth)).status == 404
        post = await http.post("/api/approvals/a1", json={"decision": "once"},
                               headers={**auth, "Origin": f"http://127.0.0.1:{http.port}"})
        assert post.status == 200
        assert (await http.post("/api/approvals/a1", data="x", headers=auth)).status == 415
    fake = scenario(config, body)
    assert ("POST", "/approvals/a1", {"decision": "once"}) in fake.calls


def test_daemon_errors_are_passed_through(config):
    async def offline(http):
        response = await http.get("/api/status", headers=local(http, Authorization="Bearer secret-key"))
        assert response.status == 503 and (await response.json())["offline"]
    scenario(config, offline, FakeClient(DaemonUnavailable("fridica is not running")))

    async def conflict(http):
        response = await http.get("/api/status", headers=local(http, Authorization="Bearer secret-key"))
        assert response.status == 409
    scenario(config, conflict, FakeClient(ControlError(409, "already decided")))
