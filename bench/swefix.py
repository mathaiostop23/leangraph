#!/usr/bin/env python3
"""Does a patch built on our context actually pass the repository's tests?

Everything measured so far says where the tool *looks*. This says whether an
answer built on what it returns is correct, which is the only question a user
asks. SWE-bench ships the tests that decide it and they have never been run
here.

The environment is the reason they never were: each instance needs its own
Python and its own installed dependencies at its own commit. The benchmark
publishes a container per instance that holds exactly that, and there is an
arm64 build, so on this machine they run natively rather than emulated.

Scoring is the benchmark's own and not negotiable: apply the candidate patch,
apply the *test* patch on top, then every FAIL_TO_PASS test must pass and every
PASS_TO_PASS test must still pass. A patch that fixes the bug and breaks
something else fails, which is the point.

Run with `--gold` first on any new instance. If the benchmark's own answer does
not pass, the harness is wrong and nothing measured through it means anything.
"""
import argparse, json, os, subprocess, sys, tempfile

IMAGE = "swebench/sweb.eval.arm64.{}:latest"


def image_for(instance_id):
    # `astropy__astropy-12907` -> `astropy_1776_astropy-12907`
    return IMAGE.format(instance_id.replace("__", "_1776_"))


def have_image(tag):
    return subprocess.run(["docker", "image", "inspect", tag],
                          capture_output=True).returncode == 0


def run_tests(rec, patch, keep_container=False):
    """Apply patch + test patch in a fresh container and run both test sets.

    Returns (resolved, detail). `resolved` is the benchmark's definition:
    every FAIL_TO_PASS passes and every PASS_TO_PASS still does.
    """
    tag = image_for(rec["instance_id"])
    if not have_image(tag):
        return None, f"image not pulled: {tag}"

    f2p = json.loads(rec["FAIL_TO_PASS"]) if isinstance(rec["FAIL_TO_PASS"], str) else list(rec["FAIL_TO_PASS"])
    p2p = json.loads(rec["PASS_TO_PASS"]) if isinstance(rec["PASS_TO_PASS"], str) else list(rec["PASS_TO_PASS"])

    with tempfile.TemporaryDirectory() as tmp:
        # Written to files rather than passed as arguments: a patch runs to
        # hundreds of lines and shell quoting it is a bug waiting to happen.
        open(os.path.join(tmp, "fix.diff"), "w").write(patch or "")
        open(os.path.join(tmp, "test.diff"), "w").write(rec["test_patch"])
        # Substitution by replace, not `.format`: the script is full of shell
        # braces and `format` reads them as fields.
        script = """
set -e
cd /testbed
git checkout -q -- .
if [ -s /work/fix.diff ]; then
  git apply -v /work/fix.diff 2>/tmp/apply.log || { echo "APPLY_FAILED"; cat /tmp/apply.log; exit 9; }
fi
git apply -v /work/test.diff 2>/dev/null || { echo "TEST_PATCH_FAILED"; exit 8; }
echo "---TESTS---"
python -m pytest -rA --no-header -q __TESTS__ 2>&1 | tail -120
""".replace("__TESTS__", " ".join(f"'{t}'" for t in f2p + p2p))
        open(os.path.join(tmp, "run.sh"), "w").write(script)

        # `bash -l`, not `bash`. The image keeps its dependencies in a conda
        # environment that the login profile activates; without it `python` is
        # a different interpreter that has never heard of the package under
        # test, and every test "fails" for a reason that has nothing to do with
        # the patch. The gold-patch self-test is what caught this.
        out = subprocess.run(
            ["docker", "run", "--rm", "-v", f"{tmp}:/work:ro", tag,
             "bash", "-l", "/work/run.sh"],
            capture_output=True, text=True, timeout=1800)
    text = out.stdout + out.stderr
    if "APPLY_FAILED" in text:
        return False, "patch did not apply"
    if "TEST_PATCH_FAILED" in text:
        return None, "the benchmark's own test patch did not apply — harness fault"

    # pytest -rA prints one PASSED/FAILED line per test id; that is the record
    # to score against, not the summary counts, which merge the two sets.
    status = {}
    for line in text.splitlines():
        for kind in ("PASSED", "FAILED", "ERROR"):
            if line.startswith(kind + " "):
                status[line[len(kind) + 1:].strip()] = kind
    def ok(t):
        return status.get(t) == "PASSED"
    f2p_ok = [t for t in f2p if ok(t)]
    p2p_ok = [t for t in p2p if ok(t)]
    resolved = len(f2p_ok) == len(f2p) and len(p2p_ok) == len(p2p)
    return resolved, f"F2P {len(f2p_ok)}/{len(f2p)}  P2P {len(p2p_ok)}/{len(p2p)}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True, help="a json record, or a list of them")
    ap.add_argument("--gold", action="store_true",
                    help="score the benchmark's own patch — the harness self-test")
    ap.add_argument("--patches", default="",
                    help="json map of instance_id -> candidate patch")
    args = ap.parse_args()

    recs = json.load(open(args.data))
    if isinstance(recs, dict):
        recs = [recs]
    patches = json.load(open(args.patches)) if args.patches else {}

    n_ok = 0
    for rec in recs:
        patch = rec["patch"] if args.gold else patches.get(rec["instance_id"], "")
        resolved, detail = run_tests(rec, patch)
        mark = {True: "\033[32mRESOLVED\033[0m", False: "\033[31mfailed  \033[0m",
                None: "\033[33mskipped \033[0m"}[resolved]
        print(f"  {mark}  {rec['instance_id']:<34} {detail}")
        n_ok += bool(resolved)
    print(f"\n  {n_ok}/{len(recs)} resolved")
    return 0


if __name__ == "__main__":
    sys.exit(main())
