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

### Incremental sync

| | django, 3,038 files | vs full |
|---|---:|---:|
| full index | 721 ms | — |
| sync, nothing changed | **90 ms** | 8.0× |
| sync, one file changed | **142 ms** | 5.1× |

`bench/converge.sh` asserts that an incremental sync produces a graph **semantically identical** to a full reindex across modify, revert, add and delete — every node identity, its location, and every edge with its confidence. Not byte-identical: node ids are an allocation detail, and a sync deliberately keeps the ids it had where a full index packs them from zero. Full indexes *are* byte-reproducible; the incremental path is not, and that is by design rather than by omission. It found that the graph was not reproducible at all — two full indexes of identical source differed, from concurrent symbol interning, an unstable sort on a partial key, hash-map iteration deciding import shadowing, and unsorted co-change edges. All fixed; five invariants hold.

### Startup — the number that decides whether an agent uses you at all

Time from process spawn to MCP `initialize` response (`bench/mcp_startup.py`, median of 5):

```
arbor          3.9 ms
codegraph    561.7 ms        144×
```

This gap is structural rather than tuning. Loading is not deserialization: `Graph::open` is an `mmap` plus a header check, so a 68,000-node graph is queryable in ~20 µs and there is nothing to warm up. CodeGraph's own `CLAUDE.md` names startup as the reason agents give up and reach for grep before it connects.

### Cost — the claim that actually matters

`bench/cost.py`, django, 40 bug-fix commits. Ground truth is the files each fix touched; the query is the commit message. The baseline is keyword search — tokenise, `git grep`, read the top *k* — which is what an agent without a structural index does.

| approach | recall | tokens/query | tokens per recall point |
|---|---:|---:|---:|
| arbor n=25 | 23.5% | 2,334 | **99** |
| **arbor n=100** | **40.9%** | **9,671** | 237 |
| arbor n=200 | 44.3% | 17,866 | 403 |
| keyword top-5 | 35.7% | 158,685 | 4,451 |
| keyword top-10 | 45.2% | 259,328 | 5,735 |

**At matched recall, 24× fewer tokens.** At n=100, arbor beats keyword top-5 on recall (40.9% vs 35.7%) *and* costs 16× less.

These numbers are lower than the ones this table carried before, and the reason is worth stating: the baseline used to pick its search terms through a Python `set`, whose iteration order depends on `PYTHONHASHSEED`. Every run drew a different twelve tokens, and the figure published was one sample from a distribution. The harvest also ran `git log` with rename detection against a shallow clone, where the answer depends on which blobs happen to be local — so the ground truth itself moved. Both are fixed; three consecutive runs now agree on every row.

Git co-change edges close part of the gap — files a fix touches but never names. They earned it the hard way: mixed into the normal ranking they made recall *worse*, because they arrive at the second hop with lower confidence than any AST edge and never survived the cut while still displacing better candidates. Given a reserved fifth of the budget instead, n=100 went from 45.1% to 50.0% at fewer tokens.

Keyword search still reaches 63% by brute force at 409k tokens; arbor tops out near 52%. Raising that is retrieval work, not budget work.

### Correctness

Differential verification against CodeGraph as an oracle (`bench/verify.py`) — same repo, both engines, symbol sets diffed:

| Repo | presence recall | kind agreement |
|---|---:|---:|
| flask | **100.0%** | 100.0% |
| excalidraw | 85.1% | 84.4% |
| django | **100.0%** | 100.0% |

excalidraw's gap is 803 function-local variables, which we skip deliberately — we record named values at module and class scope, where they are API surface, not locals inside function bodies. The real extraction gap is ~1%.

This is agreement, not truth. But it caught a real bug immediately: the first run showed 64.6% recall because Python has no distinct node kind for a method, so all 25,604 of django's were labelled `function`. A speed number published before this ran would have meant nothing.

### Edges — does confidence predict correctness?

Nodes being right does not make edges right, and 83.5% of django's call edges are name matches: a name found somewhere in the repo, with no scope or import behind it. arbor claims that ranking by confidence is what buys the token saving. That had never been tested.

`bench/edgetrace.py` runs flask's own test suite under `sys.monitoring` and records every call that actually happened. An observed edge exists — no argument.

| confidence | provenance | runtime-confirmed | 95% interval | edges / callers |
|---:|---|---:|---|---:|
| 100 | scope | **60.0%** | 53.3 – 66.4 | 210 / 173 |
| 95 | import | **92.5%** | 82.1 – 97.0 | 53 / 53 |
| 80 | name | 37.7% | 34.5 – 40.9 | 863 / 556 |
| 60 | name | 19.3% | 16.7 – 22.2 | 767 / 222 |
| 45 | name | 5.2% | 2.7 – 9.9 | 154 / 24 |

The trend is tested against a null that keeps the clustering: edges group by caller, so the confidence labels are shuffled *within* each caller and locality stratum, which destroys any real relationship while preserving how the edges are grouped. On `lib→lib` — the population the ranking claim is about — that null sits at `z≈+6.13` and the observed statistic is `z=+6.83`, permutation `p=0.0082`. A textbook Cochran-Armitage test on the same data reports `p=1.5e-12`; almost all of that is the grouping, not the confidence.

Two things the table says that the headline does not. `conf 95` (import-resolved) *beats* `conf 100` (scope-resolved) — 92.5% against 60.0% pooled — so the two top confidences are ordered wrongly. And `conf 100` is same-file by construction, because scope resolution is lexical and cannot cross a file, so part of its advantage is locality rather than confidence.

CodeGraph agreement, an entirely independent labeller, orders the name buckets identically — 70.5% / 33.3% / 7.6% for conf 80 / 60 / 45 — and disagrees sharply about import-resolved edges, accepting 33.0% of them where execution confirms 92.5%. Two labellers disagreeing that hard on one bucket is a result in itself, and it is why neither is quoted alone.

The first run of this said something else. On library-internal edges the *proven* tier scored 51.9% against the guess's 54.5% — confidence 100 was losing. Four defects came out of that: a dotted base class recorded its module as a superclass (47.4% of django's `extends` edges), the name index had no language partition (14,533 edges crossing one), the builtin filter was unreachable whenever a repo defined the name anywhere (2,878 Python `len()` calls landing in a minified JS bundle), and the receiver was discarded before resolution, so `super().x()` inside `x` resolved to itself. A real diff of the two graphs — `bench/edgediff.py`, not a subtraction of totals — says 26,921 edges left django and 19,116 arrived, and that an independent rule fires on 81% of what left. The other 19% is unexamined, which is a different statement from correct. Node presence recall is unchanged at 95.0% and runtime recall rose.

Neither labeller measures precision — `bench/edgefacts.py` bounds it from below without an oracle, and a rule that does not fire is not a correct edge.

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

### As a self-hosted issue agent

```bash
docker compose up -d

curl -X POST localhost:7777/repos -H 'content-type: application/json' \
  -d '{"url":"https://github.com/owner/name"}'
```

One binary, one volume, no database container. Point a GitHub webhook at
`/webhook/github`, label an issue `arbor`, and the answer is posted as a comment
with what it cost. The dashboard is at `/`.

The label is the point: nothing happens on an unlabelled issue, and nothing
happens on an issue from someone outside the repository. An issue body is
attacker-controlled text that ends up in front of a model, so it is wrapped and
declared as data, the agent is given no tools, and the webhook signature is
verified over the raw bytes. `bench/server_test.sh` asserts all of it — 12
webhook gates, 30 agent assertions.

**Fix mode** proposes a patch as a draft pull request. It is off by default in
three independent ways — the repository must opt in, the issue needs a *second*
label (`arbor-fix`), and a write token must exist — and it will not push to the
default branch, force-push, merge, or touch CI configuration, dependency
manifests or lockfiles. Those refusals are enforced before `git apply` runs, and
tested: 10 unit tests over the vetting rules, 23 end-to-end against a real git
remote.

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
- **Sync is ~164 ms, not the 50 ms the design targets.** Stable node ids are in place — the prerequisite for a delta overlay — but resolve and persist still rewrite the whole graph. Profiling corrected the design's premise along the way: the sequential barrier it was built to avoid turned out to be 4 ms, and the real cost is rewriting a 7 MB graph and a 25 MB cache for a one-line change.
- **Edge precision is bounded, not measured.** Edges are now verified two ways — against what flask's test suite actually executes, and against rules that falsify them without an oracle — and confidence demonstrably orders correctness. But a floor on the error rate is not the error rate, and recall against execution is measured on one repository in one language.
- **Recall tops out near 51%.** Brute-force keyword search reaches 61% if you let it read 423k tokens. Closing that gap needs better retrieval signals, not a bigger budget.
- **Cost measured on one repo.** 40 bug-fix commits in django. Directionally strong, not yet a general claim.
- **Fix mode does not run the tests.** The graph can tell you which tests import a changed file; running them safely needs a sandbox that is not built. Every pull request it opens says so.

## Documents

| | |
|---|---|
| [ENGINE.md](./ENGINE.md) | architecture, language tiers, cost design, risks |
| [BENCH.md](./BENCH.md) | full results, methodology, caveats |
| [ROADMAP.md](./ROADMAP.md) | phases and where they stand |
| [SERVER.md](./SERVER.md) | the self-hosted issue-triage service this feeds |

## License

MIT.
