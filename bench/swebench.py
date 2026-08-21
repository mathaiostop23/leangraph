#!/usr/bin/env python3
"""Localization on SWE-bench Verified: real issue text, real answers.

`cost.py` uses a bug-fix commit's message as the query and the files it touched
as the answer. That is local and reproducible, and it leaks: a commit message is
written *after* the fix, by someone who knows it, and often names the function
that changed. Real issue text does not. The leak flatters us, which is the
dangerous direction — nobody audits a number that agrees with them.

Here the query is the issue as it was filed, before anyone knew the answer, and
the ground truth is the files the accepted patch touched. Five hundred instances
across twelve repositories, each pinned to the commit the issue was filed
against.

What this measures is **localization**: of the files that had to change, how
many are in the context we return, and what did returning it cost. It does not
measure whether an answer built on that context is correct — that needs the
benchmark's tests, which is a separate run.

Usage:
  bench/swebench.py --repos <dir> [--data verified.json] [--limit N] [--only repo]
"""
import argparse, json, os, re, subprocess, sys, time
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
LEANGRAPH = os.path.join(HERE, "..", "target", "release", "leangraph")
BYTES_PER_TOKEN = 3.5  # source tokenises denser than prose

STOP = {
    "the", "and", "for", "not", "but", "with", "from", "this", "that", "when",
    "then", "than", "have", "has", "was", "are", "you", "all", "any", "can",
    "may", "use", "using", "used", "get", "set", "add", "new", "one", "two",
    "out", "off", "its", "our", "how", "why", "who", "what", "which", "where",
    "some", "only", "also", "into", "over", "same", "such", "each", "more",
    "most", "other", "should", "would", "could", "does", "did", "done", "make",
    "made", "see", "seen", "call", "called", "run", "raise", "error", "issue",
    "bug", "fix", "test", "code", "file", "line", "python", "expected",
}


def sh(args, cwd=None, check=False):
    r = subprocess.run(args, cwd=cwd, capture_output=True, text=True)
    if check and r.returncode:
        raise RuntimeError(f"{' '.join(args[:3])}: {r.stderr[:200]}")
    return r.stdout


def patched_files(patch):
    """The files the accepted fix touched — the answer.

    Test files are excluded. A localizer that points at the test proving the
    bug has not found the bug, and SWE-bench keeps the tests in `test_patch`
    anyway; any that leak into `patch` would be scored as a hit for free.
    """
    out = []
    for m in re.finditer(r"^diff --git a/(\S+) b/(\S+)", patch, re.M):
        f = m.group(2)
        base = os.path.basename(f)
        if base.startswith("test_") or base.endswith("_test.py") or "/tests/" in f:
            continue
        out.append(f)
    return sorted(set(out))


def leangraph_files(repo, text, max_nodes, max_bytes):
    out = sh([LEANGRAPH, "context", text, "-p", repo, "--files-json",
              "--max-nodes", str(max_nodes), "--max-bytes", str(max_bytes)], repo)
    try:
        d = json.loads(out.strip().splitlines()[-1])
    except (json.JSONDecodeError, IndexError):
        return [], 0.0
    return d["files"], float(d["est_tokens"])


def keyword_ranked(repo, text):
    """Files ranked by how many distinct query terms hit them.

    `git grep -l` because it respects the index and skips the junk a plain walk
    would read — the baseline should be the good version of itself.
    """
    terms = [t for t in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", text)
             if t.lower() not in STOP][:24]
    hits = defaultdict(int)
    for t in set(terms):
        for f in sh(["git", "grep", "-l", "-F", "--", t], cwd=repo).splitlines():
            if f.endswith(".py"):
                hits[f] += 1
    return sorted(hits, key=lambda f: (-hits[f], f))


def take_within(repo, ranked, budget):
    """The top of the ranking that fits in a token budget.

    The equal-cost comparison. Reading eighteen times more and finding less is
    one result; finding less *at the same cost* is the one a sceptic asks for,
    and it is the harder of the two to argue with.
    """
    out, spent = [], 0.0
    for f in ranked:
        t = file_tokens(repo, f)
        if spent + t > budget and out:
            break
        out.append(f)
        spent += t
    return out, spent


def keyword_files(repo, text, top):
    """What an agent without a structural index actually does.

    Tokenise the issue, grep for each term, rank files by how many distinct
    terms hit. `git grep -l` because it respects the index and skips the junk a
    plain walk would read.
    """
    terms = [t for t in re.findall(r"[A-Za-z_][A-Za-z0-9_]{2,}", text)
             if t.lower() not in STOP][:24]
    hits = defaultdict(int)
    for t in set(terms):
        for f in sh(["git", "grep", "-l", "-F", "--", t], cwd=repo).splitlines():
            if f.endswith(".py"):
                hits[f] += 1
    ranked = sorted(hits, key=lambda f: (-hits[f], f))[:top]
    return ranked, sum(file_tokens(repo, f) for f in ranked)


def file_tokens(repo, rel):
    try:
        return os.path.getsize(os.path.join(repo, rel)) / BYTES_PER_TOKEN
    except OSError:
        return 0.0


def recall(found, want):
    if not want:
        return None
    return len(set(found) & set(want)) / len(want)


def wilson(k, n):
    if n == 0:
        return (0.0, 0.0)
    z, p = 1.96, k / n
    d = 1 + z * z / n
    c = p + z * z / (2 * n)
    s = z * ((p * (1 - p) / n + z * z / (4 * n * n)) ** 0.5)
    return (100 * (c - s) / d, 100 * (c + s) / d)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repos", required=True, help="directory of checkouts")
    ap.add_argument("--data", default="/tmp/swebench/verified.json")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--only", default="")
    ap.add_argument("--max-nodes", type=int, default=100)
    ap.add_argument("--max-bytes", type=int, default=100_000)
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    rows = json.load(open(args.data))
    if args.only:
        rows = [r for r in rows if args.only in r["repo"]]
    have = {r for r in os.listdir(args.repos)
            if os.path.isdir(os.path.join(args.repos, r, ".git"))}
    rows = [r for r in rows if r["repo"].split("/")[1] in have]
    if args.limit:
        rows = rows[:args.limit]
    if not rows:
        print("no instances match the checkouts present")
        return 1

    print(f"\n\033[1mSWE-bench Verified — localization\033[0m")
    print(f"  {len(rows)} instances, {len({r['repo'] for r in rows})} repositories")
    print(f"  query: the issue as filed.  answer: the files the accepted patch touched.\n")

    results = []
    t0 = time.time()
    for i, r in enumerate(rows, 1):
        name = r["repo"].split("/")[1]
        # Absolute: the queries run with cwd inside the checkout, so a path
        # relative to the harness would resolve to nothing there — and
        # `context` answers an empty file set rather than failing.
        repo = os.path.abspath(os.path.join(args.repos, name))
        want = patched_files(r["patch"])
        if not want:
            continue
        # The tree as it stood when the issue was filed.
        sh(["git", "checkout", "-q", "-f", r["base_commit"]], cwd=repo)
        sh(["git", "clean", "-qfd", "-e", ".leangraph"], cwd=repo)
        subprocess.run([LEANGRAPH, "index", repo], capture_output=True)

        text = r["problem_statement"]
        lg, lg_tok = leangraph_files(repo, text, args.max_nodes, args.max_bytes)
        ranked = keyword_ranked(repo, text)
        kw = ranked[:10]
        kw_tok = sum(file_tokens(repo, f) for f in kw)
        # ...and the same baseline held to leangraph's own token budget.
        eq, eq_tok = take_within(repo, ranked, lg_tok)
        results.append({
            "instance": r["instance_id"], "repo": r["repo"], "want": want,
            "lg": recall(lg, want), "lg_tokens": lg_tok,
            "kw": recall(kw, want), "kw_tokens": kw_tok,
            "eq": recall(eq, want), "eq_tokens": eq_tok, "eq_files": len(eq),
        })
        if i % 10 == 0 or i == len(rows):
            el = time.time() - t0
            print(f"  {i}/{len(rows)}  {el:.0f}s", end="\r", flush=True)
    print(" " * 40, end="\r")

    if args.out:
        json.dump(results, open(args.out, "w"))

    def report(label, rows):
        n = len(rows)
        if not n:
            return
        lg = sum(x["lg"] for x in rows) / n
        kw = sum(x["kw"] for x in rows) / n
        eq = sum(x["eq"] for x in rows) / n
        lgt = sum(x["lg_tokens"] for x in rows) / n
        kwt = sum(x["kw_tokens"] for x in rows) / n
        eqt = sum(x["eq_tokens"] for x in rows) / n
        # Any-hit: did the context contain at least one file that had to change?
        lg_any = sum(1 for x in rows if x["lg"] > 0)
        kw_any = sum(1 for x in rows if x["kw"] > 0)
        lo, hi = wilson(lg_any, n)
        print(f"  {label:<16} {n:>4}   "
              f"{lg * 100:>5.1f}% / {lgt:>7,.0f}   "
              f"{eq * 100:>5.1f}% / {eqt:>7,.0f}   "
              f"{kw * 100:>5.1f}% / {kwt:>9,.0f}   "
              f"{100 * lg_any / n:>5.1f}% [{lo:.0f}–{hi:.0f}]")

    print(f"\n\033[1m  file recall / mean tokens per query\033[0m")
    print(f"  {'':<16} {'n':>4}   {'leangraph':^15}   {'keyword, same':^15}   "
          f"{'keyword top-10':^17}   leangraph")
    print(f"  {'':<16} {'':>4}   {'':^15}   {'budget':^15}   {'':^17}   at-least-one")
    by_repo = defaultdict(list)
    for x in results:
        by_repo[x["repo"]].append(x)
    for repo in sorted(by_repo, key=lambda r: -len(by_repo[r])):
        report(repo.split("/")[1], by_repo[repo])
    print()
    report("ALL", results)
    # --- how much of this is one repository? ---------------------------------
    # Every number above is a mean over instances, and instances are not
    # independent: django is 231 of the 500 and its idioms are its own. A
    # bootstrap over *repositories* asks the question that generalises — how
    # much would this move if the twelve had been twelve others.
    if len(by_repo) > 2:
        import random
        rng = random.Random(20260101)
        names = list(by_repo)
        draws = []
        for _ in range(2000):
            pick = [rng.choice(names) for _ in names]
            xs = [x for nm in pick for x in by_repo[nm]]
            draws.append(sum(x["lg"] for x in xs) / len(xs))
        draws.sort()
        lo, hi = draws[int(0.025 * len(draws))], draws[int(0.975 * len(draws))]
        flat = sum(x["lg"] for x in results) / len(results)
        print(f"\n\033[1m  bootstrapped over repositories\033[0m, not over instances")
        print(f"  file recall {flat * 100:.1f}%   95% CI [{lo * 100:.1f}, {hi * 100:.1f}]"
              f"   over {len(names)} repositories")
        print("  Wide, and it should be: twelve repositories is a small sample of"
              "\n  repositories however many instances they carry.")

    print("\n  Localization only. Whether an answer built on this context is"
          "\n  correct is the benchmark's tests, which is a different run.\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
