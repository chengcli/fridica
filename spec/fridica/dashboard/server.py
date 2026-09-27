"""The dashboard server: static files plus an authenticated proxy to the daemon's control socket.

It listens on 127.0.0.1 only, rejects cross-origin requests, and requires the
per-run key printed in the terminal for every API call, because the API can
approve commands on your machines. It holds no state of its own.
"""

from __future__ import annotations

from importlib.resources import files
import logging
import re
import secrets

from aiohttp import web

from ..config.schema import Config
from ..control.client import ControlClient, ControlError, DaemonUnavailable

logger = logging.getLogger(__name__)
STATIC = {"": ("index.html", "text/html"), "index.html": ("index.html", "text/html"),
          "app.js": ("app.js", "text/javascript"), "app.css": ("app.css", "text/css"),
          "fridica-logo.png": ("fridica-logo.png", "image/png")}
ALLOWED = [
    ("GET", r"/status"), ("GET", r"/threads"), ("GET", r"/threads/[^/]+"),
    ("POST", r"/threads/[^/]+/(resume|pause|close|archive|restore|clean|instruct)"),
    ("GET", r"/workers"), ("POST", r"/workers/[^/]+/(interrupt|stop)"), ("GET", r"/approvals"),
    ("POST", r"/approvals/[^/]+"), ("GET", r"/machines"), ("GET", r"/outbox"), ("POST", r"/outbox/\d+/retry"),
    ("GET", r"/activity"), ("GET", r"/jobs"), ("GET", r"/config"), ("GET", r"/attention/threads"),
    ("PATCH", r"/config/limits"), ("PATCH", r"/config/parent"),
]


def create_app(config: Config, key: str, client: ControlClient | None = None) -> web.Application:
    client = client or ControlClient(config.state.control_socket)

    @web.middleware
    async def guard(request, handler):
        host = request.host
        if (not re.fullmatch(r"(?:127\.0\.0\.1|localhost)(?::\d+)?", host)
                or request.headers.get("Origin", f"http://{host}") != f"http://{host}"
                or request.headers.get("Sec-Fetch-Site") == "cross-site"):
            raise web.HTTPForbidden(text="the dashboard only accepts same-origin local requests")
        response = await handler(request)
        response.headers.update({
            "Cache-Control": "no-store", "X-Content-Type-Options": "nosniff", "Referrer-Policy": "no-referrer",
            "Content-Security-Policy": "default-src 'self'; object-src 'none'; frame-ancestors 'none'; base-uri 'none'"})
        return response

    async def static(request):
        name = request.match_info.get("name", "")
        if name not in STATIC:
            raise web.HTTPNotFound()
        filename, content_type = STATIC[name]
        body = files("fridica.dashboard").joinpath("static", filename).read_bytes()
        return web.Response(body=body, content_type=content_type)

    async def proxy(request):
        token = request.headers.get("Authorization", "").removeprefix("Bearer ")
        if not secrets.compare_digest(token, key):
            raise web.HTTPUnauthorized(text="enter the key printed by `fridica dashboard`")
        path = "/" + request.match_info["path"]
        if not any(method == request.method and re.fullmatch(pattern, path) for method, pattern in ALLOWED):
            raise web.HTTPNotFound()
        body = None
        if request.method != "GET":
            if request.content_type != "application/json":
                raise web.HTTPUnsupportedMediaType(text="send JSON")
            body = await request.json() if request.can_read_body else {}
        query = ("?" + request.query_string) if request.query_string else ""
        try:
            return web.json_response(await client.request(request.method, path + query, body))
        except DaemonUnavailable as error:
            return web.json_response({"error": str(error), "offline": True}, status=503)
        except ControlError as error:
            return web.json_response({"error": str(error)}, status=error.status)

    app = web.Application(middlewares=[guard])
    app.router.add_route("*", "/api/{path:.+}", proxy)
    app.router.add_get("/", static)
    app.router.add_get("/{name}", static)
    return app


def run(config: Config, *, port: int = 8765) -> None:
    key = secrets.token_urlsafe(24)
    print(f"Fridica dashboard: http://127.0.0.1:{port}/#key={key}")
    print("The key unlocks approvals and thread controls; keep it private. Ctrl-C stops the dashboard.")
    web.run_app(create_app(config, key), host="127.0.0.1", port=port, print=None, access_log=None)
