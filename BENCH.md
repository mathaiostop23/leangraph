# Benchmarks

Differential measurement against [CodeGraph](https://github.com/colbymchenry/codegraph) v1.5.0 — same repos, same machine, same run.

Reproduce: `cargo build --release && ./bench/run.sh`

**Speed without a graph-size number next to it is meaningless** — anyone can be fast by extracting less. Every table here reports both.

---

## Cost — tokens to reach the files that actually changed

This is the claim that matters and the one nobody in this space publishes.
"Faster" is a stopwatch; "cheaper" needs ground truth.

`bench/cost.py`, django, 30 bug-fix commits. Ground truth is the set of files
each fix actually touched; the query is the commit message. Entirely local and
reproducible — no API token, no curated dataset. The baseline is keyword search
(tokenise, `git grep`, rank by hit count, read the top *k*), which is what an
agent without a structural index actually does.

| approach | recall | tokens/query | tokens per recall point |
|---|---:|---:|---:|
| arbor n=10 | 19.5% | 1,219 | **62** |
| arbor n=25 | 30.5% | 2,500 | **82** |
| arbor n=50 | 34.1% | 3,948 | **116** |
| **arbor n=100** | **50.0%** | **9,479** | 190 |
| arbor n=200 | 52.4% | 17,520 | 334 |
| keyword top-3 | 28.0% | 87,703 | 3,127 |
| keyword top-5 | 40.2% | 146,296 | 3,635 |
| keyword top-10 | 52.4% | 261,812 | 4,993 |
| keyword top-20 | 63.4% | 409,488 | 6,457 |

Two readings, both fair:

- **At matched recall (52.4%): 17,520 tokens against 261,812 — 14.9x fewer.**
- **arbor at n=100 beats keyword top-5 on recall (50.0% vs 40.2%) while costing
  15x less.**

The efficiency column is the durable number: a recall point costs arbor 62–334
tokens and keyword search 3,127–6,457. That gap is the product.

### Co-change: a negative result, then a positive one

Git co-change edges were added to attack exactly this ceiling — the ground-truth
files a fix touches but never names. Mixed into the normal ranking they made
recall *worse* (45.1% → 43.9% at n=100): they arrive at the second hop with
decay applied and confidence below any AST-derived edge, so they never survived
the budget cut while still displacing better candidates.

The fix was to stop treating them as weaker evidence of the same kind. They are
a *different* kind — historical coupling is precisely what the AST cannot see —
so they get a reserved fifth of the budget instead of competing on one scale.
That took n=100 from 45.1% to **50.0% at fewer tokens** (9,479 vs 10,140).

### Where it stops

Keyword search reaches 63% by brute force at 409k tokens; arbor tops out near
52%. Raising that is retrieval work, not budget work — arbor at n=200 already
leaves most of its budget unspent on the queries it fails.

### What this measurement changed

The first run flattened at 41.5% no matter how large the budget got. The cause
was a fixed cap of 8 seeds: at 200 nodes the expansion had only 8 places to
expand from, so the budget went unused. Scaling seeds with the budget and adding
a decayed second hop took it to 51.2%.

---

## Correctness — differential verification

`bench/verify.py` indexes the same repo with both engines and diffs the symbol
sets. CodeGraph validates its own extraction as byte-identical against a
reference engine across 31 repos, which makes it the best ground truth we did
not have to build. This measures **agreement, not truth** — where we differ,
either side may be right — but it makes every difference visible.

| Repo | presence recall | kind agreement | we add |
|---|---:|---:|---:|
| flask | **100.0%** | 100.0% | 89 |
| excalidraw | **85.1%** | 84.4% | 2,307 |
| django | **100.0%** | 100.0% | 9,462 |

**Presence recall across 3 repos: 95.0%.**

Presence and kind are reported separately on purpose. A symbol we extract but
label `variable` where they say `method` is a taxonomy difference, not a missing
node, and the fix is completely different.

### What the remaining gap is

excalidraw's 864 missing symbols are **803 function-local variables** plus 51
methods and 12 functions. The locals are a deliberate choice, not a bug: we
record named values at module and class scope, where they are part of the API
surface, and skip locals inside function bodies, where they are noise in a code
graph. The real extraction gap is ~1% of symbols.

### What this caught

The first run reported 64.6% recall, with 25,604 "missing" methods in django. They
were not missing — Python has no separate node kind for a method (`def` is
`function_definition` at every level), so we were labelling every one of them
`function`. Reclassifying by enclosing scope took django and flask to 100%.

A speed number published before this ran would have been meaningless.

---

## MCP startup — spawn to `initialize` response

`bench/mcp_startup.py`, median of 5 cold processes:

```
arbor          3.9 ms
codegraph    561.7 ms        144x
```

Structural, not tuning: `Graph::open` is an mmap plus a header check, so there
is nothing to warm. CodeGraph's own CLAUDE.md names startup as the reason
agents "dive into Read/grep before codegraph finishes its ~2-3s startup" — the
561.7 ms measured here is with the npm package already resolved and warm, so it
is the friendly end of their range.

---

## Full pipeline — Phase 3

| Repo | Files | arbor | CodeGraph | speedup | arbor nodes/edges | CG nodes/edges | resolved |
|---|---:|---:|---:|---:|---:|---:|---:|
| flask | 83 | **20 ms** | 0.51 s | 26x | 1,963 / 5,588 | 2,705 / 5,171 | 96.1% |
| excalidraw | 666 | **140 ms** | 3.06 s | 22x | 9,915 / 38,706 | 11,162 / 48,574 | 99.6% |
| django | 3,038 | **560 ms** | 7.87 s | 14x | 67,924 / 293,254 | 62,114 / 195,802 | 87.7% |

On disk: flask 168 KB / excalidraw 1.3 MB / **django 7.9 MB** — against CodeGraph's
5.0 MB / 48 MB / 163 MB.

---

## Earlier — Phase 1b (extract + resolve, before persistence)

Apple M1 Pro (6P + 2E), 16 GB, macOS 14.3. Median of 3 runs, warm page cache.
CodeGraph timings have its measured 0.50 s process startup subtracted.

| Repo | Files | arbor | CodeGraph | speedup | arbor nodes | CG nodes | arbor edges | CG edges |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| flask | 83 | **20 ms** | 0.51 s | 26× | 1,963 | 2,705 | 5,161 | 5,171 |
| excalidraw | 666 | **120 ms** | 3.06 s | 26× | 9,915 | 11,162 | 48,721 | 48,574 |
| django | 3,038 | **500 ms** | 7.69 s | 15× | 67,924 | 62,114 | 331,743 | 195,802 |

Edge counts land within 1% on flask and excalidraw and 69% higher on django — we emit multiple candidates for ambiguous name matches, each carrying its own confidence, where CodeGraph emits one or none.

**arbor does not persist yet.** Phase 2 adds the CSR write, so these numbers will grow. Treat them as a floor, not a product number.

### Resolution quality

| Repo | in-repo refs resolved | scope | import | name (unique) | name (ambig) |
|---|---:|---:|---:|---:|---:|
| flask | **91.7%** | 8.6% | 2.3% | 32.5% | 11.4% |
| excalidraw | **99.1%** | 10.6% | 8.8% | 21.2% | 8.8% |
| django | **81.9%** | 8.1% | 7.4% | 22.6% | 19.8% |

**Denominator matters here.** The rate is over references *whose target exists in this repository*. Calls to `len()` or `react.useState` have no in-repo definition, so failing to link them is correct behaviour, not a miss — counting them as failures would understate the resolver, and dropping them silently would flatter it. All three buckets are reported separately by the CLI:

```
django   builtin (runtime)   12,920   6.4%     ← language runtime
         external (deps)     51,219  25.5%     ← third-party packages
         too ambiguous       20,784  10.3%     ← >8 equally-ranked candidates
```

---

## Phase 0 — the premise (archived)

The question that justified building an engine at all: **is parsing the bottleneck?** It is not.

| Repo | arbor parse+extract only | CodeGraph full | parse's share |
|---|---:|---:|---:|
| flask | 10 ms | 0.49 s | 2.0% |
| excalidraw | 120 ms | 3.04 s | 3.9% |
| django | 460 ms | 7.60 s | 6.1% |

Parse + extract is **2–6%** of CodeGraph's pipeline. The gate was "proceed if under 20%". The remaining 94–98% is resolution and persistence — and resolution turned out to cost us **16 ms** on django, so the bulk of the difference is I/O and storage layout.

Our own CPU is ~75–82% tree-sitter parse, i.e. already parse-bound, which is the right place to be. blake3 content hashing is 0.9% of CPU, so hash-based change detection for incremental sync is effectively free.

---

## The persistence gap

CodeGraph's SQLite databases:

```
flask         5.0 MB      2,705 nodes /   5,171 edges
excalidraw     48 MB     11,162 nodes /  48,574 edges
django        163 MB     62,114 nodes / 195,802 edges
```

163 MB for 62k nodes is ~2.6 KB per node — row-based storage plus FTS5 indexing.

Planned CSR layout for django's graph (67,924 nodes / 331,743 edges):

```
forward:  offsets 68k × 4B  +  targets 332k × 4B  +  kinds 332k × 1B  ≈  1.9 MB
reverse:  same                                                        ≈  1.9 MB
                                                            topology  ≈  3.8 MB
```

Even with per-node metadata in SQLite for the cold path, we expect 15–25 MB against their 163 MB. That matters three times over: less to write during indexing, near-zero to load (`mmap` rather than deserialize), and far better cache locality during traversal.

---

## Methodology

- **Startup subtracted from CodeGraph.** Its 0.50 s is real and per-invocation for a CLI, but paid once for a daemon. Subtracting isolates algorithmic work, which is the fair comparison for engine design. Our own startup is not yet measured; Phase 3 will report it, and it is where a static binary with an mmap'd graph should win outright.
- **Warm page cache on both sides.** Cold-cache runs are I/O-bound and compress the difference.
- **Same file-size skip.** Both ignore files over 1 MB.
- **Not yet a general claim.** One machine, one OS, three repos, two languages.
- **CodeGraph's own progress line self-reports ~2.0 s for django**, but wall clock minus startup is 7.7 s — its counter evidently covers only part of the pipeline. We use wall clock, which is what a user experiences.

## What is still unmeasured

- **Persistence** — arbor has none yet.
- **Query latency** — the CSR-vs-SQLite claim is a design argument until `arbor query` exists.
- **Incremental sync** — the delta-overlay design is unbuilt.
- **Edge correctness** — we count edges, we do not yet verify them. Differential node/edge diffing against CodeGraph as an oracle is the Phase 2 gate.
- **Cost** — `bench/cost.sh` does not exist. Recall@k per token, on real issue→PR pairs, is the claim that actually matters and it has not been attempted.
