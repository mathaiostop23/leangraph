#!/usr/bin/env python3
"""Score both arms across many instances without filling the disk.

Each SWE-bench evaluation image is one to two gigabytes and there are five
hundred of them, so pulling the set is not an option on a laptop. This pulls
one image, runs every arm that needs it — gold first, as the gate — and removes
it again before moving on. Peak disk is one image regardless of sample size.

The gold run is not a formality. An instance whose own reference patch leaves
its tests red is measuring the harness; counting it as a loss for every arm is
a wrong denominator, so it is dropped and reported separately.
"""
import argparse, importlib.util, json, os, subprocess, sys, time

_spec = importlib.util.spec_from_file_location(
    "swefix", os.path.join(os.path.dirname(os.path.abspath(__file__)), "swefix.py"))
swefix = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(swefix)

GREEN, RED, YELL, DIM, BOLD, OFF = (
    "\033[32m", "\033[31m", "\033[33m", "\033[2m", "\033[1m", "\033[0m")


def pull(tag):
    r = subprocess.run(["docker", "pull", "-q", tag], capture_output=True, text=True)
    return r.returncode == 0, (r.stderr or r.stdout).strip()[:120]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--data", required=True)
    ap.add_argument("--arms", required=True,
                    help="name=patches.json, comma separated")
    ap.add_argument("--limit", type=int, default=0)
    ap.add_argument("--keep-images", action="store_true",
                    help="do not remove an image that was already present")
    ap.add_argument("--out", default="")
    args = ap.parse_args()

    arms = {}
    for spec in args.arms.split(","):
        name, _, path = spec.partition("=")
        arms[name] = json.load(open(path))

    recs = json.load(open(args.data))
    if isinstance(recs, dict):
        recs = [recs]
    # Only instances every arm produced something for: an arm that returned no
    # diff for an instance the other one answered would otherwise be compared
    # on a different set of problems.
    common = [r for r in recs
              if all(a.get(r["instance_id"], "").strip() for a in arms.values())]
    if args.limit:
        common = common[:args.limit]

    print(f"\n  {len(common)} instances answered by all {len(arms)} arms "
          f"(of {len(recs)} in the file)\n")

    tally = {n: 0 for n in arms}
    excluded, scored, rows = [], 0, []
    for i, rec in enumerate(common, 1):
        iid = rec["instance_id"]
        tag = swefix.image_for(iid)
        had = swefix.have_image(tag)
        t0 = time.time()
        if not had:
            ok, err = pull(tag)
            if not ok:
                print(f"  {YELL}no image{OFF}  {iid:<32} {err}")
                excluded.append((iid, "image unavailable"))
                continue

        gold_ok, gold_why = swefix.run_tests(rec, rec["patch"])
        if not gold_ok:
            print(f"  {YELL}excluded{OFF}  {iid:<32} gold fails here — {gold_why}")
            excluded.append((iid, f"gold: {gold_why}"))
        else:
            scored += 1
            marks = []
            for name, patches in arms.items():
                ok, why = swefix.run_tests(rec, patches.get(iid, ""))
                tally[name] += bool(ok)
                marks.append(f"{name}={(GREEN+'pass'+OFF) if ok else (RED+'fail'+OFF)}")
                rows.append({"instance_id": iid, "arm": name,
                             "resolved": bool(ok), "detail": why})
            print(f"  {i:>3}/{len(common)}  {iid:<32} " + "  ".join(marks)
                  + f"  {DIM}{time.time()-t0:.0f}s{OFF}")

        if not had and not args.keep_images:
            subprocess.run(["docker", "rmi", "-f", tag], capture_output=True)

    print(f"\n{BOLD}  resolved, of {scored} instances whose gold patch passes{OFF}")
    for name in arms:
        pct = 100.0 * tally[name] / scored if scored else 0.0
        print(f"    {name:<12} {tally[name]:>3}/{scored}   {pct:5.1f}%")
    if excluded:
        print(f"\n  {len(excluded)} not counted: "
              + ", ".join(f"{i} ({w})" for i, w in excluded[:6])
              + (" …" if len(excluded) > 6 else ""))
    if args.out:
        json.dump({"rows": rows, "excluded": excluded}, open(args.out, "w"), indent=1)
        print(f"  detail -> {args.out}")
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
