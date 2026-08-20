# leangraph server — self-hosted issue agent

Webhook → issue → graph context → agent → comment. Self-hosted, bring-your-own API key.

This document covers the **server product**. The engine it runs on is [ENGINE.md](./ENGINE.md);
phasing is [ROADMAP.md](./ROADMAP.md).

## 0. Engine: our own

An earlier revision of this plan proposed building on `colbymchenry/codegraph` (MIT, 67k stars).
That evaluation is still useful context and lives in [BENCH.md](./BENCH.md) — we use CodeGraph as
a differential-testing oracle and benchmark baseline.

We build our own engine instead. Phase 0 measured why: parse+extract is **2–6%** of CodeGraph's
indexing pipeline, so the headroom is real and sits in resolution and persistence. Owning the
engine is also what makes the cost axis reachable — ranking by edge confidence to return fewer
tokens at equal recall is not something you can bolt onto someone else's graph.

## 1. Architecture

```
                    GitHub App / GitLab webhook
                              │  (HMAC verified, <100ms, returns 202)
                              ▼
                    ┌──────────────────┐
                    │  ingress (Fastify)│
                    └────────┬─────────┘
                             ▼
                    ┌──────────────────┐
                    │  Redis / BullMQ  │
                    └───┬──────────┬───┘
              index queue│          │issue queue
         (CPU-bound,     │          │(IO-bound, concurrency 20+)
          concurrency=2) │          │
                         ▼          ▼
              ┌──────────────┐  ┌──────────────────────────┐
              │ IndexWorker  │  │      IssueWorker         │
              │ git fetch    │  │ 1. ensure graph fresh    │
              │ leangraph index  │  │ 2. build_context()       │
              │ leangraph sync   │  │ 3. triage (Haiku)        │
              └──────┬───────┘  │ 4. solve (Sonnet/Opus)   │
                     │          │ 5. post comment          │
                     ▼          └───────────┬──────────────┘
       ┌─────────────────────────┐          │
       │ /var/lib/ig/repos/<id>/ │◄─────────┘
       │   mirror.git  (bare)    │
       │   worktree/   (checkout)│      ┌──────────────┐
       │   .codegraph/*.db       │      │  Postgres    │
       │   [NAMED VOLUME]        │      │ tenants,repos│
       └─────────────────────────┘      │ issues, runs │
                                        │ cost_ledger  │
                                        │ pgvector     │
                                        └──────────────┘
```

### Stack

| Layer | Choice | Reason |
|---|---|---|
| Runtime | **Rust** everywhere | leangraph links in as a crate: zero IPC, zero FFI, one static binary. Using Node here would reintroduce exactly the boundary cost we beat CodeGraph on. |
| HTTP | axum + tokio | raw-body access for HMAC verification |
| Queue | **Postgres** `FOR UPDATE SKIP LOCKED` | drops Redis entirely. At issue-bot volume it is more than adequate, and self-hosted users get two containers instead of four. |
| App DB | Postgres 16 + pgvector | tenants, repos, runs, cost ledger, issue dedup embeddings |
| Graph | leangraph CSR (mmap, per repo) | **do not conflate with the app DB** |
| Git | `simple-git` shelling to real `git` | mirror + worktree; libgit2 bindings add nothing here |
| LLM | `reqwest` → Anthropic Messages API, BYO key | no official Rust SDK; raw HTTP is the sanctioned path. Provider adapter trait for others later. |
| Container | multi-stage, `scratch`/`distroless` | static binary, no runtime to ship |

### Repo layout on disk

```
/var/lib/issuegraph/repos/<repo_uuid>/
  mirror.git/          # git clone --mirror — cheap fetches, no working tree
  worktree/            # git worktree — the checkout leangraph indexes
  .codegraph/          # codegraph.db (WAL), daemon.sock, lock
```

`clone --mirror` once, then `git fetch` per push. One worktree per repo, checked out at the default branch. Fix-mode gets a **separate ephemeral worktree** so analysis is never blocked by a fix in progress.

---

## 2. Speed engineering (priority #1)

### 2.1 The decision that matters most: index at registration, not at issue time

A cold index of a 10k-file repo is 30–60s. If that happens when the first webhook arrives, every issue pays it and the product feels broken.

```
repo registered  →  enqueue INDEX job immediately (background)
                 →  repo state: pending → indexing → ready
push webhook     →  enqueue SYNC job (~0.3s of work)
issue webhook    →  graph is already warm; context build is milliseconds
```

Issues arriving while a repo is `indexing` are **queued**, not failed, and post a "indexing your repo, will respond shortly" comment. Never fail an issue because of cold state.

### 2.2 Git-driven sync — replace the file watcher

A laptop-oriented indexer syncs via FSEvents/inotify. On a server there are no local edits, only remote pushes, so leangraph's sync is git-driven by design — there is no watcher to disable.

```ts
async function syncRepo(repo: Repo) {
  await git.cwd(repo.mirrorPath).fetch(['--prune']);
  const head = await git.revparse([`origin/${repo.defaultBranch}`]);
  if (head === repo.lastIndexedSha) return;               // no-op
  await git.cwd(repo.worktreePath).checkout(head, ['--force']);
  graph.sync(&changed).await?;                            // target <50ms
  await db.updateRepo(repo.id, { lastIndexedSha: head });
}
```

**Debounce**: a push burst (10 commits in 30s) should trigger one sync, not ten. Use BullMQ's `jobId: \`sync:${repo.id}\`` — an existing queued job with the same ID is deduplicated for free.

**Force full reindex** when: branch changed, force-push detected (`git merge-base --is-ancestor` fails), >30% of files changed, or the leangraph index format version bumped.

### 2.3 Warm handle pool

Opening a graph is an `mmap` plus a header check — microseconds — but the delta overlay and interner are in-memory state worth keeping warm. Hold an LRU of open handles.

```rust
struct GraphPool { lru: Mutex<LruCache<RepoId, Arc<Graph>>> }

impl GraphPool {
    async fn acquire(&self, repo: RepoId) -> Result<Arc<Graph>> {
        if let Some(g) = self.lru.lock().get(&repo) { return Ok(g.clone()); }
        let g = Arc::new(Graph::open(path_for(repo))?);   // mmap + header check
        self.lru.lock().put(repo, g.clone());
        Ok(g)
    }
}
```

Bound the pool by RAM, not repo count. Budget ~50–150MB resident per open large graph — **measure in Phase 0**, do not guess.

### 2.4 Queue separation is not optional

A 12-minute index of a monorepo must not block a 3-second issue response.

| Queue | Concurrency | Why |
|---|---|---|
| `index` | `max(1, floor(cores/2))` | CPU-saturating. leangraph already saturates cores with rayon — running 4 indexes at once makes all 4 slower. |
| `sync` | 4 | short, bursty |
| `issue` | 20+ | almost entirely waiting on the LLM API |

**Per-repo serialization:** two workers compacting one graph will corrupt the delta overlay. Enforce with a Postgres advisory lock keyed on the repo id around every index/sync.

### 2.5 Container CPU must be real

leangraph sizes its rayon pool from the **cgroup quota**. A container with no `cpus` limit sees the host's core count and may oversubscribe; a container limited to 0.5 CPU will index the Linux kernel but slowly.

```yaml
# docker-compose.yml
services:
  worker:
    deploy:
      resources:
        limits:   { cpus: '4', memory: 8G }
        reservations: { cpus: '2', memory: 4G }
```

Document a sizing table in the README:

| Repo size | Cold index (4 cores) | Recommended |
|---|---|---|
| <1k files | <10s | 2 cores / 2GB |
| 1k–10k | 10–60s | 4 cores / 4GB |
| 10k–30k | 1–3 min | 4 cores / 8GB |
| 30k+ | 3–15 min | 8 cores / 16GB |

*(Extrapolated from our django measurement — 3,038 files in 480ms on 8 cores. Re-measure per tier before publishing.)*

### 2.6 The WAL/volume trap — get this right or it fails in the field

```yaml
volumes:
  ig_repos:            # named volume — NOT a bind mount

services:
  worker:
    volumes:
      - ig_repos:/var/lib/issuegraph/repos
```

Bind mounts from macOS/Windows hosts go through a translation layer with unreliable file locking and mmap semantics — and our CSR is mmap'd. Add a **startup check** that writes and fsyncs a test WAL db in the data dir and refuses to boot with a clear error if it fails. Cheap to write, saves every future support ticket.

### 2.7 Speed budget per issue (target)

| Step | Target |
|---|---|
| Webhook ack | <100ms |
| Graph freshness check | <500ms (usually a no-op) |
| `buildContext()` | <1s |
| Triage (Haiku, low effort) | 1–3s |
| Solution (Sonnet, high effort) | 10–40s |
| Post comment | <1s |
| **Total p50** | **<30s** |

---

## 3. Cost engineering (priority #2)

Current pricing (verified 2026-08-19):

| Model | ID | Input $/MTok | Output $/MTok | Context |
|---|---|---|---|---|
| Claude Opus 5 | `claude-opus-5` | $5.00 | $25.00 | 1M |
| Claude Sonnet 5 | `claude-sonnet-5` | $3.00 (**$2.00 intro thru 2026-08-31**) | $15.00 ($10.00 intro) | 1M |
| Claude Haiku 4.5 | `claude-haiku-4-5` | $1.00 | $5.00 | 200K |

### 3.1 Three-stage model routing

Most issues never need the expensive model.

```
Stage 0 — free filter (no LLM)
  ├─ label gate: only act on `ai-triage` or configured labels
  ├─ author gate: OWNER / MEMBER / COLLABORATOR only (see §4)
  └─ near-duplicate check via pgvector → reply from cached prior analysis
                                                          ↓ ~$0.00

Stage 1 — TRIAGE  ·  claude-haiku-4-5  ·  effort: low
  Classify {bug | feature | question | invalid} and extract
  seed symbols / error strings / stack frames from the issue text.
  Structured output, ~2k in / ~300 out  ≈  $0.0035/issue
  → not a bug? post triage comment and STOP.
                                                          ↓ ~40% continue

Stage 2 — SOLVE  ·  claude-sonnet-5  ·  effort: high
  Agent loop over graph tools + cached repo preamble.
  ~25k in (mostly cache reads) / ~2k out  ≈  $0.02–0.05/issue

Stage 3 — ESCALATE  ·  claude-opus-5  ·  effort: xhigh
  Only when Stage 2 self-reports low confidence, or the issue
  carries an `ai-deep` label. Expect <10% of issues.
```

**Blended estimate: ~$0.02–0.04 per issue.** Track the real number in the cost ledger from day one and publish it in the README — nobody else does, and it is the most persuasive number this project can show.

### 3.2 Prompt caching — the biggest single lever

Cache economics: **write costs 1.25× (5-min TTL) or 2× (1-hour TTL); read costs 0.1×.** Break-even is 2 requests at 5-min TTL, 3 requests at 1-hour TTL.

Prompt render order is `tools` → `system` → `messages`. Structure the request so the stable part comes first:

```ts
system: [
  { type: 'text', text: AGENT_INSTRUCTIONS },              // frozen, identical for all repos
  { type: 'text', text: repoPreamble,                       // arch summary, conventions, file tree
    cache_control: { type: 'ephemeral', ttl: '1h' } },      // ← breakpoint here
],
messages: [
  { role: 'user', content: issueSpecificContext },          // varies — AFTER the breakpoint
]
```

**Two hard rules:**

1. **Minimum cacheable prefix is 1024 tokens on Sonnet 5** (512 on Opus 5). A preamble below that silently will not cache — no error, just `cache_creation_input_tokens: 0`. Pad the repo preamble with genuinely useful content (architecture summary, top-50 modules by centrality, conventions) until it clears 1024.

2. **Never interpolate anything volatile before the breakpoint.** No timestamps, no issue IDs, no `new Date()`. One byte of drift invalidates the whole prefix. Assert this in a test.

**TTL choice:** 1-hour TTL for repos with steady issue traffic (≥3 issues/hour); 5-minute default otherwise. Make it a per-repo setting derived from observed traffic.

**Verify it works:** log `usage.cache_read_input_tokens` on every call. If it is 0 across repeated issues on the same repo, a silent invalidator is present. Add a dashboard panel for cache hit rate — a regression here doubles cost silently.

### 3.3 Hard context budget

`buildContext(text, { maxNodes })` gives a bound in nodes. Convert to a token bound:

```ts
let ctx = graph.build_context(issue_text, Budget { max_nodes: 25, max_tokens: repo.token_ceiling })?;
const { input_tokens } = await anthropic.messages.countTokens({ model, messages, system });
if (input_tokens > repo.tokenCeiling) {
  // re-build with maxNodes reduced, don't truncate blindly
}
```

Use `messages.countTokens` — **never `tiktoken`**, which is OpenAI's tokenizer and undercounts Claude by 15–20% on prose and far more on code.

### 3.4 Issue deduplication

Embed issue title+body, store in pgvector. On a new issue, cosine-search prior analyzed issues in the same repo. Above ~0.92 similarity, post the prior analysis with an explicit "this looks like a duplicate of #N" framing instead of running the agent. Costs one embedding call.

### 3.5 Batch API for non-urgent work

The Batch API is **50% off** with async completion (usually <1h, max 24h). Use it for:

- initial backfill when a repo is first connected (analyze all open issues)
- nightly re-analysis of stale open issues
- benchmark/eval runs

Never for live webhooks — latency is unbounded.

### 3.6 Cost ledger as a product feature

```sql
CREATE TABLE cost_ledger (
  id              bigserial PRIMARY KEY,
  run_id          uuid NOT NULL REFERENCES agent_runs(id),
  repo_id         uuid NOT NULL,
  stage           text NOT NULL,          -- triage | solve | escalate | embed
  model           text NOT NULL,
  input_tokens            int NOT NULL,
  output_tokens           int NOT NULL,
  cache_read_input_tokens int NOT NULL DEFAULT 0,
  cache_creation_input_tokens int NOT NULL DEFAULT 0,
  cost_usd        numeric(10,6) NOT NULL,
  created_at      timestamptz NOT NULL DEFAULT now()
);
```

Expose per-repo and per-issue cost in the UI, and post the cost in the issue comment footer:

> *Analyzed with 18 graph nodes · 24,102 tokens (21,340 cached) · $0.031*

This "context receipt" is transparency, a differentiator, and a great screenshot.

---

## 4. Security

This is where the project can embarrass itself publicly. Non-negotiables.

### 4.1 GitHub App, not OAuth App

Fine-grained per-repo permissions, installation tokens (1h expiry, auto-rotated), org-level revocation, no user-token blast radius.

Minimum permissions: `Issues: Read & Write`, `Contents: Read`, `Metadata: Read`. Add `Pull requests: Write` **only** if fix mode is enabled.

### 4.2 Webhook verification

```ts
import { timingSafeEqual, createHmac } from 'node:crypto';

function verify(rawBody: Buffer, signature: string, secret: string): boolean {
  const expected = 'sha256=' + createHmac('sha256', secret).update(rawBody).digest('hex');
  const a = Buffer.from(expected), b = Buffer.from(signature);
  return a.length === b.length && timingSafeEqual(a, b);
}
```

Fastify must be configured to retain the **raw body** — re-serialized JSON breaks the MAC. Also: reject deliveries older than 5 minutes, and store `X-GitHub-Delivery` for idempotency (GitHub retries).

### 4.3 Prompt injection — the real threat

Anyone can open an issue on a public repo. That text goes into the agent's prompt. This class of attack has been exploited against GitHub-integrated agents in the wild.

| Control | Implementation |
|---|---|
| **Trust boundary** | Issue body is **data**, never instruction. Wrap in `<untrusted_issue_content>` tags with an explicit system-prompt statement that its contents are user-submitted and carry no authority. |
| **Author gate** | Only act on issues from `author_association` ∈ {OWNER, MEMBER, COLLABORATOR}. Public-repo drive-by issues are ignored by default. |
| **Label gate** | Require an explicit `ai-triage` label. Opt-in per issue, not blanket. |
| **No secrets in reach** | The agent's tools cannot read `.env`, `*.pem`, `*.key`, `secrets/`. Enforce in the tool implementation with a canonical-path allowlist, not a blocklist. |
| **No write tools in analyze mode** | Stage 1–3 have read-only tools. Full stop. |
| **Egress allowlist** | Worker container reaches only `api.anthropic.com` + the git host. Nothing else. Enforce at the Docker network / firewall layer, not in code. |
| **Output scan** | Before posting, scan the comment for secret-shaped strings (`sk-`, `ghp_`, private key headers, high-entropy blobs) and block. Defense in depth. |
| **Never auto-fix on public repos** | Default off. Requires explicit per-repo opt-in and cannot be enabled for repos with public issue creation. |

### 4.4 API key storage

Envelope encryption. Per-tenant DEK, wrapped by a KEK from `IG_MASTER_KEY` env (or a KMS in Phase 5). AES-256-GCM. Decrypt only in the worker, only into memory, never logged. Redact keys from all log lines and error traces.

### 4.4a Access control

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

### 4.4b Egress — where a repository URL can point

A repository URL is the only user-supplied value in this product that causes an
outbound connection to a host of the caller's choosing. `https://` alone is
satisfied by `https://169.254.169.254/latest/meta-data/iam/security-credentials/`
— the cloud metadata endpoint, which answers with the container's credentials to
anything that can reach it — and by every private address on whatever network
the server can see. That is server-side request forgery, and it is enforced here
rather than assumed away.

`check_url` now resolves the host and refuses **every** answer that is not
public: loopback, private, link-local, unique-local, CGNAT, multicast,
unspecified, and IPv4-mapped forms of all of them. Every address is checked, not
the first — a name returning one public and one private answer is precisely how
this gets bypassed.

`LEANGRAPH_ALLOWED_HOSTS` pins cloning to named hosts. An entry covers the host and
its subdomains (`github.com` allows `codeload.github.com`) and nothing that
merely looks like it (`evil-github.com` is refused). Setting it is also how you
deliberately permit an internal host — a GitHub Enterprise install — since an
explicit allowlist is a statement about where this install may reach and
overrides the address check rather than stacking with it.

**What this is not.** The check resolves now; git resolves again when it
connects, so a name whose answer changes in between would slip past. Closing
that needs a resolver the connection itself is pinned to, which git does not
offer. `LEANGRAPH_ALLOWED_HOSTS` is the airtight in-process control, and a network
policy on the container is the real one — the compose file says where to put it.

### 4.5 Fix mode — **built, opt-in** (`src/server/fix.rs`)

Posting a comment and changing someone's repository are different decisions, so
they are gated differently. Fix mode is off in three independent ways, and all
three must be open before a single line is written:

1. the repository has `fix_mode` set — `POST /repos/{owner/name}/config`;
2. the issue carries `leangraph-fix`, a **second** label distinct from the `leangraph`
   one that triggers analysis;
3. a token with write access exists.

Any one missing and the analysis still posts, but nothing is pushed. The refusal
is logged with the reason so an operator is never left guessing why.

What it does:

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
enforced in code before `git apply` runs, not merely requested in the prompt;
the prompt states it too, so that enforcement has to reject less often.

A patch is also refused for touching more than 12 files, exceeding 60 KB,
naming no files, or reaching outside the worktree.

The context the patch is written from is the graph selection — the same one the
analysis used, and the reason a fix can be attempted at all without reading the
repository.

**Not yet built:** running the affected tests before opening the PR. The graph
knows which tests import the changed files, so the selection is cheap; executing
untrusted code in the server's container is the part that needs its own sandbox
first. Until then the PR body says plainly that nothing was run.

---

## 5. Data model (Postgres)

```sql
tenants        (id, name, created_at)
users          (id, tenant_id, github_id, email)
llm_keys       (id, tenant_id, provider, ciphertext, nonce, key_hint, created_at)

repos (
  id uuid PK, tenant_id, provider,           -- github | gitlab
  full_name, installation_id, default_branch,
  state,                                     -- pending|cloning|indexing|ready|error
  last_indexed_sha, last_indexed_at,
  node_count, edge_count, file_count, index_duration_ms,
  codegraph_version,                         -- forces reindex on upgrade
  config jsonb                               -- labels, model routing, token ceiling, fix_mode
)

issues (
  id uuid PK, repo_id, provider_number, title, body,
  author_association, embedding vector(1024), created_at
)

agent_runs (
  id uuid PK, issue_id, repo_id,
  stage, status,                             -- queued|running|posted|failed|skipped
  context_node_ids jsonb, context_files jsonb,
  total_tokens int, total_cost_usd numeric(10,6),
  duration_ms int, error text, created_at
)

cost_ledger (…)                              -- see §3.6
webhook_deliveries (delivery_id PK, received_at)   -- idempotency
```

`repos.codegraph_version` is load-bearing: when the pinned CodeGraph version bumps, mark every repo stale and reindex on a rolling basis rather than serving graphs built by a different extractor.

---

## 6. Phases

### Phase 0 — Validation (2–3 days) · **DO THIS FIRST**

Do not write the server until these numbers exist.

1. Install CodeGraph. Index 5 repos of varying size/language on the target container spec. Record: cold index time, incremental sync time, DB size on disk, peak RSS.
2. Build an eval set: 40 closed issues with linked merged PRs → ground truth = files changed in the PR.
3. Measure `recall@10` for:
   - baseline A: embedding RAG over file chunks
   - baseline B: BM25 over the repo
   - **CodeGraph `buildContext()`**
4. Measure tokens for each at equal recall.

**Kill criterion:** if `buildContext` does not beat both baselines on recall-per-token, the whole premise is wrong and this is the cheapest possible moment to learn it.

**Deliverable:** `benchmarks/README.md` with a table. This becomes the project's headline claim.

### Phase 1 — Graph service (1.5–2 weeks)

- `GraphProvider` interface + CodeGraph adapter (pinned version)
- Repo lifecycle: register → clone --mirror → worktree → index → ready
- BullMQ queues with per-repo Redis mutex; GraphPool LRU
- REST: `POST /repos`, `GET /repos/:id`, `POST /repos/:id/sync`, `POST /repos/:id/context`
- `docker compose up` works; named volume; WAL preflight check
- **Exit criterion:** register a repo via API, get context for arbitrary text in <1s

### Phase 2 — GitHub App + agent (2 weeks)

- GitHub App registration, installation flow, HMAC + idempotency
- Encrypted BYO key storage
- Three-stage routing with prompt caching + cost ledger
- Read-only tool surface: `graph_explore`, `graph_node`, `graph_callers`, `graph_callees`, `graph_impact`, `read_file`, `grep`
- Post comment with context receipt
- All security controls from §4 — **not deferred to a later phase**
- **Exit criterion:** label an issue `ai-triage` → useful comment in <30s for <$0.05

### Phase 3 — Operability (1 week)

- Minimal web UI: repo list, index status, run history, **cost dashboard with cache hit rate**
- Prometheus metrics; structured logs with key redaction
- Backfill via Batch API
- README with the Phase 0 benchmark table

### Phase 4 — Fix mode (2 weeks, opt-in, gated)

Sandbox, `codegraph affected` → targeted test run, branch + PR. Never main.

### Phase 5 — GitLab (1 week)

Same worker, different provider adapter. Webhook token verification instead of HMAC.

**Total to a genuinely useful v1: ~6 weeks.** Phases 1–3.

---

## 7. Risks

| Risk | Severity | Mitigation |
|---|---|---|
| CodeGraph API churn at 67k stars / daily merges | High | Pin exact version; `GraphProvider` adapter; SQLite direct-query fallback |
| Phase 0 shows graph context ≈ baseline RAG | **Critical** | Find out in 3 days, not 3 months. Kill criterion is explicit. |
| WAL corruption on bind-mounted volumes | High | Named volume + boot-time preflight check that refuses to start |
| Prompt injection incident | High | §4.3 in full, from Phase 2. Not deferred. |
| Cold-index latency on monorepos | Medium | Index at registration; queue issues during indexing with a status comment |
| Cache silently stops hitting → 10× cost | Medium | Log `cache_read_input_tokens` every call; alert on hit rate <50% |
| Upstream adds a server mode and eats the niche | Medium | Real possibility. A PR contributing our server layer upstream is a legitimate alternative outcome — and better for visibility than a competing repo. |
| Sonnet 5 intro pricing ends 2026-08-31 | Low | Cost model must read prices from config, not hardcode them |

---

## 8. Open decisions

1. **Name.** `issuegraph`, `triagent`, `graphtriage`, `cortex`?
2. **License.** MIT maximizes adoption; AGPL prevents a SaaS clone. CodeGraph being MIT means either is legally available.
3. **Also ship an MCP server?** Low marginal cost (the graph layer already exists) but CodeGraph already occupies that slot well. Probably skip — stay focused on the server gap.
4. **Multi-tenancy depth.** Single-org self-hosted is far simpler. Full multi-tenant is a v2 concern.

---

## 9. First commit

```bash
mkdir issuegraph && cd issuegraph && git init
npm init -y && npm pkg set engines.node=">=22.5.0"
npm i -E @colbymchenry/codegraph@1.5.0
npm i fastify bullmq ioredis pg simple-git @anthropic-ai/sdk zod pino
npm i -D typescript tsx vitest @types/node
```

Then Phase 0 — the benchmark, before anything else.
