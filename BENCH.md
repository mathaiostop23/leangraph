# Benchmarks

Differential measurement against [CodeGraph](https://github.com/colbymchenry/codegraph) v1.5.0 — same repos, same machine, same run.

Reproduce: `cargo build --release && ./bench/run.sh`

**Speed without a graph-size number next to it is meaningless** — anyone can be fast by extracting less. Every table here reports both.

---

## Current — Phase 1b (extract + resolve, not yet persisted)

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
