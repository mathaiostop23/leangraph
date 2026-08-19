# arbor — engine design

Native code-graph indexer. Target: **match CodeGraph's graph quality on a narrow language set, at 5–20× the indexing speed and sub-100ms incremental.**

Working name `arbor`. Binary `arbor`. Changeable.

---

## 1. Where the headroom actually is

CodeGraph indexes 27,000 files (Swift + C++) in ~100s on a workstation. Decompose that:

| Stage | Estimated cost for 27k files / ~135MB | Notes |
|---|---|---|
| Directory walk | ~1s | |
| File read | ~2s | |
| tree-sitter parse | **3–13s single-core** | C++/Swift grammars are heavy (~5–15 MB/s) |
| AST walk + extraction | ~1–2× parse | |
| **Resolution** | **unknown — largest suspect** | Their `src/resolution/` is **TypeScript** |
| Persistence | ~5s | `node:sqlite`, JS→C++ per statement |

**Parsing is not the bottleneck.** Even at a pessimistic 5 MB/s, pure parse is ~27s of single-core work — trivially parallelizable to ~5s across 6 performance cores. The other ~95s is extraction, resolution, and I/O.

Two structural costs in their design we do not have to pay:

1. **One FFI boundary crossing per file.** Their own README states this. 27k files = 27k TS↔Rust transitions, each serializing a result payload.
2. **Resolution runs in JavaScript.** Reference→definition linking is the most allocation- and comparison-heavy phase in the whole pipeline, and it is the one phase they did not move to Rust.

Our bet: **a single-process, zero-FFI Rust pipeline where resolution is native and string comparison is replaced by `u32` comparison.**

### Modeled target on this machine (M1 Pro, 6P+2E cores)

| Stage | Estimate |
|---|---|
| Walk (`ignore` crate) | 0.2s |
| mmap + blake3 hash 135MB | 0.1s |
| Parse, 6 threads | 1.5s |
| Extract | 2.0s |
| Resolve | 1.5–3.0s |
| CSR build (par_sort ~6M edges) | 0.5s |
| Write mmap + SQLite meta | 0.5s |
| **Total** | **~6–8s** |

**This is a model, not a measurement.** Phase 0 exists to falsify it. If we land at 20s instead of 7s we are still 5× ahead and the project is worth building; if we land at 80s the premise is wrong.

---

## 2. Pipeline

```
┌── 1. DISCOVER ─────────────────────────────────────────┐
│  ignore::WalkBuilder (ripgrep's walker)                │
│  parallel, gitignore-aware, .arborignore overlay       │
│  → Vec<FileEntry { path, len, mtime }>                 │
└────────────────────────────────────────────────────────┘
                          ↓
┌── 2. EXTRACT  (rayon par_iter — pure, no shared state) ┐
│  mmap file → &[u8] → tree-sitter parse (no String)     │
│  walk AST once, emit into thread-local arenas:         │
│    defs    : Vec<Def>     { name: SymId, kind, span }  │
│    refs    : Vec<Ref>     { name: SymId, scope, span } │
│    imports : Vec<Import>                               │
│    scopes  : Vec<Scope>   { parent, range }            │
│  every identifier interned to u32 on the way in        │
└────────────────────────────────────────────────────────┘
                          ↓
┌── 3. RESOLVE  (build once, then sharded parallel read) ┐
│  global index (immutable after build):                 │
│    by_name   : FxHashMap<SymId, SmallVec<[DefId;4]>>   │
│    by_module : FxHashMap<PathId, FileId>               │
│  par_iter over all refs, 3-tier resolution:            │
│    1. local scope chain      → confidence 1.00         │
│    2. file imports           → confidence 0.95         │
│    3. global name + ranking  → confidence 0.40–0.80    │
│  → Vec<Edge { src: NodeId, dst: NodeId, kind, conf }>  │
└────────────────────────────────────────────────────────┘
                          ↓
┌── 4. PERSIST ──────────────────────────────────────────┐
│  par_sort edges by src  → forward CSR                  │
│  par_sort edges by dst  → reverse CSR                  │
│  write as mmap-able binary (zero-copy load)            │
│  nodes/spans/names → SQLite + FTS5 (cold path only)    │
└────────────────────────────────────────────────────────┘
```

Phases 1–2 are embarrassingly parallel. Phase 3 has one sequential barrier (building the global index) then goes parallel again. Phase 4 is two parallel sorts.

---

## 3. The four decisions that produce the speed

### 3.1 Interning: `u32` instead of `String`

A 27k-file repo contains millions of identifier occurrences over maybe 200k distinct names. Resolution is fundamentally "match this name to that name" — done on `String`, it is hashing and memcmp in the inner loop; done on `u32`, it is integer equality.

```rust
// lasso::ThreadedRodeo — lock-free concurrent interner
type SymId = lasso::Spur;              // u32 newtype
let interner = ThreadedRodeo::new();
let id: SymId = interner.get_or_intern(name_bytes);
```

Strings are materialized **only** at query time, when a human or an LLM sees them. Nothing in the hot path touches a `String`.

### 3.2 CSR adjacency instead of a SQL edge table

This is the biggest single win, and it is on the **query** side as much as the build side.

```rust
pub struct Csr {
    offsets: Vec<u32>,   // len = n_nodes + 1
    targets: Vec<u32>,   // len = n_edges
    kinds:   Vec<u8>,    // parallel to targets
}

impl Csr {
    #[inline]
    pub fn neighbors(&self, n: NodeId) -> &[u32] {
        let (a, b) = (self.offsets[n as usize], self.offsets[n as usize + 1]);
        &self.targets[a as usize..b as usize]
    }
}
```

`neighbors()` is two array reads and a slice — no B-tree descent, no row decode, no allocation, and the targets are contiguous in memory so the prefetcher works.

| Operation | SQLite `WHERE dst = ?` + index | CSR slice |
|---|---|---|
| `who_calls(x)` | ~5–50 µs | ~50 ns |
| `impact_radius(x, 3)` — ~10k nodes | ~50–200 ms | ~1–3 ms |

Multi-hop traversal is where the gap compounds. We keep **both directions**: forward CSR for callees, reverse CSR for callers. Reverse is the one the issue agent actually needs — *"what breaks if this function is wrong."*

The file layout is designed to be `mmap`'d and used **without deserialization**:

```
arbor.graph
  [header: magic, version, n_nodes, n_edges, section offsets]
  [fwd.offsets : u32 × (n+1)]
  [fwd.targets : u32 × m]
  [fwd.kinds   : u8  × m]
  [rev.offsets : u32 × (n+1)]
  [rev.targets : u32 × m]
  [rev.kinds   : u8  × m]
```

Load = `mmap` + pointer casts + alignment check. A 6M-edge graph loads in **microseconds**, not seconds. This is what makes a warm daemon answer in nanoseconds and eliminates the ~2–3s MCP startup latency CodeGraph documents as a real problem in its own repo.

### 3.3 Incremental: immutable base + delta overlay (LSM-style)

CodeGraph's incremental is already ~0.3–0.4s. Beating it needs a structurally different approach: **never rewrite the base**.

```
base.graph   immutable, mmap'd CSR
delta        in-memory: FxHashMap<NodeId, Vec<Edge>> + tombstone set
```

- Query: check `delta` → fall through to `base`
- Sync: reparse changed files → write their edges to `delta`, tombstone their old ones
- Compact: when `delta` exceeds ~5% of base edges, merge and rewrite `base` in the background

Sync cost becomes proportional to **the diff**, not the repo, with no full CSR rebuild. Target: **<50ms for a one-file change on any repo size.**

Change detection: `blake3` (~10 GB/s on Apple silicon) over mmap'd bytes, gated by mtime+size as a cheap pre-filter. Hashing is never the bottleneck.

### 3.4 Zero-copy parse

tree-sitter accepts `&[u8]`. We `mmap` the file and hand the slice straight in — no `read_to_string`, no UTF-8 validation, no per-file allocation. Node spans are stored as `(file_id, start_byte, len)`; the text is recovered by slicing the mmap at query time.

```rust
let mmap = unsafe { Mmap::map(&File::open(path)?)? };
let tree = parser.parse(&mmap[..], None).ok_or(ParseError)?;
```

---

## 4. Resolution — the part that decides whether we are "as good"

Speed is winnable with engineering. **Graph quality is winnable only with careful per-language work**, and it is where a fast-but-wrong indexer dies.

Three tiers, every edge carrying its provenance:

```rust
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub conf: u8,       // 0–100
    pub prov: Provenance,   // Scope | Import | NameMatch | Framework | Heuristic
}
```

| Tier | Method | Confidence |
|---|---|---|
| 1 | Local scope chain — walk lexical scopes in-file | 100 |
| 2 | Import resolution — module path → file → exported symbol | 95 |
| 3 | Global name match, ranked by import distance, same-package, arity | 40–80 |

**Never emit an edge without provenance.** An LLM consuming the graph must be able to tell a proven call from a guess — and a low-confidence edge is still far better than nothing, as long as it is labelled.

### Language plan — breadth is cheap, depth is not

An earlier draft of this document claimed breadth was unattainable. That was wrong. Decomposed by tier, the cost per language is:

| Tier | What | Cost / language | Blocker? |
|---|---|---|---|
| 0 · Grammar | tree-sitter crate already exists | ~0 | no |
| 1 · Node extraction | map node-kind names → def / call / import | **~1 hour** | no |
| 2 · Global name matching | free once tiers 0–1 exist | 0 | no |
| 3 · Import resolution | module path → file, per-language semantics | **2–10 days** | **yes** |
| 4 · Scope resolution | lexical scoping rules | 3–10 days | yes |
| 5 · Framework routing | per framework | 1–5 days each | unbounded |

**30 languages at tier 0–2 ≈ 2–3 weeks. 30 languages at tier 3–4 ≈ 6–12 months.** There is no technical barrier to breadth — only to depth.

Context for the bar we have to clear: CodeGraph publishes 84–100% "fair coverage" across 22 languages, defined as *the share of source files with **at least one** resolved cross-file dependent*. That is an honest metric (they state the denominator explicitly) but it is measured **per file, not per edge** — a file with 200 call sites where 3 resolve counts as covered. Tier 2.5 (global name matching with confidence) reaches that bar in many languages.

### Declarative language specs, not Rust per language

Adding a language must be a **config PR, not a code PR** — that is what turns breadth into a contribution surface instead of an engineering cost.

```toml
# langs/ruby.toml
extensions = ["rb", "rake", "gemspec"]
grammar    = "tree_sitter_ruby"

[nodes]
definitions = ["method", "singleton_method", "class", "module"]
calls       = ["call"]
imports     = ["call"]          # require / require_relative

[fields]
def_name  = "name"
call_name = "method"
```

The spec compiles to `u16` node-kind and field IDs once per language at startup; the hot loop still compares integers.

### Rollout

```
Phase 1   Python + TypeScript at tier 4          (depth — proves the design)
Phase 2   declarative spec loader + ~20 languages at tier 2.5
Phase 3   tier 3 for Go, Rust, Java, C#          (static — imports are mechanical)
Phase 4+  frameworks on demand
```

### Publish a capability matrix, not a single number

| Language | Parse | Defs/Calls | Imports | Scopes | Framework |
|---|:-:|:-:|:-:|:-:|---|
| Python | ✅ | ✅ | ✅ | ✅ | Django, FastAPI |
| TypeScript | ✅ | ✅ | ✅ | ✅ | — |
| Go, Rust, Java, C# | ✅ | ✅ | ✅ | ⚠️ | — |
| ~20 more | ✅ | ✅ | ⚠️ name-match | ❌ | ❌ |

More informative than one percentage on a forgiving denominator, and it tells a user exactly what they get.

---

## 5. Correctness — CodeGraph as the oracle

We cannot ship a fast wrong graph. CodeGraph is MIT-licensed and runnable, which makes it a **free differential-testing oracle and benchmark baseline** on the same machine and the same repos.

```
scripts/bench.sh <repo>
  ├─ run codegraph index   → time, node/edge counts, sqlite dump
  ├─ run arbor index       → time, node/edge counts
  └─ diff
       nodes we miss that they find   ← recall gap (must → 0)
       nodes we find that they miss   ← either better or wrong (inspect)
       edge agreement on shared nodes ← precision proxy
```

Gate: **no speed claim ships without a recall number next to it.** "3× faster at 91% node recall" is honest and useful; "3× faster" alone is worthless.

Plus a golden-fixture suite: hand-built micro-repos per language with hand-verified expected graphs, covering decorators, re-exports, `__init__.py` re-exports, dynamic imports, class inheritance chains, method overriding.

---

## 6. Code layout

Modules now, crates when compile time demands it — a workspace split buys nothing until the grammar count makes rebuilds hurt (expect that around 10+ languages, then split `arbor-lang` out first).

```
src/
  core.rs      # SymId, FileId, NodeId, Def, Ref, Edge, Provenance, interner
  lang.rs      # Lang + Spec (node-kind / field IDs resolved once)
  extract.rs   # per-file: mmap → parse → cursor walk → FileUnit
  resolve.rs   # 3-tier resolution → Vec<Edge>          [Phase 1]
  graph.rs     # CSR build, mmap format, load/save      [Phase 2]
  query.rs     # traversal, context builder, ranking    [Phase 2]
  mcp.rs       # JSON-RPC over stdio                    [Phase 3]
  server/      # axum, webhooks, queue, agent           [Phase 4]
  main.rs      # CLI
```

## 6b. MCP server — ship it, but not as the product

Every relevant agent (Claude Code, Cursor, Codex, Windsurf, Zed, Continue, Kiro) speaks MCP. Integration is one config entry per target plus a writer to emit it (~50 lines each).

```jsonc
{ "mcpServers": { "arbor": { "command": "arbor", "args": ["serve", "--mcp"] } } }
```

**The one place we win by construction.** From CodeGraph's own CLAUDE.md:

> *"MCP attach is a startup-latency issue... the agent dives into Read/grep before codegraph finishes its ~2-3s startup, so it runs with no codegraph."*

Their mitigation is a pre-warmed daemon with sockets and an env var to skip a re-exec. Ours is structural: a static Rust binary that `mmap`s the CSR starts in **single-digit milliseconds**. Nothing to warm — the graph *is* the file. That is the difference between "the agent gave up and grepped" and "it was there."

**Tool surface — one primary tool.** Their hardest-won finding is that agents under-pick secondary tools; they *removed* `codegraph_context` and `codegraph_trace` for this reason. Copy the conclusion:

```
arbor_explore(symbols[])   PRIMARY   — symbol bag → verbatim source,
                                       call path between them, blast radius
arbor_node(symbol)         SECONDARY — full body + caller/callee trail
```

Two additions they do not have: every edge carries **confidence + provenance** so the agent can distinguish proven from guessed, and **co-change edges** from git history catch coupling no static analyzer sees.

**Strategic position:** build it for visibility and dogfooding — if the engine does not serve a local agent well, it will not serve the server agent either. Do *not* make it the headline claim; on MCP we are playing their home turf against 67k stars.

### Dependency picks (all deliberate)

| Crate | Why |
|---|---|
| `ignore` | ripgrep's walker — fastest parallel gitignore-aware traversal that exists |
| `rayon` | work-stealing parallelism, `par_sort_unstable` for CSR build |
| `memmap2` | zero-copy file access |
| `tree-sitter` | native bindings, no FFI penalty |
| `lasso` | lock-free concurrent string interner |
| `rustc-hash` (`FxHashMap`) | ~2× faster than SipHash for small integer keys |
| `blake3` | ~10 GB/s content hashing |
| `smallvec` | most symbols have 1–4 definitions; avoids heap churn |
| `rusqlite` (bundled) | metadata + FTS5 only, never the hot path |

---

## 7. Phase 0 — DONE, premise holds ✅

Full results and methodology: **[BENCH.md](./BENCH.md)**. Reproduce with `./bench/run.sh`.

Gate was: *proceed if parse+extract is under 20% of CodeGraph's total.* **Measured at 2–6%.**

| Repo | Files | arbor (parse+extract) | CodeGraph (full) | share | headroom |
|---|---:|---:|---:|---:|---:|
| flask | 83 | 10 ms | 0.49 s | 2.0% | 49× |
| excalidraw | 666 | 120 ms | 3.04 s | 3.9% | 25× |
| django | 3,038 | 460 ms | 7.60 s | 6.1% | 17× |

Extraction is comparable, not thinner: our call-site count on django (201,218) lands within 3% of their edge count (195,802), and file discovery within 1%.

Our own CPU is now **81.9% tree-sitter parse** — i.e. already parse-bound, which is the right place to be. blake3 hashing costs 0.9%, so hash-based change detection for incremental sync is free.

**Honest target for a complete pipeline: 5–7× on django (1.0–1.5s vs 7.6s), not 17×.** Resolution is unimplemented and is the phase most likely to surprise us.

---

## 7b. Cheaper — the uncontested axis

The goal is not just faster. It is **faster and cheaper**, and cheaper is where the ground is genuinely open. From CodeGraph's own CLAUDE.md:

> *"The optimization target is wall-clock latency + tool-call count — **don't optimize for token cost**."*

That is the correct call for their product: their user has a subscription and never sees tokens. **Our user pays per token.** The entire cost axis is uncontested, and it has to be designed into the engine, not bolted on at the product layer.

### Their approach to sufficiency: return more

Their explicit mechanism is *"make the tool's output complete enough that the agent stops"* — because an agent falls back to Read/Grep the instant an answer is insufficient, and that fallback costs far more than a big response. So they scale output by repo size:

| Repo | files | explore calls | chars/call |
|---|---|---|---|
| express | 147 | 1 | 18K |
| django | 3,043 | 2 | 28K |
| vscode | 10,446 | 3 | 35K |

That is a **size** heuristic, not a **relevance** heuristic. It is reliable, and it is expensive.

### Our approach: return better

Three engine-level levers they structurally lack:

1. **Confidence-ranked selection.** Every edge carries `conf: u8` + provenance. The context builder ranks by confidence × graph centrality × distance-from-seed and cuts, instead of filling a char budget.
2. **Co-change edges.** Git-derived coupling catches what no static analyzer sees. Each such edge prevents an agent turn spent re-discovering it — and a wasted turn costs more than the edge.
3. **Token-aware context builder.** `build_context()` reports its own token count and honours a hard ceiling. The product cannot enforce a budget the engine hides.

### The trap to avoid

"Cheaper" is **not** "return less." Under-returning pushes the agent back to Read/Grep, which costs more than the tokens saved. Their finding that *partial coverage is worse than none* is the same lesson from the coverage side.

So the target is precise: **fewer tokens at equal-or-better answer quality.** That is a claim you can only make with a measurement, never by assertion.

### The cost benchmark — Phase 0 for cost

Mirror `bench/run.sh` with `bench/cost.sh`:

1. 40 closed issues with linked merged PRs → ground truth = files changed
2. For arbor context, CodeGraph explore, and plain embedding RAG, measure **recall@k vs tokens returned**
3. Publish the curve

The headline claim is one sentence and it must be earned: **"same file recall, N% fewer context tokens."** Nobody in this space publishes that number.

---

## 8. Honest risk register

| Risk | Assessment |
|---|---|
| **Breadth gap** | Real and large. They have 30 languages and 17 frameworks. We will have 2 languages for months. Speed alone does not close it — this is the single biggest reason a user picks them over us. |
| **Resolution quality** | Their `provenance:'heuristic'` synthesizers (callback, EventEmitter, React re-render, JSX child) took real iteration. Matching Python/TS *quality* is harder than matching speed. |
| Speed model is wrong | Phase 0 answers this in 2 days for near-zero cost. |
| Incremental delta correctness | Tombstone bugs produce a silently stale graph — worse than a slow one. Needs an invariant test that a full reindex and an incremental sync converge to identical graphs. |
| Upstream ships the same optimizations | They are moving fast and have 4,254 forks of contributor attention. Assume they close speed gaps. |

**The strategic answer to the breadth risk:** do not position as "CodeGraph but faster." Position as the **server-side engine** — designed for a daemon indexing many repos concurrently, with sub-100ms incremental driven by git rather than a file watcher. That is a different product with different constraints, and it is the one the issue-agent needs.
