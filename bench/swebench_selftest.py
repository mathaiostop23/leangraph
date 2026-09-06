#!/usr/bin/env python3
"""Does `swebench.py` measure what it claims? Two arms with a known answer.

A benchmark can be confidently, silently wrong. The fix-mode harness in
`swefix.py` reported zeros for every django instance for a week because it was
matching pytest node ids against ids django writes in its own format, and
nothing about the output looked broken — it looked like a hard benchmark.

So the localization number gets the same treatment. Replace the retriever with
something whose score is known in advance and check the scorer agrees:

    empty    returns nothing         -> recall must be exactly 0%
    oracle   returns the gold files  -> recall must be exactly 100%

A number between the two means the scorer is not comparing the sets it thinks
it is: an absolute path on one side and a repo-relative one on the other, a
prefix dropped in normalisation, or a silent parse failure being counted as an
honest miss. Both arms must land on their endpoint exactly.

    bench/swebench_selftest.py --repos ~/.cache/swebench --limit 40
"""
import argparse, contextlib, importlib.util, io, os, sys

HERE = os.path.dirname(os.path.abspath(__file__))


def load():
    spec = importlib.util.spec_from_file_location(
        "swebench", os.path.join(HERE, "swebench.py"))
    m = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(m)
    return m


def run(mode, repos, limit):
    sb = load()
    state = {"want": []}
    real = sb.patched_files

    def spy(patch):
        state["want"] = real(patch)
        return state["want"]

    sb.patched_files = spy
    # The retriever's signature, ignoring every argument on purpose.
    sb.leangraph_files = lambda repo, text, mn, mb: (
        ([], 0.0) if mode == "empty" else (list(state["want"]), 1000.0))

    sys.argv = ["swebench.py", "--repos", repos, "--limit", str(limit),
                "--reuse-index"]
    buf = io.StringIO()
    with contextlib.redirect_stdout(buf):
        sb.main()
    for line in buf.getvalue().splitlines():
        if line.strip().startswith("ALL"):
            # `recall / tokens` — the first percentage on the row is ours.
            return float(line.split("%")[0].split()[-1])
    return None


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repos", required=True)
    ap.add_argument("--limit", type=int, default=40)
    args = ap.parse_args()

    print(f"\n  self-test over {args.limit} instances\n")
    bad = 0
    for mode, expect in (("empty", 0.0), ("oracle", 100.0)):
        got = run(mode, args.repos, args.limit)
        ok = got is not None and abs(got - expect) < 1e-9
        bad += not ok
        mark = "\033[32mok\033[0m" if ok else "\033[31mWRONG\033[0m"
        print(f"    {mode:<7} expected {expect:5.1f}%   got "
              f"{'—' if got is None else f'{got:5.1f}%'}   {mark}")

    if bad:
        print("\n  \033[31mThe scorer does not agree with a known answer. "
              "Every number it has produced is suspect.\033[0m\n")
        return 1
    print("\n  Both endpoints exact: the scorer compares the sets it claims to.\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
