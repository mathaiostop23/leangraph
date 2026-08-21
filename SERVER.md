# leangraph server — self-hosted issue agent

Webhook → issue → graph context → agent → comment. Self-hosted, bring-your-own
API key.

**This document describes what is built.** Where something was designed and not
built, it says so under [§7](#7-what-is-not-built) rather than being described in
the present tense — an earlier revision of this file was a plan written before
the code, and a reader could not tell the two apart. The engine underneath is
[ENGINE.md](./ENGINE.md); measurements are [BENCH.md](./BENCH.md); what is done
and what is next is [ROADMAP.md](./ROADMAP.md).

---

## 1. Architecture

One process. One binary. One file on disk that is a database.

```
                  GitHub webhook  (HMAC over raw bytes, returns 202)
                          │
                          ▼
          ┌───────────────────────────────┐
          │  axum — one process           │
          │                               │
          │  ingress ──► jobs table ◄──┐  │
          │                  │         │  │
          │            ┌─────┴─────┐   │  │
          │            │ N workers │───┘  │   retry / requeue
          │            └─────┬─────┘      │
          └──────────────────┼────────────┘
                             │
           ┌─────────────────┴──────────────────┐
           ▼                                    ▼
 <data>/repos/<owner>/<name>            <data>/leangraph.db
   the checkout, cloned                   repos, jobs, issues, runs,
   .leangraph/  the CSR graph             cost_ledger, secrets, deliveries
```

There is no Redis, no Postgres, no separate queue service and no second
container. The queue is a table in the same SQLite file as everything else, and
the workers are tokio tasks in the same process as the HTTP server. That is a
deliberate trade: it costs concurrent writers, which a single-process server
does not have, and it buys an install that is `docker compose up` with one
volume.

### Stack

| Layer | Choice | Reason |
|---|---|---|
| Runtime | Rust, one static binary | the engine links in as a module: no IPC, no FFI, no serialization boundary between the graph and the thing using it |
| HTTP | axum + tokio | raw-body access, which HMAC verification requires |
| Queue | SQLite table, claimed with `UPDATE … RETURNING` | see below |
| App DB | SQLite, bundled (`rusqlite`) | compiled into the binary; there is nothing to install |
| Graph | leangraph CSR, mmap'd, one per repo | **not** the app DB — different file, different format, different lifecycle |
| Git | shelling to real `git` | libgit2 adds a dependency and a second implementation of behaviour we would have to keep in sync |
| LLM | `reqwest` → Anthropic Messages API, BYO key | no official Rust SDK; raw HTTP is the sanctioned path |
| Secrets | AES-256-GCM, key from `LEANGRAPH_MASTER_KEY` | §5 |

### On disk

```
<data>/
  leangraph.db          app database (WAL)
  master.key            0600, generated on first run if absent
  admin.token           0600, generated on first run if absent
  repos/<owner>/<name>/ the checkout, and .leangraph/ inside it
```

A plain clone, not a mirror plus worktree. Fix mode adds a **throwaway worktree**
per attempt so a patch is never applied in the checkout the graph was built
from, and removes it whether the attempt succeeded or failed.

### The queue

Everything asynchronous is a row in `jobs`: cloning, indexing, syncing, and
answering an issue. Four properties are load-bearing, and each exists because of
a specific way this goes wrong.

**A worker claims a job atomically.** `UPDATE … WHERE state='queued' … RETURNING`
in one statement, so two workers cannot take the same row. This is the
single-writer equivalent of `FOR UPDATE SKIP LOCKED`; nothing else in the design
depends on SQLite semantics.

**A push burst collapses into one sync.** A partial unique index on
`dedupe_key WHERE state='queued'` makes a second enqueue of the same key a no-op
while the first is still waiting. Uniqueness only among queued rows, so the same
key can be enqueued again once it has run — ten commits in thirty seconds
produce one sync, and the eleventh commit tomorrow produces another.

**A transient failure comes back; a real one does not.** Retries are decided by
the error, not by the fact that there was one. `agent::Transient` marks HTTP 429,
5xx, and a request that never landed — only those requeue, with `run_after` set
30 s, 2 m, then 8 m out, across four attempts. A malformed request fails once and
stays failed. Retrying that would burn three more calls to reach the same
conclusion, and hide a bug behind what looks like flakiness. The base is
`LEANGRAPH_RETRY_BASE_SECS` because the right first wait depends on the provider,
and because a test that waits 30 s to prove a retry works is a test nobody runs.

**A killed process does not strand its work.** A job is marked `running` before
it runs, so a worker that dies mid-job leaves the row there — never claimed again
because it is not queued, never reported because it has not failed, and shown in
the dashboard as permanently in progress.

A claim therefore takes a **lease**, not just a state: sixty seconds, renewed by
a heartbeat at a third of that for as long as the work runs, and a sweeper
requeues anything whose holder stopped renewing. A `running` row cannot tell a
crashed worker from a busy one; an expired lease can. That distinction is what
makes two instances on one database safe — the earlier version requeued every
`running` row at startup, which recovers a crash for one process and steals the
other one's in-flight work for two.

The cost is recovery latency: a crashed job waits out its lease rather than
being reclaimed the instant someone restarts. Up to a minute, against silent
duplicate work, which is the right way round.

Every job opens the graph rather than taking one from a pool of warm handles.
An LRU was designed for this; opening is an `mmap` and a header check, measured
in microseconds, and a test asserts it stays under a millisecond so the decision
is a measurement rather than an assumption.

### Workers

Two pools, because the two sorts of work have nothing in common. Indexing
saturates every core through rayon, so a second concurrent index makes both
slower rather than either faster — `--index-workers` defaults to **1**.
Answering an issue is a socket waiting on a model, so `--workers` defaults to
**8**. They claim different job kinds, which is what keeps a three-second answer
from queuing behind a twelve-minute index.

An issue that arrives before its repository has finished indexing **waits**. The
first issue on a repository usually does arrive that way — registering it and
opening an issue about it are the same afternoon — and answering without the
graph is answering without the one thing this is for. Waiting is not failing, so
a deferred job is put back without spending an attempt; it gets 180 of those,
which outlasts a cold index of anything reasonable, and then fails with a reason.

---

## 2. HTTP surface

| | endpoint | |
|---|---|---|
| `GET` | `/` | dashboard — repos, runs, cost |
| `GET` `POST` | `/repos` | list; register (clone + index in the background) |
| `GET` | `/repos/{owner/name}` | state, counts, index time, last error |
| `POST` | `/repos/{owner/name}/sync` | fetch and incrementally reindex |
| `POST` | `/repos/{owner/name}/backfill` | answer the open backlog as one batch, at half price |
| `POST` | `/repos/{owner/name}/config` | `fix_mode`, `trigger_label`, `fix_label`, `max_nodes`, `escalate`, `test_command`, `sandbox` |
| `GET` | `/secrets` | names and hints only — never values |
| `POST` `DELETE` | `/secrets/{name}` | store encrypted; remove |
| `GET` | `/metrics` | Prometheus text: repos, queue depth, graph size, spend |
| `GET` | `/health` | **open** — counts, not contents |
| `POST` | `/webhook/github` | **open** — HMAC-verified |
| `POST` | `/webhook/gitlab` | **open** — token-verified |

Everything not marked open requires the admin token (§5.3). Registering a
repository spends your API budget, `/secrets` writes credentials, and
`/config` turns on the mode that opens pull requests — that surface does not
default to open.

---

## 3. What an issue costs

Prices live in `price()` in `src/server/agent.rs`, in one place, because
Sonnet's introductory rate expires 2026-08-31 and a ledger that quietly keeps
using it is worse than one that is obviously wrong.

| Model | ID | Input $/MTok | Output $/MTok |
|---|---|---|---|
| Claude Haiku 4.5 | `claude-haiku-4-5` | $1.00 | $5.00 |
| Claude Sonnet 5 | `claude-sonnet-5` | $3.00 | $15.00 |

### The pipeline, cheapest stage first

```
0 — free gates, no model call
    delivery replay · signature · label · author association
    then local duplicate detection
                                              ↓ costs nothing

1 — TRIAGE · claude-haiku-4-5 · 512 max_tokens
    {bug | feature | question | invalid} + seed symbols
    not a bug → post the triage note and stop
                                              ↓

2 — ANALYSE · claude-sonnet-5 · effort high · 4096 max_tokens
    graph context + cached repo preamble → the comment

2b — ESCALATE · claude-opus-5 · only where the repository opted in
     and the analysis reported low confidence of its own accord
                                              ↓

3 — FIX · claude-sonnet-5 · effort high · 8192 max_tokens
    only where all three gates in §5.5 are open
```

The analysis ends with a self-reported `confidence:` line, which the server
reads and strips — it is for routing and would read as a leak in a comment.
Escalation is off unless a repository asks: Opus is five times Sonnet's input
price, and spending that on every issue to help the few that need it is the
opposite of what this project argues. Both calls are billed under their own
stage in the ledger, so what escalation costs is visible rather than folded in.

### The backlog, at half price

A repository registered today usually has issues already. `POST
/repos/{name}/backfill` answers the open ones carrying the trigger label as a
single **Batch API** submission: 50% off, asynchronous, usually under an hour
and up to 24. That is useless for a webhook, where latency is the product, and
exactly right for work nobody is waiting on.

It is a request rather than something registration does on its own — a
repository with four hundred open issues would otherwise spend the operator's
budget the moment it was added. Triage is skipped, because its job is to pull
seed symbols out of the text and the context builder already seeds itself from
the same text; for a bulk run it is a Haiku call per issue that buys nothing.
An issue already answered is skipped, so running it twice does not bill twice.

The poll for results uses the same deferral the issue path does: waiting for a
batch is not a failed attempt, and the ledger books the result at
`BATCH_RATE` — a ledger that did not know about the discount would overstate
what backfill cost by exactly the factor this project publishes.

### Deduplication costs nothing, on purpose

The obvious design embeds the issue and compares vectors. It works, and it means
a network round trip and a bill on **every** issue including the overwhelming
majority that are not duplicates — in a product whose entire argument is that you
should not pay for context you did not need. Paying to find out you did not have
to pay is the wrong shape.

So three local signals, all exact and all free: **word shingles**, **content
words**, and **graph seeds** — the nodes the context builder would select, which
is the signal nothing else here has, since it judges whether two issues are about
the same *code*. Shingles alone are not enough: measured at 0.78 against a paste
with a comment appended but **0.19 against the same report typed again in the
reporter's own words**, and people retype rather than paste. So a near-copy is
conclusive on text alone, and everything else needs vocabulary *and* code to
agree — seeds alone are far too weak, since two unrelated bugs in one popular
function share every seed.

The thresholds are deliberately conservative because the error costs are
asymmetric: a missed duplicate costs one analysis, while a false one answers a
real issue with a link to an unrelated one and teaches the reporter that the bot
does not work. Scoring and constants are in `src/server/dedup.rs`, with ten
tests.

### Prompt caching

The repo preamble is identical across every issue in a repo, so it sits before
the cache breakpoint and the issue-specific context sits after it. Cache reads
bill at 0.1×, so a busy repo pays a fraction of what the input-token count
suggests — which is why the ledger accounts for cache reads separately instead of
reporting a flat input rate that would overstate cost by roughly an order of
magnitude.

Two rules, both learned the hard way:

1. **The minimum cacheable prefix is 1024 tokens** (512 on Opus). Below that it
   silently does not cache — no error, just `cache_creation_input_tokens: 0`.
   This bit us: a ~950-token preamble never cached and nothing said so.
   `cacheable()` now asserts the floor.
2. **Nothing volatile before the breakpoint.** No timestamps, no issue ids. One
   byte of drift invalidates the whole prefix. The agent suite asserts byte
   equality of the cached prefix *across different issues* — which is the only
   way to catch it, since a single call cannot.

### The receipt

Every comment ends with what it cost:

> *18 graph nodes · 24,102 tokens (21,340 cached) · $0.0310*

Per-run and per-repo totals go to `cost_ledger` and appear on the dashboard.
This is transparency first and a differentiator second: nobody else shows you
this number.

---

## 4. Answering an issue

```
webhook delivery
  ├─ X-GitHub-Delivery seen before? → 200, do nothing (providers retry)
  ├─ HMAC over the raw bytes, constant-time
  ├─ label gate: the configured trigger label, or nothing happens
  ├─ author gate: OWNER / MEMBER / COLLABORATOR
  └─ enqueue, return 202
       │
       ▼
  duplicate of an already-answered issue? → post the prior analysis, stop
       │
       ▼
  triage (Haiku) → not a bug? → post the note, stop
       │
       ▼
  build context from the graph, bounded in nodes
       │
       ▼
  analyse (Sonnet) → comment + receipt → ledger
       │
       ▼
  fix mode gates all open? → §5.5
```

An issue arriving before the graph exists waits for it rather than being
answered without it (§1).

---

## 5. Security

This is where the project could embarrass itself publicly.

### 5.1 Webhook verification

**GitHub:** HMAC-SHA256 over the **raw request bytes**, compared in constant
time.
Re-serializing the JSON and verifying that instead would be checking something
the sender never signed. GitHub's webhook UI also defaults to form encoding, so
deliveries arrive as `payload=<urlencoded json>` rather than as a JSON body —
the signature covers the form body, and that case has its own test because
getting it wrong fails open on exactly the deliveries a default configuration
sends.

`X-GitHub-Delivery` is stored and replays are dropped. Providers retry, and a
retried issue must not produce a second comment.

**GitLab** sends the secret itself in `X-Gitlab-Token` rather than a signature
over the body. That is weaker — it is a bearer token, and anything that logs the
request has it — but it is what the platform offers, so the comparison is
constant-time even though there is no MAC to forge. `X-Gitlab-Event-UUID` serves
the same purpose as GitHub's delivery id.

The GitLab payload is **translated** into the shape the GitHub handlers already
read, rather than getting handlers of its own. The author gate, the label gate
and replay rejection are security properties, and a second provider with its own
copy of them is a provider where one of them quietly differs. What actually
differs is authentication and field names, and that is all the adapter does.

One gate has no counterpart: GitLab's payload does not say whether the author is
a member, so that check is an API call against the project's member list.
Anything short of a definite yes — the API down, a token that cannot read
members, a malformed answer — is treated as *not a member*. A gate that exists
to stop a stranger spending the owner's budget cannot fail open, and there is a
test that restarts the stub answering 404 to prove it does not.

**Fix mode is GitHub-only.** It opens a pull request; the merge-request
equivalent is not written, so a GitLab repository with fix mode enabled logs a
refusal and posts the analysis without a patch rather than failing three steps
later on a 404.

### 5.2 Prompt injection

Anyone can open an issue on a public repository, and that text goes into a
prompt. This class of attack has been exploited against GitHub-integrated agents
in the wild.

| Control | How |
|---|---|
| **Trust boundary** | the body is wrapped and declared as user-submitted data carrying no authority — asserted in the agent suite, including that attempted delimiter escapes are neutralised |
| **Author gate** | only `author_association` ∈ {OWNER, MEMBER, COLLABORATOR} |
| **Label gate** | an explicit label. Opt-in per issue, never blanket |
| **No tools** | the analysis agent is handed no tools at all. It cannot read a file, run a command, or reach the network |
| **Egress** | the worker container reaches the model API and the git host. §5.4 |
| **Never auto-fix by default** | §5.5 |

The strongest control here is the fourth: an agent with no tools cannot be
talked into using one.

### 5.3 Access control

Seven of the nine endpoints manage the install: they register repositories,
write secrets, and turn on the mode that pushes pull requests. All of them were
unauthenticated, and the security model was a sentence in this document telling
the operator to put a proxy in front — which is a plan, not a control.

They now require a token. It comes from `LEANGRAPH_ADMIN_TOKEN`, or is generated
on first run into `admin.token` beside the master key and printed at startup.
Three ways to present it, because two different clients need it: `Authorization:
Bearer`, `X-Leangraph-Token`, or `?token=` for a browser opening the dashboard.
The query form is the weak one — it lands in history and in any proxy log on the
way — and it exists because a browser cannot send a header.

Comparison is constant-time. A byte-at-a-time check hands the prefix to anyone
willing to time the responses.

Two endpoints stay open, deliberately. The **webhook** must be reachable by the
provider and carries its own HMAC over the raw body. The **health probe** is what
the container healthcheck calls, and it reports counts rather than contents.

`--no-auth` restores the old behaviour for an operator who genuinely has
authentication in front, and says what it costs in the startup log.

### 5.4 Egress — where a repository URL can point

A repository URL is the only user-supplied value in this product that causes an
outbound connection to a host of the caller's choosing. `https://` alone is
satisfied by `https://169.254.169.254/latest/meta-data/iam/security-credentials/`
— the cloud metadata endpoint, which answers with the container's credentials to
anything that can reach it — and by every private address on whatever network
the server can see. That is server-side request forgery, and it is enforced here
rather than assumed away.

`check_url` resolves the host and refuses **every** answer that is not public:
loopback, private, link-local, unique-local, CGNAT, multicast, unspecified, and
IPv4-mapped forms of all of them. Every address is checked, not the first — a
name returning one public and one private answer is precisely how this gets
bypassed. `file://`, `ssh://`, the scp-like `git@host:path` form and `ext::` are
refused outright: they would read the host filesystem, use ambient key material,
or run an arbitrary command.

`LEANGRAPH_ALLOWED_HOSTS` pins cloning to named hosts. An entry covers the host
and its subdomains (`github.com` allows `codeload.github.com`) and nothing that
merely looks like it (`evil-github.com` is refused). Setting it is also how you
deliberately permit an internal host — a GitHub Enterprise install — since an
explicit allowlist is a statement about where this install may reach and
overrides the address check rather than stacking with it.

**What this is not.** The check resolves now; git resolves again when it
connects, so a name whose answer changes in between would slip past. Closing
that needs a resolver the connection itself is pinned to, which git does not
offer. `LEANGRAPH_ALLOWED_HOSTS` is the airtight in-process control, and a
network policy on the container is the real one — the compose file says where to
put it.

### 5.5 Fix mode — built, opt-in (`src/server/fix.rs`)

Posting a comment and changing someone's repository are different decisions, so
they are gated differently. Fix mode is off in three independent ways, and all
three must be open before a single line is written:

1. the repository has `fix_mode` set — `POST /repos/{owner/name}/config`;
2. the issue carries `leangraph-fix`, a **second** label distinct from the one
   that triggers analysis;
3. a token with write access exists.

Any one missing and the analysis still posts, but nothing is pushed. The refusal
is logged with the reason so an operator is never left guessing why.

```
clean the model's diff (fences, stray prose)
  → vet the paths before applying anything
  → git worktree add --force -B leangraph/issue-N  (isolated; never the indexed checkout)
  → git apply --check, then apply
  → stage only the vetted paths — never `git add -A`
  → push a fresh branch, no --force
  → open a DRAFT pull request
  → remove the worktree, success or failure
```

What it will never do: push to the default branch, force-push, merge, mark a PR
auto-mergeable, or touch CI configuration, dependency manifests, lockfiles,
Dockerfiles or Makefiles. That last list is the important one — a patch editing
`.github/workflows` or `package.json` is a privilege escalation wearing a bug
fix, and it is the first thing a hostile issue would reach for. The rule is
enforced in code before `git apply` runs, not merely requested in the prompt; the
prompt states it too, so that enforcement has to reject less often.

A patch is also refused for touching more than 12 files, exceeding 60 KB, naming
no files, or reaching outside the worktree.

The context the patch is written from is the graph selection — the same one the
analysis used, and the reason a fix can be attempted at all without reading the
repository.

**Running the tests.** A repository can set `test_command`, and the patch is
then checked against the repository's own suite before the branch is pushed —
`{files}` in the command is replaced with the paths the patch touched. The pull
request says which of four things happened: the suite passed, it failed, it
timed out, or no command was configured and nothing ran. A failing suite still
opens the pull request, because discarding the branch would hide the failure
rather than report it.

**This executes code from the repository.** Three things make that a decision
rather than an accident: the command is the operator's, written into the
repository's config and never inferred or asked of the model; fix mode is
already off in three independent ways before a patch exists; and the patch
cannot have touched CI configuration, dependency manifests, lockfiles,
Dockerfiles or Makefiles, which is exactly what a hostile patch would reach for
to turn "run the tests" into "run anything".

What the process gets is deliberately small:

| | |
|---|---|
| **environment** | cleared, then `PATH`, `HOME`, `LANG`, `LC_ALL`, `TZ`, `TMPDIR` and nothing else. The first version inherited the server's environment wholesale, which handed `LEANGRAPH_MASTER_KEY`, the model key and the write token to whatever the suite runs |
| **cwd** | the throwaway worktree, never the indexed checkout |
| **stdin** | `/dev/null`, so a prompt is not a hang |
| **time** | ten minutes, then the whole **process group** is killed — killing only the shell leaves the test runner holding the worktree |
| **output** | the last 4,000 characters, which is where a failing suite says what failed |

**Isolation is still the operator's, but there is somewhere to put it.** A
repository can set `sandbox`, and the command is wrapped in it — `{dir}` is the
worktree and `{cmd}` the shell-quoted command:

```
docker run --rm --network none -v {dir}:/w -w /w python:3.12 sh -c {cmd}
```

That is a container with no network and nothing mounted but the tree under
test. Left empty, the command runs as the server process does, which is what
the config field says.

### 5.6 Key storage

API keys are encrypted with AES-256-GCM under a key from `LEANGRAPH_MASTER_KEY`,
or one generated into `master.key` (0600) on first run. Decrypted only in the
worker, only into memory, never logged. `GET /secrets` returns names and hints,
never values.

---

## 6. Data model

Before any of it, `preflight` writes and reads back a probe database in the data
directory and refuses to start if that fails. A bind mount from a macOS or
Windows host has unreliable file locking and mmap semantics, and both this
database and every graph depend on them — without the check the failure is not a
refusal to boot but a corrupt WAL hours later under load, which nobody can
diagnose from the outside.

SQLite. `PRAGMA user_version` drives a forward-only migration ladder, and
`SCHEMA` is asserted against the ladder's top step on every open — so adding a
migration without bumping the constant, or the reverse, fires immediately rather
than on someone's data months later.

```
repos        id, full_name, provider, path, default_branch, url, private,
             state (pending|cloning|indexing|ready|error),
             last_indexed_sha, last_indexed_at,
             node_count, edge_count, file_count, index_ms, error,
             config_json          -- fix_mode, labels, max_nodes, escalate,
             --                       test_command
jobs         id, kind, repo_id, payload, state, dedupe_key,
             attempts, defers, run_after, error,
             created_at, started_at, finished_at
issues       id, repo_id, number, title, body, author_association, fingerprint
runs         id, issue_id, repo_id, status, outcome, context_files,
             total_tokens, cost_usd, duration_ms, error, created_at
cost_ledger  id, run_id, repo_id, stage, model,
             input_tokens, output_tokens,
             cache_read_tokens, cache_write_tokens, cost_usd, created_at
secrets      name, ciphertext, nonce, hint, created_at
deliveries   delivery_id, received_at        -- webhook idempotency
```

The DDL is entirely `IF NOT EXISTS` and runs on **every** open, not only on an
empty database. It ran only on an empty one until a table added after schema 1
turned out never to reach an existing install — and the `ALTER` for its new
column then failed against a table that had never been created.

---

## 7. What is not built

Listed because a document that describes only what exists reads as though the
rest was never considered.

Nothing, at present. Everything this document once listed here is built, or was
measured and declined with the number written down — a warm-handle pool
(`Graph::open` is microseconds) and CSR patching (15 ms of a 140 ms sync, for a
slower read path).

What remains genuinely absent is smaller and stated where it belongs: nightly
re-analysis of stale issues, which is the second thing the Batch API was meant
to serve; and a sandbox around the test command, which §5.5 says is the
operator's to provide.

---

## 8. Configuration

| Variable | |
|---|---|
| `LEANGRAPH_ADMIN_TOKEN` | admin token. Generated into `admin.token` if unset |
| `LEANGRAPH_MASTER_KEY` | 32 bytes, hex, for secret encryption. Generated into `master.key` if unset |
| `LEANGRAPH_WEBHOOK_SECRET` | the shared secret the provider signs with |
| `LEANGRAPH_ANTHROPIC_KEY` | your API key |
| `LEANGRAPH_GITHUB_TOKEN` | read for cloning; write only if fix mode is on |
| `LEANGRAPH_ALLOWED_HOSTS` | pin cloning to named hosts (§5.4) |
| `LEANGRAPH_RETRY_BASE_SECS` | first retry wait, default 30 (§1) |
| `LEANGRAPH_ANTHROPIC_BASE` `LEANGRAPH_GITHUB_API` | endpoint overrides — the test suite points them at stubs |

```bash
docker compose up -d
curl -X POST localhost:7777/repos \
  -H "authorization: Bearer $TOKEN" -H 'content-type: application/json' \
  -d '{"url":"https://github.com/owner/name"}'
```

---

## 9. Tests

The server's share of the suite. All of it runs without the benchmark corpora.

```
44  unit                 vetting rules, dedup scoring, SSRF, leases, test runs
21  webhook gates        signature, replay, authorship, labels, form encoding
38  agent assertions     prompt safety, cache correctness
23  fix mode, end-to-end against a real git remote
10  deduplication, end-to-end
17  resilience, end-to-end: rate limits, restarts, waiting, escalation
 9  backfill, end-to-end through the Batch API
```

`bench/server_test.sh` runs the ones that need a live server, a stub API and a
repo that has already answered two *different* issues — the caching assertion is
byte equality across issues, so it cannot be checked from a single call. Setting
that up by hand is how these stopped being run.
