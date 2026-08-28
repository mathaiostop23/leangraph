#!/usr/bin/env python3
"""Fetch what `swebench.py` needs: the dataset, and the twelve checkouts.

This existed only as shell history, in `/tmp`, which is why the headline number
in the README could not be reproduced from a fresh clone of this repository —
the corpus it was measured on had been swept away by a reboot. A benchmark
whose inputs cannot be rebuilt is an anecdote.

Two things are fetched:

  * SWE-bench Verified, read straight from the parquet on the Hub rather than
    through `datasets`, which is a large dependency for one file; only the five
    fields the harness reads are kept.
  * A full clone of each repository the dataset names. Full, not shallow: every
    instance is pinned to the commit its issue was filed against, and those
    commits are scattered through years of history that `--depth` would not
    fetch.

The default lives under `~/.cache`, not `/tmp`, for the reason above.

Usage:
  bench/swebench_fetch.py [--dir ~/.cache/swebench] [--jobs 4] [--data-only]
"""
import argparse, io, json, os, subprocess, sys
from concurrent.futures import ThreadPoolExecutor

PARQUET = ("https://huggingface.co/datasets/princeton-nlp/SWE-bench_Verified"
           "/resolve/main/data/test-00000-of-00001.parquet")
# Everything the harness reads, and nothing else: the full dataset carries
# environment setup and test commands this measurement has no use for.
KEEP = ["instance_id", "repo", "base_commit", "problem_statement", "patch"]
DEFAULT_DIR = os.path.expanduser("~/.cache/swebench")


def fetch_data(out_path):
    if os.path.exists(out_path):
        rows = json.load(open(out_path))
        print(f"  dataset: {len(rows)} instances already at {out_path}")
        return rows
    import pandas as pd
    import requests
    print(f"  dataset: downloading {PARQUET.rsplit('/', 1)[-1]} ...", flush=True)
    r = requests.get(PARQUET, timeout=300)
    r.raise_for_status()
    df = pd.read_parquet(io.BytesIO(r.content), columns=KEEP)
    rows = df.to_dict("records")
    os.makedirs(os.path.dirname(out_path), exist_ok=True)
    json.dump(rows, open(out_path, "w"))
    print(f"  dataset: {len(rows)} instances written to {out_path}")
    return rows


def clone(repo, dest):
    """One repository, or nothing if it is already whole.

    A previous run killed mid-clone leaves a directory that looks present and
    is not, so an existing checkout is only accepted if git itself agrees.
    """
    name = repo.split("/")[1]
    path = os.path.join(dest, name)
    if os.path.isdir(os.path.join(path, ".git")):
        ok = subprocess.run(["git", "-C", path, "rev-parse", "--git-dir"],
                            capture_output=True).returncode == 0
        if ok:
            return name, "present"
        subprocess.run(["rm", "-rf", path])
    r = subprocess.run(["git", "clone", "--quiet", f"https://github.com/{repo}.git", path],
                       capture_output=True, text=True)
    if r.returncode:
        return name, f"FAILED: {r.stderr.strip()[:120]}"
    return name, "cloned"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--dir", default=DEFAULT_DIR)
    ap.add_argument("--jobs", type=int, default=4)
    ap.add_argument("--data-only", action="store_true")
    args = ap.parse_args()

    dest = os.path.expanduser(args.dir)
    os.makedirs(dest, exist_ok=True)
    print(f"\n\033[1mSWE-bench Verified — corpus\033[0m\n  {dest}\n")

    rows = fetch_data(os.path.join(dest, "verified.json"))
    if args.data_only:
        return 0

    repos = sorted({r["repo"] for r in rows})
    print(f"\n  {len(repos)} repositories, full history, {args.jobs} at a time")
    with ThreadPoolExecutor(max_workers=args.jobs) as pool:
        for name, how in pool.map(lambda r: clone(r, dest), repos):
            mark = "!" if how.startswith("FAILED") else "."
            print(f"  {mark} {name:<16} {how}", flush=True)

    have = {d for d in os.listdir(dest) if os.path.isdir(os.path.join(dest, d, ".git"))}
    usable = [r for r in rows if r["repo"].split("/")[1] in have]
    print(f"\n  {len(have)}/{len(repos)} checkouts, {len(usable)}/{len(rows)} instances usable")
    print(f"\n  bench/swebench.py --repos {dest}\n")
    return 0 if len(have) == len(repos) else 1


if __name__ == "__main__":
    sys.exit(main())
