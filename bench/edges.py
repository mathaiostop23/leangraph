#!/usr/bin/env python3
"""Edge correctness: does confidence predict it?

arbor puts a confidence and a provenance on every edge and argues that this is
not decoration — that ranking by confidence is why it returns fewer tokens at
equal recall. That argument has never been tested. If a scope-resolved edge and
a name-matched one are right equally often, the ranking is sorting noise.

Two labellers, both local, neither sufficient alone:

  runtime    Run the repository's own test suite and record every call that
             actually happened. An observed edge exists — no argument. Gives
             RECALL, and a confirmation rate over edges whose caller ran.
             Says nothing about an edge the tests never reached.

  codegraph  Agreement with another static tool. Covers everything, proves
             nothing: where we differ either side may be right.

Neither measures precision. `bench/edgefacts.py` bounds it from below without
an oracle, and that floor is the honest complement to the numbers here.

Usage: bench/edges.py [--trace FILE] [repo]
"""
import argparse, json, math, os, sqlite3, subprocess, sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from graphjoin import norm_qual, norm_path  # noqa: E402

ARBOR = os.path.join(HERE, "..", "target", "release", "arbor")
REPOS = os.environ.get("ARBOR_BENCH_REPOS", os.path.join(HERE, "..", "..", ".bench-repos"))

BUCKETS = [(100, "scope"), (95, "import"), (80, "name"), (60, "name"), (45, "name")]


# ------------------------------------------------------------------ statistics

def wilson(k, n, z=1.96):
    """95% interval. A bucket with 40 observations and one with 4,000 do not
    deserve the same confidence, and a point estimate hides that."""
    if n == 0:
        return (0.0, 0.0)
    p = k / n
    d = 1 + z * z / n
    c = (p + z * z / (2 * n)) / d
    h = z * math.sqrt(p * (1 - p) / n + z * z / (4 * n * n)) / d
    return (100 * max(0.0, c - h), 100 * min(1.0, c + h))


def two_prop(k1, n1, k2, n2):
    """Two-proportion z test. Returns (z, p) two-sided."""
    if n1 == 0 or n2 == 0:
        return (0.0, 1.0)
    p1, p2 = k1 / n1, k2 / n2
    p = (k1 + k2) / (n1 + n2)
    se = math.sqrt(p * (1 - p) * (1 / n1 + 1 / n2))
    if se == 0:
        return (0.0, 1.0)
    z = (p1 - p2) / se
    return (z, math.erfc(abs(z) / math.sqrt(2)))


def trend(pairs):
    """Cochran-Armitage over ordered confidence levels. Answers the actual
    question — is there a monotone relationship — rather than comparing two
    buckets and hoping the rest follow."""
    pairs = [(s, k, n) for s, k, n in pairs if n > 0]
    if len(pairs) < 3:
        return (0.0, 1.0)
    N = sum(n for _, _, n in pairs)
    K = sum(k for _, k, _ in pairs)
    if N == 0 or K == 0 or K == N:
        return (0.0, 1.0)
    p = K / N
    sbar = sum(s * n for s, _, n in pairs) / N
    num = sum((s - sbar) * (k - n * p) for s, k, n in pairs)
    var = p * (1 - p) * sum(n * (s - sbar) ** 2 for s, _, n in pairs)
    if var <= 0:
        return (0.0, 1.0)
    z = num / math.sqrt(var)
    return (z, math.erfc(abs(z) / math.sqrt(2)))


# ----------------------------------------------------------------------- input

def arbor_edges(repo):
    raw = subprocess.run([ARBOR, "dump", "-p", repo, "--what", "edges"],
                         capture_output=True, text=True, check=True).stdout
    return [json.loads(l) for l in raw.splitlines()]


def load_trace(path):
    edges, ran, bases, header = [], {}, {}, {}
    for rec in map(json.loads, open(path)):
        k = rec["kind"]
        if k == "edge":
            edges.append(rec)
        elif k == "ran":
            ran[(rec["f"], rec["q"])] = rec
        elif k == "bases":
            bases[rec["q"]] = rec["b"]
        elif k == "header":
            header = rec
    return header, edges, ran, bases


def is_test(path):
    p = path.lower()
    return ("test" in os.path.basename(p) or "/tests/" in "/" + p
            or p.startswith("tests/") or "/test_" in "/" + p)


def population(e):
    return f"{'test' if is_test(e['sf']) else 'lib'}→{'test' if is_test(e['df']) else 'lib'}"


# -------------------------------------------------------------- runtime labels

class Runtime:
    """Match arbor's static edges against what actually ran.

    The staged rules matter as much as the result. Each one makes matching
    easier, so a reader who only sees the final number cannot tell how much of
    it is arbor being right and how much is the comparison being generous. The
    ladder is printed.
    """

    def __init__(self, edges, ran, bases):
        self.pairs = set()
        self.by_caller = defaultdict(set)
        for e in edges:
            cq, dq = norm_qual(e["cq"]), norm_qual(e["dq"])
            self.pairs.add((e["cf"], cq, e["df"], dq))
            self.by_caller[(e["cf"], cq)].add((e["df"], dq))
        self.ran = {(f, norm_qual(q)) for (f, q) in ran}
        self.flags = {(f, norm_qual(q)): r["flag"] for (f, q), r in ran.items()}
        # Runtime MRO, both directions: arbor may name a base where the
        # subclass's override actually executed, or the reverse.
        self.kin = defaultdict(set)
        for q, bs in bases.items():
            nq = norm_qual(q)
            for b in bs:
                nb = norm_qual(b)
                self.kin[nq].add(nb)
                self.kin[nb].add(nq)

    def caller_ran(self, e):
        return (e["sf"], norm_qual(e["sq"])) in self.ran

    def match(self, e, stage):
        """stage 1 exact, 2 + constructor, 3 + MRO."""
        src = (e["sf"], norm_qual(e["sq"]))
        dst = (e["df"], norm_qual(e["dq"]))
        if (src[0], src[1], dst[0], dst[1]) in self.pairs:
            return True
        seen = self.by_caller.get(src, ())
        if stage >= 2 and e["dk"] in ("class", "iface"):
            # `X()` is a call to the class in arbor's model and a call to
            # `X.__init__` at runtime. Same edge, two spellings.
            for ctor in ("__init__", "__new__"):
                if (dst[0], f"{dst[1]}.{ctor}") in seen:
                    return True
        if stage >= 3:
            base = dst[1].rsplit(".", 1)
            if len(base) == 2:
                owner, meth = base
                for (df, dq) in seen:
                    p = dq.rsplit(".", 1)
                    if len(p) == 2 and p[1] == meth and p[0] in self.kin.get(owner, ()):
                        return True
        return False


class Static:
    """The same matching rules, pointed the other way.

    Recall asks whether arbor has an observed edge; calibration asks whether an
    arbor edge was observed. They must use the same rules or the two numbers are
    not about the same thing — the first version of this file applied the
    constructor and MRO rules to one and not the other, which made recall look
    worse than the calibration table it sat above.
    """

    def __init__(self, calls, kin):
        self.exact = set()
        self.by_caller = defaultdict(set)
        self.classes = set()
        for e in calls:
            sq, dq = norm_qual(e["sq"]), norm_qual(e["dq"])
            self.exact.add((e["sf"], sq, e["df"], dq))
            self.by_caller[(e["sf"], sq)].add((e["df"], dq))
            if e["dk"] in ("class", "iface"):
                self.classes.add((e["df"], dq))
        self.kin = kin

    def has(self, o, stage):
        cf, cq, df, dq = o[0], o[1], o[2], o[3]
        if (cf, cq, df, dq) in self.exact:
            return True
        out = self.by_caller.get((cf, cq), ())
        if stage >= 2:
            # Runtime enters `X.__init__`; arbor records a call to the class.
            for ctor in (".__init__", ".__new__"):
                if dq.endswith(ctor):
                    owner = dq[: -len(ctor)]
                    if (df, owner) in out and (df, owner) in self.classes:
                        return True
        if stage >= 3:
            p = dq.rsplit(".", 1)
            if len(p) == 2:
                owner, meth = p
                for (af, aq) in out:
                    q = aq.rsplit(".", 1)
                    if len(q) == 2 and q[1] == meth and q[0] in self.kin.get(owner, ()):
                        return True
        return False


# ------------------------------------------------------------ codegraph labels

CG_KIND = {"calls": "calls", "instantiates": "calls", "extends": "extends",
           "references": "references", "decorates": "references"}


def codegraph_edges(repo):
    db = os.path.join(repo, ".codegraph", "codegraph.db")
    if not os.path.isfile(db):
        return None
    con = sqlite3.connect(db)
    nodes = {}
    for nid, qn, fp, kind in con.execute(
            "select id, qualified_name, file_path, kind from nodes"):
        nodes[nid] = (norm_path(fp, repo), norm_qual(qn), kind)
    out = set()
    merged = 0
    for s, t, k in con.execute("select source, target, kind from edges"):
        ek = CG_KIND.get(k)
        if not ek or s not in nodes or t not in nodes:
            continue
        sf, sq, _ = nodes[s]
        df, dq, _ = nodes[t]
        row = (sf, sq, df, dq, ek)
        if row in out:
            merged += 1
        out.add(row)
    con.close()
    return out, merged


# --------------------------------------------------------------------- reports

def calibration(name, rows, label_fn, eligible_fn, note):
    """One confidence table. `rows` are arbor edges; `label_fn` says confirmed."""
    print(f"\n  \033[1m{name}\033[0m   \033[2m{note}\033[0m")
    print(f"    {'conf':>5} {'prov':<8}{'eligible':>10}{'confirmed':>11}{'rate':>8}   95% interval")
    data = []
    for conf, prov in BUCKETS:
        el = [e for e in rows if e["conf"] == conf and e["prov"] == prov and eligible_fn(e)]
        if not el:
            continue
        k = sum(1 for e in el if label_fn(e))
        lo, hi = wilson(k, len(el))
        data.append((conf, k, len(el)))
        print(f"    {conf:>5} {prov:<8}{len(el):>10,}{k:>11,}{100*k/len(el):>7.1f}%   "
              f"{lo:5.1f} – {hi:5.1f}")
    if len(data) >= 3:
        z, p = trend(data)
        verdict = "confidence orders correctness" if z > 0 and p < 0.05 else \
                  "INVERTED — lower confidence scores better" if z < 0 and p < 0.05 else \
                  "no monotone relationship"
        print(f"    \033[1mtrend\033[0m  z={z:+.2f}  p={p:.2g}   → {verdict}")
    return data


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("repo", nargs="?", default="flask")
    ap.add_argument("--trace", default=None)
    args = ap.parse_args()
    root = args.repo if os.path.isdir(args.repo) else os.path.join(REPOS, args.repo)
    name = os.path.basename(root.rstrip("/"))

    edges = arbor_edges(root)
    calls = [e for e in edges if e["ek"] == "calls"]
    print(f"\n\033[1medge correctness — {name}\033[0m")
    print(f"  {len(edges):,} edges, {len(calls):,} of them calls")

    # ------------------------------------------------------------- runtime
    if args.trace and os.path.isfile(args.trace):
        header, tedges, ran, bases = load_trace(args.trace)
        rt = Runtime(tedges, ran, bases)
        print(f"\n  \033[2mruntime oracle: {len(tedges):,} observed edges, "
              f"{len(ran):,} executed functions, python "
              f"{'.'.join(map(str, header.get('python', [])))}\033[0m")

        # Recall, restricted to observed edges whose BOTH ends arbor knows as
        # nodes — a missing node is a node-level failure and is measured
        # elsewhere; counting it here would be counting it twice.
        known = {(e["sf"], norm_qual(e["sq"])) for e in edges} | \
                {(e["df"], norm_qual(e["dq"])) for e in edges}
        static = Static(calls, rt.kin)
        obs = [(e["cf"], norm_qual(e["cq"]), e["df"], norm_qual(e["dq"]), e["hops"])
               for e in tedges]
        joinable = [o for o in obs if (o[0], o[1]) in known and (o[2], o[3]) in known]
        direct = [o for o in joinable if o[4] == 0]
        print(f"\n  \033[1mrecall against execution\033[0m")
        print(f"    observed edges                     {len(obs):>8,}")
        print(f"    both endpoints are arbor nodes     {len(joinable):>8,}")
        print(f"    {'':<35}{'all':>12}{'direct only':>14}")
        for stage, label in ((1, "exact match"), (2, "+ constructor rule"),
                             (3, "+ MRO closure")):
            a = sum(1 for o in joinable if static.has(o, stage))
            d = sum(1 for o in direct if static.has(o, stage))
            bold = "\033[1m" if stage == 3 else ""
            end = "\033[0m" if stage == 3 else ""
            print(f"    {label:<35}{bold}{100*a/max(len(joinable),1):>11.1f}%{end}"
                  f"{100*d/max(len(direct),1):>13.1f}%")
        # Where the remainder lives. A handful of dispatch sites dominate it,
        # and a reader who is not told that will read the headline as a uniform
        # failure rather than as a small number of places no static analysis
        # can follow.
        miss = [o for o in direct if not static.has(o, 3)]
        hubs = Counter((o[0], o[1]) for o in miss).most_common(5)
        top = sum(n for _, n in hubs)
        print(f"    \033[2mof {len(miss):,} direct misses, {top:,} "
              f"({100*top/max(len(miss),1):.0f}%) are calls out of five sites:\033[0m")
        for (f, q), n in hubs:
            print(f"      \033[2m{q[:48]:<50}{n:>5}\033[0m")

        # Calibration. Eligible = the caller actually ran, so a miss means the
        # edge was available to be observed and was not.
        for stage, label in ((1, "exact match"), (2, "+ constructor rule"),
                             (3, "+ MRO closure")):
            elig = lambda e: rt.caller_ran(e)  # noqa: E731
            lab = (lambda st: lambda e: rt.match(e, st))(stage)
            data = calibration(f"runtime-confirmed, {label} — all populations",
                               calls, lab, elig,
                               "denominator: edges whose caller executed")
            if stage == 3:
                for pop in ("lib→lib", "test→lib"):
                    sub = [e for e in calls if population(e) == pop]
                    if len(sub) > 40:
                        calibration(f"runtime-confirmed, {label} — {pop} only",
                                    sub, lab, elig,
                                    "the population the ranking claim is about")
    else:
        print("\n  SKIP runtime oracle: no trace (see bench/edgetrace.py)")

    # ----------------------------------------------------------- codegraph
    cg = codegraph_edges(root)
    if cg is None:
        print("\n  SKIP codegraph: no .codegraph/codegraph.db")
    else:
        cgset, merged = cg
        sem = [e for e in edges if e["ek"] in ("calls", "extends", "references")]
        cg_nodes = {(f, q) for f, q, _, _, _ in cgset} | {(f, q) for _, _, f, q, _ in cgset}
        elig = lambda e: ((e["sf"], norm_qual(e["sq"])) in cg_nodes and  # noqa: E731
                          (e["df"], norm_qual(e["dq"])) in cg_nodes)
        lab = lambda e: (e["sf"], norm_qual(e["sq"]), e["df"],  # noqa: E731
                         norm_qual(e["dq"]), e["ek"]) in cgset
        print(f"\n  \033[2mcodegraph: {len(cgset):,} comparable edges "
              f"({merged:,} collapsed to our granularity)\033[0m")
        calibration("agreed with codegraph", sem, lab, elig,
                    "agreement, not truth — either side may be wrong")

    print("\n  \033[2mNeither labeller measures precision. See bench/edgefacts.py "
          "for a floor on the error rate.\033[0m\n")


if __name__ == "__main__":
    main()
