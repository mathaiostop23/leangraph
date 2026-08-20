#!/usr/bin/env python3
"""Differential verification against CodeGraph as an oracle.

Speed means nothing if the graph is wrong. CodeGraph is MIT-licensed, runnable,
and validates its own extraction as byte-identical against a reference engine
across 31 repos — which makes it the best available ground truth we did not
have to build.

This is *agreement*, not truth: where we differ, either side may be right. The
point is to make every difference visible and force a look, rather than shipping
a speed number with an unexamined graph behind it.

Usage: bench/verify.py [repo ...]
"""
import json, os, sqlite3, subprocess, sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
LEANGRAPH = os.path.join(HERE, "..", "target", "release", "leangraph")
REPOS = os.environ.get("LEANGRAPH_BENCH_REPOS", os.path.join(HERE, "..", "..", ".bench-repos"))

# CodeGraph models far more node kinds than we do. Comparing on kinds we do not
# claim to extract would measure our scope, not our correctness, so we fold both
# vocabularies onto the set we actually target and ignore the rest.
KINDS = {
    "function": "function", "fn": "function",
    "method": "method",
    "class": "class", "struct": "class", "enum": "class",
    "interface": "interface", "iface": "interface",
    "type_alias": "interface", "trait": "interface", "protocol": "interface",
    "variable": "variable", "var": "variable", "constant": "variable",
    "property": "variable", "field": "variable",
}


def norm_path(p, root):
    p = os.path.normpath(p)
    if os.path.isabs(p):
        try:
            p = os.path.relpath(p, root)
        except ValueError:
            pass
    return p.replace("\\", "/")


def codegraph_nodes(db, root):
    con = sqlite3.connect(db)
    out = set()
    for kind, name, path in con.execute("select kind, name, file_path from nodes"):
        k = KINDS.get(kind)
        if k:
            out.add((norm_path(path, root), name, k))
    con.close()
    return out


def leangraph_nodes(repo, root):
    raw = subprocess.run([LEANGRAPH, "dump", "-p", repo, "--what", "nodes"],
                         capture_output=True, text=True, check=True).stdout
    out = set()
    for line in raw.splitlines():
        d = json.loads(line)
        k = KINDS.get(d["kind"])
        if k:
            out.add((norm_path(d["file"], root), d["name"], k))
    return out


def report(name, ours, theirs):
    # Two different questions, and conflating them hides which one you failed:
    #   presence — is the symbol in our graph at all?
    #   kind     — do we agree on what it is?
    # A node we extract but label `variable` where they say `method` is a
    # taxonomy difference, not a missing node, and the fix is completely
    # different.
    ours_p = {(f, n) for f, n, _ in ours}
    theirs_p = {(f, n) for f, n, _ in theirs}

    present = ours_p & theirs_p
    absent = theirs_p - ours_p
    recall_p = 100.0 * len(present) / max(len(theirs_p), 1)
    recall_k = 100.0 * len(ours & theirs) / max(len(theirs), 1)

    print(f"\n\033[1m{name}\033[0m")
    print(f"  \033[1mpresence recall  {recall_p:>6.1f}%\033[0m   "
          f"({len(present)} of {len(theirs_p)} symbols found)")
    print(f"  kind agreement   {recall_k:>6.1f}%   "
          f"({len(ours & theirs)} of {len(theirs)} also match on kind)")
    print(f"  we add           {len(ours_p - theirs_p):>7} symbols they do not have")

    if absent:
        print(f"\n  \033[2mgenuinely missing — most common kinds\033[0m")
        by_kind = Counter(k for f, n, k in theirs if (f, n) in absent)
        for k, n in by_kind.most_common(4):
            print(f"    {k:<12} {n}")
        print(f"  \033[2msample\033[0m")
        for f, n in sorted(absent)[:5]:
            print(f"    {n:<28} {f}")

    mislabelled = present - {(f, n) for f, n, _ in (ours & theirs)}
    if mislabelled:
        print(f"\n  \033[2m{len(mislabelled)} found but labelled differently\033[0m")
    return recall_p


def main():
    targets = sys.argv[1:] or ["flask", "excalidraw", "django"]
    print("Differential verification vs CodeGraph (agreement, not truth)")
    recalls = []
    for r in targets:
        repo = os.path.abspath(os.path.join(REPOS, r))
        db = os.path.join(repo, ".codegraph", "codegraph.db")
        if not os.path.exists(db):
            print(f"\n{r}: no CodeGraph index — run bench/run.sh first")
            continue
        if not os.path.exists(os.path.join(repo, ".leangraph", "graph.bin")):
            subprocess.run([LEANGRAPH, "index", repo], capture_output=True, check=True)
        recalls.append(report(r, leangraph_nodes(repo, repo), codegraph_nodes(db, repo)))

    if recalls:
        print(f"\n\033[1mpresence recall across {len(recalls)} repos: "
              f"{sum(recalls)/len(recalls):.1f}%\033[0m")
        print("\nPresence recall is what gates a speed claim: a graph that is fast\n"
              "because it is missing symbols is not fast, it is wrong. 'We add' is\n"
              "not automatically wrong — but every genuinely missing symbol is\n"
              "worth one look before shipping.\n")


if __name__ == "__main__":
    main()
