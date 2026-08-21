# leangraph

**Give a coding agent the relevant code instead of making it grep for it.**

leangraph reads a repository once and builds a graph of what calls what — every
function, class, import and reference, with a confidence score on each link.
That graph then answers the question an agent actually has: *given this bug
report, which forty lines should I read?*

Without it, an agent greps. Grep returns every file containing the word, the
agent reads them all, and you pay for the ones that were irrelevant. On django
that is the difference between **9,444 tokens and 158,685** for a better answer.

```bash
cargo install --path .               # or: cargo build --release, then ./target/release/leangraph
leangraph index /path/to/repo        # django, 3,038 files → 0.75 s
leangraph install                    # wire it into Claude Code, Cursor, Codex
```

Needs a Rust toolchain (1.82+) and nothing else — no runtime, no database, no
services. There are no prebuilt binaries yet.

**Status: early.** Fourteen languages, eleven of them verified against an
oracle on a real repository — 96.5% presence recall across ten corpora. The
numbers below are measured and reproducible: every one comes from a script in
`bench/` that you can run yourself. See also
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
curl -X POST localhost:7777/repos -H "authorization: Bearer $TOKEN" \
  -H 'content-type: application/json' -d '{"url":"https://github.com/owner/name"}'
```

One binary, one volume, **no database container** — SQLite is compiled in. Point
a GitHub webhook at `/webhook/github`, label an issue `leangraph`, and the answer
arrives as a comment with what it cost.

Everything except the webhook and the health probe needs an admin token, printed
at startup and generated on first run. That surface can register a repository
against your API budget, write a secret, and enable the mode that opens pull
requests, so it does not default to open — send `Authorization: Bearer …`, or
open the dashboard at `/?token=…`. `--no-auth` turns the gate off for anyone who
already has one in front.

A rate limit is not a verdict on the issue, so it is not treated as one. Only
failures that are actually transient — 429, 5xx, a connection that never landed
— go back on the queue, backing off 30 s, 2 m, 8 m before giving up; a malformed
request fails once and stays failed. And because a job is claimed before it runs,
killing the process mid-run used to strand it in `running` for the life of the
database: never claimed again, never reported, shown as permanently in progress.
Startup requeues those now.

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

| | files | index | graph | everything on disk | CodeGraph |
|---|---:|---:|---:|---:|---:|
| flask | 83 | **0.05 s** | 172 KB | 1.0 MB | 5.0 MB |
| excalidraw | 666 | **0.18 s** | 1.0 MB | 7.6 MB | 48 MB |
| django | 3,038 | **0.75 s** | 6.9 MB | 39 MB | 156 MB |

Full pipeline on both sides — discover, parse, resolve *and* persist. **10× faster**
on django, and 4× smaller counting everything.

Two size columns because two things are true. The **graph** is what a query
loads: 6.9 MB against CodeGraph's 156 MB database, which is the comparison that
matters for startup and memory. But `.leangraph/` also holds a 33 MB extraction
cache, and quoting only the first number would be the kind of selective framing
this project exists to avoid. The cache is what makes re-running after a change
**0.11 s** instead of 0.75.

**Startup: 2.4 ms** against CodeGraph's 556 ms, spawn to MCP `initialize`
(`bench/mcp_startup.py`). This gap is structural, not tuning: loading is not
deserialization. `Graph::open` is an `mmap` plus a header check, so a
68,000-node graph is queryable in 15 µs once its pages are resident, and
under a millisecond on the very first touch. CodeGraph's own
notes name startup as the reason agents give up and reach for grep first.

### Cost — the claim that matters

`bench/cost.py`. Ground truth is the files each bug-fix commit touched; the
query is the commit message. The baseline is what an agent without a structural
index actually does: tokenise, `git grep`, read the top *k*.

| approach | recall | tokens / query |
|---|---:|---:|
| **leangraph, 100 nodes** | **40.0%** | **9,444** |
| leangraph, 25 nodes | 23.5% | 2,365 |
| keyword, top 5 | 35.7% | 158,685 |
| keyword, top 10 | 45.2% | 259,328 |

Better recall than keyword top-5 at **1/16th the tokens**. Keyword search still
wins on raw recall if you let it read 424k tokens; closing that gap is retrieval
work, not budget work.

### Is the graph actually right?

Three independent checks, because a fast graph that is wrong is just wrong.

**Nodes** — differential against CodeGraph (`bench/verify.py`), on a real
repository per language rather than a fixture:

| corpus | language | presence recall | kind agreement |
|---|---|---:|---:|
| flask | python | 100.0% | 100.0% |
| django | python | 100.0% | 100.0% |
| Newtonsoft.Json | c# | 100.0% | 100.0% |
| monolog | php | 100.0% | 100.0% |
| leangraph | rust | 100.0% | — |
| pkg/errors | go | 100.0% | — |
| upickle | scala | 99.7% | 97.3% |
| JSON-java | java | 98.7% | — |
| paint | ruby | 97.3% | — |
| leveldb | c++ | 96.3% | 91.1% |
| Alamofire | swift | 95.3% | 95.2% |
| libuv | c | 95.2% | 92.4% |
| okhttp | kotlin | 93.5% | 89.9% |
| excalidraw | typescript | 85.1% | 84.5% |

**96.5% across the ten corpora measured together.** excalidraw's gap is
function-local variables, skipped deliberately.

Every language here was checked against a real repository rather than a
fixture, and doing that is what found the gaps — seven times now, in seven
languages that all passed the fixture test:

| | was | is | what was wrong |
|---|---:|---:|---|
| kotlin | 73.5% | 93.5% | the grammar labels no field for a property's name, and requiring one dropped every `val` and `var` — 2,056 symbols |
| php | 81.0% | 100.0% | the name field points at `$errorLevelMap`, sigil included; the bare identifier is a level below. Class constants were not in the spec at all |
| c | 86.9% | 95.2% | `typedef` was not a definition, and `uv.h` is mostly typedefs |
| c++ | 89.3% | 96.3% | `.h` was read as C by extension, so in a repository that is 56 headers to 33 sources the classes were never nodes. Out-of-line `void Impl::Get()` was a plain function, because the class is named in the declarator and not in the enclosing scope |
| rust | 83.6% | 100.0% | module constants and type aliases were not extracted |
| ruby | 62.2% | 97.3% | `var_def_name` carried a hardcoded list of identifier node kinds, written for Python and TypeScript, that rejected every Ruby constant |

Kind agreement is reported separately because it is a different failure: a node
we extract but label `function` where the oracle says `method` is present in
the graph and findable, and the fix is not the same one.

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

Everything below runs in CI on every push, without the benchmark corpora — this
repository is Rust, which the indexer supports, so it can be its own corpus.

```
58  engine unit          resolution tiers, receivers, budgets, id stability, round-trips
27  server unit          vetting rules, dedup scoring, SSRF, schema migration
 5  convergence          incremental sync equals a full reindex
21  webhook gates        signature, replay, authorship, labels
38  agent assertions     prompt safety, cache correctness
23  fix mode, end-to-end against a real git remote
10  deduplication, end-to-end
 7  retry and recovery, end-to-end against a rate-limited API
13  languages, each connecting two methods on a fixture
```

Two of them exist because of how this can break silently: every node kind and
field name a spec asks for must exist in its grammar, and every language must
define and call something. `kinds()` drops a name the grammar does not know, so
a spec that a grammar upgrade has outdated still compiles, still runs, and
quietly stops finding whatever that node was for. The first run of that test
found a stale entry in the TypeScript spec that had never resolved.

The engine tests were written last and should have been written first. Until
then the only thing guarding resolution was `bench/` against three cloned
repositories — which does catch regressions, but only ones large enough to move
an aggregate, only when someone runs it, and never in CI, where the corpora do
not exist. Three had already reached `master` that way: a receiver-blind filter
that deleted 198 real edges, a `same_family` that returned false for eleven
languages and disabled cross-file name matching for all of them, and an alias
binding that removed 2,926 edges from one repository. Each is now a few lines of
source and one assertion, and each was checked by reintroducing the original bug
and confirming the right test fails. Writing them turned up two more: an
out-of-range node id answered with a real symbol name instead of nothing, and
`import x as y` interned the module as `"x as y"`, which matched no module and
quietly cost every aliased import its evidence.

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

- **Fourteen languages, six measured against an oracle.** Python, TypeScript,
  Rust, Go and Java and Ruby have been checked on a real repository; C, C++, C#,
  PHP, Kotlin, Swift and Scala are verified on a fixture only, which proves the
  spec works on ordinary code and not that it works on a codebase. CodeGraph has
  30+ with framework awareness.
- **Import resolution is uneven across languages.** Rust, Java, C#, Kotlin and
  Scala resolve module paths; the rest fall back to name matching more often
  than Python and TypeScript do.
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
