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

Every instance is indexed from scratch. `--reuse-index` is faster and is how the
first runs were done, but it makes each measurement depend on the commit measured
before it; see BENCH.md on why that is not one measurement.

Usage:
  bench/swebench_fetch.py                       # dataset + the twelve checkouts
  bench/swebench.py --repos <dir> [--limit N] [--only repo] [--baseline out.json]
"""
import argparse, json, os, re, subprocess, sys, time
from collections import defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
MODEL_NOTE = "all-MiniLM-L6-v2, chunked at 40 lines"
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


def paired(now, before):
    """The same instances, twice, compared one at a time.

    Two aggregate means cannot tell a change that helps forty instances and
    hurts thirty-nine from one that does nothing: both report +0.2%. And with
    django at 231 of 500, a mean is mostly a statement about django. Pairing
    asks the question a change to seeding actually raises — *which* instances
    moved, and in which direction.

    The sign test is over instances that moved at all, which is the only
    population the question is about; ties carry no information about direction
    and inflating n with them is how a null result gets reported as a win.
    """
    was = {x["instance"]: x for x in before}
    both = [(x, was[x["instance"]]) for x in now if x["instance"] in was]
    if not both:
        print("\n  baseline shares no instances with this run")
        return

    up = [(a, b) for a, b in both if a["lg"] > b["lg"]]
    down = [(a, b) for a, b in both if a["lg"] < b["lg"]]
    # The bucket this change was built for: context that held nothing useful.
    rescued = [a for a, b in both if b["lg"] == 0 and a["lg"] > 0]
    lost = [a for a, b in both if b["lg"] > 0 and a["lg"] == 0]
    d_recall = (sum(a["lg"] for a, _ in both) - sum(b["lg"] for _, b in both)) / len(both)
    d_tok = (sum(a["lg_tokens"] for a, _ in both)
             - sum(b["lg_tokens"] for _, b in both)) / len(both)
    blind_before = sum(1 for _, b in both if b["lg"] == 0)

    print(f"\n\033[1m  against the baseline\033[0m, the same {len(both)} instances")
    print(f"  recall      {d_recall * 100:+.1f} points      "
          f"tokens {d_tok:+,.0f} per query")
    print(f"  moved       {len(up)} better, {len(down)} worse, "
          f"{len(both) - len(up) - len(down)} unchanged")
    print(f"  empty       {len(rescued)} of {blind_before} instances that had nothing "
          f"now have something; {len(lost)} went the other way")

    # Sign test: if the change were noise, better and worse would be a coin flip.
    m = len(up) + len(down)
    if m:
        k = min(len(up), len(down))
        c = [1.0]
        for i in range(1, m + 1):
            c.append(c[-1] * (m - i + 1) / i)
        p = 2 * sum(c[i] for i in range(k + 1)) / (2 ** m)
        print(f"  sign test   p = {min(p, 1.0):.4g} over the {m} that moved")

    by_repo = defaultdict(lambda: [0, 0, 0.0])
    for a, b in both:
        r = by_repo[a["repo"].split("/")[1]]
        r[0] += a["lg"] > b["lg"]
        r[1] += a["lg"] < b["lg"]
        r[2] += a["lg"] - b["lg"]
    moved = {k: v for k, v in by_repo.items() if v[0] or v[1]}
    if moved:
        print("  per repo   ", "  ".join(
            f"{k} {v[0]}+/{v[1]}-" for k, v in
            sorted(moved.items(), key=lambda kv: -abs(kv[1][2]))))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--repos", required=True, help="directory of checkouts")
    ap.add_argument("--data", default=os.path.expanduser("~/.cache/swebench/verified.json"))
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--only", default="")
    ap.add_argument("--max-nodes", type=int, default=100)
    ap.add_argument("--max-bytes", type=int, default=100_000)
    ap.add_argument("--out", default="")
    ap.add_argument("--reuse-index", action="store_true",
                    help="keep .leangraph between instances — faster, and every "
                         "measurement then depends on the one before it")
    ap.add_argument("--baseline", default="",
                    help="a previous --out, to compare against per instance")
    ap.add_argument("--rag", action="store_true",
                    help="also run the embedding baseline (needs sentence-transformers)")
    ap.add_argument("--codegraph", default="",
                    help="path to the codegraph binary; adds it as a column")
    ap.add_argument("--codegraph-nodes", type=int, default=35,
                    help="node budget for the leangraph row held to codegraph's "
                         "own token cost. `explore` does not grow when given a "
                         "larger --max-files, so the equal-cost comparison has "
                         "to bring us down to it rather than send it up to us")
    ap.add_argument("--rag-cache",
                    default=os.path.expanduser("~/.cache/swebench/rag.pkl"))
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

    cg = None
    if args.codegraph:
        sys.path.insert(0, HERE)
        import cgbase as cg
        cg.BIN = cg.available(os.path.abspath(args.codegraph))
        print(f"  codegraph: {cg.BIN}, indexed fresh per instance\n")

    rag = None
    if args.rag:
        sys.path.insert(0, HERE)
        import ragbase as rag
        n_cached = rag.open_cache(args.rag_cache)
        print(f"  embedding baseline: {MODEL_NOTE}, {n_cached:,} files already cached\n")

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
        # A fresh index by default. Keeping `.leangraph` makes each instance an
        # incremental sync from whichever commit ran before it, and two runs of
        # the same binary then disagree on 20 of these 500 — a 4% noise floor
        # under every result. A reindex costs 11% of the run and removes it.
        if not args.reuse_index:
            subprocess.run(["rm", "-rf", os.path.join(repo, ".leangraph")])
        subprocess.run([LEANGRAPH, "index", repo], capture_output=True)

        text = r["problem_statement"]
        lg, lg_tok = leangraph_files(repo, text, args.max_nodes, args.max_bytes)
        ranked = keyword_ranked(repo, text)
        kw = ranked[:10]
        kw_tok = sum(file_tokens(repo, f) for f in kw)
        # ...and the same baseline held to leangraph's own token budget.
        eq, eq_tok = take_within(repo, ranked, lg_tok)

        cg_r, cg_any_r, cg_tok, cg_src = None, None, 0.0, 0.0
        lg_eq_r, lg_eq_tok = None, 0.0
        if cg is not None:
            cg.index(repo)
            src, cited, cg_tok, cg_src = cg.explore(repo, text)
            cg_r = recall(src, want)
            cg_any_r = recall(cited, want)
            # ...and us, held to roughly what it spent.
            eq_files, lg_eq_tok = leangraph_files(
                repo, text, args.codegraph_nodes, args.max_bytes)
            lg_eq_r = recall(eq_files, want)

        rg, rg_tok = None, 0.0
        if rag is not None:
            py = [f for f in sh(["git", "ls-files", "*.py"], cwd=repo).splitlines()]
            idx = rag.index(repo, py)
            # The same budget leangraph spent on the same query, so the two are
            # compared at equal cost rather than at equal k.
            rg_files, rg_tok = rag.retrieve(idx, text, lg_tok)
            rg = recall(rg_files, want)

        results.append({
            "instance": r["instance_id"], "repo": r["repo"], "want": want,
            "lg": recall(lg, want), "lg_tokens": lg_tok,
            "kw": recall(kw, want), "kw_tokens": kw_tok,
            "eq": recall(eq, want), "eq_tokens": eq_tok, "eq_files": len(eq),
            "rag": rg, "rag_tokens": rg_tok,
            "cg": cg_r, "cg_cited": cg_any_r,
            "cg_tokens": cg_tok, "cg_src_tokens": cg_src,
            "lg_eq": lg_eq_r, "lg_eq_tokens": lg_eq_tok,
        })
        if rag is not None and i % 25 == 0:
            rag.save_cache()
        if i % 10 == 0 or i == len(rows):
            el = time.time() - t0
            print(f"  {i}/{len(rows)}  {el:.0f}s", end="\r", flush=True)
    print(" " * 40, end="\r")
    if rag is not None:
        rag.save_cache()

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
        cg_rows = [x for x in rows if x.get("cg") is not None]
        cg_txt = le_txt = "        —      "
        if cg_rows:
            c = sum(x["cg"] for x in cg_rows) / len(cg_rows)
            ct = sum(x["cg_tokens"] for x in cg_rows) / len(cg_rows)
            cg_txt = f"{c * 100:>5.1f}% / {ct:>7,.0f}"
            le = sum(x["lg_eq"] for x in cg_rows) / len(cg_rows)
            let = sum(x["lg_eq_tokens"] for x in cg_rows) / len(cg_rows)
            le_txt = f"{le * 100:>5.1f}% / {let:>7,.0f}"
        rg_rows = [x for x in rows if x.get("rag") is not None]
        rg_txt = "        —      "
        if rg_rows:
            rg = sum(x["rag"] for x in rg_rows) / len(rg_rows)
            rgt = sum(x["rag_tokens"] for x in rg_rows) / len(rg_rows)
            rg_txt = f"{rg * 100:>5.1f}% / {rgt:>7,.0f}"
        print(f"  {label:<16} {n:>4}   "
              f"{lg * 100:>5.1f}% / {lgt:>7,.0f}   "
              f"{le_txt}   "
              f"{cg_txt}   "
              f"{rg_txt}   "
              f"{eq * 100:>5.1f}% / {eqt:>7,.0f}   "
              f"{kw * 100:>5.1f}% / {kwt:>9,.0f}   "
              f"{100 * lg_any / n:>5.1f}%")

    print(f"\n\033[1m  file recall / mean tokens per query\033[0m")
    print(f"  {'':<16} {'n':>4}   {'leangraph':^15}   {'leangraph, its':^15}   {'codegraph':^15}   {'embedding RAG':^15}   "
          f"{'keyword, same':^15}   {'keyword top-10':^17}   leangraph")
    print(f"  {'':<16} {'':>4}   {'':^15}   {'budget':^15}   {'explore':^15}   {'same budget':^15}   "
          f"{'budget':^15}   {'':^17}   at-least-one")
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

    if args.baseline:
        paired(results, json.load(open(args.baseline)))

    print("\n  Localization only. Whether an answer built on this context is"
          "\n  correct is the benchmark's tests, which is a different run.\n")
    return 0


if __name__ == "__main__":
    sys.exit(main())
