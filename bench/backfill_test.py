#!/usr/bin/env python3
"""Backfill, end to end, through the Batch API.

The Batch API is half price and asynchronous — useless for a webhook, where
latency is the product, and exactly right for the backlog a repository already
had. This asserts the three things that make it worth having: it goes through
the batch endpoint rather than the live one, the waiting is a deferral rather
than a failed attempt, and what it cost is booked at half.

Usage: bench/backfill_test.py [repo_path]
"""
import json, os, shutil, signal, sqlite3, subprocess, sys, tempfile, time
import urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "leangraph")
TOKEN = "test-admin-token"
PORT, STUB_A, STUB_G = 7793, 7989, 7988
BASE = f"http://127.0.0.1:{PORT}"

passed = failed = 0
procs = []


def check(name, ok, detail=""):
    global passed, failed
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name:<52} {detail}")
    passed, failed = passed + bool(ok), failed + (not ok)


def api(path, data=None, base=BASE, token=TOKEN):
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


def comments():
    return [c for c in api("/_seen", base=f"http://127.0.0.1:{STUB_G}", token=None)[1]["calls"]
            if c["path"].endswith("/comments")]


def wait(pred, secs=120):
    end = time.time() + secs
    while time.time() < end:
        try:
            if pred():
                return True
        except Exception:
            pass
        time.sleep(0.5)
    return False


def cleanup():
    for p in procs:
        try:
            os.killpg(os.getpgid(p.pid), signal.SIGKILL)
        except Exception:
            pass


def main():
    repo = sys.argv[1] if len(sys.argv) > 1 else ROOT
    if not os.path.isdir(repo):
        repo = ROOT
    data = tempfile.mkdtemp(prefix="leangraph-backfill-")

    for script, port in (("bench/stub_api.py", STUB_A), ("bench/stub_github.py", STUB_G)):
        procs.append(subprocess.Popen(
            [sys.executable, os.path.join(ROOT, script), str(port)],
            cwd=ROOT, preexec_fn=os.setsid,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))

    env = dict(os.environ,
               LEANGRAPH_ANTHROPIC_BASE=f"http://127.0.0.1:{STUB_A}/v1/messages",
               LEANGRAPH_GITHUB_API=f"http://127.0.0.1:{STUB_G}",
               LEANGRAPH_ANTHROPIC_KEY="sk-stub", LEANGRAPH_GITHUB_TOKEN="ghp-stub",
               LEANGRAPH_MASTER_KEY="00" * 32, LEANGRAPH_ADMIN_TOKEN=TOKEN)
    procs.append(subprocess.Popen(
        [BIN, "server", "--addr", f"127.0.0.1:{PORT}", "--data", data, "--workers", "2"],
        cwd=ROOT, env=env, preexec_fn=os.setsid,
        stdout=open(os.path.join(data, "log"), "w"), stderr=subprocess.STDOUT))

    if not wait(lambda: api("/health")[0] == 200, 25):
        print("server did not come up")
        return 1

    print(f"\nbackfill through the Batch API — {BASE}\n")
    api("/repos", {"path": os.path.abspath(repo), "full_name": "pallets/flask",
                   "branch": "main"})
    check("repository indexes",
          wait(lambda: api("/repos/pallets%2Fflask")[1]["repo"]["state"] == "ready", 90))

    code, body = api("/repos/pallets%2Fflask/backfill", {})
    check("backfill is a request, not something registration does", code < 400, f"{code}")

    got = wait(lambda: len(comments()) >= 2, 120)
    check("the open backlog is answered", got, f"{len(comments())} comment(s)")

    numbers = sorted(int(c["path"].split("/issues/")[1].split("/")[0]) for c in comments())
    check("both open issues, and not the pull request in the same list",
          numbers == [900, 901], str(numbers))

    calls = api("/_seen", base=f"http://127.0.0.1:{STUB_A}", token=None)[1]["calls"]
    check("nothing went through the live message endpoint",
          not any(c.get("model") for c in calls), f"{len(calls)} live call(s)")

    db = sqlite3.connect(os.path.join(data, "leangraph.db"))
    rows = db.execute("SELECT stage, model, input_tokens, output_tokens, cost_usd"
                      " FROM cost_ledger WHERE stage='backfill'").fetchall()
    jobs = db.execute("SELECT kind, state, defers FROM jobs WHERE kind='batch'").fetchall()
    db.close()
    check("both answers are billed under `backfill`", len(rows) == 2, str(len(rows)))
    if rows:
        stage, model, i, o, cost = rows[0]
        full = (i * 3.0 + o * 15.0) / 1e6
        check("at half price, because the Batch API is",
              abs(cost - full / 2) < 1e-9, f"${cost:.6f} vs ${full:.6f} live")
    check("the poll waited rather than spending an attempt",
          jobs and jobs[0][1] == "done" and jobs[0][2] >= 1,
          f"state={jobs[0][1] if jobs else '?'} defers={jobs[0][2] if jobs else '?'}")

    # Re-running must not re-bill what is already answered.
    api("/repos/pallets%2Fflask/backfill", {})
    time.sleep(3)
    check("a second run does not answer them again", len(comments()) == 2,
          f"{len(comments())} comment(s)")

    # --- re-analysis, gated on the code having moved --------------------------
    # The gate is the fingerprint, not the clock. A bot that posts the same
    # conclusion every night is a bot people mute.
    api("/repos/pallets%2Fflask/reanalyse", {})
    time.sleep(4)
    check("nothing is re-analysed while the code has not moved",
          len(comments()) == 2, f"{len(comments())} comment(s)")

    # Move the code the issues point at, and reindex so the seeds change.
    src = os.path.join(os.path.abspath(repo), "src", "leangraph_probe.py")
    os.makedirs(os.path.dirname(src), exist_ok=True)
    with open(src, "w") as f:
        f.write("def send_file(path):\n    return open(path)\n\n"
                "def url_for(name):\n    return name\n")
    try:
        api("/repos/pallets%2Fflask/sync", {})
        wait(lambda: api("/repos/pallets%2Fflask")[1]["repo"]["state"] == "ready", 90)
        api("/repos/pallets%2Fflask/reanalyse", {})
        moved = wait(lambda: len(comments()) > 2, 120)
        check("once it has, the answer is drawn again", moved,
              f"{len(comments())} comment(s)")
        db = sqlite3.connect(os.path.join(data, "leangraph.db"))
        stages = [r[0] for r in db.execute(
            "SELECT DISTINCT stage FROM cost_ledger").fetchall()]
        db.close()
        check("and billed under `reanalyse`, not `backfill`",
              "reanalyse" in stages, str(sorted(stages)))
    finally:
        os.remove(src)

    print(f"\n  {passed} passed, {failed} failed\n")
    shutil.rmtree(data, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        cleanup()
