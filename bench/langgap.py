#!/usr/bin/env python3
"""What one language is missing, in enough detail to fix it.

`verify.py` gives the number; this gives the reason. A recall figure tells you
a language is behind without telling you whether it is one construct missed
everywhere or a long tail of unrelated cases — and those need completely
different work.

Usage: bench/langgap.py <repo> [--sample N]
"""
import importlib.util, os, sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
spec = importlib.util.spec_from_file_location("v", os.path.join(HERE, "verify.py"))
v = importlib.util.module_from_spec(spec)
spec.loader.exec_module(v)


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 1
    name = sys.argv[1]
    n_sample = 8
    if "--sample" in sys.argv:
        n_sample = int(sys.argv[sys.argv.index("--sample") + 1])

    repo = os.path.abspath(os.path.join(v.REPOS, name))
    db = os.path.join(repo, ".codegraph", "codegraph.db")
    if not os.path.exists(db):
        print(f"no oracle index at {db}")
        return 1

    ours = v.leangraph_nodes(repo, repo)
    theirs = v.codegraph_nodes(db, repo)
    op = {(f, n) for f, n, _ in ours}
    tp = {(f, n) for f, n, _ in theirs}
    missing = tp - op

    print(f"\n\033[1m{name}\033[0m — {len(missing)} of {len(tp)} symbols missing "
          f"({100.0 * len(missing) / max(len(tp), 1):.1f}%)")

    by_kind = Counter(k for f, n, k in theirs if (f, n) in missing)
    print("\n  \033[1mmissing by kind\033[0m")
    for k, c in by_kind.most_common():
        print(f"    {k:<12} {c:>6}")

    print("\n  \033[1mmissing names that repeat\033[0m  (one construct, many sites)")
    for n, c in Counter(n for f, n in missing).most_common(10):
        if c > 1:
            print(f"    {c:>5}  {n}")

    print("\n  \033[1mfiles with the most missing\033[0m")
    for f, c in Counter(f for f, n in missing).most_common(5):
        print(f"    {c:>5}  {f}")

    print("\n  \033[1msample\033[0m")
    for f, n in sorted(missing)[:n_sample]:
        print(f"    {n:<34} {f}")

    ours_k = {(f, n): k for f, n, k in ours}
    pairs = Counter()
    for f, n, k in theirs:
        if (f, n) in op and ours_k.get((f, n)) != k:
            pairs[(k, ours_k.get((f, n)))] += 1
    if pairs:
        print("\n  \033[1mfound but labelled differently\033[0m")
        for (t, o), c in pairs.most_common(6):
            print(f"    {c:>5}  they say {t:<10} we say {o}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
