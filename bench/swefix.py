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
import argparse, re, json, shlex, os, subprocess, sys, tempfile

IMAGE = "swebench/sweb.eval.arm64.{}:latest"


def image_for(instance_id):
    # `astropy__astropy-12907` -> `astropy_1776_astropy-12907`
    return IMAGE.format(instance_id.replace("__", "_1776_"))


def have_image(tag):
    return subprocess.run(["docker", "image", "inspect", tag],
                          capture_output=True).returncode == 0


# Django does not use pytest. Its instances name tests the way its own runner
# does — `test_accent (dbshell.test_postgresql.PostgreSqlDbshellCommandTestCase)`
# — which no pytest node id will ever match, so every django instance scored
# zero passes and looked like a broken environment. It is not broken; it was
# being asked the wrong question.
def runner_for(repo, tests):
    if repo == "django/django":
        # `a.b.C.test_x` is the label the runner takes; the benchmark writes
        # the same thing as `test_x (a.b.C)`.
        labels = []
        for t in tests:
            m = re.match(r"^(\S+) \(([^)]+)\)\s*$", t)
            labels.append(f"{m.group(2)}.{m.group(1)}" if m else t)
        cmd = ("python tests/runtests.py --verbosity 2 --settings=test_sqlite "
               "--parallel 1 " + " ".join(shlex.quote(l) for l in labels))
        keep = r"grep -E ' \.\.\. (ok|FAIL|ERROR|skipped)' /tmp/t.log"
    else:
        # shlex.quote, not a pair of literal quotes. astropy names a test
        # `test_non_mapping_init[ceci n'est pas un dict]`; wrapping that in
        # single quotes ends the quote at the apostrophe, pytest receives a
        # truncated id, and one unknown id makes pytest run *nothing at all* —
        # "no tests ran", scored as 0/644 and written off as a broken image.
        cmd = ("python -m pytest -rA --no-header -q --continue-on-collection-errors "
               + " ".join(shlex.quote(t) for t in tests))
        keep = r"grep -E '^(PASSED|FAILED|ERROR) ' /tmp/t.log"
    # Never pipe the run straight into `tail`. The first version ended in
    # `tail -120`, which silently discarded the per-test lines for any instance
    # with more than about a hundred tests — a 732-test suite reported 0/732 and
    # was recorded as a broken environment. Filter first, truncate second, and
    # keep a short raw tail so a real crash is still visible.
    return (f"{cmd} > /tmp/t.log 2>&1 || true\n"
            f"{keep} | head -5000\n"
            'echo "---RAW-TAIL---"\n'
            "tail -25 /tmp/t.log")


def usable_ids(tests):
    """Drop ids the published dataset has already broken.

    SWE-bench Verified ships parametrised ids split on whitespace: astropy's
    `test_non_mapping_init[ceci n'est pas un dict]` is stored as three entries,
    the first being `test_non_mapping_init[ceci`. This is upstream, in the
    parquet on HuggingFace, not something the fetcher did — it was checked.

    Passing one of these to pytest is a usage error, not a collection error, so
    `--continue-on-collection-errors` does not save the run: pytest exits
    having run nothing, and an instance with 644 good ids reports 0/644 and
    looks like a broken image. An id that opens a bracket and never closes it
    cannot name a real test, so it is dropped and the rest are scored.
    """
    return [t for t in tests if t.count("[") == t.count("]")]


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
    f2p, p2p = usable_ids(f2p), usable_ids(p2p)

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
  # Three attempts, weakest bookkeeping requirement last. A model gets the
  # edit right and the hunk arithmetic wrong often enough that scoring only
  # what `git apply` accepts strictly measures diff clerking, not the fix.
  # None of these change which lines are added or removed.
  if   git apply -v            /work/fix.diff 2>/tmp/apply.log; then echo "APPLIED_STRICT"
  elif git apply -v --recount  /work/fix.diff 2>>/tmp/apply.log; then echo "APPLIED_RECOUNT"
  elif patch -p1 --fuzz=3 -f < /work/fix.diff >>/tmp/apply.log 2>&1; then echo "APPLIED_FUZZ"
  else echo "APPLY_FAILED"; cat /tmp/apply.log; exit 9; fi
fi
git apply -v /work/test.diff 2>/dev/null || { echo "TEST_PATCH_FAILED"; exit 8; }
echo "---TESTS---"
__RUN__
""".replace("__RUN__", runner_for(rec["repo"], f2p + p2p))
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
        # Django's runner: `test_x (a.b.C) ... ok`. The part before ` ... ` is
        # byte-for-byte the id the benchmark lists, so it needs no conversion
        # back — only the outcome word does.
        if " ... " in line:
            name, _, verdict = line.rpartition(" ... ")
            v = verdict.strip()
            if v.startswith("ok"):
                status[name.strip()] = "PASSED"
            elif v.startswith(("FAIL", "ERROR")):
                status[name.strip()] = "FAILED"

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
    ap.add_argument("--gate", action="store_true",
                    help="score the gold patch first and drop any instance it "
                         "fails; those measure the harness, not the candidate")
    args = ap.parse_args()

    recs = json.load(open(args.data))
    if isinstance(recs, dict):
        recs = [recs]
    patches = json.load(open(args.patches)) if args.patches else {}

    # An instance whose own gold patch does not turn its tests green is not
    # measuring the candidate — astropy-13236 reported F2P 0/2 and P2P 0/644
    # under gold, and scored every candidate against it as a failure. Dropping
    # it is not leniency; counting it is a wrong denominator.
    excluded = []
    if args.gate and not args.gold:
        for rec in list(recs):
            ok, why = run_tests(rec, rec["patch"])
            if not ok:
                excluded.append((rec["instance_id"], why))
                recs.remove(rec)
                print(f"  \033[33mexcluded\033[0m  {rec['instance_id']:<34} "
                      f"gold itself fails here — {why}")

    n_ok = 0
    for rec in recs:
        patch = rec["patch"] if args.gold else patches.get(rec["instance_id"], "")
        resolved, detail = run_tests(rec, patch)
        mark = {True: "\033[32mRESOLVED\033[0m", False: "\033[31mfailed  \033[0m",
                None: "\033[33mskipped \033[0m"}[resolved]
        print(f"  {mark}  {rec['instance_id']:<34} {detail}")
        n_ok += bool(resolved)

    if not recs:
        print("\n  nothing scorable — every instance failed its own gold patch")
        return 1
    print(f"\n  {n_ok}/{len(recs)} resolved")
    if excluded:
        print(f"  {len(excluded)} excluded by the gold gate, and not in that "
              f"denominator: {', '.join(i for i, _ in excluded)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
