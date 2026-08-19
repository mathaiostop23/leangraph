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

use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;
use std::sync::{Arc, Mutex};

/// Schema version. Bump alongside a step in `migrate`.
const SCHEMA: u32 = 2;

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

    pub fn memory() -> Result<Db> {
        let conn = Connection::open_in_memory()?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        let db = Db(Arc::new(Mutex::new(conn)));
        db.migrate()?;
        Ok(db)
    }

    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> Result<T>) -> Result<T> {
        let guard = self.0.lock().map_err(|_| anyhow::anyhow!("db mutex poisoned"))?;
        f(&guard)
    }

    /// Forward-only ladder. Each step runs once, in order, and the version is
    /// bumped after each — so an interrupted upgrade resumes rather than
    /// replaying steps that already applied.
    fn migrate(&self) -> Result<()> {
        self.with(|c| {
            let mut have: u32 = c.pragma_query_value(None, "user_version", |r| r.get(0))?;
            if have == 0 {
                c.execute_batch(DDL)?;
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
            Ok(())
        })
    }
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
}

fn repo_from_row(r: &rusqlite::Row) -> rusqlite::Result<Repo> {
    Ok(Repo {
        id: r.get("id")?,
        full_name: r.get("full_name")?,
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
    })
}

impl Db {
    pub fn upsert_repo(
        &self,
        full_name: &str,
        path: &str,
        branch: &str,
        url: Option<&str>,
    ) -> Result<Repo> {
        self.with(|c| {
            c.execute(
                "INSERT INTO repos (full_name, path, default_branch, url, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5)
                 ON CONFLICT(full_name) DO UPDATE SET
                   path = ?2, default_branch = ?3, url = COALESCE(?4, repos.url)",
                params![full_name, path, branch, url, now()],
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
    pub attempts: i64,
}

impl Db {
    /// Enqueue, collapsing against anything already queued with the same key.
    /// Returns false when the job was folded into an existing one.
    pub fn enqueue(&self, kind: &str, repo_id: i64, payload: &str, dedupe: Option<&str>) -> Result<bool> {
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
    pub fn claim(&self) -> Result<Option<Job>> {
        self.with(|c| {
            let job = c
                .query_row(
                    "UPDATE jobs SET state='running', started_at=?1, attempts = attempts + 1
                     WHERE id = (SELECT id FROM jobs WHERE state='queued' ORDER BY id LIMIT 1)
                     RETURNING id, kind, repo_id, payload, attempts",
                    params![now()],
                    |r| {
                        Ok(Job {
                            id: r.get(0)?,
                            kind: r.get(1)?,
                            repo_id: r.get(2)?,
                            payload: r.get(3)?,
                            attempts: r.get(4)?,
                        })
                    },
                )
                .optional()?;
            Ok(job)
        })
    }

    pub fn finish_job(&self, id: i64, error: Option<&str>) -> Result<()> {
        self.with(|c| {
            c.execute(
                "UPDATE jobs SET state = ?2, finished_at = ?3, error = ?4 WHERE id = ?1",
                params![id, if error.is_some() { "failed" } else { "done" }, now(), error],
            )?;
            Ok(())
        })
    }

    pub fn queue_depth(&self) -> Result<(i64, i64)> {
        self.with(|c| {
            let q: i64 = c.query_row("SELECT COUNT(*) FROM jobs WHERE state='queued'", [], |r| r.get(0))?;
            let run: i64 = c.query_row("SELECT COUNT(*) FROM jobs WHERE state='running'", [], |r| r.get(0))?;
            Ok((q, run))
        })
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
