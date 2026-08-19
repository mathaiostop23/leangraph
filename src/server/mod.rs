//! Self-hosted server: repositories in, graph-backed answers out.
//!
//! One process. The HTTP surface accepts work and returns immediately; a worker
//! task drains the queue. Indexing is CPU-bound and runs on the blocking pool so
//! a 700 ms full index cannot stall the reactor and drop a webhook.

pub mod agent;
pub mod clone;
pub mod db;
pub mod webhook;

use crate::index;
use anyhow::{Context, Result};
use axum::{
    extract::{Path as AxPath, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use db::Db;
use serde::Deserialize;
use serde_json::json;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

pub struct Config {
    pub addr: SocketAddr,
    /// Shared secret for webhook signatures. Absent means the endpoint refuses
    /// everything — failing closed, because an unverified webhook is an open
    /// door to whatever the agent can do.
    pub webhook_secret: Option<String>,
    /// Label an issue must carry before the bot acts.
    pub trigger_label: String,
    /// Where repositories and their graphs live.
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// Worker concurrency. Indexing saturates cores on its own, so more than a
    /// couple of concurrent indexes makes all of them slower.
    pub workers: usize,
}

#[derive(Clone)]
pub struct App {
    pub db: Db,
    pub cfg: Arc<Config>,
}

impl App {
    pub fn webhook_secret(&self) -> Option<&str> {
        self.cfg.webhook_secret.as_deref()
    }

    /// Per-repo override would live in `config_json`; for now one label for the
    /// whole install.
    pub fn trigger_label(&self, _repo: &db::Repo) -> String {
        self.cfg.trigger_label.clone()
    }

    pub fn repo(&self, id: i64) -> anyhow::Result<db::Repo> {
        self.db
            .repos()?
            .into_iter()
            .find(|r| r.id == id)
            .context("repo vanished")
    }
}

pub async fn run(cfg: Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir).ok();
    let db = Db::open(&cfg.db_path)?;
    let app = App {
        db,
        cfg: Arc::new(cfg),
    };

    for i in 0..app.cfg.workers {
        let w = app.clone();
        tokio::spawn(async move { worker(w, i).await });
    }

    let router = Router::new()
        .route("/health", get(health))
        .route("/repos", get(list_repos).post(add_repo))
        .route("/repos/{name}", get(get_repo))
        .route("/repos/{name}/sync", post(sync_repo))
        .route("/webhook/github", post(webhook::github))
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(app.cfg.addr)
        .await
        .with_context(|| format!("bind {}", app.cfg.addr))?;

    println!(
        "\n  \x1b[1marbor server\x1b[0m  http://{}\n  data      {}\n  workers   {}\n  webhook   {}\n  trigger   `{}` label\n",
        app.cfg.addr,
        app.cfg.data_dir.display(),
        app.cfg.workers,
        if app.cfg.webhook_secret.is_some() {
            "\x1b[32mverified\x1b[0m"
        } else {
            "\x1b[33mdisabled — set ARBOR_WEBHOOK_SECRET\x1b[0m"
        },
        app.cfg.trigger_label
    );

    axum::serve(listener, router)
        .with_graceful_shutdown(shutdown())
        .await?;
    Ok(())
}

async fn shutdown() {
    let _ = tokio::signal::ctrl_c().await;
    println!("\n  shutting down");
}

// ------------------------------------------------------------------ handlers

/// Errors carry a status and a message, and nothing else. An internal error
/// string is for the log, not for the caller.
pub struct ApiError(pub StatusCode, pub String);

impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        (self.0, Json(json!({ "error": self.1 }))).into_response()
    }
}

impl From<anyhow::Error> for ApiError {
    fn from(e: anyhow::Error) -> Self {
        tracing_line("error", &e.to_string());
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "internal error".into())
    }
}

pub type ApiResult<T> = std::result::Result<T, ApiError>;

async fn health(State(app): State<App>) -> ApiResult<Json<serde_json::Value>> {
    let (queued, running) = app.db.queue_depth()?;
    let repos = app.db.repos()?;
    Ok(Json(json!({
        "ok": true,
        "version": env!("CARGO_PKG_VERSION"),
        "repos": repos.len(),
        "ready": repos.iter().filter(|r| r.state == "ready").count(),
        "queue": { "queued": queued, "running": running },
    })))
}

async fn list_repos(State(app): State<App>) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "repos": app.db.repos()? })))
}

#[derive(Deserialize)]
struct AddRepo {
    /// https URL to clone. Either this or `path`.
    #[serde(default)]
    url: Option<String>,
    /// Existing working tree, for a repository already on disk.
    #[serde(default)]
    path: Option<String>,
    /// `owner/name`. Derived from the URL when omitted.
    #[serde(default)]
    full_name: Option<String>,
    #[serde(default = "default_branch")]
    branch: String,
}

fn default_branch() -> String {
    "main".into()
}

async fn add_repo(
    State(app): State<App>,
    Json(req): Json<AddRepo>,
) -> ApiResult<(StatusCode, Json<serde_json::Value>)> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, m);

    let (path, url, name) = match (&req.url, &req.path) {
        (Some(url), _) => {
            clone::check_url(url).map_err(|e| bad(e.to_string()))?;
            let name = req
                .full_name
                .clone()
                .or_else(|| clone::name_from_url(url))
                .ok_or_else(|| bad("cannot derive owner/name from that URL".into()))?;
            let dir = clone::work_dir(&app.cfg.data_dir, &name);
            (dir, Some(url.clone()), name)
        }
        (None, Some(p)) => {
            let dir = PathBuf::from(p);
            if !dir.is_dir() {
                return Err(bad(format!("{p} is not a directory")));
            }
            let abs = dir.canonicalize().map_err(|e| bad(e.to_string()))?;
            let name = req
                .full_name
                .clone()
                .ok_or_else(|| bad("full_name is required with `path`".into()))?;
            (abs, None, name)
        }
        (None, None) => return Err(bad("one of `url` or `path` is required".into())),
    };

    let repo = app
        .db
        .upsert_repo(&name, &path.to_string_lossy(), &req.branch, url.as_deref())?;
    app.db.set_repo_state(repo.id, "pending", None)?;

    // Index at registration, not on the first issue: a cold index at answer
    // time is the difference between a product that feels instant and one that
    // feels broken. A clone job indexes when it finishes.
    let kind = if url.is_some() { "clone" } else { "index" };
    app.db
        .enqueue(kind, repo.id, "{}", Some(&format!("{kind}:{}", repo.id)))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "repo": repo, "queued": kind })),
    ))
}

async fn get_repo(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    match app.db.repo_by_name(&name)? {
        Some(r) => Ok(Json(json!({ "repo": r }))),
        None => Err(ApiError(StatusCode::NOT_FOUND, "no such repo".into())),
    }
}

async fn sync_repo(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(repo) = app.db.repo_by_name(&name)? else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such repo".into()));
    };
    let fresh = app
        .db
        .enqueue("sync", repo.id, "{}", Some(&format!("sync:{}", repo.id)))?;
    Ok(Json(json!({
        "queued": fresh,
        // A push burst collapses into one sync rather than ten.
        "note": if fresh { "queued" } else { "already queued; collapsed" }
    })))
}

// -------------------------------------------------------------------- worker

async fn worker(app: App, id: usize) {
    loop {
        let claimed = match app.db.claim() {
            Ok(j) => j,
            Err(e) => {
                tracing_line("error", &format!("worker {id}: claim failed: {e}"));
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                continue;
            }
        };
        let Some(job) = claimed else {
            tokio::time::sleep(std::time::Duration::from_millis(250)).await;
            continue;
        };

        let outcome = run_job(&app, &job).await;
        let err = outcome.as_ref().err().map(|e| e.to_string());
        if let Some(e) = &err {
            tracing_line("error", &format!("job {} ({}) failed: {e}", job.id, job.kind));
        }
        if let Err(e) = app.db.finish_job(job.id, err.as_deref()) {
            tracing_line("error", &format!("worker {id}: finish failed: {e}"));
        }
    }
}

async fn run_job(app: &App, job: &db::Job) -> Result<()> {
    match job.kind.as_str() {
        "clone" => clone_repo(app, job.repo_id).await,
        "index" => index_repo(app, job.repo_id, true).await,
        "sync" => index_repo(app, job.repo_id, false).await,
        "issue" => answer_issue(app, job).await,
        other => anyhow::bail!("unknown job kind `{other}`"),
    }
}

/// Clone or fast-forward, then index. Network-bound, so it goes on the blocking
/// pool for the same reason indexing does.
async fn clone_repo(app: &App, repo_id: i64) -> Result<()> {
    let repo = app.repo(repo_id)?;
    let url = repo.url.clone().context("repo has no url")?;
    let dir = PathBuf::from(&repo.path);
    let branch = repo.default_branch.clone();
    let name = repo.full_name.clone();

    app.db.set_repo_state(repo_id, "cloning", None)?;
    let existed = dir.join(".git").is_dir();
    let t = Instant::now();
    let res = tokio::task::spawn_blocking(move || clone::fetch(&dir, &url, &branch)).await?;
    let ms = t.elapsed().as_millis();

    match res {
        Ok((before, after)) => {
            tracing_line(
                "info",
                &format!(
                    "{name}: {} at {} in {ms}ms",
                    if existed { "updated" } else { "cloned" },
                    &after[..12.min(after.len())]
                ),
            );
            // A fetch that moved HEAD can diff two trees; a fresh clone has
            // nothing to diff against.
            let payload = match before {
                Some(b) if b != after => json!({ "since": b }).to_string(),
                _ => "{}".into(),
            };
            let kind = if existed { "sync" } else { "index" };
            app.db
                .enqueue(kind, repo_id, &payload, Some(&format!("{kind}:{repo_id}")))?;
            Ok(())
        }
        Err(e) => {
            app.db.set_repo_state(repo_id, "error", Some(&e.to_string()))?;
            Err(e)
        }
    }
}

/// Indexing is CPU-bound and saturates every core. Running it on the reactor
/// would stall every other task in the process, including whatever webhook
/// arrives mid-index.
async fn index_repo(app: &App, repo_id: i64, full: bool) -> Result<()> {
    let repo = app.repo(repo_id)?;

    app.db
        .set_repo_state(repo_id, if full { "indexing" } else { "syncing" }, None)?;

    let path = PathBuf::from(&repo.path);
    let db = app.db.clone();
    let name = repo.full_name.clone();

    let res = tokio::task::spawn_blocking(move || -> Result<(i64, i64, i64, i64)> {
        let t = Instant::now();
        let summary = index::run(&index::Config {
            path: path.clone(),
            threads: None,
            by_lang: false,
            top: None,
            no_resolve: false,
            no_cochange: false,
            incremental: !full,
            since: None,
            out: None,
            dry_run: false,
            quiet: true,
        })?;
        Ok((
            summary.files as i64,
            summary.nodes as i64,
            summary.edges as i64,
            t.elapsed().as_millis() as i64,
        ))
    })
    .await?;

    match res {
        Ok((files, nodes, edges, ms)) => {
            let sha = git_head(&PathBuf::from(&repo.path));
            db.record_index(repo_id, sha.as_deref(), files, nodes, edges, ms)?;
            tracing_line(
                "info",
                &format!("{name}: {} in {ms}ms — {nodes} nodes, {edges} edges",
                    if full { "indexed" } else { "synced" }),
            );
            Ok(())
        }
        Err(e) => {
            db.set_repo_state(repo_id, "error", Some(&e.to_string()))?;
            Err(e)
        }
    }
}

/// Issue in, comment out.
///
/// Every exit path records a run: a job that silently did nothing is
/// indistinguishable from one that never ran, and the cost ledger is only
/// trustworthy if it accounts for the cheap outcomes too.
async fn answer_issue(app: &App, job: &db::Job) -> Result<()> {
    let payload: serde_json::Value = serde_json::from_str(&job.payload).unwrap_or_default();
    let issue_id = payload
        .get("issue_id")
        .and_then(serde_json::Value::as_i64)
        .context("issue job without an issue_id")?;
    let issue = app.db.issue(issue_id)?.context("issue vanished")?;
    let repo = app.repo(issue.repo_id)?;

    let run_id = app.db.start_run(issue.id, repo.id)?;
    let started = Instant::now();

    let Some(client) = agent::Client::from_env() else {
        app.db.finish_run(run_id, "skipped", Some("no api key"), "", 0, 0.0,
            started.elapsed().as_millis() as i64, None)?;
        tracing_line("warn", "no ARBOR_ANTHROPIC_KEY — issue skipped");
        return Ok(());
    };

    // --- stage 1: triage -----------------------------------------------------
    let (triage, r1) = agent::triage(&client, &issue.title, &issue.body).await?;
    let mut tokens = r1.usage.total();
    let mut cached = r1.usage.cache_read;
    let mut cost = agent::record(&app.db, run_id, repo.id, "triage", &r1)?;

    let mut nodes = 0usize;
    let mut files_json = String::from("[]");
    let mut comment;

    if !triage.worth_analysing() {
        comment = agent::kind_note(&triage);
    } else {
        // --- stage 2: analyse ------------------------------------------------
        // Seeds come from both the triage extraction and the raw text: the model
        // is better at spotting names in prose, the graph is better at knowing
        // which of them exist.
        let seed_text = format!("{} {} {}", issue.title, triage.symbols.join(" "), issue.body);
        let repo_path = PathBuf::from(&repo.path);
        let built = tokio::task::spawn_blocking(move || build_context(&repo_path, &seed_text))
            .await??;
        nodes = built.nodes;
        files_json = built.files_json;

        let r2 = agent::analyse(
            &client,
            &built.preamble,
            &built.text,
            &issue.title,
            &issue.body,
        )
        .await?;
        tokens += r2.usage.total();
        cached += r2.usage.cache_read;
        cost += agent::record(&app.db, run_id, repo.id, "analyse", &r2)?;
        comment = r2.text;
    }

    comment.push_str(&agent::receipt(nodes, tokens, cached, cost));

    let posted = post_comment(&repo.full_name, issue.number, &comment).await;
    let status = match &posted {
        Ok(true) => "posted",
        Ok(false) => "dry-run",
        Err(_) => "post-failed",
    };
    if let Ok(false) = posted {
        tracing_line(
            "info",
            &format!(
                "{}#{} — no ARBOR_GITHUB_TOKEN, comment not posted:\n{comment}",
                repo.full_name, issue.number
            ),
        );
    }

    app.db.finish_run(
        run_id,
        status,
        Some(triage.kind.as_str()),
        &files_json,
        tokens,
        cost,
        started.elapsed().as_millis() as i64,
        posted.as_ref().err().map(|e| e.to_string()).as_deref(),
    )?;
    tracing_line(
        "info",
        &format!(
            "{}#{} — {} · {nodes} nodes · {tokens} tokens · ${cost:.4} · {status}",
            repo.full_name,
            issue.number,
            triage.kind.as_str()
        ),
    );
    Ok(())
}

struct Built {
    text: String,
    /// Stable across issues, so it can sit before the cache breakpoint.
    preamble: String,
    files_json: String,
    nodes: usize,
}

/// Graph lookup is synchronous and mmap-backed; it belongs on the blocking pool
/// like indexing does.
fn build_context(repo_path: &std::path::Path, text: &str) -> Result<Built> {
    use crate::{graph::Graph, query};
    let g = Graph::open(&repo_path.join(".arbor").join("graph.bin"))
        .context("repository has no graph yet")?;
    let ctx = query::build_from_text(&g, text, &query::Budget::default());
    let preamble = g.preamble(60);

    let mut out = String::new();
    let mut files: Vec<&str> = Vec::new();
    for it in &ctx.items {
        let (f, a, b) = g.location(it.node);
        let path = g.path(f);
        files.push(path);
        out.push_str(&format!(
            "## {} [{}]\n{}@{}\n",
            g.name(it.node),
            it.why.label(),
            path,
            a
        ));
        if let Ok(bytes) = std::fs::read(g.abs_path(f)) {
            let end = (b as usize).min(bytes.len());
            let start = (a as usize).min(end);
            let src = String::from_utf8_lossy(&bytes[start..end]);
            out.push_str("```\n");
            out.push_str(src.chars().take(2400).collect::<String>().as_str());
            out.push_str("\n```\n\n");
        }
    }
    files.sort_unstable();
    files.dedup();
    Ok(Built {
        text: out,
        preamble,
        files_json: serde_json::to_string(&files).unwrap_or_else(|_| "[]".into()),
        nodes: ctx.items.len(),
    })
}

/// Returns false when no token is configured — a deliberate dry run rather than
/// an error, so the whole pipeline can be exercised without write access.
async fn post_comment(full_name: &str, number: i64, body: &str) -> Result<bool> {
    let Ok(token) = std::env::var("ARBOR_GITHUB_TOKEN") else {
        return Ok(false);
    };
    if token.is_empty() {
        return Ok(false);
    }
    let url = format!("https://api.github.com/repos/{full_name}/issues/{number}/comments");
    let res = reqwest::Client::new()
        .post(&url)
        .header("authorization", format!("Bearer {token}"))
        .header("accept", "application/vnd.github+json")
        .header("user-agent", "arbor")
        .json(&serde_json::json!({ "body": body }))
        .send()
        .await?;
    if !res.status().is_success() {
        anyhow::bail!("github returned {}", res.status());
    }
    Ok(true)
}

fn git_head(root: &std::path::Path) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Deliberately minimal: one line, one level, no key-value ceremony. A
/// self-hosted binary's log is read by a person tailing it, not by a pipeline.
pub fn tracing_line(level: &str, msg: &str) {
    let colour = match level {
        "error" => "\x1b[31m",
        "warn" => "\x1b[33m",
        _ => "\x1b[2m",
    };
    eprintln!("{colour}{level:>5}\x1b[0m  {msg}");
}
