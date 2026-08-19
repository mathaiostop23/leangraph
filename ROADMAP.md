# arbor — roadmap

**Goal:** something overall better — **faster and cheaper**. A native code-graph engine, shipped first as an MCP server for editors, then as a self-hosted issue-triage service.

Design details: [ENGINE.md](./ENGINE.md) · Measurements: [BENCH.md](./BENCH.md) · Server product: [SERVER.md](./SERVER.md)

---

## Why both A and B

**A** = self-hosted server: webhook → issue → graph context → agent → comment.
**B** = MCP server: arbor as the context layer for Claude Code / Cursor / OpenHands.

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
| 1b · Resolution | ✅ **Done.** 3 tiers; 87.7–99.6% of in-repo refs |
| 2 · CSR + persistence | ✅ **Done.** 7.9 MB vs 163 MB; ~20 µs load |
| 2b · Context builder | ✅ **Done.** Confidence-ranked, budget-capped |
| 3 · MCP server (**B ships**) | ✅ **Done.** 3.9 ms startup vs 561.7 ms |
| 4 · Incremental + daemon | ⬜ **next** |
| 5 · Server (**A ships**) | ⬜ |
| 6 · Breadth | ⬜ |
| — · Node verification | ✅ **Done.** 95.0% presence recall vs oracle |
| — · Edge verification | ⬜ **gating** — nodes verified, edges not |
| — · Cost benchmark | ⬜ **gating** — "cheaper" is unproven without it |

**Measured today** — django, 3,038 files / 19.7 MB, M1 Pro:

```
                        arbor        CodeGraph
index (full pipeline)   560 ms       7.87 s        14x
graph on disk           7.9 MB       163 MB        21x
graph load              ~20 us       —
MCP startup             3.9 ms       561.7 ms      144x
nodes / edges           67,924 / 293,254   62,114 / 195,802
in-repo refs resolved   87.7%
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

`arbor serve --mcp` over JSON-RPC/stdio. Two tools only (`arbor_explore` primary, `arbor_node` secondary) — CodeGraph's hardest-won finding is that agents under-pick secondary tools.

Installers for Claude Code, Cursor, Codex. Structural win: static binary + mmap'd CSR starts in **single-digit ms** where CodeGraph documents ~2–3 s MCP startup as a real problem that makes agents give up and grep.

Ship with the honest capability matrix (per language, per tier). Narrow-and-deep is a fine position; overclaiming is what would hurt.

### 4 · Incremental + daemon

Immutable base CSR + in-memory delta overlay, compacted in the background. Git-driven, not file-watcher-driven. Target **<50 ms** for a one-file change at any repo size. blake3 change detection already measured at 0.9% of CPU — effectively free.

### 5 · Server — **A ships**

See [SERVER.md](./SERVER.md). Single Rust binary + Postgres, two containers. GitHub App, webhook → queue → context → three-stage model routing → comment with cost receipt.

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
