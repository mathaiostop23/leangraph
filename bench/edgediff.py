#!/usr/bin/env python3
"""What actually changed between two graphs.

Subtracting two totals is not an audit. "The falsified count fell by 24,076 and
the graph shrank by 26,113, so 92% of what left was provably wrong" is a
sentence about two aggregates that were never joined, and it is wrong in both
directions: edges are added as well as removed, and no removed edge was ever
individually checked.

This joins them. Given two edge dumps it reports what left, what arrived, and —
for the edges that left — how many a falsification rule fires on. An edge that
left with no rule firing is unexamined, and saying so is the point.

Usage: bench/edgediff.py before.jsonl after.jsonl [--repo PATH]
"""
import argparse, json, os, sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
import edgefacts as F  # noqa: E402

SEMANTIC = ("calls", "extends", "references")


def key(e):
    return (e["sf"], e["sq"], e["df"], e["dq"], e["ek"])


def load(path):
    out = {}
    for line in open(path):
        e = json.loads(line)
        if e["ek"] in SEMANTIC:
            out.setdefault(key(e), e)
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("before")
    ap.add_argument("after")
    ap.add_argument("--repo", default=None, help="source tree, for the rules that read it")
    args = ap.parse_args()

    a, b = load(args.before), load(args.after)
    removed = [a[k] for k in a.keys() - b.keys()]
    added = [b[k] for k in b.keys() - a.keys()]
    src = F.Source(args.repo or ".")

    print(f"\n\033[1medge diff\033[0m  {os.path.basename(args.before)} → "
          f"{os.path.basename(args.after)}")
    print(f"  semantic edges     {len(a):>9,}  →{len(b):>9,}   "
          f"net {len(b)-len(a):+,}")
    print(f"  removed            {len(removed):>9,}")
    print(f"  added              {len(added):>9,}")

    flagged, by_rule = set(), Counter()
    for i, e in enumerate(removed):
        for rname, fn, _ in F.RULES:
            try:
                hit = fn(e, src)
            except Exception:
                hit = False
            if hit:
                flagged.add(i)
                by_rule[rname] += 1
    print(f"\n  \033[1mof what was removed\033[0m")
    for rname, n in by_rule.most_common():
        print(f"    a rule fires: {rname:<40}{n:>8,}")
    print(f"    \033[1many rule fires{'':<41}{len(flagged):>8,}   "
          f"{100.0*len(flagged)/max(len(removed),1):.1f}%\033[0m")
    unexamined = len(removed) - len(flagged)
    print(f"    \033[1mno rule fires — unexamined{'':<29}{unexamined:>8,}   "
          f"{100.0*unexamined/max(len(removed),1):.1f}%\033[0m")

    print(f"\n  \033[2mremoved, by bucket\033[0m")
    for (prov, conf), n in Counter((e["prov"], e["conf"]) for e in removed).most_common(6):
        print(f"    {prov:<10}conf {conf:<5}{n:>8,}")
    print(f"\n  \033[2madded, by bucket\033[0m")
    for (prov, conf), n in Counter((e["prov"], e["conf"]) for e in added).most_common(6):
        print(f"    {prov:<10}conf {conf:<5}{n:>8,}")
    print("\n  An edge no rule fires on was not checked. It is not therefore wrong,"
          "\n  and it is not therefore right.\n")


if __name__ == "__main__":
    main()
