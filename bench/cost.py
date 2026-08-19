#!/usr/bin/env python3
"""Cost benchmark: how many tokens to reach the files that actually changed?

This is the claim that matters and the one nobody publishes. "Faster" is a
stopwatch; "cheaper" needs ground truth.

Ground truth comes from git history, so it is entirely local and reproducible:
a bug-fix commit's message is the issue text, and the files it touched are the
answer. That is the same signal an issue→PR pair carries, without needing an
API token or a curated dataset.

Baseline is keyword search — tokenise the query, grep the repo, rank files by
hit count, read the top ones. That is what an agent without a graph actually
does, so it is the honest thing to beat.

Usage: bench/cost.py [repo] [--n 40]
"""
import argparse, json, os, re, subprocess, sys
from collections import Counter

HERE = os.path.dirname(os.path.abspath(__file__))
ARBOR = os.path.join(HERE, "..", "target", "release", "arbor")
SRC_EXT = (".py", ".ts", ".tsx", ".js", ".jsx", ".mts", ".cts", ".mjs", ".cjs")
BYTES_PER_TOKEN = 3.5  # source code tokenises denser than prose


def sh(args, cwd):
    return subprocess.run(args, cwd=cwd, capture_output=True, text=True).stdout


def harvest(repo, want, min_files=2, max_files=8):
    """Bug-fix commits with a bounded blast radius.

    Too few files and the task is trivial; too many and it is a refactor, where
    'the right files' is not a meaningful target for retrieval.
    """
    raw = sh(["git", "log", "--no-merges", "-n", "4000",
              "--pretty=format:%x00%H%x01%s%x01%b", "--name-only"], repo)
    cases = []
    for chunk in raw.split("\x00"):
        if not chunk.strip():
            continue
        head, _, rest = chunk.partition("\n")
        parts = head.split("\x01")
        if len(parts) < 2:
            continue
        sha, subject, body = parts[0], parts[1], (parts[2] if len(parts) > 2 else "")
        files = [f for f in rest.splitlines() if f.strip().endswith(SRC_EXT)]
        # a fix, not a feature or a formatting sweep
        if not re.search(r"\b(fix|bug|error|crash|regress|incorrect|broken)\w*\b",
                         subject, re.I):
            continue
        if not (min_files <= len(files) <= max_files):
            continue
        text = (subject + "\n" + body).strip()
        if len(text) < 30:
            continue
        cases.append({"sha": sha, "text": text, "truth": set(files)})
        if len(cases) >= want:
            break
    return cases


def file_tokens(repo, path, cache={}):
    key = (repo, path)
    if key not in cache:
        try:
            cache[key] = os.path.getsize(os.path.join(repo, path)) / BYTES_PER_TOKEN
        except OSError:
            cache[key] = 0.0
    return cache[key]


def tokenize(text):
    toks = re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", text)
    return [t for t in toks if not t.islower() or "_" in t or len(t) > 6]


def keyword_baseline(repo, text, k, tracked):
    """Rank files by how many query tokens they contain, then read the top k.

    Uses `git grep -l` per token: fast, and it is exactly the move an agent
    makes when it has no structural index.
    """
    counts = Counter()
    for tok in list(set(tokenize(text)))[:12]:
        out = sh(["git", "grep", "-l", "-F", "-w", "--", tok], repo)
        for f in out.splitlines():
            if f.endswith(SRC_EXT) and f in tracked:
                counts[f] += 1
    ranked = [f for f, _ in counts.most_common(k)]
    return ranked, sum(file_tokens(repo, f) for f in ranked)


def arbor_context(repo, text, max_nodes, max_bytes):
    out = sh([ARBOR, "context", text, "-p", repo, "--files-json",
              "--max-nodes", str(max_nodes), "--max-bytes", str(max_bytes)], repo)
    try:
        d = json.loads(out.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        return [], 0.0
    return d["files"], float(d["est_tokens"])


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", nargs="?", default=os.path.join(HERE, "..", "..", ".bench-repos", "django"))
    ap.add_argument("--n", type=int, default=40)
    args = ap.parse_args()
    repo = os.path.abspath(args.repo)

    if not os.path.exists(os.path.join(repo, ".arbor", "graph.bin")):
        subprocess.run([ARBOR, "index", repo], capture_output=True, check=True)

    tracked = set(sh(["git", "ls-files"], repo).splitlines())
    cases = harvest(repo, args.n)
    if not cases:
        print("no suitable bug-fix commits found — try a repo with more history")
        return

    print(f"\ncost benchmark — {os.path.basename(repo)}, {len(cases)} bug-fix commits")
    print("ground truth = files the fix actually touched\n")

    # Sweep both sides. A single operating point tells you nothing about
    # whether an approach is cheap or just truncated.
    approaches = [
        (f"arbor n={n}", (lambda n: lambda t: arbor_context(repo, t, n, n * 4_000))(n))
        for n in (10, 25, 50, 100, 200)
    ] + [
        (f"keyword top-{k}", (lambda k: lambda t: keyword_baseline(repo, t, k, tracked))(k))
        for k in (3, 5, 10, 20)
    ]

    rows = []
    for label, fn in approaches:
        hit, total, toks = 0, 0, 0.0
        for c in cases:
            picked, t = fn(c["text"])
            hit += len(set(picked) & c["truth"])
            total += len(c["truth"])
            toks += t
        recall = 100.0 * hit / max(total, 1)
        avg = toks / len(cases)
        rows.append((label, recall, avg))

    w = max(len(r[0]) for r in rows)
    print(f"  {'approach':<{w}}  {'recall':>8}  {'tokens/query':>13}  {'tokens per':>12}")
    print(f"  {'':<{w}}  {'':>8}  {'':>13}  {'recall point':>12}")
    print("  " + "-" * (w + 40))
    for label, recall, avg in rows:
        per = avg / recall if recall > 0 else float("inf")
        print(f"  {label:<{w}}  {recall:>7.1f}%  {avg:>13,.0f}  {per:>12,.0f}")

    print("\n  recall       share of files the fix touched that the approach surfaced")
    print("  tokens       what the agent would have to read, at 3.5 bytes/token")
    print("  per point    the efficiency number — lower is cheaper for equal answer\n")


if __name__ == "__main__":
    main()
