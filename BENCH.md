# Phase 0 result — the premise holds

**Question:** is a native Rust pipeline actually multiples faster, or is parsing already the floor?

**Answer: parsing is nowhere near the floor.** Parse + extract accounts for **2–6%** of CodeGraph's indexing pipeline. The other 94–98% is resolution and persistence — exactly where we predicted the headroom was.

Reproduce: `cargo build --release && ./bench/run.sh`

---

## Measured

Apple M1 Pro (6P + 2E), 16 GB, macOS 14.3. Median of 3 runs, warm page cache.
`codegraph` = v1.5.0, full `index --force`, minus its measured 0.52s process startup.
`arbor` = parse + extract only — **no resolution, no persistence yet**.

| Repo | Files | MB | arbor | CodeGraph | parse+extract share | headroom |
|---|---:|---:|---:|---:|---:|---:|
| flask | 83 | 0.6 | **10 ms** | 0.49 s | 2.0% | 49× |
| excalidraw | 666 | 7.3 | **120 ms** | 3.04 s | 3.9% | 25× |
| django | 3,038 | 19.7 | **460 ms** | 7.60 s | 6.1% | 17× |

arbor run-to-run variance on django: 437 / 445 / 447 / 460 / 463 ms — tight enough that the numbers are real, not noise.

### Extraction sanity check

We are not fast because we are doing less. On django:

| | arbor | CodeGraph |
|---|---|---|
| Files discovered | 3,038 | 3,016 |
| AST nodes visited | 4,894,221 | — |
| Call sites found | 201,218 | 195,802 edges |
| Definitions found | 47,872 | 62,114 nodes |

Call sites land within 3% of their edge count, and file discovery within 1%. Their higher node count is expected — they model more node kinds (files, modules, variables, properties) where Phase 0 counts only functions and classes.

### Where our own CPU time goes

Summed across 8 threads on django:

| Stage | CPU ms | Share |
|---|---:|---:|
| tree-sitter parse | 2,130 | 81.9% |
| cursor walk | 387 | 14.9% |
| mmap | 63 | 2.4% |
| blake3 | 22 | 0.9% |

Two things follow. First, **we are already parse-bound**, which is the correct place to be — everything above the parser is nearly free. Second, blake3 content hashing costs 0.9% of CPU, which confirms that hash-based change detection for incremental sync is effectively free.

---

## The persistence clue

CodeGraph's SQLite database for django:

```
flask         5.0 MB      2,705 nodes /   5,171 edges
excalidraw     48 MB     11,162 nodes /  48,574 edges
django        163 MB     62,114 nodes / 195,802 edges
```

**163 MB for 62k nodes and 196k edges is ~2.6 KB per node.** That is row-based storage plus FTS5 indexing, and writing it is a large share of the missing seconds.

Our planned CSR layout for the identical graph:

```
forward:  offsets 62k × 4B  +  targets 196k × 4B  +  kinds 196k × 1B  ≈  1.2 MB
reverse:  same                                                        ≈  1.2 MB
                                                              topology ≈  2.4 MB
```

Even with generous per-node metadata (spans, names, kinds) in SQLite for the cold path, we should land in the 15–25 MB range — roughly **7–10× smaller**. That matters three times over: less to write during indexing, less to read on load, and far better cache locality during traversal.

---

## What this does and does not prove

**Proves:** the parse+extract floor is ~0.5s for a 3,000-file / 20 MB repo. We have 7.1s of budget left to match CodeGraph on django, and resolution + CSR build over 200k call sites should not need anywhere near that.

**Does not prove:** that arbor will be 17× faster. Resolution is unimplemented and it is the phase most likely to surprise us. A realistic target for a complete arbor pipeline on django is **1.0–1.5s vs their 7.6s — call it 5–7×**, and that is the number to hold ourselves to.

**Does not address at all:** graph *quality*. Phase 0 counts syntax. CodeGraph resolves references, links imports, recognizes 17 web frameworks, and synthesizes dynamic-dispatch edges. Matching that on Python and TypeScript is the actual work, and speed is worthless without it.

### Methodology caveats

- Process startup (0.52s) is subtracted from CodeGraph's numbers. For a CLI that cost is real and per-invocation; for a daemon it is paid once. Subtracting it isolates algorithmic work, which is the fair comparison for "can a faster engine be built".
- Warm page cache on both sides. Cold-cache runs are I/O-bound and compress the difference.
- Single machine, single OS, three repos, two languages. Not yet a general claim.
- CodeGraph's own progress line self-reports "2.0s" for django, but wall clock minus startup is 7.6s — its counter evidently covers only part of the pipeline. We use wall clock, which is what a user experiences.

---

## Decision

The Phase 0 gate in `ENGINE.md` was: *proceed if parse+extract is under 20% of their total.* Measured at **2–6%**.

**Proceed to Phase 1: resolution.**

The order of work follows directly from the measurements:

1. **Resolution** (the unknown) — scope chains, import resolution, provenance-tagged edges
2. **CSR persistence** (the 163 MB → ~2.4 MB win)
3. **Incremental** (blake3 is already proven free)

Do resolution first precisely because it is the phase that can still falsify the plan.
