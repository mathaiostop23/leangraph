# arbor

A native code-graph engine. It indexes a repository into a memory-mapped graph of what calls what, then hands agents the *relevant* code instead of making them grep for it.

**Status: early.** Python and TypeScript only. The numbers below are measured and reproducible; the language coverage is not yet competitive. See [what it does not do](#what-it-does-not-do).

---

## Measured

Against [CodeGraph](https://github.com/colbymchenry/codegraph) v1.5.0 — same repos, same machine, same run. Apple M1 Pro, median of 3. Reproduce with `./bench/run.sh`.

| Repo | Files | arbor | CodeGraph | | arbor graph | CodeGraph graph |
|---|---:|---:|---:|---:|---:|---:|
| flask | 83 | **20 ms** | 0.51 s | 26× | 1,963 nodes / 5,588 edges | 2,705 / 5,171 |
| excalidraw | 666 | **140 ms** | 3.06 s | 22× | 9,915 / 38,706 | 11,162 / 48,574 |
| django | 3,038 | **560 ms** | 7.87 s | 14× | 67,924 / 293,254 | 62,114 / 195,802 |

On-disk size for django: **7.9 MB** against 163 MB. Row-based storage with a full-text index costs about 2.6 KB per node; CSR costs about 120 bytes.

### Startup — the number that decides whether an agent uses you at all

Time from process spawn to MCP `initialize` response (`bench/mcp_startup.py`, median of 5):

```
arbor          3.9 ms
codegraph    561.7 ms        144×
```

This gap is structural rather than tuning. Loading is not deserialization: `Graph::open` is an `mmap` plus a header check, so a 68,000-node graph is queryable in ~20 µs and there is nothing to warm up. CodeGraph's own `CLAUDE.md` names startup as the reason agents give up and reach for grep before it connects.

### Cost — the claim that actually matters

`bench/cost.py`, django, 30 bug-fix commits. Ground truth is the files each fix touched; the query is the commit message. The baseline is keyword search — tokenise, `git grep`, read the top *k* — which is what an agent without a structural index does.

| approach | recall | tokens/query | tokens per recall point |
|---|---:|---:|---:|
| arbor n=25 | 31.7% | 2,546 | **80** |
| **arbor n=100** | **45.1%** | **10,140** | 225 |
| arbor n=200 | 51.2% | 17,843 | 348 |
| keyword top-5 | 41.5% | 144,652 | 3,489 |
| keyword top-10 | 51.2% | 244,223 | 4,768 |

**At matched recall, 13.7× fewer tokens.** At n=100, arbor beats keyword top-5 on recall *and* costs 14× less.

Keyword search does reach 61% by brute force at 423k tokens; arbor tops out near 51%. The files it cannot reach are tests, docs and migrations touched by a fix but never named in its message — that is retrieval work (git co-change edges are the obvious next signal), not budget work.

### Correctness

Differential verification against CodeGraph as an oracle (`bench/verify.py`) — same repo, both engines, symbol sets diffed:

| Repo | presence recall | kind agreement |
|---|---:|---:|
| flask | **100.0%** | 100.0% |
| excalidraw | 85.1% | 84.4% |
| django | **100.0%** | 100.0% |

excalidraw's gap is 803 function-local variables, which we skip deliberately — we record named values at module and class scope, where they are API surface, not locals inside function bodies. The real extraction gap is ~1%.

This is agreement, not truth. But it caught a real bug immediately: the first run showed 64.6% recall because Python has no distinct node kind for a method, so all 25,604 of django's were labelled `function`. A speed number published before this ran would have meant nothing.

### Resolution

Share of references **whose target exists in the repository** that we linked:

| Repo | resolved | scope | import | name |
|---|---:|---:|---:|---:|
| flask | 96.1% | 8.6% | 2.3% | 43.9% |
| excalidraw | 99.6% | 10.6% | 8.8% | 30.0% |
| django | 87.7% | 8.4% | 4.5% | 10.6% |

The denominator matters. `len()` and `react.useState` have no in-repo definition, so failing to link them is correct behaviour — counting them as failures would understate the resolver, and dropping them silently would flatter it. Runtime builtins, third-party dependencies and over-ambiguous names are each reported separately.

---

## Use

```bash
cargo build --release

arbor index /path/to/repo          # build the graph
arbor status /path/to/repo         # nodes, edges, load time

arbor explore QuerySet SQLCompiler -p /path/to/repo    # path between two symbols + context
arbor callers QuerySet -p /path/to/repo                # what would break
arbor impact QuerySet -p /path/to/repo --depth 3       # blast radius
```

### As an MCP server

```bash
arbor install                      # wires Claude Code, Cursor, Codex
arbor install cursor -p ~/myrepo   # or one target
arbor install --uninstall
```

Config edits merge rather than overwrite — these files hold your other MCP servers. Two tools are exposed, deliberately:

- **`arbor_explore(symbols[])`** — the path between the named symbols plus the surrounding code, ranked by graph confidence
- **`arbor_node(symbol)`** — one symbol's source, callers and callees

Agents reliably call the first tool offered and under-pick the rest; CodeGraph *removed* two of its own tools after measuring this. There is no third tool here on purpose.

---

## How it works

```
discover ──▶ extract ──▶ resolve ──▶ persist
 ignore     tree-sitter   3 tiers      CSR
 crate      zero-copy     u32 ids      mmap
```

Four decisions carry the performance:

**Everything is a `u32`.** Resolution is name-matching; on `String` that is hashing and memcmp in the inner loop, on interned `u32` it is integer equality. Text is materialised only when a human or an agent sees it.

**Adjacency is CSR, both directions.** `callers(n)` is two array reads and a slice — no B-tree descent, no row decode, no allocation, and targets are contiguous so the prefetcher works. Both directions are stored because the question an agent actually asks is "what breaks if this is wrong", which is the reverse edge.

**Every edge carries confidence and provenance.** 100 means resolved through lexical scope, 95 through an explicit import, 45–80 matched by name. This is not decoration: ranking by confidence is how the context builder returns *fewer* tokens at equal recall, and an agent that cannot tell a proven call from a guess will weigh them equally.

**Tier 3 requires syntactic evidence.** A call, an instantiation or a superclass may be resolved by global name matching. A bare identifier read may not — matching a local variable `request` against every repo symbol called `request` is noise, and it was the single largest source of false edges when we measured it.

---

## What it does not do

Being explicit, because the gap is large:

- **Two languages.** CodeGraph has 30+, with 17 web frameworks and Swift↔ObjC / React Native bridging. Breadth is cheap at the extraction tier (~1 hour per language via declarative specs) and expensive at the import/scope tier (2–10 days). Neither has been spent yet.
- **No incremental sync.** Every index is a full index. The delta-overlay design is in [ENGINE.md](./ENGINE.md); blake3 change detection already measures at 0.9% of CPU, so the mechanism is free — it just is not built.
- **Edges are unverified.** Node-level verification runs (95% presence recall above), but we do not yet check that individual *edges* point where they should. That is the next gate.
- **Recall tops out near 51%.** Brute-force keyword search reaches 61% if you let it read 423k tokens. Closing that gap needs better retrieval signals, not a bigger budget.
- **Cost measured on one repo.** 30 bug-fix commits in django. Directionally strong, not yet a general claim.

## Documents

| | |
|---|---|
| [ENGINE.md](./ENGINE.md) | architecture, language tiers, cost design, risks |
| [BENCH.md](./BENCH.md) | full results, methodology, caveats |
| [ROADMAP.md](./ROADMAP.md) | phases and where they stand |
| [SERVER.md](./SERVER.md) | the self-hosted issue-triage service this feeds |

## License

MIT.
