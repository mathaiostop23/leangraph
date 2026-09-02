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
| 4d · Delta cache | ✅ **Done.** A one-file sync appends 10 KB instead of rewriting 33 MB; persist 84 ms → 41 ms |
| 4e · CSR patching | ⛔ **Not worth it, measured.** 140 ms sync: discover 38, resolve 48, persist 44. Patching the CSR touches ~15 ms of that and complicates the read path the 15 µs open and 2.4 ms startup depend on |
| 5 · Server (**A ships**) | ✅ **Done.** SQLite, webhooks, agent, dashboard, Docker |
| 5b · Fix mode | ✅ **Done, opt-in.** Three independent switches; draft PRs only |
| 5c · Issue dedup | ✅ **Done.** Local signals, no embedding API; a duplicate costs zero |
| 5d · Egress policy | ✅ **Done.** SSRF closed on clone URLs; optional host allowlist |
| 5e · Escalation | ✅ **Done, opt-in.** Self-reported confidence routes to Opus; both calls billed separately |
| 5f · Tests before a PR | ✅ **Done, opt-in.** The operator's command, in the throwaway worktree; the PR states which of four things happened |
| 5g · Backfill via Batch API | ✅ **Done.** The open backlog as one submission at half price, booked at half |
| 5h · Re-analysis | ✅ **Done, opt-in.** Gated on the graph seeds moving, not on a clock |
| 6 · Breadth | 🟡 **14 languages.** Specs verified against each grammar; 11 verified against an oracle on a real repository, 96.5% presence recall across 10 corpora |
| — · Node verification | ✅ **Done.** 96.5% presence recall vs oracle, 10 corpora |
| — · Edge verification | ✅ **Done.** Runtime oracle + falsifiers; confidence orders correctness, clustered p=0.008 |
| — · Cost benchmark | ✅ **Done on SWE-bench Verified.** 500 real issues, 81.8% file recall at 18x fewer tokens than keyword top-10 |
| — · Prose indexing | ✅ **Done.** Comments, docstrings and string literals, interned and ranked as BM25. The half of a repository written in the words users use: +4.2 points on SWE-bench, +22.6 on reports written as a user would file them |
| — · CodeGraph on retrieval | ✅ **Done, first time.** At its own token cost, 65.1% against 37.1%. It wins outright on 1.4% of instances |
| — · Reproducible corpus | ✅ **Done.** `bench/swebench_fetch.py` rebuilds dataset and checkouts from nothing; the headline number could not be reproduced before |
| — · Retrieval beyond Python | 🟠 **Measured, and it does not carry over.** 224 TypeScript instances: 42.6% against 81.8% on Python. Still 2.5x CodeGraph and ahead of it at lower cost, but the headline is a Python number. Resolution (96–97%) and prose density are both ruled out as causes |

**Measured** — django, 3,038 files / 19.7 MB, M1 Pro:

```
                        leangraph        CodeGraph
index (full pipeline)   0.83 s       7.76 s         9x
graph on disk           9.0 MB       156 MB        17x
graph load              ~15 us       —
MCP startup             2.3 ms       552 ms       239x
nodes / edges           67,970 / 270,108   62,114 / 195,802
in-repo refs resolved   88.4%

file recall, SWE-bench   81.8%        37.1%     at its cost, 65.1% vs 37.1%
```

Index and size moved *against* us when prose indexing landed — 0.75 s to 0.83 s
and 6.9 MB to 9.0 MB — because comments and docstrings are now in the graph. The
retrieval those buy is the last row.

One number from a larger repository, worth having beside these: material-ui is
27,730 files, nine times django. leangraph indexes it in 1.64 s and CodeGraph in
25.6 s — 15.6x. One repo, one machine, and not yet in the table it belongs in.

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

Fourteen languages ship, from declarative specs in `lang.rs` rather than the TOML
this once planned — the specs stayed in Rust because they resolve node-kind ids
against each grammar at startup, and a text format would have moved that check to
runtime. Eleven are verified against an oracle on a real repository.

What is *not* known is how well each one resolves, as opposed to how many of its
definitions are found. Presence recall is measured and reported per corpus;
resolution tier is reported per corpus and compared against nothing. Retrieval is
measured in Python only — the corpus for TypeScript exists and the run does not.

---

## The two claims, and where they stand

Both were written here as things to earn. Both have been measured, and the
harder one came out better than the plan asked for.

1. **Faster** — earned. 9× on django's full pipeline against the honest 5–7×
   target, and 15.6× on a 27,730-file monorepo. The 17× the partial pipeline
   once suggested was right to distrust; the real number is repo-dependent and
   moves *down* as prose indexing adds work.

2. **Cheaper** — earned, and against a better opponent than planned. The plan
   was 40 closed issues and a comparison with CodeGraph explore and embedding
   RAG. What exists is 500 real issues from SWE-bench Verified across twelve
   repositories, with CodeGraph explore measured on the same 500:

   ```
   leangraph, 100 nodes           81.8%   15,742 tok
   leangraph, at CodeGraph's cost 65.1%    5,904 tok
   codegraph explore              37.1%    6,099 tok
   keyword top-10                 50.6%  282,720 tok
   ```

   The headline the plan asked for — *"same file recall, N% fewer context
   tokens"* — turned out to understate it: better recall at an eighteenth of the
   tokens, and nearly double CodeGraph's recall at its own cost.

**One third of that plan is still undone:** embedding RAG. `bench/ragbase.py`
implements it at leangraph's own token budget and has never been run to
completion. The bar it has to clear went up in the process — BM25 over the prose
already inside the code is most of what a small embedding model would find, and
costs no model at all.

From CodeGraph's own CLAUDE.md: *"don't optimize for token cost."* That axis was
uncontested, and it is where the difference turned out to live.

---

## What we are not competing on

- **Auto-fix / issue → PR.** [OpenHands](https://github.com/OpenHands/OpenHands) has 84k stars and GitHub ships a first-party coding agent. Not our differentiation.
- **Language breadth.** CodeGraph has 30+ languages with framework awareness; we have 14, eleven of them oracle-verified. That gap is real and is not closing soon. Worth separating from quality, though: covering more languages is not retrieving better in them, and only the second was measured.
- **MCP as the headline.** Building it for distribution and dogfooding, not as the claim.

## What nobody occupies

A **fast native graph + issue triage + cost optimisation**. The two leaders ([OpenHands](https://github.com/OpenHands/OpenHands) 84k, [SWE-agent](https://github.com/SWE-agent/SWE-agent) 20k) pay full discovery cost on every issue — the agent greps the repo from scratch, every time. [PR-Agent](https://github.com/The-PR-Agent/pr-agent) (12.6k) uses vector similarity, not a structural graph. [Potpie](https://github.com/potpie-ai/potpie) (5.6k) has a real graph but on Neo4j, and positions as a broad agent platform.
