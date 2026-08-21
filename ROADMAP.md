# leangraph — roadmap

**Goal:** something overall better — **faster and cheaper**. A native code-graph engine, shipped first as an MCP server for editors, then as a self-hosted issue-triage service.

Design details: [ENGINE.md](./ENGINE.md) · Measurements: [BENCH.md](./BENCH.md) · Server product: [SERVER.md](./SERVER.md)

---

## Why both A and B

**A** = self-hosted server: webhook → issue → graph context → agent → comment.
**B** = MCP server: leangraph as the context layer for Claude Code / Cursor / OpenHands.

They share ~90% of the code. B's unique surface is one week; A's is four.

B is not a detour — it is the checkpoint that de-risks A:

- **Dogfooding.** If the context builder does not serve a local agent well, it will not serve the server agent either. Learn that in month 2, not month 5.
- **Agent-adaptation knowledge.** CodeGraph's real moat is not its code, it is months of measured findings about how agents actually behave (*"errors teach abandonment"*, *"partial coverage is worse than none"*, explore-budget scaling). That knowledge is only obtainable with real agents using the thing. B gets us those users.
- **Shared eval harness.** The with/without A/B harness B needs is the same one A needs for its cost benchmark.

One linear path. B ships ~week 4, A ships ~week 8.

---

## Where we are

| Phase | Status |
|---|---|
| 0 · Speed premise | ✅ **Done, measured.** Parse+extract is 2–6% of CodeGraph's pipeline |
| 1 · Extraction | ✅ **Done.** Interned symbols, containment scopes, imports |
| 1b · Resolution | ✅ **Done.** 3 tiers, receiver-aware; 88.4–99.4% of in-repo refs |
| 2 · CSR + persistence | ✅ **Done.** 6.8 MB vs 156 MB; ~15 µs load |
| 2b · Context builder | ✅ **Done.** Confidence-ranked, budget-capped |
| 3 · MCP server (**B ships**) | ✅ **Done.** 2.3 ms startup vs 552 ms |
| 4 · Incremental sync | ✅ **Done.** 5–8x; semantically identical to a full reindex |
| 4b · Stable node ids | ✅ **Done.** Persistent key table; ids survive edits |
| 4c · Git tree-diff sync | ✅ **Done.** `--since <sha>` for push webhooks |
| 4d · Per-file edge cache + CSR patching | ⬜ — the rest of the delta overlay |
| 5 · Server (**A ships**) | ✅ **Done.** SQLite, webhooks, agent, dashboard, Docker |
| 5b · Fix mode | ✅ **Done, opt-in.** Three independent switches; draft PRs only |
| 5c · Issue dedup | ✅ **Done.** Local signals, no embedding API; a duplicate costs zero |
| 5d · Egress policy | ✅ **Done.** SSRF closed on clone URLs; optional host allowlist |
| 6 · Breadth | 🟡 **14 languages.** Specs verified against each grammar; 11 verified against an oracle on a real repository, 96.5% presence recall across 10 corpora |
| — · Node verification | ✅ **Done.** 96.5% presence recall vs oracle, 10 corpora |
| — · Edge verification | ✅ **Done.** Runtime oracle + falsifiers; confidence orders correctness, clustered p=0.008 |
| — · Cost benchmark | ✅ **Done, and now reproducible.** 17x fewer tokens at matched recall |

**Measured today** — django, 3,038 files / 19.7 MB, M1 Pro:

```
                        leangraph        CodeGraph
index (full pipeline)   0.77 s       7.76 s        10x
graph on disk           6.8 MB       156 MB        23x
graph load              ~15 us       —
MCP startup             2.3 ms       552 ms       239x
nodes / edges           67,970 / 270,108   62,114 / 195,802
in-repo refs resolved   88.4%
```

Full table and methodology: [BENCH.md](./BENCH.md).

---

## Phases

### 1b · Resolution — the risk phase

Three tiers, every edge tagged with `conf` + `prov` (already in `core.rs`):

| Tier | Method | conf |
|---|---|---:|
| 1 | local scope chain | 100 |
| 2 | import resolution → file → exported symbol | 95 |
| 3 | global name match, ranked by import distance / same-package / arity | 40–80 |

Python and TypeScript at full depth. Gate: differential-test against CodeGraph as oracle (MIT, runnable) — node/edge recall must be reportable before any speed claim ships.

### 2 · CSR + persistence

Forward + reverse CSR, `mmap`-able binary, zero-copy load. SQLite for metadata + FTS5 only, never the hot path. Target: ~2.4 MB topology where CodeGraph's SQLite is 163 MB.

### 3 · MCP server — **B ships**

`leangraph serve --mcp` over JSON-RPC/stdio. Two tools only (`leangraph_explore` primary, `leangraph_node` secondary) — CodeGraph's hardest-won finding is that agents under-pick secondary tools.

Installers for Claude Code, Cursor, Codex. Structural win: static binary + mmap'd CSR starts in **single-digit ms** where CodeGraph documents ~2–3 s MCP startup as a real problem that makes agents give up and grep.

Ship with the honest capability matrix (per language, per tier). Narrow-and-deep is a fine position; overclaiming is what would hurt.

### 4 · Incremental + daemon

Immutable base CSR + in-memory delta overlay, compacted in the background. Git-driven, not file-watcher-driven. Target **<50 ms** for a one-file change at any repo size. blake3 change detection already measured at 0.9% of CPU — effectively free.

### 5 · Server — **A ships**

See [SERVER.md](./SERVER.md). Shipped as a single Rust binary with SQLite compiled in — one container rather than the two this line planned for, and no Postgres. Webhook → queue → context → model routing → comment with cost receipt.

**Guardrails ship with the server, not after it** — prompt injection defence, author/label gating, egress allowlist, encrypted keys. They belong to this phase and cannot be deferred past it.

### 6 · Breadth

Declarative TOML language specs → ~20 languages at tier 2.5 in 2–3 weeks. Tier 3 for Go, Rust, Java, C#.

---

## The two claims we have to earn

Both need measurement, neither can be asserted:

1. **Faster** — `bench/run.sh` exists and runs. Honest target: 5–7× on a full pipeline, not the 17× the partial pipeline suggests.
2. **Cheaper** — `bench/cost.sh` does not exist yet. 40 closed issues with linked merged PRs as ground truth; measure recall@k vs tokens returned against CodeGraph explore and plain embedding RAG. Headline: *"same file recall, N% fewer context tokens."* Nobody publishes that number.

The cost claim is the harder one and the more valuable one. From CodeGraph's own CLAUDE.md: *"don't optimize for token cost."* That axis is uncontested.

---

## What we are not competing on

- **Auto-fix / issue → PR.** [OpenHands](https://github.com/OpenHands/OpenHands) has 84k stars and GitHub ships a first-party coding agent. Not our differentiation.
- **Language breadth, today.** CodeGraph has 30+ languages and 17 frameworks. We will have 2 for months.
- **MCP as the headline.** Building it for distribution and dogfooding, not as the claim.

## What nobody occupies

A **fast native graph + issue triage + cost optimisation**. The two leaders ([OpenHands](https://github.com/OpenHands/OpenHands) 84k, [SWE-agent](https://github.com/SWE-agent/SWE-agent) 20k) pay full discovery cost on every issue — the agent greps the repo from scratch, every time. [PR-Agent](https://github.com/The-PR-Agent/pr-agent) (12.6k) uses vector similarity, not a structural graph. [Potpie](https://github.com/potpie-ai/potpie) (5.6k) has a real graph but on Neo4j, and positions as a broad agent platform.
