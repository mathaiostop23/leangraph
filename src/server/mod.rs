//! Self-hosted server: repositories in, graph-backed answers out.
//!
//! One process. The HTTP surface accepts work and returns immediately; a worker
//! task drains the queue. Indexing is CPU-bound and runs on the blocking pool so
//! a 700 ms full index cannot stall the reactor and drop a webhook.

pub mod db;

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
        .with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(app.cfg.addr)
        .await
        .with_context(|| format!("bind {}", app.cfg.addr))?;

    println!(
        "\n  \x1b[1marbor server\x1b[0m  http://{}\n  data      {}\n  workers   {}\n",
        app.cfg.addr,
        app.cfg.data_dir.display(),
        app.cfg.workers
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
struct ApiError(StatusCode, String);

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

type ApiResult<T> = std::result::Result<T, ApiError>;

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
    /// `owner/name`, used as the identity everywhere.
    full_name: String,
    /// Working tree on disk. Cloning is the next phase; for now the repo is
    /// already somewhere local.
    path: String,
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
    let path = PathBuf::from(&req.path);
    if !path.is_dir() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            format!("{} is not a directory", req.path),
        ));
    }
    let abs = path
        .canonicalize()
        .map_err(|e| ApiError(StatusCode::BAD_REQUEST, e.to_string()))?;

    let repo = app
        .db
        .upsert_repo(&req.full_name, &abs.to_string_lossy(), &req.branch)?;
    app.db.set_repo_state(repo.id, "pending", None)?;
    // Index at registration, not on the first issue: a cold index at answer
    // time is the difference between a product that feels instant and one that
    // feels broken.
    app.db
        .enqueue("index", repo.id, "{}", Some(&format!("index:{}", repo.id)))?;

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({ "repo": repo, "queued": "index" })),
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
        "index" => index_repo(app, job.repo_id, true).await,
        "sync" => index_repo(app, job.repo_id, false).await,
        other => anyhow::bail!("unknown job kind `{other}`"),
    }
}

/// Indexing is CPU-bound and saturates every core. Running it on the reactor
/// would stall every other task in the process, including whatever webhook
/// arrives mid-index.
async fn index_repo(app: &App, repo_id: i64, full: bool) -> Result<()> {
    let repo = app
        .db
        .repos()?
        .into_iter()
        .find(|r| r.id == repo_id)
        .context("repo vanished")?;

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
fn tracing_line(level: &str, msg: &str) {
    let colour = match level {
        "error" => "\x1b[31m",
        "warn" => "\x1b[33m",
        _ => "\x1b[2m",
    };
    eprintln!("{colour}{level:>5}\x1b[0m  {msg}");
}
