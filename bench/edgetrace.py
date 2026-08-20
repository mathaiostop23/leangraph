#!/usr/bin/env python3
"""Record the call edges a program actually takes.

Every other check in this directory compares leangraph to another static tool, and
two static tools can agree and both be wrong. Running the code settles it: if
`Flask.send_file` called `send_file` during the test suite, that edge exists,
and a static graph missing it is missing it. No amount of agreement changes
that.

This gives RECALL and nothing else. An edge the tests never exercised is
unobserved, not absent, so nothing here may be read as precision — the
denominator for any statement about leangraph's edges has to be restricted to
callers that actually ran, which the reader downstream does.

`sys.monitoring` (3.12+) rather than `setprofile`: it is per-tool, it is
process-global so threads are covered without extra plumbing, and returning
DISABLE permanently silences a code object, which is what keeps the overhead to
roughly 1.5x rather than the 10x a Python-level profile hook costs.

Usage:
  bench/edgetrace.py --root <repo> --out <file.jsonl> -- -m pytest tests -q
"""
import argparse, json, os, runpy, sys

CO_OPTIMIZED = 0x01      # a real function frame; class and module bodies lack it
CO_GENERATOR = 0x20
CO_COROUTINE = 0x80
CO_ASYNC_GEN = 0x200
TOOL_ID = 2              # coverage.py takes 1; leave it free


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--root", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--require", action="append", default=[],
                    help="package that must import from --root, else abort")
    ap.add_argument("rest", nargs=argparse.REMAINDER)
    args = ap.parse_args()

    root = os.path.abspath(args.root)
    argv = args.rest[1:] if args.rest and args.rest[0] == "--" else args.rest
    if not argv:
        sys.exit("nothing to run: pass the command after `--`")

    # Fail loud on the mistake that presents as an interesting number rather
    # than as an error: importing the *installed* package instead of the
    # repository's, which yields a real trace of the wrong source tree.
    import importlib.util
    for pkg in args.require:
        spec = importlib.util.find_spec(pkg)
        origin = getattr(spec, "origin", None) if spec else None
        if not origin or not os.path.abspath(origin).startswith(root + os.sep):
            sys.exit(f"{pkg} resolves to {origin}, not to {root} — set PYTHONPATH")

    mon = sys.monitoring
    E = mon.events

    edges = {}          # (cf, cq, cl, df, dq, dl, hops) -> count
    executed = {}       # (file, qualname) -> times entered
    flags = {}          # (file, qualname) -> "generator" | "coroutine" | ""
    bases = {}          # qualname -> [base qualnames], filled after the run
    stats = {"events": 0, "disabled": 0, "no_caller": 0, "nonlocal_caller": 0,
             "stale_bytecode": 0}
    # A rejected path that ends with the repository's own name is almost always
    # stale bytecode: `__pycache__` records the co_filename the module had when
    # it was compiled, so moving a checkout leaves every cached module claiming
    # its old location. The tracer then silences them all and reports a
    # perfectly plausible, badly wrong number — 584 edges instead of 2,712,
    # with no error anywhere.
    marker = os.sep + os.path.basename(root) + os.sep

    seen_paths = {}
    # The directory the run started in. A code object's co_filename can be
    # relative, and `abspath` would resolve it against whatever the *current*
    # directory is — which pytest changes constantly, because tests chdir into
    # temporary directories. A file resolved while cwd was elsewhere looks
    # foreign, gets DISABLE'd, and is then silenced for the rest of the process.
    # Measured on flask: 19 of 485 test functions were ever seen.
    base = os.getcwd()

    def rel(path):
        if not path:
            return None
        # `<frozen posixpath>`, `<string>`, `<stdin>` and friends are not files.
        # `abspath` happily turns them into <cwd>/<frozen posixpath>, which lands
        # inside the repository root and pulls a third of the standard library
        # into the trace — measured at 36.4% of flask's edges before this guard.
        # The number that produces is not obviously wrong, which is what makes
        # it dangerous.
        if path.startswith("<"):
            return None
        ap = path if os.path.isabs(path) else os.path.join(base, path)
        ap = os.path.normpath(ap)
        if not ap.startswith(root + os.sep):
            return None
        r = os.path.relpath(ap, root).replace("\\", "/")
        # A vendored environment inside the tree is not the repository.
        if set(r.split("/")) & {".venv", "venv", "site-packages", ".tox",
                                "node_modules", "__pycache__"}:
            return None
        # And it has to be a file that exists, because a synthetic name without
        # angle brackets would otherwise slip through the check above.
        if r not in seen_paths:
            seen_paths[r] = os.path.isfile(ap)
        return r if seen_paths[r] else None

    def on_start(code, _offset):
        stats["events"] += 1
        callee_rel = rel(code.co_filename)
        if callee_rel is None:
            stats["disabled"] += 1
            if marker in code.co_filename:
                stats["stale_bytecode"] += 1
            # Permanently silence this code object: foreign libraries dominate
            # the event count and none of their frames can ever be an edge we
            # claim to have.
            return mon.DISABLE
        if not (code.co_flags & CO_OPTIMIZED):
            # A class body or a module body. Both fire PY_START and neither is
            # a call an agent would ever ask about.
            return mon.DISABLE

        dq = code.co_qualname
        dkey = (callee_rel, dq)
        executed[dkey] = executed.get(dkey, 0) + 1
        if dkey not in flags:
            f = code.co_flags
            flags[dkey] = ("coroutine" if f & (CO_COROUTINE | CO_ASYNC_GEN)
                           else "generator" if f & CO_GENERATOR else "")

        # _getframe(1) is the frame that just started; the caller is above it.
        # Walk past foreign frames so a call routed through a decorator or a
        # library callback still attributes to the repository function that
        # started it — but record how far we walked, because those edges are
        # weaker evidence and must be reportable apart.
        hops = 0
        try:
            f = sys._getframe(2)
        except ValueError:
            stats["no_caller"] += 1
            return None
        while f is not None:
            c = f.f_code
            r = rel(c.co_filename)
            if r is not None and (c.co_flags & CO_OPTIMIZED):
                k = (r, c.co_qualname, c.co_firstlineno,
                     callee_rel, dq, code.co_firstlineno, hops)
                edges[k] = edges.get(k, 0) + 1
                return None
            hops += 1
            f = f.f_back
        stats["nonlocal_caller"] += 1
        return None

    mon.use_tool_id(TOOL_ID, "leangraph-edgetrace")
    mon.register_callback(TOOL_ID, E.PY_START, on_start)
    mon.set_events(TOOL_ID, E.PY_START)

    code = 0
    try:
        if argv[0] == "-m":
            sys.argv = argv[1:]
            runpy.run_module(argv[1], run_name="__main__", alter_sys=True)
        else:
            sys.argv = argv
            runpy.run_path(argv[0], run_name="__main__")
    except SystemExit as e:
        code = e.code if isinstance(e.code, int) else 1
    finally:
        mon.set_events(TOOL_ID, 0)
        mon.register_callback(TOOL_ID, E.PY_START, None)
        mon.free_tool_id(TOOL_ID)

    # The runtime class hierarchy, so a downstream rule can tell that a call
    # recorded against `Sub.method` satisfies a static edge to `Base.method`.
    for mod in list(sys.modules.values()):
        f = getattr(mod, "__file__", None)
        if not f or rel(f) is None:
            continue
        try:
            members = list(vars(mod).values())
        except Exception:
            continue
        for obj in members:
            if isinstance(obj, type):
                try:
                    bases[obj.__qualname__] = [b.__qualname__ for b in obj.__mro__[1:]]
                except Exception:
                    pass

    with open(args.out, "w") as out:
        out.write(json.dumps({
            "kind": "header",
            "python": list(sys.version_info[:3]),
            "root": root,
            "argv": argv,
            "stats": stats,
            "distinct_edges": len(edges),
            "executed_functions": len(executed),
            "exit_code": code,
        }) + "\n")
        for (cf, cq, cl, df, dq, dl, hops), n in sorted(edges.items()):
            out.write(json.dumps({
                "kind": "edge", "cf": cf, "cq": cq, "cl": cl,
                "df": df, "dq": dq, "dl": dl, "hops": hops, "n": n}) + "\n")
        for (f, q), n in sorted(executed.items()):
            out.write(json.dumps({"kind": "ran", "f": f, "q": q, "n": n,
                                  "flag": flags.get((f, q), "")}) + "\n")
        for q, bs in sorted(bases.items()):
            out.write(json.dumps({"kind": "bases", "q": q, "b": bs}) + "\n")

    if stats["stale_bytecode"] > 20:
        print(f"\n  \033[31mWARNING\033[0m {stats['stale_bytecode']:,} code objects "
              f"named a path under a *different* checkout of this repository.\n"
              f"  That is stale bytecode from a moved or renamed directory. Delete\n"
              f"  __pycache__ and .pytest_cache under {root} and run again — the\n"
              f"  numbers below are wrong, and wrong in a way that looks fine.\n",
              file=sys.stderr)
    print(f"  traced {len(edges):,} distinct in-repo edges over "
          f"{len(executed):,} executed functions "
          f"({stats['events']:,} events, {stats['disabled']:,} silenced)",
          file=sys.stderr)
    return 0


if __name__ == "__main__":
    sys.exit(main())
