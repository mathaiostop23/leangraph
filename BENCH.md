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

## Incremental sync

Parsing is 75–82% of our CPU, and re-parsing 3,000 unchanged files to discover
that nothing changed is the expensive part of a sync. Extracted units are
persisted beside the graph, keyed by size and mtime captured during the walk, so
only what actually changed is re-parsed.

django, 3,038 files:

| | time | vs full |
|---|---:|---:|
| full index (`--force`) | 721 ms | — |
| sync, nothing changed | **90 ms** | 8.0x |
| sync, one file changed | **142 ms** | 5.1x |

### Where the time actually goes

Profiling the phases (`ARBOR_PROFILE=1`) on a one-file sync corrected the design's
assumption about what the delta overlay would buy:

```
discover  26–54 ms
extract      13 ms
resolve      45 ms    keys+ids 9 · global index 4 · parallel 23
co-change     9 ms    (cached)
persist      60 ms    graph 28 · unit cache 29
```

**The sequential global-index barrier — the thing the delta overlay was designed
around — is 4 ms.** It was never the problem. The real costs are the parallel
resolve pass and, more than anything, persistence: rewriting a 7 MB graph and a
25 MB extraction cache for a one-line change.

So the remaining work is not "avoid the barrier" but "stop rewriting whole
files": per-file cached edges to skip the parallel pass, a chunked cache format
to rewrite only changed slices, and in-place CSR patching. Each is real work with
a bounded payoff, and none of it was needed to make sync 5x faster.

### Stable node ids

Ids used to be positional — file index plus a running offset — which is simpler
and fatal to anything incremental: adding one definition renumbers everything
after it and invalidates the entire adjacency structure. They now come from a
persistent table keyed by a content-derived `NodeKey` (file, qualified name,
kind, occurrence), so a node keeps its id across syncs and retired nodes leave
holes that the next full index reclaims.

That cost 9 ms per sync and is the prerequisite for every remaining optimisation.

### The server path — measured, and narrower than expected

`arbor index --since <sha>` diffs two commits instead of walking the tree, which
is what a push webhook can supply.

The first attempt applied it to the local case too and made discovery **worse**:
140 ms against the walk's 37 ms. `git diff --name-only <sha>` compares against
the *working tree*, so git must stat every tracked file to answer — precisely the
work we were trying to avoid. Comparing two commits is a tree read and costs
almost nothing. The flag is therefore opt-in and scoped to the server case;
everything else walks.

Two costs were found by measuring rather than assuming:

- **Discovery was doing two `stat` calls per file** — one for the size limit,
  one for the cache check. Merging them and parallelising the walk took discovery
  from 76 ms to 37 ms.
- **Co-change was 147 ms and invisible**, because its time was never added to the
  reported total. It is now cached against the HEAD it was computed at and reused
  until HEAD drifts more than 25 commits: coupling over 3,000 commits does not
  change because one more landed. 147 ms → 9 ms.

### The invariant that makes it safe

`bench/converge.sh` asserts that an incremental sync produces a **byte-identical**
graph to a full reindex, across modify, revert, add and delete. A
stale-but-plausible graph is worse than a slow one — it answers confidently and
wrongly, and nothing downstream can tell.

The first run failed, and failed in a more interesting way than expected: **two
full indexes of identical source produced different bytes.** The graph was not
reproducible at all. Four causes, all fixed:

1. The concurrent interner assigns symbol ids in thread-arrival order.
2. The CSR sort keyed only on the source node, so an unstable sort left ties in
   arbitrary order.
3. `or_insert` over a hash map let iteration order decide which import shadowed
   another on a name collision.
4. Co-change edges were appended after the resolver's sort, unordered.

And one that only appeared on delete: the cache revives symbols belonging to
files that no longer exist. The symbol table is now emitted from the symbols the
graph actually references, making it a pure function of the graph rather than of
interner history — which also took the django graph from 7.7 MB to 6.6 MB.

All five invariants now hold.

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

## Fix mode — the gates, end to end

Fix mode writes to someone's repository, so the tests are about what it
*refuses*, not what it produces. Two layers:

**Unit** (`cargo test`) — the vetting rules, which are a pure function and so
can be tested exhaustively rather than sampled:

```
accepts an ordinary source patch
rejects CI configuration        .github/workflows, .gitlab-ci, .circleci, Jenkinsfile
rejects manifests and lockfiles package.json, Cargo.toml, requirements, poetry.lock, …
rejects build and container     Dockerfile, docker-compose, Makefile, .env
rejects escapes                 /etc/passwd, ../.., src/../../outside.py
rejects git internals           .git/config, sub/.git/hooks/pre-commit
rejects the empty and the enormous   >12 files, >60 KB, no file headers
keeps a blank line of context   (see below)
                                                            10/10
```

**End to end** (`bench/fix_test.py`) — a real git repository with a real bare
remote, stubs only for the two HTTP APIs. What is asserted is the behaviour that
cannot be checked from the pure functions: 23 assertions, including that the
pull request is a **draft**, that its head is `arbor/issue-N` and never the
default branch, that the remote branch contains the fix and *nothing else*, that
the indexed checkout is untouched, that no worktree is left behind, that a
forbidden patch opens no pull request and pushes no branch, that declining to
patch still posts the analysis, and that the token never appears in a comment.

```
23/23
```

### What this caught

`clean_patch` called `trim_end()` on the model's diff. A context line in a
unified diff is a space followed by the source line, so a **blank** line of
context is a lone space — and trimming it left the hunk header promising three
lines while the body supplied two. `git apply` rejected every such patch.

The failure was invisible from the inside: the model produced a correct diff,
the vetting passed, and the only symptom was a comment saying the patch could
not be applied. It would have read as the model being unreliable. Fix mode
would have been shipped mostly broken, and the fault would have been blamed on
the wrong component.

Only the end-to-end test could find it. Every unit test of `clean_patch` was
written against what the function was *for* — stripping fences — and passed.

## Prompt caching — the minimum that is not documented in the response

The agent puts a cache breakpoint after a repo preamble that is identical
across issues; that is the whole cost argument, and `bench/agent_test.py`
asserts the prefix is byte-identical across every analyse call.

It now also asserts the prefix is **large enough to be cached at all**. The API
will not cache a block below roughly 1024 tokens, and on a small repository the
preamble came to ~950 — under the floor. Nothing in the response says so: there
is no error, no warning, and `cache_read_input_tokens` is simply always zero.
The breakpoint was decorative and the saving never happened.

The preamble now grows its file list until it clears the threshold, and the
marker is attached only when it does — claiming a saving that cannot occur is
worse than not claiming one.

```
before   3,809 chars ~=   952 tokens   breakpoint silently ignored
after    5,000+ chars > 1,024 tokens   cached
```

## Seeds — a fallback for repositories the stopword list was not written for

Seed selection drops common English words, because `using`, `when` and `raises`
all resolve in django and all are noise. On a small repository that filter can
remove *every* seed: an issue reading "add() subtracts instead of adding" found
nothing at all, and the agent was sent the preamble and no code.

The stopword list is a proxy for "too common to be a lead", and where the
repository disagrees the repository is the better authority. A stopword is now
admitted as a seed if it names something defined once or twice in this tree —
but only when nothing else survived, so a large codebase never reaches it.

A/B on django, 40 bug-fix commits:

```
                recall            tokens/query
  before        13.9 / 42.6 %     1,199 / 8,831     (n=10 / n=100)
  after         13.9 / 42.6 %     1,234 / 8,907
```

Recall identical, tokens +0.9%. It fires rarely on a large repository, which is
the intent; the gain is on the small ones, where it is the difference between
some context and none.

## Docker — the image, and the 24 seconds hiding in it

The image builds and runs: healthcheck green, unprivileged uid 10001, git
present, one volume holding the database and every clone.

```
image                289 MB   debian-slim + git + a stripped binary
clone + index        pallets/click, from the container, over the network
dashboard            200, 3.7 KB
healthcheck          healthy
```

Registering a repository through the API and waiting for `ready` is the first
thing any user does, and it took **25.8 s** for a 78-file repository. Almost all
of it was one flag.

```
git clone --filter=blob:none            2.2 s
git log --name-only  (warm_history)    23.8 s
git log --name-only --no-renames        0.04 s      600x
```

A blobless clone has every tree but no file contents. Rename detection compares
blob *contents*, so on such a clone each comparison is a lazy fetch — one
network round trip per historical blob, thousands of them, which is why the cost
is invisible in CPU time and enormous in wall clock. It is also not wanted here:
a rename genuinely touched both paths, and that is what co-change should record.

Applied to both `git log` call sites, since warming the wrong argument list
warms nothing:

```
clone + index, in the container    25,816 ms  ->  2,087 ms     12.4x
graph produced                     2,477 nodes / 8,789 edges — identical
django graph hash                  unchanged; 5/5 invariants still hold
cost benchmark                     unchanged at every budget
```

The lesson generalises: the pipeline had been profiled repeatedly and this never
appeared, because it is not compute. It only shows up when you run the thing a
user runs, from the state a user starts in.

## Deduplication — the only saving that is total rather than fractional

Every restatement of an already-answered issue is a triage call, an analysis
call and a context window spent to reach a conclusion sitting in a comment
thread. Skipping one saves 100%, not 15×.

The obvious design embeds each issue and compares vectors. It would work, and it
would mean a network round trip and a bill on *every* issue — including the
overwhelming majority that are not duplicates — in a product whose argument is
that you should not pay for context you did not need.

So: three local signals, all exact, all free. The measurements are what chose
them. Four pairs, real issue wording:

```
                          3-gram   content words
  reworded                 0.188   0.692
  pasted, comment added    0.783   0.880
  unrelated issue          0.000   0.043
  same file, other bug     0.000   0.026
```

Word shingles were the obvious first choice and are nearly useless alone: a
genuine restatement scores **0.19**, because people retype rather than paste.
Content-word overlap separates the same four pairs by a factor of sixteen. Graph
seeds — the nodes the context builder would select — are the third signal, and
the only one that knows whether two issues are about the same *code*.

The rule that came out of it: a near-copy decides on text, unless the seeds
actively contradict it; anything else needs vocabulary **and** code to agree.

```
duplicate  =  (shingles >= 0.60 AND seeds >= 0.20)
              OR (content words >= 0.50 AND seeds >= 0.50)
```

The asymmetry sets the thresholds. A missed duplicate costs one analysis. A
false one answers a real report with a link to an unrelated issue, and the
reporter concludes the bot does not work. The comment always names the issue it
matched and invites a correction.

### What the tests had to establish

Both directions of the failure, which is why seeds and text are both required:

```
a different bug in the same file        seeds identical, refused on text
a template filled in twice              text identical, refused on seeds
a restatement in the reporter's words   shingles 0.19, matched on vocabulary
a paste with a comment appended         matched on shingles alone
a four-word issue                       not judged at all
                                                                21/21 unit
```

End to end (`bench/dedup_test.py`), where the point is that the saving is real:

```
first issue                  reaches the model      2 calls
reworded restatement         comment posted         0 additional calls
genuinely different issue    analysed normally      2 more calls
                                                                10/10
```

It also caught a fixture: `bench/fix_test.py` had been sending three issues with
identical text, and dedup correctly refused to analyse the second and third.
The feature working broke a test that only passed because the test was wrong.

### The upgrade path, which nobody exercises

`config_json` had reached the schema DDL without a migration step. A fresh
database had the column; one created before fix mode did not, and every read of
a repository row would have failed on upgrade. Development always starts from an
empty database, so nothing had ever run the other path.

There is now a migration, an `add_column` helper that is a no-op when DDL
already supplied the column, and a test that builds a schema-2 database by hand
and asserts it opens, reads and re-opens.

## Egress — where a repository URL is allowed to point

A repository URL is the only user-supplied value in this product that causes an
outbound connection to a host of the caller's choosing. Requiring `https://` is
satisfied by:

```
https://169.254.169.254/latest/meta-data/iam/security-credentials/
```

— the cloud metadata endpoint, which answers with the container's credentials to
anything that can reach it. Every private address on the host's network is
equally reachable. That is server-side request forgery, and until now the only
check was on the scheme.

`check_url` resolves the host and refuses every answer that is not public:
loopback, private, link-local, unique-local, CGNAT, multicast, unspecified, and
the IPv4-mapped IPv6 forms of all of them. *Every* address, not the first — a
name that returns one public and one private answer is exactly how this gets
bypassed.

```
https://169.254.169.254/latest/meta-data/     refused
https://192.168.1.1/repo.git                  refused
https://localhost/x/y                         refused  (resolves to ::1)
https://metadata.google.internal/...          refused
https://github.com/pallets/click              queued
```

`ARBOR_ALLOWED_HOSTS` pins cloning to named hosts, and is also how an internal
one is permitted deliberately — a GitHub Enterprise install:

```
ARBOR_ALLOWED_HOSTS=ghe.internal.test,github.com

https://codeload.github.com/o/n     queued    (subdomains are covered)
https://evil-github.com/o/n         refused   (a suffix match would allow this)
https://gitlab.com/o/n              refused
https://169.254.169.254/x/y         refused
```

**What it is not.** The check resolves now; git resolves again when it connects,
so a name whose answer changes in between slips past. Closing that needs a
resolver the connection is pinned to, which git does not offer. The allowlist is
the airtight in-process control; a network policy on the container is the real
one, and the compose file says where to put it.

```
27/27 unit — scheme rules, address classification, host parsing, allowlist matching
```

## Edge correctness — does confidence predict it?

Nodes were verified against an oracle at 95% presence recall. Edges were counted
and never checked, which mattered more than it sounds: **69% of django's semantic
edges and 83.5% of its call edges were name matches** — a name found somewhere in
the repository, with no scope and no import behind it. If those were mostly
wrong, the graph was mostly noise and every claim resting on it was worthless.

Worse, the confidence score itself was untested. arbor puts a confidence and a
provenance on every edge and argues that ranking by them is *why* it returns
fewer tokens at equal recall. Nobody had ever asked whether a confidence-100
edge is right more often than a confidence-45 one.

### Three labellers, in order of how much they can be argued with

**`bench/edgefacts.py` — falsifiable without an oracle.** Rules that state a
property the edge violates. No dependencies, all three corpora, under a second.
The self-call rule reads the caller's source before firing, because recursion is
real and a rule that merely counted self-loops would overclaim; it fires only
where the body never names itself.

**`bench/edgetrace.py` — what the code actually does.** flask's own test suite
under `sys.monitoring`, recording every call that happened. An observed edge
exists; no amount of agreement between two static tools changes that. 2,712
distinct in-repo edges over 485 passing tests, deterministic across runs
(symmetric difference 0), 1.6× wall clock. This gives recall, and a confirmation
rate over edges whose caller ran. It says nothing about precision.

**CodeGraph agreement** — covers everything, proves nothing. Where we differ,
either side may be right.

### The answer

Both independent labellers, same direction, overwhelming:

```
flask, runtime-confirmed, callers that executed     all populations
  conf 100  scope     60.0%   (53.3 – 66.4)
  conf  95  import    94.3%   (81.4 – 98.4)
  conf  80  name      36.8%   (33.6 – 40.1)
  conf  60  name      20.3%   (17.6 – 23.2)
  conf  45  name       5.2%   ( 2.7 –  9.9)
  Cochran-Armitage   z=+15.1   p=8.6e-52
```

Confidence orders correctness. That is the claim the ranking rests on, and it is
now measured rather than asserted.

### What it found first, though, was that we were wrong

Before the fixes below, the same measurement said something uncomfortable. On
`lib→lib` — the population the ranking claim is actually about, rather than test
code calling library code — the *proven* tier lost to the guess:

```
                      before      after
  conf 100 scope      51.9%      65.2%
  conf  80 name       54.5%      46.6%
  trend             z=+4.85    z=+7.07
```

And the falsification floor at confidence 100 was the highest of any bucket on
two of three corpora. 12.9% of flask's scope-resolved call edges were provably
false, and every sampled one was the same shape: `super().x()` inside `x`,
resolved to itself.

### Four defects, four different kinds of mistake

| | before | after |
|---|---|---|
| `extends` into a non-class (django) | 8,655 | 146 |
| edges crossing a language boundary | 14,533 | 0 |
| calls into a vendored bundle | 13,718 | 918 |
| self-calls with no recursion in the source | 1,285 | 425 |
| **falsification floor, django** | **12.41%** | **0.83%** |
| **semantic edges, django** | **206,100** | **179,987** |

26,113 edges left django's graph and **24,076 of them — 92% — were ones a rule
could prove wrong**. The remaining 2,037 are unverified: they may have been
right. Structural edges are untouched, and excalidraw *gained* 184, because
resolving `super()` through the classes a class declares finds edges that were
previously dropped as too ambiguous.

On flask, the one corpus where recall is measurable, the cost of all this was 7
confirmed edges out of 541 — while `lib→lib` confirmation at confidence 100 went
from 51.9% to 65.2%.

**Dotted heritage.** `class X(a.B)` has two identifiers in its base list and only
one is a base class. Both were tagged, so `a` — a module — was recorded as a
superclass. 47.4% of every `extends` edge django had.

**No language partition.** One global name index for the whole repository, so a
name defined in both a Python and a JavaScript tree could bind across them.

**The builtin filter was unreachable.** It ran only when a name was absent from
the index entirely, so it was disabled the moment a repository defined that name
anywhere. django ships a minified bundle containing a function called `len`, so
2,878 Python `len()` calls resolved into it.

**The receiver was discarded.** `app_config.get_models()` was reduced to
`get_models` — correct — and the receiver thrown away, after which a call through
another object was indistinguishable from a bare one and the scope chain bound it
to the nearest same-named method at confidence 100. References now carry how they
reached their name.

Node presence recall is unchanged at 95.0%, convergence still holds 5/5, and a
full django index is 757 ms against 742 ms.

### Recall against execution

```
observed edges                          2,712
both endpoints are arbor nodes          2,682
                                     all      direct only
  exact match                       19.9%           24.8%
  + constructor rule                23.3%           29.1%
  + MRO closure                     23.7%           29.7%
```

The staged ladder is printed rather than summarised: each rule makes matching
easier, and a reader who sees only the last number cannot tell how much is arbor
being right and how much is the comparison being generous.

A quarter of the direct misses come out of five call sites. `Flask.dispatch_request`
alone accounts for 258 of them, and it does `self.view_functions[rule.endpoint](...)`
— a dictionary lookup, then a call. No static analysis follows that.

### What this does not measure

Neither labeller measures **precision**. `edgefacts` bounds the error rate from
below; runtime confirms edges that ran and is silent on the rest. A rule that
does not fire is not a correct edge.

Recall is measured on **one repository, in one language**. django's test suite
needs dependencies this machine does not have offline; excalidraw has no Python
at all.

Runtime only reaches code the tests exercise, and well-tested code may differ
systematically from the rest.

## Reproducibility of the benchmarks themselves

Two defects found while re-running everything after the fixes, both of which had
been silently moving published numbers.

**The keyword baseline was a random draw.** `bench/cost.py` picked its search
terms with `list(set(tokenize(text)))[:12]`. Set iteration order for strings
depends on `PYTHONHASHSEED`, so every run selected a different twelve tokens —
the same command on the same repository gave keyword top-3 at 24.3% and then
27.8%. arbor was being compared against one sample from a distribution.

**The ground truth moved.** The case list is harvested with `git log --name-only`
and rename detection was on. django's corpus is a shallow clone; rename detection
compares blob *contents* and fetches them lazily, so its answers depend on which
blobs happen to be local. Running the indexer in between changed the object store
and therefore changed which commits qualified.

Three consecutive runs now agree on every row.

## Methodology

- **Startup subtracted from CodeGraph.** Its 0.50 s is real and per-invocation for a CLI, but paid once for a daemon. Subtracting isolates algorithmic work, which is the fair comparison for engine design. Our own startup is not yet measured; Phase 3 will report it, and it is where a static binary with an mmap'd graph should win outright.
- **Warm page cache on both sides.** Cold-cache runs are I/O-bound and compress the difference.
- **Same file-size skip.** Both ignore files over 1 MB.
- **Not yet a general claim.** One machine, one OS, three repos, two languages.
- **CodeGraph's own progress line self-reports ~2.0 s for django**, but wall clock minus startup is 7.7 s — its counter evidently covers only part of the pipeline. We use wall clock, which is what a user experiences.

## What is still unmeasured

- **Edge precision.** Bounded from below by `edgefacts` and confirmed-where-observed by the runtime oracle; never measured directly. That needs blind hand adjudication of a stratified sample, which is human time nobody has spent yet.
- **Edge recall outside flask.** django's suite needs dependencies this machine cannot fetch offline; excalidraw has no Python. One repository, one language.
- **Cost against real issue→PR pairs.** The ground truth is bug-fix commits from git history, which is honest and reproducible but not the same distribution as issues people actually file.
- **The agent against the real API.** Every agent assertion runs against a stub. Shape, safety and caching structure are checked; answer quality is not.
- **Fix mode against a real provider.** The git half is real; GitHub is a stub, so nothing here says how often a proposed patch is *correct* — only that a wrong one cannot escalate.
- **Tests are not run before a PR is opened.** The graph can select which tests import the changed files; executing them needs a sandbox that does not exist yet.
- **Languages beyond Python and TypeScript.** Tier 0 is measured; the rest is a plan.
- **Anything other than one machine.** M1 Pro, macOS, three repositories.
