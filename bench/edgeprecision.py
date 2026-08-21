#!/usr/bin/env python3
"""Precision, on the subset where it can be settled rather than estimated.

Everything else here bounds the error rate without measuring it. `edgefacts`
gives a floor — 0.8% of django's edges break a rule — and the fan-out gives a
ceiling — at most 51% of call edges can be right, because a call site has one
target and we emit several candidates. Between 0.8% and 49% is not a bound worth
quoting.

There is a population where the answer is not an estimate. When a caller runs
and the tracer records which function it actually reached, every *other*
candidate we emitted for that call is wrong. Not unconfirmed — wrong. That makes
precision measurable directly, on the subset the test suite exercised.

The assumption, stated because it is the whole weight of the method: a caller's
calls to one name are taken to have all been exercised. Where a function calls
`render` at two sites that reach different targets and only one ran, the other
target is counted as an error it is not. That inflates the error rate, so the
number here is **pessimistic** rather than flattering — which is the direction
to be wrong in.

Usage: bench/edgeprecision.py <repo> --trace <trace.jsonl>
"""
import json, os, subprocess, sys
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from graphjoin import norm_qual, norm_path  # noqa: E402

LEANGRAPH = os.path.join(HERE, "..", "target", "release", "leangraph")
REPOS = os.environ.get("LEANGRAPH_BENCH_REPOS", os.path.join(HERE, "..", "..", ".bench-repos"))


def wilson(k, n):
    """95% interval. A rate over 30 edges is not a rate without one."""
    if n == 0:
        return (0.0, 0.0)
    z, p = 1.96, k / n
    d = 1 + z * z / n
    c = p + z * z / (2 * n)
    s = z * ((p * (1 - p) / n + z * z / (4 * n * n)) ** 0.5)
    return (100 * (c - s) / d, 100 * (c + s) / d)


def main():
    if len(sys.argv) < 2 or "--trace" not in sys.argv:
        print(__doc__)
        return 1
    name = sys.argv[1]
    trace_path = sys.argv[sys.argv.index("--trace") + 1]
    repo = os.path.abspath(os.path.join(REPOS, name))

    raw = subprocess.run([LEANGRAPH, "dump", "-p", repo, "--what", "edges"],
                         capture_output=True, text=True, check=True).stdout
    emitted = [json.loads(l) for l in raw.splitlines()]

    ran = set()
    observed = defaultdict(set)      # caller -> {(file, qual)}
    for rec in map(json.loads, open(trace_path)):
        if rec["kind"] == "ran":
            ran.add((norm_path(rec["f"], repo), norm_qual(rec["q"])))
        # hops 0 is the direct call. Anything above it is the tracer walking
        # up through frames the graph never claimed an edge for, and counting
        # those as observed targets would make an unrelated caller look right.
        elif rec["kind"] == "edge" and rec.get("hops", 0) == 0:
            caller = (norm_path(rec["cf"], repo), norm_qual(rec["cq"]))
            observed[caller].add((norm_path(rec["df"], repo), norm_qual(rec["dq"])))

    # Emitted call edges, grouped by the call they are candidates for.
    groups = defaultdict(list)
    for e in emitted:
        if e["ek"] != "calls":
            continue
        caller = (norm_path(e["sf"], repo), norm_qual(e["sq"]))
        target = (norm_path(e["df"], repo), norm_qual(e["dq"]))
        leaf = target[1].rsplit(".", 1)[-1]
        groups[(caller, leaf)].append((target, e))

    # Adjudicate: only where the caller ran and reached something by this name.
    right = defaultdict(int)
    wrong = defaultdict(int)
    settled_groups = 0
    for (caller, leaf), cands in groups.items():
        if caller not in ran:
            continue
        hit = {t for t in observed.get(caller, ()) if t[1].rsplit(".", 1)[-1] == leaf}
        if not hit:
            continue
        settled_groups += 1
        for target, e in cands:
            bucket = (e["conf"], e["prov"])
            if target in hit:
                right[bucket] += 1
            else:
                wrong[bucket] += 1

    print(f"\n\033[1m  precision on the adjudicated subset\033[0m — {name}")
    print(f"  {settled_groups:,} call sites where the tracer settled the target\n")
    print(f"     {'conf':>4} {'prov':<10} {'edges':>7} {'right':>7} {'wrong':>7} "
          f"{'precision':>10}   95% interval")
    tot_r = tot_w = 0
    for bucket in sorted(set(right) | set(wrong), key=lambda b: (-b[0], b[1])):
        r, w = right[bucket], wrong[bucket]
        n = r + w
        lo, hi = wilson(r, n)
        tot_r, tot_w = tot_r + r, tot_w + w
        note = "" if n >= 30 else "  (too few to read as a rate)"
        print(f"     {bucket[0]:>4} {bucket[1]:<10} {n:>7,} {r:>7,} {w:>7,} "
              f"{100 * r / n:>9.1f}% {lo:>6.1f} – {hi:>5.1f}{note}")
    n = tot_r + tot_w
    if n:
        lo, hi = wilson(tot_r, n)
        print(f"\n     {'all':>4} {'':<10} {n:>7,} {tot_r:>7,} {tot_w:>7,} "
              f"{100 * tot_r / n:>9.1f}% {lo:>6.1f} – {hi:>5.1f}")
    print("\n  Pessimistic: a second call site for the same name that did not run"
          "\n  counts its target as an error. Wrong in the safe direction.\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
