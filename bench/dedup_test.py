#!/usr/bin/env python3
"""Deduplication, end to end.

The unit tests cover the scoring. What they cannot show is the thing that
matters commercially: that a duplicate costs *nothing* — no triage call, no
analysis call, no tokens — because the check runs before the API client is even
constructed. So this counts model calls at the stub and asserts the count does
not move.

Usage: bench/dedup_test.py [repo_path]
"""
import hashlib, hmac, json, os, shutil, signal, subprocess, sys, tempfile, time
import urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LEANGRAPH = os.path.join(ROOT, "target", "release", "leangraph")
LOG = "/tmp/leangraph-dedup-test.log"
SECRET = b"dedup-test-secret"
PORT, STUB_A, STUB_G = 7793, 7995, 7996
BASE = f"http://127.0.0.1:{PORT}"

passed = failed = 0
procs = []

# One report, then the same report reworded, then a different one. The reworded
# pair is deliberately not a copy — a bot that only catches literal duplicates
# catches nothing, since people retype rather than paste.
ORIGINAL = ("Flask.send_file leaks a file descriptor on large downloads",
            "When send_file streams a large file the descriptor is never closed, "
            "so a long-running server eventually hits the open file limit and "
            "every subsequent request fails with EMFILE.")
REWORDED = ("send_file leaks file descriptors when serving large downloads",
            "The descriptor is never closed when send_file streams a large file, "
            "so a server that runs for a long time hits the open file limit and "
            "then every request after that fails with EMFILE.")
DIFFERENT = ("url_for builds the wrong scheme behind a reverse proxy",
             "With ProxyFix installed url_for still returns http:// links even "
             "though the original request arrived over https, so every generated "
             "URL in the rendered page is wrong and browsers block them.")


def check(name, ok, detail=""):
    global passed, failed
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name:<50} {detail}")
    passed, failed = passed + bool(ok), failed + (not ok)


def req(path, data=None, base=BASE):
    body = json.dumps(data).encode() if data is not None else None
    r = urllib.request.Request(base + path, data=body,
                               method="POST" if body else "GET")
    r.add_header("content-type", "application/json")
    # The management API is gated; the webhook is not.
    r.add_header("authorization", "Bearer test-admin-token")
    try:
        with urllib.request.urlopen(r, timeout=20) as res:
            return res.status, json.loads(res.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def webhook(number, title, body):
    payload = {"action": "labeled", "repository": {"full_name": "pallets/flask"},
               "issue": {"number": number, "title": title, "body": body,
                         "author_association": "OWNER",
                         "labels": [{"name": "leangraph"}]}}
    raw = json.dumps(payload).encode()
    r = urllib.request.Request(BASE + "/webhook/github", data=raw, method="POST")
    r.add_header("content-type", "application/json")
    r.add_header("x-github-event", "issues")
    r.add_header("x-github-delivery", f"dd-{number}-{time.time_ns()}")
    r.add_header("x-hub-signature-256",
                 "sha256=" + hmac.new(SECRET, raw, hashlib.sha256).hexdigest())
    with urllib.request.urlopen(r, timeout=20) as res:
        return json.loads(res.read() or b"{}")


def model_calls():
    return len(req("/_seen", base=f"http://127.0.0.1:{STUB_A}")[1]["calls"])


def comments():
    return [c for c in req("/_seen", base=f"http://127.0.0.1:{STUB_G}")[1]["calls"]
            if c["path"].endswith("/comments")]


def wait(pred, secs=45, every=0.4):
    end = time.time() + secs
    while time.time() < end:
        try:
            if pred():
                return True
        except Exception:
            pass
        time.sleep(every)
    return False


def cleanup():
    for p in procs:
        try:
            os.killpg(os.getpgid(p.pid), signal.SIGKILL)
        except Exception:
            pass


def main():
    # Any repository the indexer supports will do; nothing here depends on what
    # is in it. Falling back to leangraph itself is what lets this run in CI.
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        ROOT, "..", ".bench-repos", "flask")
    if not os.path.isdir(repo):
        repo = ROOT
    data = tempfile.mkdtemp(prefix="leangraph-dedup-")

    env = dict(os.environ,
               LEANGRAPH_ANTHROPIC_BASE=f"http://127.0.0.1:{STUB_A}/v1/messages",
               LEANGRAPH_GITHUB_API=f"http://127.0.0.1:{STUB_G}",
               LEANGRAPH_ANTHROPIC_KEY="sk-stub", LEANGRAPH_GITHUB_TOKEN="ghp-stub",
               LEANGRAPH_WEBHOOK_SECRET=SECRET.decode(), LEANGRAPH_MASTER_KEY="00" * 32,
               LEANGRAPH_ADMIN_TOKEN="test-admin-token")

    for script, port in (("bench/stub_api.py", STUB_A), ("bench/stub_github.py", STUB_G)):
        procs.append(subprocess.Popen([sys.executable, os.path.join(ROOT, script), str(port)],
                                      cwd=ROOT, preexec_fn=os.setsid,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    procs.append(subprocess.Popen(
        [LEANGRAPH, "server", "--addr", f"127.0.0.1:{PORT}", "--data", data, "--workers", "1"],
        cwd=ROOT, env=env, preexec_fn=os.setsid,
        stdout=open(LOG, "w"), stderr=subprocess.STDOUT))

    if not wait(lambda: req("/health")[0] == 200, 25):
        print("server did not come up"); return 1

    print(f"\ndeduplication, end to end — {BASE}\n")

    req("/repos", {"path": os.path.abspath(repo), "full_name": "pallets/flask",
                   "branch": "main"})
    check("repository indexes",
          wait(lambda: req("/repos/pallets%2Fflask")[1]["repo"]["state"] == "ready", 90))

    # --- the first issue is answered normally --------------------------------
    webhook(301, *ORIGINAL)
    check("the first issue reaches the model", wait(lambda: model_calls() >= 2, 40),
          f"{model_calls()} calls")
    after_first = model_calls()
    n_comments = len(comments())

    # --- the reworded restatement must cost nothing --------------------------
    webhook(302, *REWORDED)
    check("a restatement gets a comment", wait(lambda: len(comments()) > n_comments, 40))
    time.sleep(3)
    check("and it cost zero model calls", model_calls() == after_first,
          f"{model_calls()} vs {after_first}")
    body = comments()[-1]["body"].get("body", "")
    check("the comment names the issue it matched", "#301" in body, body[:60])
    check("and invites a correction", "wrong" in body.lower(), body[:60])

    # --- a genuinely different issue must not be swallowed -------------------
    before_third = model_calls()
    webhook(303, *DIFFERENT)
    check("a different issue is still analysed",
          wait(lambda: model_calls() > before_third, 40),
          f"{model_calls()} vs {before_third}")
    time.sleep(3)
    last = comments()[-1]["body"].get("body", "")
    check("and is not reported as a duplicate", "restatement" not in last, last[:60])

    # --- the ledger tells the truth about it ---------------------------------
    runs = req("/health")[1]
    check("the server stayed healthy", runs.get("ok") is True)
    with open(LOG) as f:
        log = f.read()
    check("the skip is logged with the issue it matched",
          "duplicate of #301" in log and "no model call" in log)

    if failed:
        print("\n  server log:")
        for line in log.splitlines()[-20:]:
            print("   ", line)
    print(f"\n  {passed} passed, {failed} failed\n")
    shutil.rmtree(data, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        cleanup()
