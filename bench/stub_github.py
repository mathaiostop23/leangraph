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
