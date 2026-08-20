#!/usr/bin/env python3
"""Fix mode, end to end, against a real git repository.

The vetting rules have unit tests; this exercises the parts those cannot reach —
the three switches, the worktree, the push, and the shape of the pull request
that comes out. It runs against stubs for Anthropic and GitHub but a genuine
git remote, because the failure modes worth catching here (a patch applied to
the indexed checkout, a push to the default branch, a non-draft PR) are all in
the git and HTTP behaviour rather than in the logic above it.

Usage: bench/fix_test.py
"""
import json, hashlib, hmac, os, shutil, signal, subprocess, sys, tempfile, time
import urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
LOG = "/tmp/leangraph-fix-test.log"
LEANGRAPH = os.path.join(ROOT, "target", "release", "leangraph")
SECRET = b"fix-test-secret"
PORT, STUB_A, STUB_G = 7791, 7993, 7994
BASE = f"http://127.0.0.1:{PORT}"

passed = failed = 0
procs = []


def check(name, ok, detail=""):
    global passed, failed
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name:<52} {detail}")
    passed, failed = passed + bool(ok), failed + (not ok)


def git(*args, cwd):
    return subprocess.run(["git", *args], cwd=cwd, capture_output=True, text=True,
                          check=True).stdout.strip()


def req(path, data=None, method=None, base=BASE):
    body = json.dumps(data).encode() if data is not None else None
    r = urllib.request.Request(base + path, data=body, method=method or ("POST" if body else "GET"))
    r.add_header("content-type", "application/json")
    try:
        with urllib.request.urlopen(r, timeout=20) as res:
            return res.status, json.loads(res.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


# Every issue needs its own wording. Dedup runs before anything else and will
# correctly refuse to re-analyse the same report twice, so a fixture that reuses
# one body tests dedup rather than fix mode.
REPORTS = {
    1: ("add() subtracts instead of adding",
        "The helper in src/app.py returns a minus b where it should return the sum."),
    2: ("run() reports a negative total for two positive inputs",
        "Calling run with 1 and 2 gives -1. Every caller downstream then treats "
        "the result as an error code."),
    3: ("arithmetic helper has its operator reversed",
        "Whatever is passed in, the second argument is taken away from the first "
        "rather than combined with it, which breaks the totals on every report page."),
    4: ("FORBIDDEN the release workflow needs a new step",
        "Our continuous integration should also publish the wheel, so the pipeline "
        "definition has to gain a publish stage after the existing build one."),
    5: ("DECLINE intermittent timeout in the nightly job",
        "About once a week the scheduled run stops responding partway through and "
        "there is nothing useful in the output to say where."),
}


def webhook(number, labels, title=None, body=None):
    t, b = REPORTS.get(number, ("untitled", "no body"))
    payload = {
        "action": "labeled",
        "repository": {"full_name": "test/fixdemo"},
        "issue": {"number": number, "title": title or t,
                  "body": body or b,
                  "author_association": "OWNER",
                  "labels": [{"name": n} for n in labels]},
    }
    body = json.dumps(payload).encode()
    r = urllib.request.Request(BASE + "/webhook/github", data=body, method="POST")
    r.add_header("content-type", "application/json")
    r.add_header("x-github-event", "issues")
    r.add_header("x-github-delivery", f"fix-{number}-{time.time_ns()}")
    r.add_header("x-hub-signature-256",
                 "sha256=" + hmac.new(SECRET, body, hashlib.sha256).hexdigest())
    with urllib.request.urlopen(r, timeout=20) as res:
        return json.loads(res.read() or b"{}")


def wait(predicate, secs=30, every=0.3):
    end = time.time() + secs
    while time.time() < end:
        try:
            if predicate():
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
    tmp = tempfile.mkdtemp(prefix="leangraph-fix-")
    origin, work, data = (os.path.join(tmp, d) for d in ("origin.git", "work", "data"))

    # --- a real remote and a real checkout -----------------------------------
    os.makedirs(origin); os.makedirs(work); os.makedirs(data)
    git("init", "--bare", "-b", "main", ".", cwd=origin)
    git("init", "-b", "main", ".", cwd=work)
    git("config", "user.email", "t@t", cwd=work)
    git("config", "user.name", "t", cwd=work)
    os.makedirs(os.path.join(work, "src"))
    with open(os.path.join(work, "src", "app.py"), "w") as f:
        f.write("def add(a, b):\n    return a - b\n\n")
    with open(os.path.join(work, "src", "main.py"), "w") as f:
        f.write("from src.app import add\n\n\ndef run():\n    return add(1, 2)\n")
    git("add", "-A", cwd=work)
    git("commit", "-m", "initial", cwd=work)
    git("remote", "add", "origin", origin, cwd=work)
    git("push", "-u", "origin", "main", cwd=work)

    env = dict(os.environ,
               LEANGRAPH_ANTHROPIC_BASE=f"http://127.0.0.1:{STUB_A}/v1/messages",
               LEANGRAPH_GITHUB_API=f"http://127.0.0.1:{STUB_G}",
               LEANGRAPH_ANTHROPIC_KEY="sk-stub",
               LEANGRAPH_GITHUB_TOKEN="ghp-stub",
               LEANGRAPH_MASTER_KEY="00" * 32,
               LEANGRAPH_WEBHOOK_SECRET=SECRET.decode())

    for script, port in ((["bench/stub_api.py", str(STUB_A)], STUB_A),
                         (["bench/stub_github.py", str(STUB_G)], STUB_G)):
        procs.append(subprocess.Popen([sys.executable, os.path.join(ROOT, script[0]), script[1]],
                                      cwd=ROOT, preexec_fn=os.setsid,
                                      stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    procs.append(subprocess.Popen(
        [LEANGRAPH, "server", "--addr", f"127.0.0.1:{PORT}", "--data", data,
         "--workers", "1"],
        cwd=ROOT, env=env, preexec_fn=os.setsid,
        stdout=open(LOG, "w"), stderr=subprocess.STDOUT))

    if not wait(lambda: req("/health")[0] == 200, 25):
        print("server did not come up"); return 1

    print(f"\nfix mode, end to end — {BASE}\n")

    req("/repos", {"path": work, "full_name": "test/fixdemo", "branch": "main"})
    ready = wait(lambda: req("/repos/test%2Ffixdemo")[1]["repo"]["state"] == "ready", 40)
    check("repository indexes from a local path", ready)

    # --- switch 1: the label alone does nothing ------------------------------
    r = webhook(1, ["leangraph"])
    check("no fix label -> no patch requested", r.get("fix") is False, r.get("fix"))

    # --- switch 2: the label without the repo opting in does nothing ---------
    r = webhook(2, ["leangraph", "leangraph-fix"])
    check("fix label but fix mode off -> refused", r.get("fix") is False, r.get("fix"))
    time.sleep(2)
    _, seen = req("/_seen", base=f"http://127.0.0.1:{STUB_G}")
    check("nothing was pushed while fix mode was off",
          not any(c["path"].endswith("/pulls") for c in seen["calls"]))

    # --- both switches open --------------------------------------------------
    st, _ = req("/repos/test%2Ffixdemo/config", {"fix_mode": True})
    check("fix mode can be enabled per repository", st == 200, st)
    r = webhook(3, ["leangraph", "leangraph-fix"])
    check("both switches open -> patch requested", r.get("fix") is True, r.get("fix"))

    got_pr = wait(lambda: any(c["path"].endswith("/pulls") for c in
                              req("/_seen", base=f"http://127.0.0.1:{STUB_G}")[1]["calls"]), 45)
    check("a pull request was opened", got_pr)

    _, seen = req("/_seen", base=f"http://127.0.0.1:{STUB_G}")
    prs = [c for c in seen["calls"] if c["path"].endswith("/pulls")]
    if prs:
        pr = prs[0]["body"]
        check("the pull request is a draft", pr.get("draft") is True, pr.get("draft"))
        check("it targets the default branch", pr.get("base") == "main", pr.get("base"))
        check("from a branch of its own, not main",
              pr.get("head") == "leangraph/issue-3", pr.get("head"))
        check("the body says it is untested",
              "not been run or tested" in pr.get("body", ""))

    # --- the remote actually has the change ----------------------------------
    branches = git("branch", "--list", "leangraph/issue-3", cwd=origin)
    check("the branch exists on the remote", "leangraph/issue-3" in branches, branches)
    if "leangraph/issue-3" in branches:
        blob = git("show", "leangraph/issue-3:src/app.py", cwd=origin)
        check("the fix is in it", "return a + b" in blob, blob.split("\n")[1].strip())
        touched = git("diff", "--name-only", "main", "leangraph/issue-3", cwd=origin)
        check("and nothing else changed", touched == "src/app.py", touched)

    # --- the indexed checkout was not touched --------------------------------
    with open(os.path.join(work, "src", "app.py")) as f:
        check("the indexed checkout is untouched", "return a - b" in f.read())
    check("no worktree was left behind",
          not any(d.startswith(".leangraph-fix") for d in os.listdir(tmp)))
    check("still on the default branch", git("rev-parse", "--abbrev-ref", "HEAD", cwd=work) == "main")

    # --- a forbidden patch is refused, not applied ---------------------------
    before = len([c for c in seen["calls"] if c["path"].endswith("/pulls")])
    webhook(4, ["leangraph", "leangraph-fix"])
    time.sleep(6)
    _, seen = req("/_seen", base=f"http://127.0.0.1:{STUB_G}")
    after = [c for c in seen["calls"] if c["path"].endswith("/pulls")]
    check("a patch touching CI config opens no pull request", len(after) == before,
          f"{len(after)} vs {before}")
    comments = [c for c in seen["calls"] if c["path"].endswith("/comments")]
    check("and the issue is told why",
          any("could not be applied" in c["body"].get("body", "") for c in comments))
    check("no branch was pushed for it",
          "leangraph/issue-4" not in git("branch", "--list", "leangraph/issue-4", cwd=origin))

    # --- an empty answer is a valid answer -----------------------------------
    webhook(5, ["leangraph", "leangraph-fix"])
    time.sleep(6)
    _, seen = req("/_seen", base=f"http://127.0.0.1:{STUB_G}")
    comments = [c for c in seen["calls"] if c["path"].endswith("/comments")]
    check("declining to patch still posts the analysis",
          any("could not produce one" in c["body"].get("body", "") for c in comments))
    check("declining pushes nothing",
          "leangraph/issue-5" not in git("branch", "--list", "leangraph/issue-5", cwd=origin))

    # --- the token never leaves ---------------------------------------------
    check("the token is not echoed into any comment",
          not any("ghp-stub" in json.dumps(c["body"]) for c in comments))

    if failed:
        print("\n  server log:")
        with open(LOG) as f:
            for line in f.read().splitlines()[-25:]:
                print("   ", line)
    print(f"\n  {passed} passed, {failed} failed\n")
    shutil.rmtree(tmp, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        cleanup()
