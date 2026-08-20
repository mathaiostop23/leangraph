# leangraph

**Give a coding agent the relevant code instead of making it grep for it.**

leangraph reads a repository once and builds a graph of what calls what — every
function, class, import and reference, with a confidence score on each link.
That graph then answers the question an agent actually has: *given this bug
report, which forty lines should I read?*

Without it, an agent greps. Grep returns every file containing the word, the
agent reads them all, and you pay for the ones that were irrelevant. On django
that is the difference between **9,563 tokens and 158,685** for a better answer.

```bash
cargo build --release
leangraph index /path/to/repo        # django, 3,038 files → 0.75 s
leangraph install                    # wire it into Claude Code, Cursor, Codex
```

**Status: early.** Python and TypeScript only. The numbers below are measured
and reproducible — every one of them comes from a script in `bench/` that you
can run yourself. The language coverage is not yet competitive; see
[what it does not do](#what-it-does-not-do).

---

## Two ways to use it

### In your editor, as an MCP server

```bash
leangraph install                      # Claude Code, Cursor, Codex
leangraph install cursor -p ~/myrepo   # or just one
leangraph install --uninstall
```

Config edits merge rather than overwrite — those files hold your other MCP
servers. Two tools are exposed, deliberately:

| tool | what it returns |
|---|---|
| `leangraph_explore(symbols[])` | the path between the named symbols plus surrounding code, ranked by confidence |
| `leangraph_node(symbol)` | one symbol's source, its callers and its callees |

Agents reliably call the first tool offered and under-pick the rest. CodeGraph
*removed* two of its own tools after measuring this. There is no third tool here
on purpose.

### On a server, answering your issues

```bash
docker compose up -d
curl -X POST localhost:7777/repos -H 'content-type: application/json' \
  -d '{"url":"https://github.com/owner/name"}'
```

One binary, one volume, **no database container** — SQLite is compiled in. Point
a GitHub webhook at `/webhook/github`, label an issue `leangraph`, and the answer
arrives as a comment with what it cost. Dashboard at `/`.

The label is the point. Nothing happens on an unlabelled issue, and nothing
happens on an issue from someone outside the repository. An issue body is
attacker-controlled text that ends up in front of a model, so it is wrapped and
declared as data, the agent is handed no tools, and the webhook signature is
verified over the raw bytes. Duplicate issues are detected locally and cost
nothing at all.

**Fix mode** proposes a patch as a *draft* pull request, and is off by default in
three independent ways: the repository must opt in, the issue needs a second
label, and a write token must exist. It will not push to the default branch,
force-push, merge, or touch CI configuration, dependency manifests or lockfiles
— refusals enforced in code before `git apply` runs.

---

## Measured

Apple M1 Pro. Against [CodeGraph](https://github.com/colbymchenry/codegraph)
v1.5.0 — same repos, same machine, same run, median of 3, its process startup
subtracted from its side. Reproduce with `./bench/run.sh`.

| | files | index | graph on disk | vs CodeGraph |
|---|---:|---:|---:|---:|
| flask | 83 | **0.05 s** | 168 KB | 10× faster, 30× smaller |
| excalidraw | 666 | **0.18 s** | 1.0 MB | 17× faster, 48× smaller |
| django | 3,038 | **0.75 s** | 6.8 MB | 10× faster, 23× smaller |

Full pipeline on both sides — discover, parse, resolve *and* persist. Re-running
after a change is **0.11 s** on django, because unchanged files come from cache.

**Startup: 2.4 ms** against CodeGraph's 556 ms, spawn to MCP `initialize`
(`bench/mcp_startup.py`). This gap is structural, not tuning: loading is not
deserialization. `Graph::open` is an `mmap` plus a header check, so a
68,000-node graph is queryable in 31 µs with nothing to warm up. CodeGraph's own
notes name startup as the reason agents give up and reach for grep first.

### Cost — the claim that matters

`bench/cost.py`. Ground truth is the files each bug-fix commit touched; the
query is the commit message. The baseline is what an agent without a structural
index actually does: tokenise, `git grep`, read the top *k*.

| approach | recall | tokens / query |
|---|---:|---:|
| **leangraph, 100 nodes** | **40.9%** | **9,563** |
| leangraph, 25 nodes | 23.5% | 2,364 |
| keyword, top 5 | 35.7% | 158,685 |
| keyword, top 10 | 45.2% | 259,328 |

Better recall than keyword top-5 at **1/16th the tokens**. Keyword search still
wins on raw recall if you let it read 424k tokens; closing that gap is retrieval
work, not budget work.

### Is the graph actually right?

Three independent checks, because a fast graph that is wrong is just wrong.

**Nodes** — differential against CodeGraph (`bench/verify.py`): 100% presence
recall on flask and django, 85.1% on excalidraw, **95.0% overall**. The gap is
function-local variables we skip deliberately.

**Edges** — flask's own test suite is run under `sys.monitoring` and every call
that actually happened is recorded (`bench/edgetrace.py`). An observed edge
exists; no amount of agreement between two static tools changes that.

| confidence | how it was resolved | runtime-confirmed |
|---:|---|---:|
| 100 | lexical scope | 60.0% |
| 95 | explicit import | 92.5% |
| 80 | unique name match | 37.7% |
| 60 | 2–4 candidates | 19.3% |
| 45 | 5–8 candidates | 5.2% |

**Confidence orders correctness** — which is the assumption the whole ranking
rests on, and it had never been tested. Tested against a null that preserves how
edges cluster by caller, the trend holds at `p = 0.0082`. A textbook test on the
same data claims `p = 1.5e-12`; almost all of that is the clustering, not the
confidence.

The first run said something else: the *proven* tier was losing to the guess.
Four real defects came out of that, and fixing them removed 26,921 edges from
django. Full account, including what the fixes broke and how that was caught, in
[BENCH.md](./BENCH.md).

**Incremental sync** — `bench/converge.sh` asserts that syncing produces a graph
semantically identical to a full reindex across modify, revert, add and delete.
Its first run found the graph was not reproducible *at all*: two full indexes of
identical source differed. Five invariants hold now.

### Tests

```
27  unit                 vetting rules, dedup scoring, SSRF, schema migration
 5  convergence          incremental sync equals a full reindex
12  webhook gates        signature, replay, authorship, labels
30  agent assertions     prompt safety, cache correctness
23  fix mode, end-to-end against a real git remote
10  deduplication, end-to-end
```

---

## How it works

```
discover ──▶ extract ──▶ resolve ──▶ persist
 ignore     tree-sitter   3 tiers      CSR
 crate      single pass   u32 ids      mmap
```

**Everything is a `u32`.** Resolution is name-matching; on `String` that is
hashing and memcmp in the inner loop, on interned integers it is equality. Text
is materialised only when a human or an agent sees it.

**Adjacency is CSR, both directions.** `callers(n)` is two array reads and a
slice — no B-tree descent, no row decode, no allocation. Both directions are
stored because the question an agent asks is "what breaks if this is wrong",
which is the reverse edge.

**Every edge carries confidence and provenance.** Not decoration: ranking by
confidence is how the context builder returns *fewer* tokens at equal recall, and
an agent that cannot tell a proven call from a guess weighs them equally.

**A guess must have syntactic evidence.** A call, an instantiation or a
superclass may be resolved by matching a name across the repository. A bare
identifier read may not — matching a local variable `request` against every
symbol called `request` was the single largest source of false edges when we
measured it.

---

## What it does not do

- **Two languages.** CodeGraph has 30+, with framework awareness and
  Swift↔ObjC bridging. Breadth is cheap at the extraction tier and expensive at
  the import/scope tier; neither cost has been paid yet.
- **Edge precision is bounded, not measured.** Runtime confirms edges that ran
  and is silent on the rest; the falsification rules give a floor, and a rule
  that does not fire is not a correct edge. Measured on one repository, in one
  language.
- **Recall tops out near 44%.** Raising it needs better retrieval signals.
- **Cost measured on one repository.** 40 bug-fix commits in django.
  Directionally strong, not a general claim.
- **Sync rewrites the whole graph.** Stable node ids are in place — the
  prerequisite for a delta overlay — but resolve and persist still redo
  everything for a one-line change.
- **Fix mode does not run the tests.** The graph knows which tests import a
  changed file; running them safely needs a sandbox that is not built. Every
  pull request it opens says so.

---

## Documents

| | |
|---|---|
| [BENCH.md](./BENCH.md) | every measurement, its methodology, and its caveats |
| [ENGINE.md](./ENGINE.md) | architecture, language tiers, cost design, risks |
| [SERVER.md](./SERVER.md) | the self-hosted issue service |
| [ROADMAP.md](./ROADMAP.md) | phases and where they stand |

MIT.
