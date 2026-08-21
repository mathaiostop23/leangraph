//! Storage.
//!
//! SQLite, bundled into the binary. An earlier draft of SERVER.md specified
//! Postgres, but that was written when the server was going to be Node and
//! needed an external queue anyway. With a single Rust process the only thing
//! Postgres buys is concurrent writers, which one tokio runtime does not have —
//! and it costs the whole "one binary, no containers" story. Postgres remains a
//! swap for anyone who outgrows this; nothing here depends on SQLite semantics
//! beyond `SKIP LOCKED` having a single-writer equivalent.
//!
//! All access goes through `spawn_blocking`: rusqlite is synchronous, and
//! pretending otherwise inside an async runtime is how you stall a reactor.

use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Schema version. Bump alongside a step in `migrate`.
const SCHEMA: u32 = 6;

#[derive(Clone)]
pub struct Db(Arc<Mutex<Connection>>);

impl Db {
    pub fn open(path: &Path) -> Result<Db> {
        if let Some(d) = path.parent() {
            std::fs::create_dir_all(d).ok();
        }
        let conn = Connection::open(path).with_context(|| format!("open {}", path.display()))?;
        // WAL so a long read cannot block the writer; NORMAL because a lost
        // transaction on power failure costs us a re-run of an idempotent job,
        // not data we cannot reconstruct.
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        let db = Db(Arc::new(Mutex::new(conn)));
        db.migrate()?;
        Ok(db)
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self
            .0
            .lock()
            .map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        f(&guard)
    }

    /// Forward-only ladder. Each step runs once, in order, and the version is
    /// bumped after each — so an interrupted upgrade resumes rather than
    /// replaying steps that already applied.
    fn migrate(&self) -> Result<()> {
        self.with(|c| {
            let mut have: u32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            // Every statement in DDL is `IF NOT EXISTS`, so running it on each
            // open is free and it is the only thing that creates a *table*
            // added after schema 1. Running it only on an empty database left
            // those missing forever on an existing one, and the ALTER below
            // would then fail against a table that was never created.
            c.execute_batch(DDL)?;
            if have == 0 {
                have = 1;
                c.pragma_update(None, "user_version", have)?;
            }
            if have < 2 {
                // Remote origin, so a repository can be cloned rather than
                // having to already exist on disk.
                c.execute_batch(
                    "ALTER TABLE repos ADD COLUMN url TEXT;
                     ALTER TABLE repos ADD COLUMN private INTEGER NOT NULL DEFAULT 0;",
                )?;
                have = 2;
                c.pragma_update(None, "user_version", have)?;
            }
            if have < 3 {
                // `config_json` reached DDL without a migration step, so a
                // database created before fix mode has the row but not the
                // column and every read of it fails. Adding it here repairs
                // those; `add_column` is a no-op where DDL already supplied it.
                add_column(c, "repos", "config_json", "TEXT NOT NULL DEFAULT '{}'")?;
                add_column(c, "issues", "fingerprint", "TEXT NOT NULL DEFAULT ''")?;
                have = 3;
                c.pragma_update(None, "user_version", have)?;
            }
            if have < 4 {
                add_column(c, "jobs", "run_after", "INTEGER NOT NULL DEFAULT 0")?;
                have = 4;
                c.pragma_update(None, "user_version", have)?;
            }
            if have < 5 {
                add_column(c, "jobs", "defers", "INTEGER NOT NULL DEFAULT 0")?;
                have = 5;
                c.pragma_update(None, "user_version", have)?;
            }
            if have < 6 {
                add_column(c, "jobs", "lease_until", "INTEGER NOT NULL DEFAULT 0")?;
                add_column(c, "jobs", "owner", "TEXT")?;
                have = 6;
                c.pragma_update(None, "user_version", have)?;
            }
            // The constant is the ladder's top step. Asserting it here is what
            // makes it a guard rather than a comment: add a migration without
            // bumping it, or bump it without adding one, and this fires on the
            // next open instead of on someone's data months later.
            debug_assert_eq!(
                have, SCHEMA,
                "migration ladder stops at {have} but SCHEMA says {SCHEMA}"
            );
            if have > SCHEMA {
                bail!("database is at schema {have}; this build only knows {SCHEMA}");
            }
            Ok(())
        })
    }
}

/// Refuse to start on storage that cannot hold this database.
///
/// A bind mount from a macOS or Windows host goes through a translation layer
/// with unreliable file locking and mmap semantics, and both the app database
/// and the graphs depend on those. The failure without this check is not a
/// refusal to boot — it is a server that runs, indexes, and corrupts a WAL
/// hours later under load, which is a support ticket nobody can diagnose from
/// the outside. Writing and reading back one row costs milliseconds once.
pub fn preflight(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir).with_context(|| format!("creating {}", dir.display()))?;
    let probe = dir.join(".preflight.db");
    let _ = std::fs::remove_file(&probe);

    let check = || -> Result<i64> {
        let c = Connection::open(&probe)?;
        let mode: String = c.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            bail!("this filesystem refused write-ahead logging (got journal_mode={mode})");
        }
        c.execute_batch("CREATE TABLE t (n INTEGER); INSERT INTO t VALUES (42);")?;
        // A second connection, because the failure mode is locking between
        // handles rather than anything one handle can see on its own.
        let d = Connection::open(&probe)?;
        Ok(d.query_row("SELECT n FROM t", [], |r| r.get(0))?)
    };

    let got = check().with_context(|| {
        format!(
            "{} cannot hold the database. On Docker Desktop this is usually a bind mount — \
             use a named volume",
            dir.display()
        )
    })?;
    for suffix in ["", "-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", probe.display()));
    }
    if got != 42 {
        bail!("{} read back {got} where 42 was written", dir.display());
    }
    Ok(())
}

/// `ALTER TABLE ADD COLUMN` errors when the column is already there, and it is
/// already there whenever DDL was applied fresh. Checking first is what lets a
/// migration be written for a column that also exists in DDL — the case that
/// otherwise gets silently skipped and breaks only on upgrade, where nobody is
/// looking.
fn add_column(c: &Connection, table: &str, column: &str, decl: &str) -> Result<()> {
    let present: bool = c
        .prepare(&format!("PRAGMA table_info({table})"))?
        .query_map([], |r| r.get::<_, String>(1))?
        .filter_map(|x| x.ok())
        .any(|name| name == column);
    if !present {
        c.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column} {decl};"))?;
    }
    Ok(())
}

const DDL: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
  id                INTEGER PRIMARY KEY,
  full_name         TEXT    NOT NULL UNIQUE,
  provider          TEXT    NOT NULL DEFAULT 'github',
  path              TEXT    NOT NULL,
  default_branch    TEXT    NOT NULL DEFAULT 'main',
  state             TEXT    NOT NULL DEFAULT 'pending',
  last_indexed_sha  TEXT,
  last_indexed_at   INTEGER,
  node_count        INTEGER NOT NULL DEFAULT 0,
  edge_count        INTEGER NOT NULL DEFAULT 0,
  file_count        INTEGER NOT NULL DEFAULT 0,
  index_ms          INTEGER NOT NULL DEFAULT 0,
  error             TEXT,
  config_json       TEXT    NOT NULL DEFAULT '{}',
  created_at        INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS jobs (
  id          INTEGER PRIMARY KEY,
  kind        TEXT    NOT NULL,
  repo_id     INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  payload     TEXT    NOT NULL DEFAULT '{}',
  state       TEXT    NOT NULL DEFAULT 'queued',
  dedupe_key  TEXT,
  attempts    INTEGER NOT NULL DEFAULT 0,
  -- Not before this time. A retry that comes straight back does not wait out
  -- whatever it was that failed.
  run_after   INTEGER NOT NULL DEFAULT 0,
  -- Times this job stood aside for something it was waiting on, as distinct
  -- from times it was tried and failed. Waiting for an index to finish is not
  -- an attempt, and counting it as one would exhaust the retry budget before
  -- the work could start.
  defers      INTEGER NOT NULL DEFAULT 0,
  -- Who holds this job and until when. A `running` row on its own cannot tell
  -- a crashed worker from a busy one, which is fine for a single process and
  -- wrong for two: the second one's startup would requeue the first one's
  -- work out from under it. A lease expires; a state does not.
  lease_until INTEGER NOT NULL DEFAULT 0,
  owner       TEXT,
  error       TEXT,
  created_at  INTEGER NOT NULL,
  started_at  INTEGER,
  finished_at INTEGER
);
-- A burst of pushes should collapse into one sync, not ten. Uniqueness only
-- among queued rows, so the same key can be enqueued again once it has run.
CREATE UNIQUE INDEX IF NOT EXISTS jobs_dedupe
  ON jobs(dedupe_key) WHERE state = 'queued' AND dedupe_key IS NOT NULL;
CREATE INDEX IF NOT EXISTS jobs_pending ON jobs(state, id);

-- Webhook idempotency: providers retry, and a retried issue must not produce a
-- second comment.
CREATE TABLE IF NOT EXISTS deliveries (
  delivery_id TEXT    PRIMARY KEY,
  received_at INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS issues (
  id                 INTEGER PRIMARY KEY,
  repo_id            INTEGER NOT NULL REFERENCES repos(id) ON DELETE CASCADE,
  number             INTEGER NOT NULL,
  title              TEXT    NOT NULL,
  body               TEXT    NOT NULL,
  author_association TEXT    NOT NULL,
  created_at         INTEGER NOT NULL,
  -- Shingles and graph seeds, filled in when the issue is answered. Empty for
  -- an issue that was never analysed, which is why the dedup query filters on
  -- it rather than on the run status.
  fingerprint        TEXT    NOT NULL DEFAULT '',
  UNIQUE(repo_id, number)
);

CREATE TABLE IF NOT EXISTS runs (
  id            INTEGER PRIMARY KEY,
  issue_id      INTEGER NOT NULL REFERENCES issues(id) ON DELETE CASCADE,
  repo_id       INTEGER NOT NULL,
  status        TEXT    NOT NULL,
  outcome       TEXT,
  context_files TEXT,
  total_tokens  INTEGER NOT NULL DEFAULT 0,
  cost_usd      REAL    NOT NULL DEFAULT 0,
  duration_ms   INTEGER NOT NULL DEFAULT 0,
  error         TEXT,
  created_at    INTEGER NOT NULL
);

-- Per-call accounting. This is a product feature, not just ops: the comment
-- footer reports what the answer cost, and a user who can see that trusts it.
CREATE TABLE IF NOT EXISTS cost_ledger (
  id                 INTEGER PRIMARY KEY,
  run_id             INTEGER NOT NULL REFERENCES runs(id) ON DELETE CASCADE,
  repo_id            INTEGER NOT NULL,
  stage              TEXT    NOT NULL,
  model              TEXT    NOT NULL,
  input_tokens       INTEGER NOT NULL DEFAULT 0,
  output_tokens      INTEGER NOT NULL DEFAULT 0,
  cache_read_tokens  INTEGER NOT NULL DEFAULT 0,
  cache_write_tokens INTEGER NOT NULL DEFAULT 0,
  cost_usd           REAL    NOT NULL DEFAULT 0,
  created_at         INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS ledger_repo ON cost_ledger(repo_id, created_at);

-- Encrypted at rest with a key from the environment. Never logged, never
-- returned by the API.
CREATE TABLE IF NOT EXISTS secrets (
  name       TEXT    PRIMARY KEY,
  nonce      BLOB    NOT NULL,
  ciphertext BLOB    NOT NULL,
  hint       TEXT,
  created_at INTEGER NOT NULL
);
"#;

pub fn now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

// --------------------------------------------------------------------- repos

#[derive(Debug, Clone, serde::Serialize)]
pub struct Repo {
    pub id: i64,
    pub full_name: String,
    /// `github` or `gitlab`. Decides how a comment is posted and how a webhook
    /// from this repository is authenticated.
    pub provider: String,
    pub path: String,
    pub url: Option<String>,
    pub default_branch: String,
    pub state: String,
    pub last_indexed_sha: Option<String>,
    pub node_count: i64,
    pub edge_count: i64,
    pub file_count: i64,
    pub index_ms: i64,
    pub error: Option<String>,
    pub config_json: String,
}

fn repo_from_row(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get("id")?,
        full_name: r.get("full_name")?,
        provider: r.get("provider")?,
        path: r.get("path")?,
        url: r.get("url")?,
        default_branch: r.get("default_branch")?,
        state: r.get("state")?,
        last_indexed_sha: r.get("last_indexed_sha")?,
        node_count: r.get("node_count")?,
        edge_count: r.get("edge_count")?,
        file_count: r.get("file_count")?,
        index_ms: r.get("index_ms")?,
        error: r.get("error")?,
        config_json: r.get("config_json")?,
    })
}

impl Db {
    pub fn upsert_repo(
        &self,
        full_name: &str,
        path: &str,
        branch: &str,
        url: Option<&str>,
        provider: &str,
    ) -> Result<Repo> {
        self.with(|c| {
            c.execute(
                "INSERT INTO repos (full_name, path, default_branch, url, provider, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(full_name) DO UPDATE SET
                   path = ?2, default_branch = ?3, url = COALESCE(?4, repos.url),
                   provider = ?5",
                params![full_name, path, branch, url, provider, now()],
            )?;
            let repo = c.query_row(
                "SELECT * FROM repos WHERE full_name = ?1",
                params![full_name],
                repo_from_row,
            )?;
            Ok(repo)
        })
    }

    pub fn repo_by_name(&self, full_name: &str) -> Result<Option<Repo>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT * FROM repos WHERE full_name = ?1",
                params![full_name],
                repo_from_row,
            )
            .optional()?)
        })
    }

    pub fn repos(&self) -> Result<Vec<Repo>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT * FROM repos ORDER BY full_name")?;
            let rows = st.query_map([], repo_from_row)?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn set_repo_state(&self, id: i64, state: &str, error: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE repos SET state = ?2, error = ?3 WHERE id = ?1",
                params![id, state, error],
            )?;
            Ok(())
        })
    }

    pub fn record_index(
        &self,
        id: i64,
        sha: Option<&str>,
        files: i64,
        nodes: i64,
        edges: i64,
        ms: i64,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE repos SET state='ready', error=NULL, last_indexed_sha=?2,
                        last_indexed_at=?3, file_count=?4, node_count=?5,
                        edge_count=?6, index_ms=?7
                 WHERE id = ?1",
                params![id, sha, now(), files, nodes, edges, ms],
            )?;
            Ok(())
        })
    }
}

// ---------------------------------------------------------------------- jobs

#[derive(Debug, Clone)]
pub struct Job {
    pub id: i64,
    pub kind: String,
    pub repo_id: i64,
    pub payload: String,
    /// How many times this has been claimed, this one included.
    pub attempts: i64,
    /// How many times it stood aside for something it was waiting on.
    pub defers: i64,
}

impl Db {
    /// Enqueue, collapsing against anything already queued with the same key.
    /// Returns false when the job was folded into an existing one.
    pub fn enqueue(
        &self,
        kind: &str,
        repo_id: i64,
        payload: &str,
        dedupe: Option<&str>,
    ) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO jobs (kind, repo_id, payload, dedupe_key, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)",
                params![kind, repo_id, payload, dedupe, now()],
            )?;
            Ok(n > 0)
        })
    }

    /// Claim the oldest queued job. The update is the claim: two workers cannot
    /// both transition the same row out of `queued`.
    /// Take the oldest ready job of one of these kinds.
    ///
    /// Kinds rather than everything, because the two sorts of work here have
    /// nothing in common: indexing saturates every core for as long as the
    /// repository is large, and answering an issue is a socket waiting on a
    /// model. One pool over one queue puts a three-second answer behind a
    /// twelve-minute index, and no amount of worker count fixes that — the
    /// index workers would just multiply and make each other slower.
    /// Recording who took it, so the lease can be renewed and so one instance
    /// cannot reclaim what another is still working on.
    pub fn claim_as(&self, kinds: &[&str], owner: &str) -> Result<Option<Job>> {
        let list = kinds
            .iter()
            .map(|k| format!("'{k}'"))
            .collect::<Vec<_>>()
            .join(",");
        self.with(|c| {
            let sql = format!(
                "UPDATE jobs SET state='running', started_at=?1, attempts = attempts + 1,
                                lease_until = ?2, owner = ?3
                 WHERE id = (SELECT id FROM jobs
                             WHERE state='queued' AND run_after <= ?1 AND kind IN ({list})
                             ORDER BY id LIMIT 1)
                 RETURNING id, kind, repo_id, payload, attempts, defers"
            );
            let job = c
                .query_row(&sql, params![now(), now() + Self::LEASE_SECS, owner], |r| {
                    Ok(Job {
                        id: r.get(0)?,
                        kind: r.get(1)?,
                        repo_id: r.get(2)?,
                        payload: r.get(3)?,
                        attempts: r.get(4)?,
                        defers: r.get(5)?,
                    })
                })
                .optional()?;
            Ok(job)
        })
    }

    pub const HEAVY_KINDS: [&'static str; 4] = ["clone", "index", "sync", "backfill"];
    /// `batch` sits here rather than with the heavy work: it is a poll against
    /// an HTTP endpoint that spends almost all its time waiting, and putting it
    /// behind an index would leave a finished batch unread for minutes.
    pub const ISSUE_KINDS: [&'static str; 2] = ["issue", "batch"];

    /// How long a job may stand aside waiting before it is given up on.
    ///
    /// A cold index of a large repository is minutes, so this has to outlast
    /// one; past it the wait is not a slow index, it is a stuck one.
    pub const MAX_DEFERS: i64 = 180;

    /// Put a job back without charging it an attempt.
    ///
    /// It was never tried — it is waiting on something else — and spending the
    /// retry budget on waiting would fail it before the work could begin.
    pub fn defer_job(&self, id: i64, secs: i64) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE jobs
                   SET state='queued', run_after=?2, started_at=NULL,
                       attempts = MAX(attempts - 1, 0), defers = defers + 1
                 WHERE id = ?1",
                params![id, now() + secs],
            )?;
            Ok(())
        })
    }

    /// How many times a job is tried before it is given up on.
    ///
    /// Four attempts spread over about ten minutes. A rate limit clears well
    /// inside that; a bad API key never will, which is why only errors the
    /// caller judged transient come back here at all.
    pub const MAX_ATTEMPTS: i64 = 4;

    /// Seconds before the next attempt. Exponential, because whatever failed is
    /// usually busy rather than broken, and hammering it is how a rate limit
    /// becomes a longer one.
    fn backoff(attempts: i64) -> i64 {
        // The right first wait depends on the provider, so it is tunable rather
        // than a constant someone has to fork the binary to change. The tests
        // set it to a second; nothing else should need to.
        let base: i64 = std::env::var("LEANGRAPH_RETRY_BASE_SECS")
            .ok()
            .and_then(|v| v.parse().ok())
            .filter(|v| *v > 0)
            .unwrap_or(30);
        base * 4i64.saturating_pow(attempts.clamp(1, 4) as u32 - 1)
    }

    /// Mark a job done, failed, or due for another attempt.
    ///
    /// `retry` is true only when the caller judged the failure transient.
    /// Everything else lands as `failed`: a permanent error tried four times is
    /// four times the cost and the same answer.
    pub fn finish_job(&self, id: i64, error: Option<&str>, retry: bool) -> Result<()> {
        self.with(|c| {
            if retry {
                let attempts: i64 = c.query_row(
                    "SELECT attempts FROM jobs WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )?;
                if attempts < Self::MAX_ATTEMPTS {
                    let due = now() + Self::backoff(attempts);
                    c.execute(
                        "UPDATE jobs SET state='queued', run_after=?2, error=?3 WHERE id=?1",
                        params![id, due, error],
                    )?;
                    return Ok(());
                }
            }
            c.execute(
                "UPDATE jobs SET state = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                params![
                    id,
                    if error.is_some() { "failed" } else { "done" },
                    now(),
                    error
                ],
            )?;
            Ok(())
        })
    }

    /// Put back anything a stopped process left mid-flight.
    ///
    /// One process owns this database, so a job still marked `running` at
    /// startup was interrupted — a deploy, an OOM, a Ctrl-C. Nothing picked
    /// those up again: not queued, so never claimed; not failed, so never
    /// reported. They sat in the dashboard as permanently running while the
    /// work they stood for was quietly lost.
    /// How long a claim is good for without being renewed.
    ///
    /// Long enough that a worker doing real work keeps it comfortably — the
    /// heartbeat renews at a third of this — and short enough that a killed
    /// process does not strand its job for the rest of the afternoon.
    pub const LEASE_SECS: i64 = 60;

    /// Requeue anything whose holder stopped renewing it.
    ///
    /// This replaces requeuing every `running` row at startup, which was right
    /// for one process and actively harmful for two: the second instance would
    /// take the first one's in-flight work and both would do it. An expired
    /// lease is evidence the holder is gone; a `running` state is not.
    pub fn reclaim_expired(&self) -> Result<usize> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE jobs SET state='queued', run_after=0, owner=NULL
                 WHERE state='running' AND lease_until < ?1",
                params![now()],
            )?;
            Ok(n)
        })
    }

    /// Say the job is still being worked on.
    ///
    /// Returns false when the lease was lost — the row is no longer ours,
    /// because something reclaimed it while we were slow. The caller should
    /// stop rather than finish a job another worker has already taken.
    pub fn renew_lease(&self, id: i64, owner: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "UPDATE jobs SET lease_until = ?2 WHERE id = ?1 AND owner = ?3
                   AND state = 'running'",
                params![id, now() + Self::LEASE_SECS, owner],
            )?;
            Ok(n > 0)
        })
    }

    /// Issues answered, and what they cost.
    pub fn run_totals(&self) -> Result<(i64, f64)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COUNT(*), COALESCE(SUM(cost_usd), 0) FROM runs",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }

    pub fn queue_depth(&self) -> Result<(i64, i64)> {
        self.with(|c| {
            let q: i64 =
                c.query_row("SELECT COUNT(*) FROM jobs WHERE state='queued'", [], |r| {
                    r.get(0)
                })?;
            let run: i64 =
                c.query_row("SELECT COUNT(*) FROM jobs WHERE state='running'", [], |r| {
                    r.get(0)
                })?;
            Ok((q, run))
        })
    }
}

impl Db {
    pub fn set_repo_config(&self, id: i64, json: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE repos SET config_json = ?2 WHERE id = ?1",
                params![id, json],
            )?;
            Ok(())
        })
    }
}

impl Repo {
    /// Fix mode is off unless the stored config says otherwise. Anything
    /// unparseable is treated as off — a corrupt config must not be a way to
    /// turn on the one feature that writes to someone's repository.
    pub fn fix_mode(&self) -> bool {
        self.config().and_then(|v| v.as_bool()).unwrap_or(false)
    }

    fn config(&self) -> Option<serde_json::Value> {
        serde_json::from_str::<serde_json::Value>(&self.config_json)
            .ok()?
            .get("fix_mode")
            .cloned()
    }

    /// A per-repository setting, or the server-wide default.
    ///
    /// One repository wanting a different trigger label, or a tighter context
    /// budget than the rest, is the ordinary case on a shared install — and
    /// making those flags means the operator restarts everyone to change one.
    pub fn setting<'a>(&self, key: &str, fallback: &'a str) -> std::borrow::Cow<'a, str> {
        serde_json::from_str::<serde_json::Value>(&self.config_json)
            .ok()
            .and_then(|v| v.get(key).and_then(|x| x.as_str().map(String::from)))
            .map_or(
                std::borrow::Cow::Borrowed(fallback),
                std::borrow::Cow::Owned,
            )
    }

    pub fn setting_usize(&self, key: &str, fallback: usize) -> usize {
        serde_json::from_str::<serde_json::Value>(&self.config_json)
            .ok()
            .and_then(|v| v.get(key).and_then(serde_json::Value::as_u64))
            .map_or(fallback, |n| n as usize)
    }
}

// -------------------------------------------------------------------- issues

impl Db {
    /// Upsert by (repo, number). An issue that is edited and re-labelled should
    /// update in place rather than accumulate rows.
    pub fn upsert_issue(
        &self,
        repo_id: i64,
        number: i64,
        title: &str,
        body: &str,
        assoc: &str,
    ) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO issues (repo_id, number, title, body, author_association, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)
                 ON CONFLICT(repo_id, number) DO UPDATE SET
                   title = ?3, body = ?4, author_association = ?5",
                params![repo_id, number, title, body, assoc, now()],
            )?;
            let id: i64 = c.query_row(
                "SELECT id FROM issues WHERE repo_id = ?1 AND number = ?2",
                params![repo_id, number],
                |r| r.get(0),
            )?;
            Ok(id)
        })
    }
}

#[derive(Debug, Clone)]
pub struct Issue {
    pub id: i64,
    pub repo_id: i64,
    pub number: i64,
    pub title: String,
    pub body: String,
}

impl Db {
    /// Fingerprints of issues already analysed for this repository, newest
    /// first, excluding the one being answered. Only issues that actually got
    /// an answer are compared — matching against something the bot never looked
    /// at would point the reporter at a thread with nothing in it.
    pub fn prior_fingerprints(
        &self,
        repo_id: i64,
        except_issue: i64,
        limit: usize,
    ) -> Result<Vec<(i64, String)>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT number, fingerprint FROM issues
                 WHERE repo_id = ?1 AND id != ?2 AND fingerprint != ''
                 ORDER BY id DESC LIMIT ?3",
            )?;
            let rows = st
                .query_map(params![repo_id, except_issue, limit as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .filter_map(std::result::Result::ok)
                .collect();
            Ok(rows)
        })
    }

    pub fn set_fingerprint(&self, issue_id: i64, fp: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE issues SET fingerprint = ?2 WHERE id = ?1",
                params![issue_id, fp],
            )?;
            Ok(())
        })
    }

    pub fn issue(&self, id: i64) -> Result<Option<Issue>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT id, repo_id, number, title, body FROM issues WHERE id = ?1",
                params![id],
                |r| {
                    Ok(Issue {
                        id: r.get(0)?,
                        repo_id: r.get(1)?,
                        number: r.get(2)?,
                        title: r.get(3)?,
                        body: r.get(4)?,
                    })
                },
            )
            .optional()?)
        })
    }

    pub fn start_run(&self, issue_id: i64, repo_id: i64) -> Result<i64> {
        self.with(|c| {
            c.execute(
                "INSERT INTO runs (issue_id, repo_id, status, created_at) VALUES (?1,?2,'running',?3)",
                params![issue_id, repo_id, now()],
            )?;
            Ok(c.last_insert_rowid())
        })
    }

    /// Has this issue already been answered?
    ///
    /// Backfill is re-runnable — an operator will run it twice — and an issue
    /// whose comment is already on the thread must not be billed again.
    pub fn has_run(&self, issue_id: i64) -> Result<bool> {
        self.with(|c| {
            let n: i64 = c.query_row(
                "SELECT COUNT(*) FROM runs WHERE issue_id = ?1 AND status IN ('posted','dry-run')",
                params![issue_id],
                |r| r.get(0),
            )?;
            Ok(n > 0)
        })
    }

    pub fn finish_run(
        &self,
        id: i64,
        status: &str,
        outcome: Option<&str>,
        files: &str,
        tokens: i64,
        cost: f64,
        ms: i64,
        error: Option<&str>,
    ) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE runs SET status=?2, outcome=?3, context_files=?4, total_tokens=?5,
                        cost_usd=?6, duration_ms=?7, error=?8 WHERE id=?1",
                params![id, status, outcome, files, tokens, cost, ms, error],
            )?;
            Ok(())
        })
    }

    /// Rolling spend, for the dashboard and for a future per-repo ceiling.
    pub fn spend(&self, repo_id: i64, since: i64) -> Result<(i64, f64)> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(input_tokens+output_tokens+cache_read_tokens),0),
                        COALESCE(SUM(cost_usd),0)
                 FROM cost_ledger WHERE repo_id=?1 AND created_at>=?2",
                params![repo_id, since],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )?)
        })
    }
}

/// One answered issue, joined with enough of its repository and issue to be
/// readable without three more queries.
#[derive(Debug, Clone)]
pub struct RunRow {
    pub repo: String,
    pub number: i64,
    pub status: String,
    pub outcome: Option<String>,
    pub total_tokens: i64,
    pub cost_usd: f64,
    pub duration_ms: i64,
    pub created_at: i64,
}

impl Db {
    pub fn recent_runs(&self, limit: i64) -> Result<Vec<RunRow>> {
        self.with(|c| {
            let mut st = c.prepare(
                "SELECT p.full_name, i.number, r.status, r.outcome, r.total_tokens,
                        r.cost_usd, r.duration_ms, r.created_at
                 FROM runs r
                 JOIN issues i ON i.id = r.issue_id
                 JOIN repos  p ON p.id = r.repo_id
                 ORDER BY r.id DESC LIMIT ?1",
            )?;
            let rows = st.query_map(params![limit], |r| {
                Ok(RunRow {
                    repo: r.get(0)?,
                    number: r.get(1)?,
                    status: r.get(2)?,
                    outcome: r.get(3)?,
                    total_tokens: r.get(4)?,
                    cost_usd: r.get(5)?,
                    duration_ms: r.get(6)?,
                    created_at: r.get(7)?,
                })
            })?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }
}

// -------------------------------------------------------------------- secrets

impl Db {
    pub fn put_secret(&self, name: &str, nonce: &[u8], ct: &[u8], hint: &str) -> Result<()> {
        self.with(|c| {
            c.execute(
                "INSERT INTO secrets (name, nonce, ciphertext, hint, created_at)
                 VALUES (?1,?2,?3,?4,?5)
                 ON CONFLICT(name) DO UPDATE SET
                   nonce = ?2, ciphertext = ?3, hint = ?4, created_at = ?5",
                params![name, nonce, ct, hint, now()],
            )?;
            Ok(())
        })
    }

    pub fn get_secret(&self, name: &str) -> Result<Option<(Vec<u8>, Vec<u8>)>> {
        self.with(|c| {
            Ok(c.query_row(
                "SELECT nonce, ciphertext FROM secrets WHERE name = ?1",
                params![name],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?)
        })
    }

    /// Names and hints only. The API must never be able to return a secret it
    /// was given, or storing it encrypted buys nothing.
    pub fn secret_hints(&self) -> Result<Vec<(String, String)>> {
        self.with(|c| {
            let mut st = c.prepare("SELECT name, COALESCE(hint,'') FROM secrets ORDER BY name")?;
            let rows = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?;
            Ok(rows.collect::<rusqlite::Result<Vec<_>>>()?)
        })
    }

    pub fn delete_secret(&self, name: &str) -> Result<bool> {
        self.with(|c| Ok(c.execute("DELETE FROM secrets WHERE name = ?1", params![name])? > 0))
    }
}

// ---------------------------------------------------------------- deliveries

impl Db {
    /// True the first time a delivery id is seen. Providers retry, and a retried
    /// issue event must not produce a second comment.
    pub fn claim_delivery(&self, id: &str) -> Result<bool> {
        self.with(|c| {
            let n = c.execute(
                "INSERT OR IGNORE INTO deliveries (delivery_id, received_at) VALUES (?1, ?2)",
                params![id, now()],
            )?;
            Ok(n > 0)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    /// A database of its own, with one repository to hang jobs on.
    fn fresh(tag: &str) -> Db {
        static N: AtomicU32 = AtomicU32::new(0);
        let path = std::env::temp_dir().join(format!(
            "leangraph-{tag}-{}-{}.db",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        for suffix in ["", "-wal", "-shm"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
        let db = Db::open(&path).expect("open");
        db.with(|c| {
            c.execute(
                "INSERT INTO repos (id, full_name, path, created_at) VALUES (1, 'a/b', '/tmp', 0)",
                [],
            )?;
            Ok(())
        })
        .expect("seed repo");
        db
    }

    fn sample_repo() -> Repo {
        Repo {
            id: 1,
            full_name: "a/b".into(),
            provider: "github".into(),
            path: "/tmp".into(),
            url: None,
            default_branch: "main".into(),
            state: "ready".into(),
            last_indexed_sha: None,
            node_count: 0,
            edge_count: 0,
            file_count: 0,
            index_ms: 0,
            error: None,
            config_json: "{}".into(),
        }
    }

    /// The upgrade path is the one nobody exercises, because development always
    /// starts from an empty database. `config_json` reached DDL without a
    /// migration and broke every read on an existing install; this is the test
    /// that would have caught it.
    #[test]
    fn a_worker_only_claims_the_kinds_it_asked_for() {
        // One pool over one queue puts a three-second answer behind a
        // twelve-minute index, and adding index workers only makes each index
        // slower. The separation has to happen at the claim.
        let db = fresh("kinds");
        db.enqueue("index", 1, "{}", None).unwrap();
        db.enqueue("issue", 1, "{}", None).unwrap();

        let issue = db
            .claim_as(&Db::ISSUE_KINDS, "test")
            .unwrap()
            .expect("an issue was queued");
        assert_eq!(issue.kind, "issue", "must not have taken the index job");

        let heavy = db
            .claim_as(&Db::HEAVY_KINDS, "test")
            .unwrap()
            .expect("index");
        assert_eq!(heavy.kind, "index");

        assert!(
            db.claim_as(&Db::ISSUE_KINDS, "test").unwrap().is_none(),
            "and neither pool sees the other's work twice"
        );
    }

    #[test]
    fn waiting_does_not_spend_an_attempt() {
        // An issue that arrives during a cold index waits for it. Charging that
        // to the retry budget would fail the issue before the graph it needs
        // even exists.
        let db = fresh("defer");
        db.enqueue("issue", 1, "{}", None).unwrap();
        let job = db.claim_as(&Db::ISSUE_KINDS, "test").unwrap().unwrap();
        assert_eq!(job.attempts, 1);
        assert_eq!(job.defers, 0);

        db.defer_job(job.id, 0).unwrap();
        let again = db
            .claim_as(&Db::ISSUE_KINDS, "test")
            .unwrap()
            .expect("it must come back");
        assert_eq!(again.attempts, 1, "the attempt was given back");
        assert_eq!(again.defers, 1, "and the wait was counted separately");
    }

    #[test]
    fn a_deferred_job_is_not_claimable_before_its_time() {
        let db = fresh("defer-time");
        db.enqueue("issue", 1, "{}", None).unwrap();
        let job = db.claim_as(&Db::ISSUE_KINDS, "test").unwrap().unwrap();
        db.defer_job(job.id, 300).unwrap();
        assert!(
            db.claim_as(&Db::ISSUE_KINDS, "test").unwrap().is_none(),
            "waiting five minutes means five minutes"
        );
    }

    #[test]
    fn a_lease_holds_a_job_against_a_second_instance() {
        // The failure this replaces: a second process starting up requeued
        // every `running` row, which for one instance meant recovering after a
        // crash and for two meant taking the other one's in-flight work.
        let db = fresh("lease");
        db.enqueue("index", 1, "{}", None).unwrap();
        let job = db
            .claim_as(&Db::HEAVY_KINDS, "instance-a:0")
            .unwrap()
            .unwrap();

        assert_eq!(
            db.reclaim_expired().unwrap(),
            0,
            "a live lease must not be reclaimable, however many instances look"
        );
        assert!(
            db.claim_as(&Db::HEAVY_KINDS, "instance-b:0")
                .unwrap()
                .is_none(),
            "and the job is not claimable while it is held"
        );

        assert!(
            db.renew_lease(job.id, "instance-a:0").unwrap(),
            "the holder can renew"
        );
        assert!(
            !db.renew_lease(job.id, "instance-b:0").unwrap(),
            "and nobody else can"
        );
    }

    #[test]
    fn an_expired_lease_is_reclaimed_and_can_be_taken_again() {
        let db = fresh("lease-expiry");
        db.enqueue("index", 1, "{}", None).unwrap();
        let job = db.claim_as(&Db::HEAVY_KINDS, "gone:0").unwrap().unwrap();

        // Stand in for a process that stopped renewing.
        db.with(|c| {
            c.execute(
                "UPDATE jobs SET lease_until = 1 WHERE id = ?1",
                params![job.id],
            )?;
            Ok(())
        })
        .unwrap();

        assert_eq!(db.reclaim_expired().unwrap(), 1);
        let again = db
            .claim_as(&Db::HEAVY_KINDS, "fresh:0")
            .unwrap()
            .expect("it comes back");
        assert_eq!(again.id, job.id);
        assert!(
            !db.renew_lease(job.id, "gone:0").unwrap(),
            "the old holder cannot renew a lease it lost"
        );
    }

    #[test]
    fn preflight_accepts_a_directory_it_can_use() {
        let dir = std::env::temp_dir().join(format!("leangraph-pre-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        preflight(&dir).expect("a normal temp directory must pass");
        // And it must leave nothing behind, or the next boot inherits a probe.
        let left: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .collect();
        assert!(left.is_empty(), "probe files were left: {left:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn preflight_refuses_what_it_cannot_write() {
        // A path that cannot be a directory at all stands in for the storage
        // this exists to catch: the point is that it fails at boot, not later.
        let file = std::env::temp_dir().join(format!("leangraph-pre-file-{}", std::process::id()));
        std::fs::write(&file, b"not a directory").unwrap();
        assert!(preflight(&file.join("under")).is_err());
        let _ = std::fs::remove_file(&file);
    }

    #[test]
    fn a_repo_setting_falls_back_to_the_server_default() {
        let mut r = Repo {
            config_json: "{}".into(),
            ..sample_repo()
        };
        assert_eq!(r.setting("trigger_label", "leangraph"), "leangraph");
        assert_eq!(r.setting_usize("max_nodes", 25), 25);

        r.config_json = r#"{"trigger_label":"triage","max_nodes":80}"#.into();
        assert_eq!(r.setting("trigger_label", "leangraph"), "triage");
        assert_eq!(r.setting_usize("max_nodes", 25), 80);
        // Untouched keys still fall through.
        assert_eq!(r.setting("fix_label", "leangraph-fix"), "leangraph-fix");
    }

    #[test]
    fn migrating_an_old_database_adds_the_missing_columns() {
        let dir = std::env::temp_dir().join(format!("leangraph-db-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old.db");
        let _ = std::fs::remove_file(&path);

        {
            // A database as it looked at schema 2: no `config_json`, no
            // `fingerprint`.
            let c = Connection::open(&path).unwrap();
            c.execute_batch(
                "CREATE TABLE repos (id INTEGER PRIMARY KEY, full_name TEXT NOT NULL UNIQUE,
                   provider TEXT NOT NULL DEFAULT 'github', path TEXT NOT NULL,
                   default_branch TEXT NOT NULL DEFAULT 'main', state TEXT NOT NULL DEFAULT 'pending',
                   last_indexed_sha TEXT, last_indexed_at INTEGER,
                   node_count INTEGER NOT NULL DEFAULT 0, edge_count INTEGER NOT NULL DEFAULT 0,
                   file_count INTEGER NOT NULL DEFAULT 0, index_ms INTEGER NOT NULL DEFAULT 0,
                   error TEXT, created_at INTEGER NOT NULL, url TEXT,
                   private INTEGER NOT NULL DEFAULT 0);
                 CREATE TABLE issues (id INTEGER PRIMARY KEY, repo_id INTEGER NOT NULL,
                   number INTEGER NOT NULL, title TEXT NOT NULL, body TEXT NOT NULL,
                   author_association TEXT NOT NULL, created_at INTEGER NOT NULL,
                   UNIQUE(repo_id, number));",
            )
            .unwrap();
            c.execute(
                "INSERT INTO repos (full_name, path, created_at) VALUES ('a/b', '/tmp', 0)",
                [],
            )
            .unwrap();
            c.pragma_update(None, "user_version", 2u32).unwrap();
        }

        let db = Db::open(&path).expect("an existing database must still open");
        let repos = db
            .repos()
            .expect("reading repos must not fail after upgrade");
        assert_eq!(repos.len(), 1);
        assert_eq!(repos[0].config_json, "{}");
        assert!(
            !repos[0].fix_mode(),
            "fix mode must default to off on upgrade"
        );

        // A table introduced after schema 1 must exist too — `jobs` is only
        // ever created by DDL, so an upgrade that skipped DDL left the queue
        // missing and every enqueue failing.
        db.enqueue("sync", repos[0].id, "{}", None)
            .expect("the job queue must exist after an upgrade");
        assert!(
            db.claim_as(&Db::HEAVY_KINDS, "test")
                .expect("claiming must work")
                .is_some(),
            "an upgraded database must be able to run jobs"
        );

        // Idempotent: opening again re-runs migrate and must not error on a
        // column that is now present.
        drop(db);
        Db::open(&path).expect("second open must be a no-op");

        let _ = std::fs::remove_file(&path);
    }
}
