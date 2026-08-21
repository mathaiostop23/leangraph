#!/usr/bin/env python3
"""GitLab-shaped stub.

Two endpoints: the member lookup the author gate depends on, and posting a note.
The member lookup is the interesting one — GitLab has no `author_association`,
so the gate that keeps a stranger from spending the owner's budget is an API
call, and a test that does not exercise it is not testing the gate.

`STUB_ACCESS_LEVEL` sets what the member lookup reports; 0 means "not a member"
and answers 404, which is what GitLab does.

Usage: bench/stub_gitlab.py [port]
"""
import json, os, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7996
SEEN = []
ACCESS = int(os.environ.get("STUB_ACCESS_LEVEL", "40"))


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")
        SEEN.append({"path": self.path, "body": req,
                     "token": self.headers.get("private-token", "")})
        return self._json({"id": len(SEEN), "body": req.get("body", "")}, 201)

    def do_GET(self):
        if self.path == "/_seen":
            return self._json({"calls": SEEN})
        if "/members/all/" in self.path:
            SEEN.append({"path": self.path, "body": None,
                         "token": self.headers.get("private-token", "")})
            if ACCESS <= 0:
                return self._json({"message": "404 Not found"}, 404)
            return self._json({"id": 1, "username": "someone",
                               "access_level": ACCESS})
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
