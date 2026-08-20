#!/usr/bin/env python3
"""Webhook gate tests.

This endpoint is the security boundary of the whole product: anyone can open an
issue on a public repository, and its body ends up in front of a model holding
tools. Every gate below is a thing that must fail closed, so each one gets a
test that asserts it actually does.

Usage: bench/webhook_test.py [base_url] [secret]
"""
import hashlib, hmac, json, subprocess, sys, time, urllib.error, urllib.parse, urllib.request

BASE = sys.argv[1] if len(sys.argv) > 1 else "http://127.0.0.1:7788"
SECRET = (sys.argv[2] if len(sys.argv) > 2 else "test-secret").encode()

passed = failed = 0


def post(payload, event="issues", delivery=None, sign=True, secret=SECRET):
    body = json.dumps(payload).encode()
    sig = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    req = urllib.request.Request(f"{BASE}/webhook/github", data=body, method="POST")
    req.add_header("content-type", "application/json")
    req.add_header("x-github-event", event)
    req.add_header("x-github-delivery", delivery or f"d-{time.time_ns()}")
    if sign:
        req.add_header("x-hub-signature-256", sig)
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def post_form(payload, event="issues", delivery=None, secret=SECRET):
    """A delivery encoded the way GitHub's webhook form does by default.

    The signature is over the raw form body, not over the JSON inside it, which
    is what makes this worth a test of its own: verifying the decoded payload
    instead would check something the sender never signed.
    """
    body = urllib.parse.urlencode({"payload": json.dumps(payload)}).encode()
    sig = "sha256=" + hmac.new(secret, body, hashlib.sha256).hexdigest()
    req = urllib.request.Request(f"{BASE}/webhook/github", data=body, method="POST")
    req.add_header("content-type", "application/x-www-form-urlencoded")
    req.add_header("x-github-event", event)
    req.add_header("x-github-delivery", delivery or f"f-{time.time_ns()}")
    req.add_header("x-hub-signature-256", sig)
    try:
        with urllib.request.urlopen(req) as r:
            return r.status, json.loads(r.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def check(name, got, want):
    global passed, failed
    ok = want in str(got)
    print(f"  {'\033[32m✓\033[0m' if ok else '\033[31m✗\033[0m'} {name:<44} {got}")
    passed, failed = passed + ok, failed + (not ok)


def issue(number=1, assoc="OWNER", labels=("leangraph",), repo="pallets/flask", pr=False):
    body = {
        "action": "labeled",
        "repository": {"full_name": repo},
        "issue": {
            "number": number,
            "title": "QuerySet.get_or_create raises IntegrityError",
            "body": "Happens on multiple databases.",
            "author_association": assoc,
            "labels": [{"name": n} for n in labels],
        },
    }
    if pr:
        body["issue"]["pull_request"] = {"url": "..."}
    return body


print(f"\nwebhook gates — {BASE}\n")

# --- signature ---------------------------------------------------------------
code, _ = post(issue(), sign=False)
check("unsigned request is rejected", code, "401")

code, _ = post(issue(), secret=b"wrong-secret")
check("wrong signature is rejected", code, "401")

# A signature over different bytes than were sent is the classic mistake made by
# frameworks that re-serialise the body.
body = json.dumps(issue()).encode()
tampered = body.replace(b"OWNER", b"NONE~")
req = urllib.request.Request(f"{BASE}/webhook/github", data=tampered, method="POST")
req.add_header("x-github-event", "issues")
req.add_header("x-github-delivery", f"t-{time.time_ns()}")
req.add_header("x-hub-signature-256",
               "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest())
try:
    with urllib.request.urlopen(req) as r:
        code = r.status
except urllib.error.HTTPError as e:
    code = e.code
check("tampered body is rejected", code, "401")

# --- idempotency -------------------------------------------------------------
d = f"dup-{time.time_ns()}"
post(issue(number=901), delivery=d)
_, res = post(issue(number=901), delivery=d)
check("replayed delivery is a no-op", res.get("status"), "duplicate")

# --- authorisation -----------------------------------------------------------
_, res = post(issue(number=902, assoc="NONE"))
check("drive-by author is ignored", res.get("reason"), "untrusted")

_, res = post(issue(number=903, assoc="CONTRIBUTOR"))
check("contributor is not trusted either", res.get("reason"), "untrusted")

# --- opt-in ------------------------------------------------------------------
_, res = post(issue(number=904, labels=("bug",)))
check("unlabelled issue is ignored", res.get("reason"), "label")

# --- shape -------------------------------------------------------------------
_, res = post(issue(number=905, pr=True))
check("pull request is not an issue", res.get("reason"), "pull request")

_, res = post(issue(number=906, repo="someone/unknown"))
check("unknown repository is ignored", res.get("status"), "unknown repo")

_, res = post({"action": "closed", "repository": {"full_name": "pallets/flask"},
               "issue": {"number": 907, "labels": []}})
check("uninteresting action is ignored", res.get("status"), "ignored")

# --- the one that should work ------------------------------------------------
_, res = post(issue(number=908))
check("trusted + labelled issue is queued", res.get("status"), "queued")

# --- push --------------------------------------------------------------------
_, res = post({"ref": "refs/heads/feature/x", "before": "abc",
               "repository": {"full_name": "pallets/flask"}}, event="push")
check("push to another branch is ignored", res.get("reason"), "not the indexed branch")

# --- encoding -----------------------------------------------------------------
# GitHub's webhook form defaults to x-www-form-urlencoded. Rejecting it meant
# the default configuration failed every time, with an error that said only
# "malformed payload".
check("form-encoded delivery is accepted",
      post_form(issue(90))[1], "queued")
check("form-encoded with a bad signature is still refused",
      post_form(issue(91), secret=b"wrong")[1], "bad signature")

print(f"\n  \033[1m{passed}/{passed + failed} gates hold\033[0m\n")
sys.exit(1 if failed else 0)
