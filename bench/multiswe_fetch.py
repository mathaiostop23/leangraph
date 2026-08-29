#!/usr/bin/env python3
"""Multi-SWE-bench, converted to the shape `swebench.py` already reads.

SWE-bench Verified is Python and only Python, so every retrieval number this
project publishes is a Python number. leangraph claims fourteen languages and
the second one it supports at full depth is TypeScript; nothing measured it.

Multi-SWE-bench (ByteDance) has the same ingredients under different names — a
repository, a commit the issue was filed against, the issue itself, and the
patch that fixed it — for TypeScript, Java, Go, Rust, C and C++.

One conversion decision matters more than the rest. Each record carries a
top-level `title` and `body` *of the pull request*, written by whoever fixed the
bug, and a separate `resolved_issues` list holding the issue as filed. The PR
title on the first vue record is "fix(compiler-sfc): nested css supports atrule
and comment" — it names the package the patch touches. Using it would be the
commit-message leak again, so only `resolved_issues` is used, and a record with
none is dropped rather than filled in from the PR.

Usage:
  bench/multiswe_fetch.py --lang ts [--dir ~/.cache/multiswe] [--jobs 3]
"""
import argparse, ast, json, os, re, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

HUB = ("https://huggingface.co/datasets/ByteDance-Seed/Multi-SWE-bench"
       "/resolve/main/{lang}/{repo}_dataset.jsonl")
SPLITS = {
    "ts": ["darkreader__darkreader", "mui__material-ui", "vuejs__core"],
    "js": ["Kong__insomnia", "anuraghazra__github-readme-stats", "axios__axios",
           "expressjs__express", "iamkun__dayjs", "sveltejs__svelte"],
    "rust": ["BurntSushi__ripgrep", "clap-rs__clap", "nushell__nushell",
             "rayon-rs__rayon", "serde-rs__serde", "sharkdp__bat", "sharkdp__fd",
             "tokio-rs__bytes", "tokio-rs__tokio", "tokio-rs__tracing"],
    "go": ["cli__cli", "grpc__grpc-go", "zeromicro__go-zero"],
}
# Extensions that are source for each split, used to drop a patch that only
# touches documentation, lockfiles or CI config.
CODE = {
    "ts": r"\.(ts|tsx|js|jsx|mjs|cjs)$",
    "js": r"\.(js|jsx|mjs|cjs|ts)$",
    "rust": r"\.rs$",
    "go": r"\.go$",
}
# Pointing at the test that proves a bug is not finding the bug.
TEST = re.compile(
    r"(^|/)(__tests__|__mocks__|tests?|e2e|cypress|testdata)/"
    r"|\.(spec|test)\.[jt]sx?$|_test\.go$|/tests\.rs$")


def records(path):
    """Tolerant of a truncated tail: these files run to a hundred megabytes and
    a cut-off download otherwise loses every record rather than the last one."""
    text = open(path, encoding="utf-8", errors="replace").read()
    dec, i, n = json.JSONDecoder(), 0, len(text)
    while i < n:
        while i < n and text[i] in " \r\n\t":
            i += 1
        if i >= n:
            break
        try:
            obj, i = dec.raw_decode(text, i)
        except json.JSONDecodeError:
            print(f"  ! truncated tail in {os.path.basename(path)}", file=sys.stderr)
            return
        yield obj


def as_list(v):
    if isinstance(v, list):
        return v
    try:
        return ast.literal_eval(v) or []
    except Exception:
        return []


def convert(rec, code_re):
    """One record, or None if it cannot be asked fairly."""
    issues = as_list(rec.get("resolved_issues"))
    text = "\n\n".join(
        ((i.get("title") or "") + "\n" + (i.get("body") or "")).strip()
        for i in issues).strip()
    if len(text) < 40:
        return None
    files = sorted({m.group(2) for m in re.finditer(
        r"^diff --git a/(\S+) b/(\S+)", rec.get("fix_patch", ""), re.M)})
    files = [f for f in files if re.search(code_re, f) and not TEST.search(f)]
    if not files:
        return None
    base = rec.get("base") or {}
    if isinstance(base, str):
        try:
            base = ast.literal_eval(base)
        except Exception:
            return None
    sha = base.get("sha")
    if not sha:
        return None
    return {
        "instance_id": rec["instance_id"],
        "repo": f"{rec['org']}/{rec['repo']}",
        "base_commit": sha,
        "problem_statement": text,
        # `swebench.py` re-derives the answer from this, so hand it a patch
        # containing exactly the files that count.
        "patch": "".join(f"diff --git a/{f} b/{f}\n" for f in files),
    }


def clone(repo, dest):
    name = repo.split("/")[1]
    path = os.path.join(dest, name)
    if os.path.isdir(os.path.join(path, ".git")):
        if subprocess.run(["git", "-C", path, "rev-parse", "--git-dir"],
                          capture_output=True).returncode == 0:
            return name, "present"
        subprocess.run(["rm", "-rf", path])
    r = subprocess.run(["git", "clone", "--quiet",
                        f"https://github.com/{repo}.git", path],
                       capture_output=True, text=True)
    return name, "cloned" if not r.returncode else f"FAILED: {r.stderr.strip()[:110]}"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--lang", default="ts", choices=sorted(SPLITS))
    ap.add_argument("--dir", default="~/.cache/multiswe")
    ap.add_argument("--jobs", type=int, default=3)
    ap.add_argument("--data-only", action="store_true")
    args = ap.parse_args()

    dest = os.path.join(os.path.expanduser(args.dir), args.lang)
    os.makedirs(dest, exist_ok=True)
    print(f"\n\033[1mMulti-SWE-bench — {args.lang}\033[0m\n  {dest}\n")

    out, seen_repos = [], set()
    for split in SPLITS[args.lang]:
        raw = os.path.join(dest, f"{split}.jsonl")
        if not os.path.exists(raw) or os.path.getsize(raw) == 0:
            url = HUB.format(lang=args.lang, repo=split)
            # curl rather than requests: these redirect to a CDN that requests
            # was observed to stall on, and curl -L follows it without fuss.
            subprocess.run(["curl", "-sL", "-o", raw, url], check=True)
        kept = [c for c in (convert(r, CODE[args.lang]) for r in records(raw)) if c]
        out += kept
        if kept:
            seen_repos.add(kept[0]["repo"])
        print(f"  {split:<34} {len(kept):>3} usable")

    data = os.path.join(dest, "verified.json")
    json.dump(out, open(data, "w"))
    print(f"\n  {len(out)} instances across {len(seen_repos)} repositories -> {data}")
    if args.data_only:
        return 0

    print(f"\n  cloning, {args.jobs} at a time")
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for name, how in pool.map(lambda r: clone(r, dest), sorted(seen_repos)):
            print(f"  {'!' if how.startswith('FAILED') else '.'} {name:<18} {how}", flush=True)

    have = {d for d in os.listdir(dest) if os.path.isdir(os.path.join(dest, d, ".git"))}
    ok = [r for r in out if r["repo"].split("/")[1] in have]
    print(f"\n  {len(have)}/{len(seen_repos)} checkouts, {len(ok)}/{len(out)} instances usable")
    print(f"\n  bench/swebench.py --repos {dest} --data {data}\n")
    return 0 if len(have) == len(seen_repos) else 1


if __name__ == "__main__":
    sys.exit(main())
