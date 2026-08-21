#!/usr/bin/env python3
"""GitHub-shaped stub for the fix path.

Only two endpoints matter: posting a comment and opening a pull request. It
records every request so the test can assert on what was actually sent —
notably that the pull request is a draft and that its head is not the default
branch.

Usage: bench/stub_github.py [port]
"""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7998
SEEN = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")
        SEEN.append({"path": self.path, "body": req,
                     "auth": self.headers.get("authorization", "")})
        if self.path.endswith("/pulls"):
            num = len([s for s in SEEN if s["path"].endswith("/pulls")])
            return self._json({"html_url": f"https://github.example/pull/{num}",
                               "number": num, "draft": req.get("draft")}, 201)
        return self._json({"html_url": "https://github.example/comment/1", "id": 1}, 201)

    def do_GET(self):
        if self.path == "/_seen":
            return self._json({"calls": SEEN})
        # The backlog a backfill run works through.
        if "/issues?" in self.path or self.path.endswith("/issues"):
            SEEN.append({"path": self.path, "body": None, "auth": ""})
            return self._json([
                {"number": 900, "title": "send_file leaks a descriptor",
                 "body": "The descriptor is never closed on a large download."},
                {"number": 901, "title": "url_for wrong behind a proxy",
                 "body": "SERVER_NAME is ignored when X-Forwarded-Host is set."},
                # A pull request arrives in this list too and is a different
                # thing; the caller must drop it.
                {"number": 902, "title": "Bump werkzeug", "body": "",
                 "pull_request": {"url": "https://example/pull/902"}},
            ])
        self._json({"ok": True})

    def _json(self, obj, code=200):
        b = json.dumps(obj).encode()
        self.send_response(code)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(b)))
        self.end_headers()
        self.wfile.write(b)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
