# Benchmarks

Differential measurement against [CodeGraph](https://github.com/colbymchenry/codegraph) v1.5.0 — same repos, same machine, same run.

Reproduce: `cargo build --release && ./bench/run.sh`

**Speed without a graph-size number next to it is meaningless** — anyone can be fast by extracting less. Every table here reports both.

---

## Cost — tokens to reach the files that actually changed

This is the claim that matters and the one nobody in this space publishes.
"Faster" is a stopwatch; "cheaper" needs ground truth.

### SWE-bench Verified — 500 real issues, 12 repositories

`bench/swebench.py`. Each instance is pinned to the commit its issue was filed
against. The query is the issue **as filed**, before anyone knew the answer; the
ground truth is the files the accepted patch touched, with test files dropped,
since pointing at the test that proves a bug is not finding the bug.

| approach | file recall | tokens/query |
|---|---:|---:|
| **leangraph, 100 nodes** | **81.8%** | **15,742** |
| keyword, top 10 | 50.6% | 282,720 |
| keyword, at our budget | 12.3% | 41,229 |

**Better recall than reading ten whole files, for an eighteenth of the tokens.**
At least one file that had to change is in the context 85.6% of the time
(95% CI 82–88). Bootstrapped over *repositories* rather than instances — django
is 231 of the 500 and its idioms are its own — recall is 81.8%, CI [77.0, 87.0].
Wide, and it should be: twelve repositories is a small sample of repositories
however many instances they carry.

| repo | n | recall | | repo | n | recall |
|---|---:|---:|---|---|---:|---:|
| django | 231 | 82.5% | | astropy | 22 | 94.3% |
| sympy | 75 | 77.5% | | xarray | 22 | 88.6% |
| sphinx | 44 | 68.9% | | pytest | 19 | 94.7% |
| matplotlib | 34 | 76.5% | | pylint | 10 | 50.8% |
| scikit-learn | 32 | 93.8% | | requests, seaborn, flask | 11 | 100% |

The third row needs its caveat stated rather than left to be found. At our token
budget keyword can afford **1.07 files**, because a single Python file usually
exceeds the whole budget already, and the first file is taken whether it fits or
not — so that row spends 34,541 tokens against our 15,742. It is generous on
tokens and narrow on files. Read it as "its top-ranked file is the right one 15%
of the time", not as a rich comparison.

Reproduce with `bench/swebench_fetch.py` (dataset from the Hub, full clones of
the twelve repositories, ~2.1 GB) and then `bench/swebench.py --repos <dir>`.

The keyword rows moved when the harness stopped being Python-only — it now ranks
`.js`, `.ts` and the rest alongside `.py`, so grep sees the JavaScript these
repositories also contain. That is the more correct baseline, grep having no
notion of a project's language, and it is a touch weaker: 51.3% at 244,672
tokens under the old filter, 50.6% at 282,720 under this one. Our own rows are
untouched by it, and the ground truth is identical on all 500 instances.

### The protocol, and why it is not the obvious one

Every instance is indexed from scratch. `--reuse-index` keeps `.leangraph`
between them, which is faster and is how the first runs were done — but then each
measurement is an incremental sync from whatever commit was measured *before* it,
and two things follow.

**A measurement that depends on its predecessor is not one measurement.** Same
binary, same code, carried state versus fresh: **20 of 500 instances move**
(14 better, 6 worse, p = 0.12). That 4% is the noise floor of this harness, and
it is the number any result here has to clear to mean anything.

**The co-change cache was reused from the future.** Its staleness gate asked
`git rev-list --count <cached>..HEAD`, which counts only what HEAD has that the
cache does not. Check out an older commit and it returns zero — no drift, reuse
the cache. Measured on the astropy checkout: a cache **16,722 commits ahead**
reported zero drift. In this benchmark that is co-change evidence derived from
history containing the fix itself, which is the same class of leak the commit
message query had. Fixed in `cochange::drift`, which now counts both directions
(`--left-right <cached>...HEAD`), with regression tests for a HEAD that moved
forward, a HEAD that moved *backwards*, and a commit the repository no longer
has.

Worth stating plainly: **the leak was not inflating the numbers.** Under the
fresh protocol recall goes *up*, 76.6% → 77.6%. The reuse was adding noise, not
signal. But the gate was wrong outside this benchmark too — `git bisect`, an
older branch, and a worktree pinned to an old tag all move HEAD backwards.

### Seeding from paths, and the hypothesis it refuted

Issues that describe behaviour often name no symbol that resolves anywhere while
addressing a file squarely by where it lives. "Use subprocess.run and PGPASSWORD
for client in postgres backend" names nothing in django and points at
`django/db/backends/postgresql/client.py`; three segments of that path are in
the sentence. `seeds_from_path` requires two distinct tokens — one is a
coincidence, every repository has a `utils` — and ranks by tokens matched, then
by the shallower path.

Measured against the same 500 instances, both runs on a fresh index:

| | recall | tokens | contexts with nothing useful |
|---|---:|---:|---:|
| before | 72.0% | 13,555 | 126 |
| after | **77.6%** | 14,863 | **96** |

Paired per instance, which is the only comparison that answers the question —
two aggregate means cannot separate a change that helps 56 and hurts 23 from one
that does nothing, and with django at 231 of 500 a mean is mostly a statement
about django:

```
recall    +5.6 points        tokens +1,308 per query (+9.6%)
moved     56 better, 23 worse, 421 unchanged
empty     47 of 126 rescued; 17 went the other way
sign test p = 0.00026 over the 79 that moved
per repo  django 28+/7-  pylint 6+/0-  sphinx 7+/6-  sympy 6+/5-  matplotlib 3+/3-
```

**The obvious version of this change is the wrong one.** "24% of contexts come
back with nothing useful" reads as "no name resolved there", so restricting path
seeds to that case ought to buy the same rescues while disturbing nothing. It
does not: the fallback branch fires on only 25 of those 121, rescues 5, and
lands at −0.5 points with p = 0.58 (measured under the earlier protocol, but a
null that size does not survive a cleaner one). In the other 96 the names
resolve perfectly well and simply point at the wrong code. Path evidence is
worth having precisely where it sits *beside* a name that did not pan out —
appended after symbol seeds, never instead of them.

It is not free. The expansion has a hundred nodes to spend and every extra root
spreads them thinner, so 17 contexts that held the right file lost it —
`django__django-11815` went from 3,904 tokens holding the answer to 10,316
tokens without it. Net of both halves the change is worth making; the losing
half is real.

**A reserved share was tried against that losing half, and declined.** Co-change
edges had the same shape — a different kind of evidence losing on one scale —
and a reserved fifth of the budget fixed it. Path seeds capped the same way come
out worse:

| | recall | tokens | tokens / recall point |
|---|---:|---:|---:|
| before path seeds | 72.0% | 13,555 | 188.2 |
| **appended, uncapped** | **77.6%** | 14,863 | 191.5 |
| capped at a reserved fifth | 76.0% | 14,159 | **186.3** |

The cap saves 6 of the 17 losses and gives up 13 of the 47 rescues — about two
rescues surrendered per loss prevented. What that says is worth more than the
decision: the **tail** of the path ranking is not padding. The file that had to
change is frequently not the best-scoring path match, so the extra roots earn
the dilution they cause, and ranking path candidates better is a more promising
line than rationing them.

One honest wrinkle, since this benchmark's older sibling treats tokens per
recall point as the durable number: by *that* measure the capped variant wins,
186.3 against 191.5. It is the cheaper engine and the worse one. The published
figure is recall at a fixed budget, both variants sit well inside it, and 5%
more tokens for 1.6 points is a trade worth making — but a reader who prefers
the efficiency ratio should know it points the other way.

### The half of a repository nobody indexed

Extraction kept definitions, references, imports and scopes, and threw away
every comment, docstring and string literal. That is the only part of a codebase
written in the language its *users* speak, and without it an issue that names no
identifier has nothing to seed from.

How much that costs was not visible on SWE-bench, whose issues are filed by
developers who paste stack traces and function names. It is visible on a
codebase audited on purpose: 31 bug reports against a real FastAPI service,
each written as a user would file it and then *mechanically checked* so that no
report contains a word which alone retrieves its own answer — the operational
form of the commit-message leak, settled by asking the tool one query per word.

| | file recall | tokens/query |
|---|---:|---:|
| leangraph, symbols only | 22.0% | 18,569 |
| keyword top-10 | 28.5% | 397,998 |

Losing to grep, at a twenty-first of its cost. Grep matches text anywhere —
docstrings, log lines, error messages; we matched only names.

Prose is now stored as interned word ids rather than text, so a word costs four
bytes per node however often it repeats across the tree and matching a query
against a repository is integer work. It is collected inside the existing cursor
walk, and deduplicated per definition: prose node kinds nest, so the same text
arrives more than once, and how often a comment repeats itself is not evidence
about the code beneath it.

Ranked as BM25 with a **binary** term frequency. Because a word is stored once
per definition the usual saturation term collapses to a constant, leaving
length-normalised IDF — and the length normalisation is what stops a 2,000-line
module with a licence header from answering every question.

Query words are found by binary search over the symbol table, which `write`
already sorts to keep the format a pure function of the graph. Nothing is built
at open time, because opening the graph being an mmap and nothing else is the
property the 15 µs load and the 3.0 ms startup rest on.

### Prose was not outranked, it was never asked

Seeds are capped at 32 and were filled in order: names, then paths, then prose.
Path seeding is greedy by construction — two nodes per matching file until the
budget is gone — so on any issue whose names resolve thinly it took every
remaining slot before the question of what the comments say was ever put.
Counted over a stratified sample, six instances from each of the twelve
repositories:

```
seeds by origin   path 1,145   name 657   prose 213
instances with no prose seed at all   49 of 63   (78%)
```

A quarter of the seed budget is now held back for prose; paths take what the
names left minus that slice, and get the remainder back when prose does not
spend it. Seeds also carry their origin — `seed`, `path`, `prose` — which is
what made the starvation countable, and which tells whoever reads the context
back which fragments came from a comment rather than from a name they typed.

**This is the opposite move to the reservation declined above, though the two
look identical.** That one capped path seeds and lost 13 rescues to save 6: it
removed seeds that were working. This one removes none — it puts a floor under
a signal that was never consulted, and hands back what the floor does not use.

Both benchmarks, one change at a time:

| | audited reports | SWE-bench |
|---|---:|---:|
| symbols and paths | 22.0% | 77.6% |
| + prose | 41.4% | 79.1% |
| + a reserved quarter | **44.6%** | **81.8%** |

Note how surgical prose is on SWE-bench — 9 instances better, 1 worse — against
path seeding's 56 and 23. Prose fills only what the names left empty, and on
issues written by developers the names usually fill it. It earns its keep
exactly where they do not.

It is not free. django's graph grows from 6.9 MB to 9.0 MB and its index from
750 ms to 830 ms, roughly 30% and 10%. Query cost is unchanged.

### Query expansion, and why there is none

The seeder matches a query word to a stored word exactly. Two local expansions
were built and measured against that, neither needing a synonym list or a model.

**Splitting identifiers into words does nothing.** `WaitApprovalStepExecutor`
holds `approval`, so indexing the parts of every definition's name alongside its
prose ought to reach code the comments never describe. Measured over the audited
reports it adds 27,000 words to 297,000 — 9% more index — and moves not one
number, at any rank. The reason is visible once stated: a docstring almost
always says what its function is called, so the name's words were already there.

**Stemming hurts, and the file-level prototype said it would help.** A blunt
suffix table so that a report saying "denying" reaches a docstring that says
"denied". Prototyped in Python over whole files it looked worth building —
recall@10 rose from 57.0% to 61.8%. Built, it cost 9.7 points:

| | audited reports | SWE-bench |
|---|---:|---:|
| exact match | **44.6%** | **81.8%** |
| stemmed, one combined suffix list | 34.9% | 81.4% |
| stemmed, plural and verb rules ordered | 34.9% | — |

The first stemmer over-cut — `refused` lost `ed` to give `refus`, which then
looked like a plural and gave `refu` — so a second was built with the two stages
ordered and `ss`/`us` protected, and it scored identically. Two stemmers of very
different aggressiveness landing on the same number is the answer: the loss is
not over-stemming, it is stemming.

What the prototype measured was a *file*, ranked against 653 others, scored at
k=10. What the seeder does is pick a handful of *nodes* out of 16,000 on a tight
budget. Merging words lowers the IDF that separates them, and recall bought at
rank 10 is worth nothing to something that only ever takes the first few. A
proxy that ranks a different unit at a different depth can point the wrong way,
and this one did.

Left undone rather than left unmeasured: expansion that reaches a word the
repository never writes down at all needs a source of synonyms, and there is no
local one.

### CodeGraph on the same 500 issues

Speed and size have been measured against CodeGraph since Phase 0. Retrieval
never had, and the objection that leaves open is the one that matters: being ten
times faster to index is worth nothing if the other engine puts better files in
front of the agent. `codegraph explore` is its counterpart to `leangraph
context` — its *primary* MCP tool, by its own design note that agents under-pick
secondary ones — and it answers the same question from the same text.

Both engines indexed fresh for every instance, `@colbymchenry/codegraph@1.5.0`.

| | file recall | tokens/query | at-least-one |
|---|---:|---:|---:|
| **leangraph, 100 nodes** | **81.8%** | 15,742 | 85.6% |
| **leangraph, at CodeGraph's cost** | **65.1%** | 5,904 | 69.2% |
| codegraph `explore` | 37.1% | 6,099 | 41.0% |
| codegraph, counting files it only *names* | 45.4% | — | — |

**At matched cost, 65.1% against 37.1%.** CodeGraph wins outright on 1.4% of
instances, and 6.4% when we are held to its budget.

Three things had to be decided before this was a fair table, and each is a place
the number could have been made to say what we wanted.

**The equal-cost row runs the wrong way round, because it has to.** `explore`
takes `--max-files`, so the obvious move is to raise it until CodeGraph spends
what we spend. It does not respond: at 3, 5, 8 and 12 files it returns the same
2.5 files and the same ~6,700 tokens. That is not a cap being lifted, it is the
whole of its answer. So the comparison brings *us* down to its budget instead —
35 nodes, which lands at 5,904 tokens against its 6,099.

**What counts as returned.** `explore` prints source for a couple of files and
*names* others in a blast-radius list. Source is the like-for-like set; a name
is a pointer, not content. Both are scored, and the generous reading is in the
table rather than in a footnote.

**Cost is the whole output.** The prose framing and the blast-radius list are
tokens the agent pays for, so they count. For the record, 3,934 of CodeGraph's
6,099 tokens are source and the remaining third is framing.

Worth saying plainly, because this benchmark exists to be believed rather than
to flatter: **CodeGraph is far better than the keyword baseline it is being
graphed against.** 37.1% at 6,099 tokens beats reading grep's top ten — 50.6% at
282,720 — on any per-token reading, and it does it while returning a fifth of
what we return. The gap here is recall at a budget, not efficiency.

And the usual limits hold. Twelve repositories, all Python, one machine.
CodeGraph covers 30+ languages with framework awareness where we cover fourteen;
nothing here measures that.

### Rust, where it does carry over

239 instances across ten repositories — clap, tokio, tracing, ripgrep, nushell,
fd, bat, bytes, rayon, serde — fetched the same way and asked the same question.

| | file recall | tokens/query | at-least-one |
|---|---:|---:|---:|
| **leangraph, 100 nodes** | **74.2%** | 12,646 | 92.5% |
| leangraph, at CodeGraph's cost | 42.7% | 4,816 | — |
| codegraph `explore` | 12.4% | 3,220 | — |
| keyword top-10 | 55.0% | 134,730 | — |

74.2% against Python's 81.8% understates it, because Rust's patches are far
larger: only 41% touch a single file against Python's 86%. Standardised to
Python's mix of patch sizes it is **83.8%** — a little *above* Python — and it
is at or above Python in every band taken separately:

| files in patch | Python | Rust |
|---|---:|---:|
| 1 | 84.4% | 85.7% |
| 2–3 | 72.7% | 73.9% |
| 4–10 | 31.3% | 63.9% |
| 11+ | 9.5% | 38.6% |

At-least-one is 92.5%, higher than Python's 85.6%. Ten repositories also make
the bootstrap worth reading for once: 74.2%, CI [71.0, 83.3], against
TypeScript's uselessly wide [38.3, 75.0] over three.

CodeGraph struggles here more than anywhere: 12.4%, against its 37.1% on Python
and 17.0% on TypeScript.

### TypeScript, the one that does not

Every figure above is a Python figure, because SWE-bench Verified is Python.
Multi-SWE-bench supplies 224 TypeScript instances across darkreader,
material-ui and vuejs/core, converted by `bench/multiswe_fetch.py` — issue text
as filed, patch files as the answer, tests excluded, and the pull request's own
title and body discarded because they are written by whoever fixed the bug.

| | file recall | tokens/query |
|---|---:|---:|
| **leangraph, 100 nodes** | **42.6%** | 9,149 |
| leangraph, 35 nodes | 25.3% | 2,852 |
| codegraph `explore` | 17.0% | 5,301 |
| keyword top-10 | 16.2% | 1,203,945 |

**42.6%, against 81.8% on Python and 74.2% on Rust.** With Rust measured, the
first reading of this — "the headline is a Python number" — is wrong. Two of
three languages agree; TypeScript is the exception, and the exception has a
cause.

We stay ahead — 2.5x CodeGraph's recall, and still ahead of it at a *lower* cost
than it spends, 25.3% at 2,852 tokens against its 17.0% at 5,301. Both engines
fall by roughly half moving from Python to TypeScript, so whatever this is, it
is not specific to us. And grep collapses outright: 16.2% for 1.2 million tokens
a query, because material-ui is a monorepo whose files are enormous.

Two explanations were tested and neither survives:

**It is not resolution.** These repositories resolve *better* than django:
97.0% of in-repo references on vue and 96.1% on material-ui, against django's
87.7%. The graph being built is a good graph.

**It is not prose density.** The seeding win came from comments and docstrings,
so the obvious guess is that TypeScript writes fewer. material-ui carries 8.2
prose words per node against django's 4.4 — more, not fewer. vue is thinner at
3.1, and vue scores *better* than material-ui (59.0% against 37.7%), which is
the wrong way round for that theory.

Nor is it size alone: vue is 488 files and scores 59.0%, django is 3,038 and
scores 82.5%.

**Two thirds of the gap is real and one third is the question.** Patches are far
larger here — 86% of SWE-bench patches touch a single file against 47% of these,
and 13% of these touch more than ten, the largest 163. Recall tracks patch size
almost identically in both languages once you hold it constant:

| files in patch | TypeScript | Python |
|---|---:|---:|
| 1 | 60.4% | 84.4% |
| 2–3 | 34.7% | 72.7% |
| 4–10 | 29.8% | 31.3% |
| 11+ | 9.1% | 9.5% |

Standardising TypeScript's own per-band rates to Python's mix of patch sizes
lifts it from 42.6% to **56.6%**. So of the 39.3-point gap, 14.0 points are the
benchmark asking a harder question and **25.3 points are us doing genuinely
worse**. Documentation in the ground truth is not the culprit: 51 of 224
instances include a `docs/` file but only 2 are documentation alone.

**Where the real gap goes: a repository that ships its own documentation as
code.** On a sample of material-ui instances, 38% of the files returned come
from `docs/`, `benchmark/` or `examples/` — trees holding 2,691 of the
repository's 26,753 source files, 10%. They are over-represented four to one.

The reason is uncomfortable, because it is the prose feature working exactly as
designed. A documentation demo *is* the component's behaviour described in the
words a user would use — that is what makes it documentation. Asked "the sx
field's outlineColor ignores the theme", the demo that renders an `sx` prop with
an outline colour matches the sentence better than the implementation does, and
the implementation is what has to change.

The obvious fix — exclude `docs/` — is wrong: 51 instances here have a
documentation file as part of the answer.

**The structural fix was built three ways and declined three times.** A demo is a
leaf and an implementation has dependents; the graph knows this and seeding
never asked. Each formulation puts that question at a coarser grain than the
last:

| | sample (n=12) | TypeScript (n=224) | Python (n=500) |
|---|---:|---:|---:|
| no penalty | 30.2% | **42.6%** | **81.8%** |
| demote nodes with no dependents | 30.2% | — | — |
| demote nodes with no *cross-file* dependents | 38.6% | 42.9% | 81.6% |
| demote *trees* nothing outside consumes | — | 40.9% | — |

The first formulation changed nothing, for a reason worth keeping: a
documentation demo *does* have dependents. `SimpleDialogDemo` calls
`SimpleDialog` from the very file that defines it — being self-contained is what
being a demo means. Asking instead for a dependent in another file separates
them properly.

And on twelve instances it looked like it worked: 30.2% to 38.6%. On 224 it is
+0.3 points, p = 0.23. **The sample lied**, and the warning was visible at the
time — the share of documentation in the returned context barely moved, 38% to
35%, so whatever produced the sample's gain was not the mechanism the change was
built on.

What did move is cost: 9,149 tokens to 8,107, about 11% at flat recall. Real,
and not what the change was for; keeping it on that basis would rationalise an
untuned constant into a result it did not earn.

**The third formulation is the one that explains the other two.** Asking whether
anything *outside a tree* consumes it separates `docs/` from `packages/` exactly
as intended — and makes recall worse, 42.6% to 40.9%, 22 instances better
against 35 worse. Where it did the damage is the finding:

| | n | before | after |
|---|---:|---:|---:|
| answer includes a `docs/` file | 51 | 15.8% | 12.7% |
| answer is `packages/` only | 169 | 50.3% | 49.0% |

Losing 3.1 points where documentation *was* the answer is the price the change
knowingly bought. Losing 1.3 points where the answer was implementation is not —
those are the instances it existed to help.

The simplest reading of that second row: a demo imports the component it
demonstrates, so it is a **bridge** as much as a distraction. Seed the demo and
the expansion walks its imports into the implementation; demote the demo and the
path leaves with the noise. Not proven — but it accounts for all three
formulations failing at once, which "documentation merely competes for slots"
does not.

So the 38% documentation share is not waste waiting to be reclaimed, and the
next attempt should not begin by assuming it is. Declined. The decoy is real,
measured from both sides, and has no fix here.

### Where it loses

Keyword top-10 beats us outright on **5.2%** of instances, and we return nothing
useful at all on **14.4%**. That is a retrieval problem and a bigger budget will
not fix it. pylint is the weakest repository at 50.8% and sphinx the next at
68.9%, though pylint is ten instances and should not be read as a finding.

### The earlier benchmark, and why it was replaced

`bench/cost.py` used a bug-fix commit's message as the query and the files it
touched as the answer — local, reproducible, and leaky. A commit message is
written *after* the fix, by someone who knows it, and frequently names the
function that changed; real issue text does not. The leak flattered us, which is
the direction nobody audits. Its numbers are kept here because the co-change
result below was established on them.

`bench/cost.py`, django, 30 bug-fix commits. Ground truth is the set of files
each fix actually touched; the query is the commit message. Entirely local and
reproducible — no API token, no curated dataset. The baseline is keyword search
(tokenise, `git grep`, rank by hit count, read the top *k*), which is what an
agent without a structural index actually does.

| approach | recall | tokens/query | tokens per recall point |
|---|---:|---:|---:|
| leangraph n=10 | 19.5% | 1,219 | **62** |
| leangraph n=25 | 30.5% | 2,500 | **82** |
| leangraph n=50 | 34.1% | 3,948 | **116** |
| **leangraph n=100** | **50.0%** | **9,479** | 190 |
| leangraph n=200 | 52.4% | 17,520 | 334 |
| keyword top-3 | 28.0% | 87,703 | 3,127 |
| keyword top-5 | 40.2% | 146,296 | 3,635 |
| keyword top-10 | 52.4% | 261,812 | 4,993 |
| keyword top-20 | 63.4% | 409,488 | 6,457 |

Two readings, both fair:

- **At matched recall (52.4%): 17,520 tokens against 261,812 — 14.9x fewer.**
- **leangraph at n=100 beats keyword top-5 on recall (50.0% vs 40.2%) while costing
  15x less.**

The efficiency column is the durable number: a recall point costs leangraph 62–334
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

Keyword search reaches 63% by brute force at 409k tokens; leangraph tops out near
52%. Raising that is retrieval work, not budget work — leangraph at n=200 already
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

Profiling the phases (`LEANGRAPH_PROFILE=1`) on a one-file sync corrected the design's
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

`leangraph index --since <sha>` diffs two commits instead of walking the tree, which
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

`bench/converge.sh` asserts that an incremental sync produces a **semantically identical**
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
leangraph          3.0 ms   (2.3 ms before prose indexing)
codegraph    580.0 ms        190x
```

Structural, not tuning: `Graph::open` is an mmap plus a header check, so there
is nothing to warm. CodeGraph's own CLAUDE.md names startup as the reason
agents "dive into Read/grep before codegraph finishes its ~2-3s startup" — the
580 ms measured here is with the npm package already resolved and warm, so it
is the friendly end of their range.

---

## Full pipeline — Phase 3

| Repo | Files | leangraph | CodeGraph | speedup | leangraph nodes/edges | CG nodes/edges | resolved |
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

| Repo | Files | leangraph | CodeGraph | speedup | leangraph nodes | CG nodes | leangraph edges | CG edges |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| flask | 83 | **20 ms** | 0.51 s | 26× | 1,963 | 2,705 | 5,161 | 5,171 |
| excalidraw | 666 | **120 ms** | 3.06 s | 26× | 9,915 | 11,162 | 48,721 | 48,574 |
| django | 3,038 | **500 ms** | 7.69 s | 15× | 67,924 | 62,114 | 331,743 | 195,802 |

Edge counts land within 1% on flask and excalidraw and 69% higher on django — we emit multiple candidates for ambiguous name matches, each carrying its own confidence, where CodeGraph emits one or none.

**leangraph does not persist yet.** Phase 2 adds the CSR write, so these numbers will grow. Treat them as a floor, not a product number.

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

| Repo | leangraph parse+extract only | CodeGraph full | parse's share |
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
pull request is a **draft**, that its head is `leangraph/issue-N` and never the
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

`LEANGRAPH_ALLOWED_HOSTS` pins cloning to named hosts, and is also how an internal
one is permitted deliberately — a GitHub Enterprise install:

```
LEANGRAPH_ALLOWED_HOSTS=ghe.internal.test,github.com

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

Nodes were verified against an oracle at 96.5% presence recall across ten repositories. Edges were counted
and never checked, which mattered more than it sounds: **69% of django's semantic
edges and 83.5% of its call edges were name matches** — a name found somewhere in
the repository, with no scope and no import behind it. If those were mostly
wrong, the graph was mostly noise and every claim resting on it was worthless.

Worse, the confidence score itself was untested. leangraph puts a confidence and a
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
distinct in-repo edges over 485 passing tests, symmetric difference 0 across
repeated runs, 1.6× wall clock.

**CodeGraph agreement** — covers everything, proves nothing. Where we differ,
either side may be right.

### The answer, and what it costs to state honestly

```
flask lib→lib, callers that executed, + MRO closure
  conf 100  scope     65.2%   [56.8 – 72.7]   135 edges / 106 callers
  conf  95  import    20.0%                     5 edges /   5 callers  — too few to read
  conf  80  name      48.0%   [40.2 – 55.9]   150 edges / 101 callers
  conf  60  name      19.5%   [13.5 – 27.4]   123 edges /  43 callers
  conf  45  name      34.8%                    23 edges /   6 callers  — too few to read

  null holds caller and locality fixed:  z ≈ +6.13 ± 0.27, 31 informative strata
  observed z = +6.83                     permutation p = 0.0082
```

Confidence orders correctness over the range that carries data. Three things
about how that is measured are load-bearing, and each of them makes the claim
weaker than the first version of this file said.

**The p-value had to be thrown away and recomputed.** Cochran-Armitage assumes
independent observations. These are not: edges cluster hard by caller, and the
buckets have very different caller counts. A caller whose edges all happen to be
confirmed contributes a run of successes to whichever bucket it populates, and
the statistic reads that as signal. So the null is generated rather than
assumed — shuffle the confidence labels *within* each caller-and-locality
stratum, which destroys any relationship between confidence and correctness
while preserving exactly how the edges are grouped. That null sits at **z ≈ +6.13,
not 0**. The textbook test reported `p = 1.5e-12` for this table; the honest
figure is `p = 0.0082`. Almost all of the apparent significance was the grouping.

**Confidence 100 is same-file by construction.** Scope resolution is lexical, so
it cannot cross a file — every conf-100 edge is intra-file, while the name
buckets are 5–18%. Part of the gap is locality, not confidence. Holding locality
fixed is why the stratum count above is 31 rather than hundreds.

**Two buckets are one or two functions wearing a percentage sign.** flask's
`lib→lib` conf-45 bucket is 23 edges from 6 callers, and 7 of its 8 confirmations
are inside a single function. It is printed, marked, and excluded from the
monotonicity check rather than quoted as 34.8%.

Pooled across populations the ordering is **not** monotone, and the tool now says
so instead of asserting otherwise:

```
all populations, + MRO closure
  conf 100  scope     60.0%   210 edges / 173 callers
  conf  95  import    92.5%    53 edges /  53 callers
  conf  80  name      37.7%   863 edges / 556 callers
  conf  60  name      19.3%   767 edges / 222 callers
  conf  45  name       5.2%   154 edges /  24 callers
```

**Import-resolved edges beat scope-resolved ones, 92.5% against 60.0%.** The two
top confidences are ordered wrongly. One corpus is not enough to reorder the
confidence model, so this is reported rather than acted on — but it is the most
interesting thing the benchmark found, and it would not have been visible from
any amount of agreement between static tools.

The second labeller both confirms and complicates it. CodeGraph orders the name
buckets identically — 70.5% / 33.3% / 7.6% for conf 80 / 60 / 45, over 919 / 745
/ 66 edges — and puts import-resolved edges at **33.0%**, against execution's
92.5%. Two independent labellers disagreeing by sixty points on one bucket is
itself a finding, and the reason neither is ever quoted alone. Whichever is
right, the name buckets are ordered the same way by both, which is the part the
ranking actually depends on.

### What it found first, though, was that we were wrong

Before the fixes below, the same measurement said something worse. On `lib→lib`
the *proven* tier lost to the guess: conf 100 at 51.9% against conf 80's 54.5%.
And the falsification floor at confidence 100 was the highest of any bucket on
two of three corpora — 12.9% of flask's scope-resolved call edges were provably
false, every sampled one the same shape: `super().x()` inside `x`, resolved to
itself.

### Four defects, four different kinds of mistake

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
reached their name, and `super()` is resolved through the classes its own class
declares.

### Three regressions those fixes introduced

Found by an adversarial re-check, not by the fixes' own tests. Each is a case
where a rule that was right in general was wrong in a shape nobody had listed.

**The builtin filter became receiver-blind.** Moving it in front of the index
made `client.open(url)` the builtin `open`. Every edge into flask's
`FlaskClient.open` disappeared — all 198 of which the test suite executes — along
with 8,867 python-to-python edges in django. `PY_BUILTINS` contains `open`,
`set`, `list`, `filter`, `compile` and `type`; the names collide constantly. The
check is now gated on a bare receiver.

**Factory base classes lost their class.** Taking the last segment of a dotted
base is right for `a.B` and wrong for `BaseManager.from_queryset(QuerySet)`,
where it named a *method* — the same defect the fix was written to remove, one
level along. A dotted base that is being called now yields the object it hangs
off: `Manager extends BaseManager`, confidence 100.

**A module receiver is not an opaque one.** `flask.redirect()` was demoted to a
name match along with `client.open()`, because both are dotted. But an import is
real evidence about a module and none about an object. References now carry the
receiver's name, and the import tier applies when it matches something the file
imports.

### What actually changed in the graph

A subtraction of two totals is not an audit. `bench/edgediff.py` joins the two
dumps and reports what left, what arrived, and how much of what left any rule can
prove wrong:

```
django, semantic edges     201,641  →  193,836     net -7,805
  removed                   26,921
  added                     19,116

of what was removed
  crosses a language boundary          11,962
  targets a vendored bundle            10,562
  extends a non-class                   8,499
  self-call, no recursion in source        905
  extends self                             473
  any rule fires                        21,816    81.0%
  no rule fires — unexamined             5,105    19.0%
```

Nineteen percent of what was removed was never checked. It is not therefore
wrong, and it is not therefore right.

### The floor is partly self-certifying, and says so

Two of the five rules are the literal negation of a check the resolver now
performs — `crosses a language boundary` is the complement of `same_family()`,
and `extends self` is the `dst != src` suppression. After the fix they report
zero *by construction*. Keeping them is worth it, because a regression would
light them up; quoting a floor that includes them as proof the resolver improved
is circular. `edgefacts` marks them and computes the headline over the rest:

```
rules the resolver does not enforce
  django        1,487 of 196,607   0.76%
  flask             28 of   3,356   0.83%
  excalidraw        81 of  28,144   0.29%
```

The percentage has a denominator that moves when the resolver changes, so a
before/after comparison has to quote counts as well — which is what the diff
above does.

### Recall against execution

```
observed edges                       2,712
both endpoints are leangraph nodes       2,682
of those, direct calls               2,101
and 581 attributed by walking up past foreign frames

                                   direct    incl. walked
  exact match                       25.4%           20.5%
  + constructor rule                29.8%           23.9%
  + MRO closure                     30.3%           24.3%
```

The staged ladder is printed rather than summarised: each rule makes matching
easier, and a reader who sees only the last number cannot tell how much is leangraph
being right and how much is the comparison being generous.

**The walked column is reported second and never as the headline.** When a
repository function calls into a library and the library calls back, the hop
counter blames the nearest in-repo frame above — which did not make that call.
`save_session → _lazy_sha1` at seven hops is a call `itsdangerous` made, and
nothing in `save_session` names `_lazy_sha1`. Those 581 edges match at 2.8%,
which is what you would expect of edges that are not ours to have.

A quarter of the direct misses come out of five call sites.
`Flask.dispatch_request` alone accounts for 258, and it does
`self.view_functions[rule.endpoint](...)` — a dictionary lookup, then a call. No
static analysis follows that. One of the five, a decorator wrapper, is
followable in principle and is not dispatch; the framing is "five sites" because
that is where the fraction was counted, and callers six through fourteen are the
same mechanism.

The fixes moved recall **up**, once the three regressions above were repaired.
Measured with the same matcher against the same trace, direct calls only:
exact-match 24.9% → 25.4%, `+MRO closure` 30.0% → 30.3%. The
intermediate state — after the four fixes and before the regressions were found —
was *below* the starting point, and nothing in the suite flagged it. Runtime
recall is now printed on every run for that reason.

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

Three defects found while re-running everything after the fixes, all of which
had been silently moving published numbers.

**The keyword baseline was a random draw.** `bench/cost.py` picked its search
terms with `list(set(tokenize(text)))[:12]`. Set iteration order for strings
depends on `PYTHONHASHSEED`, so every run selected a different twelve tokens —
the same command on the same repository gave keyword top-3 at 24.3% and then
27.8%. leangraph was being compared against one sample from a distribution.

**The ground truth moved.** The case list is harvested with `git log --name-only`
and rename detection was on. django's corpus is a shallow clone; rename detection
compares blob *contents* and fetches them lazily, so its answers depend on which
blobs happen to be local. Running the indexer in between changed the object store
and therefore changed which commits qualified.

**A benchmark inherited its predecessor's state.** `bench/swebench.py` kept
`.leangraph` between instances, so each measurement was an incremental sync from
the commit measured before it rather than an index of the commit being measured.
Two runs of the same binary differed on 20 of 500 instances. Worse, the
co-change cache's staleness gate was asymmetric and reported zero drift for a
cache built 16,722 commits *ahead* of the checkout, feeding the ranker history
that contained the fix. Both are fixed — a fresh index per instance, and a
symmetric `drift` — and the full account is under [Cost](#cost--tokens-to-reach-the-files-that-actually-changed).

The shape these three share is worth naming: every one of them is a cache or a
carried state that was correct in the direction the code normally moves, and
wrong the moment something moved the other way.

Three consecutive runs now agree on every row.

## Methodology

- **Startup subtracted from CodeGraph.** Its 0.50 s is real and per-invocation for a CLI, but paid once for a daemon. Subtracting isolates algorithmic work, which is the fair comparison for engine design. Our own startup is not yet measured; Phase 3 will report it, and it is where a static binary with an mmap'd graph should win outright.
- **Warm page cache on both sides.** Cold-cache runs are I/O-bound and compress the difference.
- **Same file-size skip.** Both ignore files over 1 MB.
- **Not yet a general claim.** The *speed* numbers are one machine, one OS, three repos, two languages. Node verification is broader — ten repositories across eleven languages, `bench/langverify.sh` — but timing was not re-measured on the seven added for it.
- **CodeGraph's own progress line self-reports ~2.0 s for django**, but wall clock minus startup is 7.7 s — its counter evidently covers only part of the pipeline. We use wall clock, which is what a user experiences.

## Where a sync actually spends its time

A one-file change, measured at two scales:

```
                django (3,038 files)   material-ui (27,730)
discover              57 ms                  107 ms
extract                2 ms                   16 ms   served from cache
resolve               51 ms                  312 ms
co-change              9 ms                   19 ms
persist               48 ms                  101 ms
total                167 ms                  555 ms
```

Persist was 84 ms on django before the extraction cache became a base plus a
delta; rewriting 33 MB to record one changed file was the single largest cost in
a sync, and it is now 10 KB.

**The second column is the point.** Rewriting the graph is 48 ms of 167 on
django and 101 of 555 on the monorepo — 18%, and only part of that is the CSR. `resolve` is 56% of
a monorepo sync, because a single changed file still re-resolves the entire
repository.

Of the persist that remains, the graph is: symbol table 6 ms, CSR sort 10 ms,
write 1 ms, fsync 11 ms. Patching the CSR in place would recover perhaps 15 of
those — and the CSR is offset-addressed, so an overlay means every `callees` and
`callers` merges a base with a delta on the read path. That path is the
project's headline: `Graph::open` is an mmap and a header check, 15 µs on a
68,000-node graph and 3.0 ms to MCP `initialize`.

Fifteen milliseconds against complicating the thing the whole design is built to
be fast at is the wrong trade, and it is a worse trade at scale than it looked
when only django was measured — the share it addresses *falls* as a repository
grows, while resolve's rises.

So the order worth doing is the opposite of the obvious one:

1. **A watcher.** `--since <sha>` already replaces the tree walk with a git tree
   diff (57 ms to 30 ms on django) and the server uses it on push webhooks. What
   does not exist is a local daemon that watches the filesystem and calls it —
   the pieces are there and nothing binds them into a loop.
2. **Incremental resolve.** The 56%. Hard for a real reason rather than an
   incidental one: resolution is *global*. A definition added anywhere can
   change what a reference elsewhere binds to — that is what tier 3's global
   name match is — so doing it incrementally means knowing which references
   could be affected, and getting that wrong costs the convergence invariant,
   which is the strongest correctness guarantee here.
3. **CSR patching**, last, for the reasons above.

Written down because "not built" and "measured and declined" are different
things, and only one of them is a decision.

## Scoring a patch, and five ways the scoring was wrong

`bench/swefix.py` applies a candidate patch in the instance's own Docker image,
applies the benchmark's test patch on top, and requires every FAIL_TO_PASS test
to pass and every PASS_TO_PASS test to still pass. That is the benchmark's
definition and it is not negotiable. Getting *there* took three corrections,
each of which had been quietly producing zeros.

**django was never being asked the right question.** Its instances name tests
the way django's own runner does — `test_accent (dbshell.test_postgresql.PostgreSqlDbshellCommandTestCase)`
— and no pytest node id matches that, so every django instance reported zero
passes and read as a broken image. django is 13 of the first 20 instances, so
this was most of the sample. With `tests/runtests.py` and the matching parser,
`django-11087` scores its gold patch at F2P 1/1, P2P 41/41.

**The output was being truncated before it was read.** The run ended in
`tail -120`; `pytest -rA` prints one line per test, so any instance with more
than about a hundred tests lost the evidence that it had passed. A 732-test
suite reported 0/732. Filter to result lines first, truncate second.

**The published dataset contains ids that cannot match.** astropy names a test
`test_non_mapping_init[ceci n'est pas un dict]`, and SWE-bench Verified stores
it split on whitespace — the first fragment being `test_non_mapping_init[ceci`.
This was checked against the parquet on HuggingFace; it is upstream, not
something the fetcher did. Handing pytest such an id is a *usage* error rather
than a collection error, so `--continue-on-collection-errors` does not help:
pytest exits having run nothing at all. 676 ids are affected, which is 1.1% of
them — but one is enough to zero an instance, so they poison 65 of the 500.
Dropping ids whose brackets do not balance took `astropy-13236` from 0/644 to
642/642.

**pytest was colouring its output.** `astropy-14995` ran 180 tests, passed all
180, and was recorded as 0/179. The lines read
`\033[32mPASSED\033[0m …\033[1mtest_name\033[0m`, so a parser looking for a
line starting `PASSED ` matched nothing, and the test id carried escape codes
of its own so matching the id would have failed too. Some images make pytest
believe it is attached to a terminal and some do not, which is why this struck
three instances and left the rest looking healthy. `--color=no` stops it and
the parser strips escapes anyway.

    astropy-14309   0/141 -> 141/141
    astropy-14369   0/716 -> 716/716
    astropy-14995   0/179 -> 179/179

All three had been written off as broken environments — twice, in reports
written here.

**django ids name a docstring, not a method.** unittest at `--verbosity 2`
prints two lines for a test carrying one — the id, then the docstring — and the
benchmark stored whichever it saw. So 3,643 ids across 167 django instances are
a bare docstring: `Regression for #9362`, with no class attached. Turning
`test_x (a.b.C)` into the label `a.b.C.test_x` therefore produces
`a.b.C.Regression for #9362`, which names nothing, and unittest answers
`unittest.loader._FailedTest`; ten of those on django-11149 were counted as ten
tests the candidate had broken. Running the *module* and matching what django
prints needs no translation and cannot drift.

    django-10973   F2P 4/5  P2P   0/0  ->  5/5   0/0
    django-11141   F2P 0/1  P2P 18/24  ->  1/1  24/24
    django-11149   F2P 2/2  P2P 36/46  ->  2/2  46/46
    django-11292   F2P 1/1  P2P 28/31  ->  1/1  31/31

One of those had been recorded here as needing a postgres server. It needed
nothing.

### The gold gate

`--gate` scores the benchmark's own patch first and drops any instance it
fails. This is not leniency. An instance whose reference patch leaves its tests
red is measuring the harness, the image, or the network, and counting it as a
loss for every arm is a wrong denominator. What survives the gate is small and
honest; what it excludes is named in the output rather than absorbed.

What the gate excluded fell from 8 of 14 to 1 of 14 as those five defects were
found, on the same instances and the same stored patches — nothing was
regenerated and nothing was spent. That is the measure of how much of an
"exclusion" was ever about the benchmark: almost none of it. What remains is
`astropy-13398`, whose coordinate tests want IERS data the container cannot
download.

The lesson is not that these were hard to fix. Each was a few lines. It is that
all five produced *zeros*, and a zero is indistinguishable from a hard problem
until someone reads the container's own output.

## What is still unmeasured

- **Edge precision outside flask.** Now measured directly where the runtime oracle can settle it — 78.1% over 688 edges at 560 call sites, and 94.8% in the confidence-100 bucket (`bench/edgeprecision.py`). That subset is one repository in one language, because it needs a test suite that runs offline. Everywhere else it is still a floor from `edgefacts` and a ceiling from fan-out.
- **Edge recall outside flask.** django's suite needs dependencies this machine cannot fetch offline; excalidraw has no Python. One repository, one language.
- **Whether the answer is right, as opposed to the context.** Localization is measured on SWE-bench Verified — 81.8% of the files that had to change, over 500 real issues (`bench/swebench.py`). The benchmark also ships the tests that decide whether a *patch* is correct, and those now run end to end against both arms with a model behind them. The result so far does not carry a conclusion: 6 of 13 against keyword's 4 of 13, four discordant pairs, three of them our way. A sign test on four pairs cannot reach significance whichever way they fall — the smallest p available is 0.062. What blocked this was never the benchmark: five defects in the scoring reported zeros that read as hard instances, and fixing them took the gate's exclusions from 8 of 14 to 1 of 14 on the same stored patches. A fifty-instance run is what decides it.
- **Embedding retrieval as a baseline.** Keyword search is what an agent without an index falls back to; what people who build context for agents actually deploy is embedding retrieval over chunked source. `bench/ragbase.py` implements it — chunked so it pays only for what it reads, at leangraph's own token budget so the comparison is at equal cost — but the column is not filled in here yet.
- **The agent against the real API.** Every agent assertion runs against a stub. Shape, safety and caching structure are checked; answer quality is not.
- **Fix mode against a real provider.** The git half is real; GitHub is a stub, so nothing here says how often a proposed patch is *correct* — only that a wrong one cannot escalate.
- **Whether running the tests catches anything.** They run now, and the pull request reports what happened; nobody has measured how often a proposed patch passes a suite it should have failed, because that needs proposed patches against real repositories.
- **Languages by tier, rather than by node recall.** Presence recall is measured for eleven of the fourteen against an oracle. How well each one *resolves* — which tier its references reach — is reported per corpus but not compared against anything.
- **Anything other than one machine.** M1 Pro, macOS, three repositories.
