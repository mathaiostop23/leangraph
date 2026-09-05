# leangraph

**Give a coding agent the relevant code instead of making it grep for it.**

leangraph reads a repository once and builds a graph of what calls what — every
function, class, import and reference, with a confidence score on each link.
That graph then answers the question an agent actually has: *given this bug
report, which forty lines should I read?*

Without it, an agent greps. Grep returns every file containing the word, the
agent reads them all, and you pay for the ones that were irrelevant. leangraph
indexes both halves of a repository — the call graph, and the comments,
docstrings and messages that are the only part of it written in the words its
users actually use — and returns the few dozen places most likely to matter.
Across 500 real issues in SWE-bench Verified that is the difference between
**15,742 tokens and 282,720** — for better recall of the files that actually
had to change.

```bash
cargo install --path .               # or: cargo build --release, then ./target/release/leangraph
leangraph index /path/to/repo        # django, 3,038 files → 0.84 s
leangraph install                    # wire it into Claude Code, Cursor, Codex
```

Needs a Rust toolchain (1.82+) and nothing else — no runtime, no database, no
services. Tagged releases also carry prebuilt binaries for macOS (Apple silicon
and Intel) and Linux x86-64.

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
a GitHub webhook at `/webhook/github` or a GitLab one at `/webhook/gitlab`,
label an issue `leangraph`, and the answer arrives as a comment with what it
cost.

Everything except the webhook and the health probe needs an admin token, printed
at startup and generated on first run. That surface can register a repository
against your API budget, write a secret, and enable the mode that opens pull
requests, so it does not default to open — send `Authorization: Bearer …`, or
open the dashboard at `/?token=…`. `--no-auth` turns the gate off for anyone who
already has one in front.

Two worker pools, because indexing and answering have nothing in common: one
saturates every core, the other waits on a socket. An issue that arrives before
its repository has finished indexing waits for the graph rather than being
answered without it — and waiting is not failing, so it does not spend the
issue's retries.

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
— refusals enforced in code before `git apply` runs. Give it a `test_command`
and the repository's own suite judges the patch first; the pull request says
whether that passed, failed, timed out, or never ran.

**The backlog** a repository already had is answered on request, as one **Batch
API** submission at half price — asynchronous, which is useless for a webhook
and right for work nobody is waiting on. The same machinery runs the other way
on a schedule: an answered issue is looked at again only when the code beneath
it has moved, because a bot that posts the same conclusion every night is a bot
people mute.

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

Those rows predate prose indexing, which grew django's graph from 6.9 MB to
9.0 MB and its index from 0.75 s to 0.84 s — roughly 30% and 12%, for the
retrieval that buys, and takes the speed factor from 10× to about 9×. They are
left as measured rather than half-updated:
re-running them fairly needs the same checkouts and the same CodeGraph install,
and excalidraw is no longer on this machine.

Two size columns because two things are true. The **graph** is what a query
loads: 9.0 MB against CodeGraph's 156 MB database, which is the comparison that
matters for startup and memory. But `.leangraph/` also holds a 36 MB extraction
cache, and quoting only the first number would be the kind of selective framing
this project exists to avoid. The cache is what makes re-running after a
one-file change **0.20 s** instead of 0.84.

It is written as a base plus a delta, because rewriting all 33 MB to record that
one file moved was the single largest cost in a sync — more than resolution and
the graph put together. A one-file change now appends **10 KB**.

**Startup: 3.0 ms** against CodeGraph's 580 ms, spawn to MCP `initialize`
(`bench/mcp_startup.py`, median of three runs) — about 190×. It was 2.4 ms
before prose indexing grew the graph by a third; the cost of that is on this
line, not hidden. The gap is structural rather than tuning: loading is not
deserialization. `Graph::open` is an `mmap` plus a header check, so a
68,000-node graph is queryable in 15 µs once its pages are resident, and
under a millisecond on the very first touch. CodeGraph's own
notes name startup as the reason agents give up and reach for grep first.

### Cost — the claim that matters

**SWE-bench Verified**, 500 real issues across twelve repositories, each pinned
to the commit its issue was filed against. It holds outside Python: the same
harness returns **74.2%** over 239 Rust issues across ten repositories — 83.8%
once patch sizes are held constant, since Rust's patches are much larger. It
does *not* hold on TypeScript, 42.6%, where the cause is diagnosed and
uncomfortable. See [BENCH.md](./BENCH.md). The query is the issue as filed; the
answer is the files the accepted patch touched, test files excluded. Fetch the
corpus with `bench/swebench_fetch.py`, then reproduce with `bench/swebench.py
--repos <dir>`.

| | file recall | tokens / query |
|---|---:|---:|
| **leangraph, 100 nodes** | **81.8%** | **15,742** |
| keyword, top 10 | 50.6% | 282,720 |
| keyword, what fits in our budget | 12.4% | 41,290 |

**Better recall than reading ten whole files, for 1/18th the tokens.** At least
one file that had to change is in the context 85.6% of the time (95% CI
82–88). Bootstrapped over *repositories* rather than instances — django is 231
of the 500 and its idioms are its own — recall is 81.8%, CI [77.0, 87.0]. Wide,
and it should be: twelve repositories is a small sample of repositories however
many instances they carry.

The third row needs its caveat stated rather than left to be discovered: at our
token budget keyword can afford **1.07 files**, because a single Python file
usually exceeds the whole budget already. It is generous on tokens — 41,290
against our 15,742, since the first file is taken whether it fits or not — and
narrow on files. Read it as "its top-ranked file is the right one 15% of the
time", not as a rich comparison.

Where it loses: keyword top-10 beats us outright on **5.2%** of instances, and
we return nothing useful at all on **14.4%**.

<details>
<summary>The benchmark this replaced, and why</summary>

`bench/cost.py` used a bug-fix commit's message as the query and the files it
touched as the answer — local, reproducible, and leaky. A commit message is
written *after* the fix by someone who knows it, and frequently names the
function that changed. It reported 40.0% for us against 45.2% for keyword
top-10.

Removing the leak moved the numbers **in our favour**, which is worth saying
because it is the opposite of what you would assume. The commit message was
handing the keyword baseline the exact identifier it needed; real issue text
does not. It still runs, as a second and weaker signal.

</details>

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

Runtime-confirmed is not precision: an edge that did not run is unconfirmed,
not wrong. But there is a subset where the question is settled rather than
estimated — where the tracer recorded which target a call **actually reached**,
every other candidate emitted for that call is provably wrong. Over the 560 call
sites flask's suite settles that way (`bench/edgeprecision.py`):

| confidence | edges | precision | 95% interval |
|---:|---:|---:|---|
| 100 · lexical scope | 116 | **94.8%** | 89.2 – 97.6 |
| 95 · explicit import | 28 | **100.0%** | 87.9 – 100 |
| 80 · unique name match | 340 | **91.2%** | 87.7 – 93.7 |
| 60 · 2–4 candidates | 197 | **42.1%** | 35.5 – 49.1 |
| **all** | **688** | **78.1%** | 74.8 – 81.0 |

The measure is deliberately pessimistic: a second call site for the same name
that never ran has its target counted as an error. Being wrong in that direction
is the point.

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
78  engine unit          resolution tiers, receivers, budgets, id stability, round-trips
48  server unit          vetting rules, dedup scoring, SSRF, leases, sandboxing
 5  convergence          incremental sync equals a full reindex
21  webhook gates        signature, replay, authorship, labels
38  agent assertions     prompt safety, cache correctness
23  fix mode, end-to-end against a real git remote
10  deduplication, end-to-end
17  resilience, end-to-end: rate limits, restarts, waiting, escalation
14  gitlab, end-to-end: its own gates, and the one GitHub does not need
12  backfill and re-analysis, end-to-end through the Batch API
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

- **Fourteen languages, eleven measured against an oracle.** Each on a real
  repository rather than a fixture — 96.5% presence recall across ten corpora.
  TSX/JSX and the two remaining variants ride on their parent grammar's spec and
  have no corpus of their own. CodeGraph has 30+ with framework awareness.
- **Import resolution is uneven across languages.** Rust, Java, C#, Kotlin and
  Scala resolve module paths; the rest fall back to name matching more often
  than Python and TypeScript do.
- **Edge precision is measured on one repository.** 78.1% overall and 94.8% at
  confidence 100, over the 560 call sites flask's test suite settles. Elsewhere
  it is still a floor from the falsification rules and a ceiling from fan-out,
  because settling it needs a suite that runs offline.
- **We return nothing useful on a seventh of issues.** 14.4% of SWE-bench
  Verified, and keyword top-10 beats us outright on 5.2%. Raising that needs
  better retrieval signals, not a bigger budget.
- **TypeScript is the weak language, and the reason is understood.** 42.6%
  against 81.8% on Python and 74.2% on Rust. A third of that gap is the
  benchmark asking a harder question — its patches reach 163 files — and the
  rest is a repository that ships its own documentation as code, where a demo
  matches a bug report better than the implementation does. Three fixes for it
  were built and all three declined; the reasoning is in
  [BENCH.md](./BENCH.md).
- **Localization is not an answer, and this is the big one.** 81.8% of the
  files that had to change is where the *context* is right. Whether a patch
  built on it passes the repository's own tests is a different question, and
  nobody has answered it. `bench/swefix.py` runs those tests in the benchmark's
  own per-instance containers and is validated three ways — the gold patch
  resolves, an empty patch does not, a meaningless one does not — but it has
  never been run with a model behind it.
- **A one-file change re-resolves the whole repository.** 167 ms on django,
  555 ms on a 27,730-file monorepo — and 56% of the larger one is `resolve`,
  not the graph rewrite the heading of that section in
  [BENCH.md](./BENCH.md) implies. Rewriting the graph is 18%, and patching the
  CSR would address part of that while complicating the read path the 15 µs
  open depends on; measured and declined. The order worth doing is a
  filesystem watcher first (`--since` already exists and the server uses it),
  incremental resolution second, CSR patching last.
- **No local watcher.** A push webhook syncs the server automatically. On a
  developer's machine nothing watches the filesystem — you re-run `index`, and
  the extraction cache makes that cheap rather than instant.
- **Fix mode runs the tests, but supplies no sandbox.** A repository sets
  `test_command` and the patch is checked against its own suite. The process
  gets a scrubbed environment, its own process group and a timeout — not
  isolation. A `sandbox` wrapper is where the operator puts that, and leaving it
  empty means the command runs as the server does.

---

## Documents

| | |
|---|---|
| [BENCH.md](./BENCH.md) | every measurement, its methodology, and its caveats |
| [ENGINE.md](./ENGINE.md) | architecture, language tiers, cost design, risks |
| [SERVER.md](./SERVER.md) | the self-hosted issue service |
| [ROADMAP.md](./ROADMAP.md) | phases and where they stand |
| [SECURITY.md](./SECURITY.md) | what is in scope, what is not, and where to report it privately |

MIT.
