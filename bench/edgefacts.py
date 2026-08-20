#!/usr/bin/env python3
"""Edges that are wrong without needing an oracle to say so.

Node correctness is checked against another tool. Edges have never been
checked at all, and 69% of the semantic ones in django are guesses — a name
matched across the repository with no scope or import behind it. If those are
mostly wrong, the graph is mostly noise, and every claim built on top of it is
worth nothing.

Before reaching for an oracle it is worth asking what can be falsified from the
graph alone. Quite a lot, as it turns out. These rules produce a **floor** on
the error rate in each confidence bucket, never the error rate: an edge no rule
fires on is unexamined, not correct.

Every rule here is a claim about the *product*, so each one is stated as the
property it violates and checked against the source where that is possible.

Usage: bench/edgefacts.py [repo ...]
"""
import json, os, re, subprocess, sys
from collections import Counter, defaultdict

HERE = os.path.dirname(os.path.abspath(__file__))
sys.path.insert(0, HERE)
from graphjoin import norm_qual  # noqa: E402

LEANGRAPH = os.path.join(HERE, "..", "target", "release", "leangraph")
REPOS = os.environ.get("LEANGRAPH_BENCH_REPOS", os.path.join(HERE, "..", "..", ".bench-repos"))

PY = (".py", ".pyi")
JS = (".js", ".jsx", ".ts", ".tsx", ".mjs", ".cjs", ".mts", ".cts")
VENDORED = ("vendor", "vendored", "dist", "node_modules", "third_party", "external")


def load(repo):
    raw = subprocess.run([LEANGRAPH, "dump", "-p", repo, "--what", "edges"],
                         capture_output=True, text=True, check=True).stdout
    return [json.loads(l) for l in raw.splitlines()]


class Source:
    """Caller bodies, for the rules that have to look."""

    def __init__(self, root):
        self.root, self._c = root, {}

    def text(self, rel):
        if rel not in self._c:
            try:
                with open(os.path.join(self.root, rel), "rb") as f:
                    self._c[rel] = f.read().decode("utf-8", "replace")
            except OSError:
                self._c[rel] = ""
        return self._c[rel]

    def span(self, rel, start, end):
        t = self.text(rel)
        return t[start:end] if t else ""


def is_lang(path, exts):
    return path.lower().endswith(exts)


def vendored(path):
    return any(p in VENDORED for p in path.split("/")) or ".min." in path.lower()


# ------------------------------------------------------------------- the rules


def rule_self_loop_extends(e, src):
    """A class cannot be its own base.

    `class Migration(migrations.Migration)` — the extractor tags every
    identifier in the heritage list, the scope stack already holds the class
    being defined, and the resolver binds the name straight back to it.
    """
    return e["ek"] == "extends" and e["sq"] == e["dq"] and e["sf"] == e["df"]


BARE_CALL = {}


def rule_self_call_not_recursive(e, src):
    """A self-edge on `calls` that the source does not support.

    Recursion is real, so a self-loop is not wrong on its own — this is where
    a rule that merely counted self-loops would overclaim. It is wrong when the
    caller's body never names itself bare: `app_config.get_models(...)` is a
    call on another object, and binding it to the enclosing class's own
    `get_models` through the lexical scope chain is a false edge at confidence
    100.
    """
    if e["ek"] != "calls" or e["sq"] != e["dq"] or e["sf"] != e["df"]:
        return False
    name = e["dq"].rsplit(".", 1)[-1]
    k = (e["sf"], e["sb"], name)
    if k not in BARE_CALL:
        body = src.span(e["sf"], e["sb"], e["sb"] + 20000)
        # `name(` not preceded by a dot — a genuine recursive call. `self.name(`
        # counts too: the receiver is the same object.
        pat = re.compile(r"(?<![.\w])" + re.escape(name) + r"\s*\(|self\." + re.escape(name) + r"\s*\(")
        BARE_CALL[k] = bool(pat.search(body[body.find("\n"):] if "\n" in body else body))
    return not BARE_CALL[k]


def rule_extends_non_class(e, src):
    """A base that is not a class.

    `class SettingsReference(str)` resolved to a *variable* named `str`;
    `class ActionForm(forms.Form)` resolved to a *method* named `forms`. Both
    come from the extractor tagging every identifier in a Python heritage list,
    including the module segment of a dotted base and names that shadow a
    builtin.
    """
    return e["ek"] == "extends" and e["dk"] not in ("class", "iface")


def rule_cross_language(e, src):
    """A Python function cannot call a JavaScript one.

    The global name index has no language partition, so any name defined in
    both trees can bind across them.
    """
    if e["prov"] not in ("name",):
        return False
    a, b = e["sf"], e["df"]
    return (is_lang(a, PY) and is_lang(b, JS)) or (is_lang(a, JS) and is_lang(b, PY))


def rule_vendored_target(e, src):
    """A call into a vendored or minified bundle.

    Not impossible in principle, but a Python `len()` landing in minified
    JavaScript is not a real edge, and vendored trees are not what an agent
    should be pointed at in any case.
    """
    return e["prov"] == "name" and vendored(e["df"]) and not vendored(e["sf"])


# `enforced` marks a rule the resolver now guarantees can never fire — the rule
# is the literal negation of a check in src/. Those rules are worth keeping,
# because a regression would light them up, but they cannot be evidence that the
# resolver improved: after the guard exists, they report zero by construction.
# Quoting a floor that includes them as proof of a fix is circular, so the
# headline is computed over the independent rules and both are printed.
RULES = [
    ("extends self", rule_self_loop_extends, True),
    ("self-call, no recursion in source", rule_self_call_not_recursive, False),
    ("extends a non-class", rule_extends_non_class, False),
    ("crosses a language boundary", rule_cross_language, True),
    ("targets a vendored bundle", rule_vendored_target, False),
]

SEMANTIC = ("calls", "extends", "references")


def fanout(edges):
    """How many edges leangraph emits per (caller, callee name).

    A name matched to eight definitions emits eight edges. Only one target is
    usually right, so the ratio of groups to edges indicates a bound on
    precision — *indicative*, not a proof, in two ways worth stating: a caller
    with two separate call sites for the same name is over-merged here, and
    genuine polymorphism can make more than one member of a group correct.
    """
    groups = defaultdict(int)
    for e in edges:
        if e["ek"] != "calls":
            continue
        groups[(e["sq"], e["sf"], e["sb"], e["dq"].rsplit(".", 1)[-1])] += 1
    n_edges = sum(groups.values())
    return len(groups), n_edges, (100.0 * len(groups) / n_edges if n_edges else 0.0)


def report(repo):
    root = os.path.join(REPOS, repo) if not os.path.isdir(repo) else repo
    name = os.path.basename(root.rstrip("/"))
    edges = load(root)
    src = Source(root)
    BARE_CALL.clear()

    sem = [e for e in edges if e["ek"] in SEMANTIC]
    fired = defaultdict(set)          # rule -> set of edge indices
    by_bucket = Counter()
    falsified = defaultdict(set)      # bucket -> indices

    indep = defaultdict(set)
    for i, e in enumerate(sem):
        b = (e["ek"], e["prov"], e["conf"])
        by_bucket[b] += 1
        for rname, fn, enforced in RULES:
            try:
                hit = fn(e, src)
            except Exception:
                hit = False
            if hit:
                fired[rname].add(i)
                falsified[b].add(i)
                if not enforced:
                    indep[b].add(i)

    print(f"\n\033[1m{name}\033[0m — {len(sem):,} semantic edges "
          f"({len(edges):,} total, {len(edges)-len(sem):,} structural)")

    print("\n  \033[2mfalsified by rule\033[0m")
    for rname, _, enforced in RULES:
        n = len(fired[rname])
        tag = "  \033[2m← the resolver now forbids this\033[0m" if enforced else ""
        print(f"    {rname:<38} {n:>7,}  {100.0*n/len(sem):>5.2f}%{tag}")
    any_fired = set().union(*fired.values()) if fired else set()
    any_indep = set().union(*indep.values()) if indep else set()
    print(f"    {'—— any rule ——':<38} {len(any_fired):>7,}  "
          f"{100.0*len(any_fired)/len(sem):>5.2f}%")
    print(f"    \033[1m{'—— rules the resolver does not enforce ——':<38} {len(any_indep):>7,}  "
          f"{100.0*len(any_indep)/len(sem):>5.2f}%\033[0m")

    print("\n  \033[2mfloor on the error rate, by confidence bucket\033[0m")
    print(f"    {'kind':<11}{'provenance':<11}{'conf':>5}{'edges':>9}"
          f"{'any rule':>10}{'independent':>13}{'floor':>8}")
    for b in sorted(by_bucket, key=lambda x: (x[0], -x[2])):
        ek, prov, conf = b
        n, f, ind = by_bucket[b], len(falsified[b]), len(indep[b])
        print(f"    {ek:<11}{prov:<11}{conf:>5}{n:>9,}{f:>10,}{ind:>13,}"
              f"{100.0*ind/n:>7.1f}%")

    g, n, ratio = fanout(edges)
    print(f"\n  \033[2mfan-out\033[0m  {n:,} call edges over {g:,} (caller, name) groups"
          f"  →  indicative precision ceiling {ratio:.1f}%")
    return name, len(sem), len(any_indep)


def main():
    targets = sys.argv[1:] or ["flask", "django", "excalidraw"]
    rows = []
    for t in targets:
        try:
            rows.append(report(t))
        except subprocess.CalledProcessError:
            print(f"\n  SKIP {t}: no graph — run `leangraph index` first")
    if len(rows) > 1:
        print("\n\033[1m  floor across corpora (independent rules only)\033[0m")
        for n, sem, bad in rows:
            print(f"    {n:<14}{bad:>8,} of {sem:>9,}   {100.0*bad/sem:>5.2f}%")
        print("\n  \033[2mThe percentage has a denominator that moves when the resolver\n"
              "  changes, so a before/after comparison must quote the counts too.\033[0m")
    print("\n  A rule that does not fire is not a correct edge. This is a floor.\n")


if __name__ == "__main__":
    main()
