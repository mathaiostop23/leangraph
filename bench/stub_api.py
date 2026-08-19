#!/usr/bin/env python3
"""Anthropic-shaped stub, so the whole agent path can be exercised for free.

It answers in the real response shape — including the usage fields the cost
ledger reads — and echoes back what it was asked, which is how the prompt-safety
assertions below can check what actually reached the model.

Usage: bench/stub_api.py [port]
"""
import json, sys
from http.server import BaseHTTPRequestHandler, HTTPServer

PORT = int(sys.argv[1]) if len(sys.argv) > 1 else 7999
SEEN = []


class Handler(BaseHTTPRequestHandler):
    def log_message(self, *_):
        pass

    def do_POST(self):
        n = int(self.headers.get("content-length", 0))
        req = json.loads(self.rfile.read(n) or b"{}")

        if self.path == "/_seen":
            return self._json({"calls": SEEN})

        SEEN.append(req)
        model = req.get("model", "")
        structured = "output_config" in req and "format" in req.get("output_config", {})

        if structured:
            text = json.dumps({
                "kind": "bug",
                "summary": "get_or_create raises IntegrityError across databases",
                "symbols": ["get_or_create", "IntegrityError", "QuerySet"],
            })
            usage = {"input_tokens": 210, "output_tokens": 48,
                     "cache_read_input_tokens": 0, "cache_creation_input_tokens": 0}
        else:
            text = ("The failure is in `QuerySet.get_or_create`, which does not route the "
                    "lookup and the create through the same database alias.\n\n"
                    "A fix belongs in `django/db/models/query.py`.")
            # A cache hit on the repo preamble, which is the whole point of the
            # breakpoint placement.
            usage = {"input_tokens": 900, "output_tokens": 260,
                     "cache_read_input_tokens": 8400, "cache_creation_input_tokens": 0}

        self._json({
            "id": "msg_stub", "type": "message", "role": "assistant",
            "model": model, "stop_reason": "end_turn",
            "content": [{"type": "text", "text": text}],
            "usage": usage,
        })

    def do_GET(self):
        if self.path == "/_seen":
            return self._json({"calls": SEEN})
        self._json({"ok": True})

    def _json(self, obj):
        body = json.dumps(obj).encode()
        self.send_response(200)
        self.send_header("content-type", "application/json")
        self.send_header("content-length", str(len(body)))
        self.end_headers()
        self.wfile.write(body)


if __name__ == "__main__":
    HTTPServer(("127.0.0.1", PORT), Handler).serve_forever()
