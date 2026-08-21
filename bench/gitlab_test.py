#!/usr/bin/env python3
"""GitLab, end to end.

The gates are the product's security boundary, and a second provider is exactly
where one of them quietly comes out different. So this asserts the same things
the GitHub suite does — token, replay, author, label — against GitLab's own
payload shape, plus the one gate that has no counterpart there: GitLab does not
say whether an author is a member, so that check is an API call, and an API call
can fail open.

Usage: bench/gitlab_test.py [repo_path]
"""
import json, os, shutil, signal, subprocess, sys, tempfile, time
import urllib.error, urllib.request

ROOT = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
BIN = os.path.join(ROOT, "target", "release", "leangraph")
SECRET = "gitlab-secret"
TOKEN = "test-admin-token"
PORT, STUB_A, STUB_L = 7794, 7993, 7996
BASE = f"http://127.0.0.1:{PORT}"

passed = failed = 0
procs = []


def check(name, ok, detail=""):
    global passed, failed
    mark = "\033[32m✓\033[0m" if ok else "\033[31m✗\033[0m"
    print(f"  {mark} {name:<54} {detail}")
    passed, failed = passed + bool(ok), failed + (not ok)


def api(path, data=None, token=TOKEN, base=BASE):
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


def hook(payload, token=SECRET, uuid=None):
    raw = json.dumps(payload).encode()
    r = urllib.request.Request(BASE + "/webhook/gitlab", data=raw, method="POST")
    r.add_header("content-type", "application/json")
    r.add_header("x-gitlab-event-uuid", uuid or f"u-{time.time_ns()}")
    if token is not None:
        r.add_header("x-gitlab-token", token)
    try:
        with urllib.request.urlopen(r, timeout=20) as res:
            return res.status, json.loads(res.read() or b"{}")
    except urllib.error.HTTPError as e:
        return e.code, json.loads(e.read() or b"{}")


def issue_event(iid, labels=("leangraph",), action="open", title=None):
    return {
        "object_kind": "issue",
        "user": {"id": 7, "username": "someone"},
        "project": {"id": 42, "path_with_namespace": "grp/proj",
                    "default_branch": "main"},
        "object_attributes": {
            "iid": iid, "title": title or f"issue {iid}",
            "description": "send_file leaks a descriptor on large downloads.",
            "action": action,
        },
        "labels": [{"title": t} for t in labels],
    }


def notes():
    with urllib.request.urlopen(f"http://127.0.0.1:{STUB_L}/_seen", timeout=20) as r:
        return [c for c in json.loads(r.read())["calls"] if c["path"].endswith("/notes")]


def wait(pred, secs=90):
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
    repo = sys.argv[1] if len(sys.argv) > 1 else os.path.join(
        ROOT, "..", ".bench-repos", "flask")
    if not os.path.isdir(repo):
        repo = ROOT
    data = tempfile.mkdtemp(prefix="leangraph-gitlab-")

    for script, port, extra in (("bench/stub_api.py", STUB_A, {}),
                                ("bench/stub_gitlab.py", STUB_L, {"STUB_ACCESS_LEVEL": "40"})):
        procs.append(subprocess.Popen(
            [sys.executable, os.path.join(ROOT, script), str(port)],
            cwd=ROOT, env=dict(os.environ, **extra), preexec_fn=os.setsid,
            stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))

    env = dict(os.environ,
               LEANGRAPH_ANTHROPIC_BASE=f"http://127.0.0.1:{STUB_A}/v1/messages",
               LEANGRAPH_GITLAB_API=f"http://127.0.0.1:{STUB_L}",
               LEANGRAPH_ANTHROPIC_KEY="sk-stub", LEANGRAPH_GITLAB_TOKEN="glpat-stub",
               LEANGRAPH_WEBHOOK_SECRET=SECRET, LEANGRAPH_MASTER_KEY="00" * 32,
               LEANGRAPH_ADMIN_TOKEN=TOKEN)
    procs.append(subprocess.Popen(
        [BIN, "server", "--addr", f"127.0.0.1:{PORT}", "--data", data, "--workers", "2"],
        cwd=ROOT, env=env, preexec_fn=os.setsid,
        stdout=open(os.path.join(data, "log"), "w"), stderr=subprocess.STDOUT))

    if not wait(lambda: api("/health")[0] == 200, 25):
        print("server did not come up")
        return 1

    print(f"\ngitlab, end to end — {BASE}\n")

    code, body = api("/repos", {"path": os.path.abspath(repo),
                                "full_name": "grp/proj", "branch": "main",
                                "provider": "gitlab"})
    check("a repository can be registered as gitlab", code < 400, f"{code}")
    check("indexes", wait(lambda: api("/repos/grp%2Fproj")[1]["repo"]["state"] == "ready"))

    # --- the gates ----------------------------------------------------------
    code, _ = hook(issue_event(1), token=None)
    check("a delivery with no token is refused", code == 401, f"{code}")
    code, _ = hook(issue_event(2), token="wrong-secret")
    check("a delivery with the wrong token is refused", code == 401, f"{code}")

    uuid = "replay-me"
    hook(issue_event(3), uuid=uuid)
    code, body = hook(issue_event(3), uuid=uuid)
    check("a replayed delivery is dropped", body.get("status") == "duplicate", str(body))

    _, body = hook(issue_event(4, labels=("bug",)))
    check("an unlabelled issue does nothing", body.get("status") == "ignored", str(body))

    _, body = hook({"object_kind": "push", "ref": "refs/heads/main",
                    "before": "0" * 40,
                    "project": {"id": 42, "path_with_namespace": "grp/proj"}})
    check("a push to the indexed branch queues a sync",
          body.get("status") == "queued", str(body))

    _, body = hook({"object_kind": "push", "ref": "refs/heads/feature",
                    "before": "0" * 40,
                    "project": {"id": 42, "path_with_namespace": "grp/proj"}})
    check("a push to another branch does not", body.get("status") == "ignored", str(body))

    # --- the answer ---------------------------------------------------------
    hook(issue_event(10, title="url_for fails outside a request context"))
    got = wait(lambda: any("/issues/10/notes" in n["path"] for n in notes()), 90)
    check("a labelled issue from a member is answered", got)
    mine = [n for n in notes() if "/issues/10/notes" in n["path"]]
    if mine:
        check("the note goes to the project's escaped path",
              "/projects/grp%2Fproj/issues/10/notes" in mine[0]["path"], mine[0]["path"])
        check("and carries the token as private-token",
              mine[0]["token"] == "glpat-stub", mine[0]["token"])
        check("with the cost receipt attached",
              "graph nodes" in mine[0]["body"].get("body", ""))

    # --- the gate GitHub does not need --------------------------------------
    # GitLab's payload does not say whether the author is a member, so that
    # check is an API call — and an API call is something that can fail open.
    # Restart the stub answering 404 to every member lookup, which is what
    # GitLab returns for a stranger.
    os.killpg(os.getpgid(procs[1].pid), signal.SIGKILL)
    time.sleep(0.5)
    procs.append(subprocess.Popen(
        [sys.executable, os.path.join(ROOT, "bench/stub_gitlab.py"), str(STUB_L)],
        cwd=ROOT, env=dict(os.environ, STUB_ACCESS_LEVEL="0"), preexec_fn=os.setsid,
        stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL))
    wait(lambda: urllib.request.urlopen(
        f"http://127.0.0.1:{STUB_L}/_seen", timeout=5).status == 200, 15)

    _, body = hook(issue_event(11, title="a stranger asks"))
    check("an issue from someone who is not a member is refused",
          body.get("status") == "ignored", str(body))
    time.sleep(2)
    check("and nothing was posted for it",
          not any("/issues/11/notes" in n["path"] for n in notes()))

    print(f"\n  {passed} passed, {failed} failed\n")
    shutil.rmtree(data, ignore_errors=True)
    return 1 if failed else 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    finally:
        cleanup()
