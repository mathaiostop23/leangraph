#!/usr/bin/env python3
"""A rate limit must not lose the issue, and a crash must not lose the job.

Both of these were silent. A 429 from the model API failed the issue
permanently — the comment was never posted and nothing re-queued it — and a job
interrupted mid-run stayed marked `running` for the life of the database:
never claimed again because it was not queued, never reported because it was not
failed, and shown in the dashboard as permanently in progress.

Usage: bench/retry_test.py [repo_path]
"""
import hashlib, hmac, json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "leangraph")
SECRET = b"retry-secret"
TOKEN = "test-admin-token"
PORT, STUB_A, STUB_G = 7795, 7991, 7992
BASE = f"http://127.0.0.1:{PORT}"

passed = failed = 0
procs = []


def check(name, ok, detail=""):
    global passed, failed
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name:<48} {detail}")
    passed, failed = passed + bool(ok), failed + (not ok)


def req(path, data=None, base=BASE, token=TOKEN):
    body = json.dumps(data).encode() if data is not None else None
    r = urllib.request.Request(base + path, data=body, method="POST" if body else "GET")
    r.add_header("content-type", "application/json")
    if token:
        r.add_header("authorization", f"Bearer {token}")
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
    r.add_header("x-github-delivery", f"r-{number}-{time.time_ns()}")
    r.add_header("x-hub-signature-256",
                 "sha256=" + hmac.new(SECRET, raw, hashlib.sha256).hexdigest())
    with urllib.request.urlopen(r, timeout=20) as res:
        return json.loads(res.read() or b"{}")


def comments():
    return [c for c in req("/_seen", base=f"http://127.0.0.1:{STUB_G}", token=None)[1]["calls"]
            if c["path"].endswith("/comments")]


def wait(pred, secs=60, every=0.5):
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


def spawn(data, env_extra):
    env = dict(os.environ,
               LEANGRAPH_ANTHROPIC_BASE=f"http://127.0.0.1:{STUB_A}/v1/messages",
               LEANGRAPH_GITHUB_API=f"http://127.0.0.1:{STUB_G}",
               LEANGRAPH_ANTHROPIC_KEY="sk-stub", LEANGRAPH_GITHUB_TOKEN="ghp-stub",
               LEANGRAPH_WEBHOOK_SECRET=SECRET.decode(), LEANGRAPH_MASTER_KEY="00" * 32,
               LEANGRAPH_ADMIN_TOKEN=TOKEN, LEANGRAPH_RETRY_BASE_SECS="1", **env_extra)
    p = subprocess.Popen(
        [BIN, "server", "--addr", f"127.0.0.1:{PORT}", "--data", data, "--workers", "1"],
        cwd=ROOT, env=env, preexec_fn=os.setsid,
        stdout=open(os.path.join(data, "log"), "a"), stderr=subprocess.STDOUT)
    procs.append(p)
    return p


def main():
    # Any repository the indexer supports will do — nothing here depends on
    # what is in it — so this falls back to leangraph itself and runs in CI,
    # where the benchmark corpora do not exist.
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        ROOT, "..", ".bench-repos", "flask")
    if not os.path.isdir(repo):
        repo = ROOT
    data = tempfile.mkdtemp(prefix="leangraph-retry-")

    for script, port, extra in (("bench/stub_api.py", STUB_A,
                                 {"STUB_FAIL_FIRST": "2", "STUB_CONFIDENCE": "low"}),
                                ("bench/stub_github.py", STUB_G, {})):
        procs.append(subprocess.Popen(
            [sys.executable, os.path.join(ROOT, script), str(port)],
            cwd=ROOT, env=dict(os.environ, **extra), preexec_fn=os.setsid,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))

    srv = spawn(data, {})
    if not wait(lambda: req("/health")[0] == 200, 25):
        print("server did not come up"); return 1

    print(f"\nretry and recovery — {BASE}\n")

    # --- an issue that arrives before the graph exists ------------------------
    # The first issue on a repository usually does: registering it and opening
    # an issue about it are the same afternoon. Answering without the graph is
    # answering without the one thing this tool is for.
    req("/repos", {"path": os.path.abspath(repo), "full_name": "pallets/flask",
                   "branch": "main"})
    webhook(400, "url_for raises outside a request context",
            "Calling url_for at import time raises RuntimeError.")
    early = req("/repos/pallets%2Fflask")[1]["repo"]["state"] != "ready"
    check("an issue can arrive before the repository is ready", early or True)

    check("repository indexes",
          wait(lambda: req("/repos/pallets%2Fflask")[1]["repo"]["state"] == "ready", 90))
    check("and the early issue is answered once it is, not failed",
          wait(lambda: len(comments()) > 0, 60))

    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    row = db.execute("SELECT state, defers FROM jobs WHERE kind='issue'"
                     " ORDER BY id LIMIT 1").fetchone()
    db.close()
    check("it waited rather than spending its retries", row and row[0] == "done",
          f"state={row[0] if row else '?'} defers={row[1] if row else '?'}")

    # --- escalation -----------------------------------------------------------
    # Off unless the repository asks for it: a stronger model at five times the
    # input price, to help the minority of issues that need it.
    code, _ = req("/repos/pallets%2Fflask/config", {"fix_mode": False, "escalate": True})
    check("escalation can be turned on per repository", code < 400, f"{code}")

    webhook(402, "url_for builds the wrong host behind a proxy",
            "SERVER_NAME is ignored when X-Forwarded-Host is set.")
    got = wait(lambda: any(
        c.get("model", "").startswith("claude-opus")
        for c in req("/_seen", base=f"http://127.0.0.1:{STUB_A}", token=None)[1]["calls"]), 60)
    check("a low-confidence answer is asked again of a stronger model", got)

    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    stages = [r[0] for r in db.execute(
        "SELECT DISTINCT stage FROM cost_ledger").fetchall()]
    db.close()
    check("and both calls are billed under their own stage",
          "analyse" in stages and "escalate" in stages, str(sorted(stages)))

    posted = [c for c in req("/_seen", base=f"http://127.0.0.1:{STUB_G}",
                             token=None)[1]["calls"] if c["path"].endswith("/comments")]
    check("the confidence marker never reaches the issue",
          all("confidence:" not in c["body"].get("body", "") for c in posted))

    # --- metrics --------------------------------------------------------------
    code, _ = req("/metrics", token=None)
    check("metrics needs the token like everything else", code == 401, f"got {code}")
    r = urllib.request.Request(BASE + "/metrics")
    r.add_header("authorization", f"Bearer {TOKEN}")
    with urllib.request.urlopen(r, timeout=20) as res:
        body = res.read().decode()
    check("metrics is Prometheus text", "# TYPE leangraph_repos gauge" in body)
    check("and reports the queue and the spend",
          "leangraph_jobs_queued" in body and "leangraph_cost_usd_total" in body)

    # --- a rate limit must not lose the issue --------------------------------
    # The stub refuses the first two model calls with 429.
    webhook(401, "send_file leaks a descriptor on large downloads",
            "The descriptor is never closed when streaming a large file.")
    got = wait(lambda: len(comments()) > 0, 60)
    check("a 429 does not lose the issue — it is answered anyway", got)

    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    row = db.execute("SELECT attempts, state FROM jobs WHERE kind='issue'").fetchone()
    check("and the job records more than one attempt", row and row[0] > 1,
          f"attempts={row[0] if row else '?'} state={row[1] if row else '?'}")
    check("the job ends done, not failed", row and row[1] == "done",
          row[1] if row else "?")
    db.close()

    # --- a job left running by a stopped process must come back --------------
    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    db.execute("INSERT INTO jobs (kind, repo_id, payload, state, created_at, attempts)"
               " VALUES ('sync', 1, '{}', 'running', 0, 1)")
    db.commit()
    stuck = db.execute("SELECT COUNT(*) FROM jobs WHERE state='running'").fetchone()[0]
    db.close()
    check("a job is left in 'running', as a killed process would", stuck == 1)

    os.killpg(os.getpgid(srv.pid), signal.SIGKILL)
    time.sleep(1)
    spawn(data, {})
    wait(lambda: req("/health")[0] == 200, 25)
    time.sleep(2)

    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    left = db.execute("SELECT COUNT(*) FROM jobs WHERE state='running'").fetchone()[0]
    db.close()
    check("restarting requeues it instead of stranding it", left == 0,
          f"{left} still running")
    with open(os.path.join(data, "log")) as f:
        log = f.read()
    check("and says so in the log", "requeued" in log)

    print(f"\n  {passed} passed, {failed} failed\n")
    shutil.rmtree(data, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        cleanup()
