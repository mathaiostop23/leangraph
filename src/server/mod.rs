//! Self-hosted server: repositories in, graph-backed answers out.
//!
//! One process. The HTTP surface accepts work and returns immediately; a worker
//! task drains the queue. Indexing is CPU-bound and runs on the blocking pool so
//! a 700 ms full index cannot stall the reactor and drop a webhook.

pub mod agent;
pub mod clone;
pub mod crypto;
pub mod db;
pub mod dedup;
pub mod fix;
pub mod provider;
pub mod ui;
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
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

pub struct Config {
    pub addr: SocketAddr,
    /// Label that additionally requests a patch. Distinct from the analysis
    /// label on purpose: asking for an explanation and asking for a change to
    /// your repository are different decisions.
    pub fix_label: String,
    /// Shared secret for webhook signatures. Absent means the endpoint refuses
    /// everything — failing closed, because an unverified webhook is an open
    /// door to whatever the agent can do.
    pub webhook_secret: Option<String>,
    /// Label an issue must carry before the bot acts.
    pub trigger_label: String,
    /// Where repositories and their graphs live.
    pub data_dir: PathBuf,
    pub db_path: PathBuf,
    /// Concurrency for answering issues. Almost entirely waiting on the model
    /// API, so this can be far higher than the core count.
    pub workers: usize,
    /// Concurrency for cloning and indexing. Separate because the two have
    /// nothing in common: indexing already saturates every core through rayon,
    /// so running several at once makes each of them slower, while a queue
    /// shared with issues would put a three-second answer behind a
    /// twelve-minute index.
    pub index_workers: usize,
    /// How often to look for answered issues whose code has since moved. Zero
    /// turns it off entirely.
    pub reanalyse_hours: u32,
    /// Gate on everything except the webhook and the health probe.
    ///
    /// `None` means the operator passed `--no-auth` and accepts that anyone who
    /// can reach the port can register repositories against their API budget
    /// and turn on the mode that opens pull requests. Absent that flag one is
    /// generated on first run, because a default that depends on the operator
    /// placing the port correctly is not a security model.
    pub admin_token: Option<String>,
}

#[derive(Clone)]
pub struct App {
    pub db: Db,
    pub cfg: Arc<Config>,
    pub vault: Arc<crypto::Vault>,
}

impl App {
    pub fn webhook_secret(&self) -> Option<&str> {
        self.cfg.webhook_secret.as_deref()
    }

    /// Per-repo override would live in `config_json`; for now one label for the
    /// whole install.
    /// The label this repository acts on.
    ///
    /// Per repository first, because one project wanting its own word for this
    /// should not mean restarting the server for everyone else.
    pub fn trigger_label(&self, repo: &db::Repo) -> String {
        repo.setting("trigger_label", &self.cfg.trigger_label)
            .into_owned()
    }

    pub fn fix_label_for(&self, repo: &db::Repo) -> String {
        repo.setting("fix_label", &self.cfg.fix_label).into_owned()
    }

    /// Stored secret first, environment second.
    ///
    /// The database wins so an operator can rotate a key through the API
    /// without restarting; the environment remains the way to bootstrap, and
    /// the way to run without persisting a secret at all.
    pub fn secret(&self, name: &str) -> Option<String> {
        if let Ok(Some((nonce, ct))) = self.db.get_secret(name) {
            match self.vault.open_sealed(&nonce, &ct) {
                Ok(v) => return Some(v),
                Err(e) => tracing_line("warn", &format!("secret `{name}`: {e}")),
            }
        }
        std::env::var(env_name(name)).ok().filter(|v| !v.is_empty())
    }

    /// Which model each job runs on, when the operator has said.
    ///
    /// `model` covers every role; `model_fix` and friends beat it for one. Both
    /// go through `secret`, so either the encrypted store or the environment
    /// works, and neither needs a rebuild. Nothing is validated here: the
    /// provider is the authority on what ids exist, a name it does not know
    /// comes back as its own error, and a model released after this binary was
    /// compiled is reachable the day it ships.
    pub fn models(&self) -> provider::Models {
        let mut m = provider::Models {
            all: self.secret("model"),
            ..Default::default()
        };
        for role in provider::Role::ALL {
            let chosen = self.secret(role.secret_name());
            match role {
                provider::Role::Triage => m.triage = chosen,
                provider::Role::Analyse => m.analyse = chosen,
                provider::Role::Fix => m.fix = chosen,
                provider::Role::Escalate => m.escalate = chosen,
            }
        }
        m
    }

    /// The write token for whichever host this repository lives on.
    pub fn provider_token(&self, repo: &db::Repo) -> Option<String> {
        self.secret(if repo.provider == "gitlab" {
            "gitlab_token"
        } else {
            "github_token"
        })
    }

    pub fn repo(&self, id: i64) -> anyhow::Result<db::Repo> {
        self.db
            .repos()?
            .into_iter()
            .find(|r| r.id == id)
            .context("repo vanished")
    }
}

/// Read the admin token from the environment, or mint and persist one.
///
/// Generated rather than defaulted-open: the management surface can register a
/// repository against the owner's API budget, write a secret, and enable the
/// mode that pushes pull requests. The one endpoint that must be public — the
/// webhook — carries its own signature, so it is the only one this does not
/// cover.
pub fn admin_token(data_dir: &Path) -> Result<String> {
    if let Ok(t) = std::env::var("LEANGRAPH_ADMIN_TOKEN") {
        if !t.trim().is_empty() {
            return Ok(t);
        }
    }
    let path = data_dir.join("admin.token");
    if let Ok(t) = std::fs::read_to_string(&path) {
        if !t.trim().is_empty() {
            return Ok(t.trim().to_string());
        }
    }
    use aes_gcm::aead::rand_core::RngCore;
    let mut raw = [0u8; 24];
    aes_gcm::aead::OsRng.fill_bytes(&mut raw);
    let token = hex::encode(raw);
    std::fs::create_dir_all(data_dir).ok();
    std::fs::write(&path, &token).with_context(|| format!("writing {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(token)
}

/// Reject anything that does not carry the token.
///
/// Three ways to present it, because two different clients need it: a script
/// sends a header, and a browser opening the dashboard can only put it in the
/// URL. The query form is documented as the weaker one — it lands in browser
/// history and in any proxy log on the way.
async fn require_token(
    State(app): State<App>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> axum::response::Response {
    let Some(want) = app.cfg.admin_token.as_deref() else {
        return next.run(req).await;
    };
    let h = req.headers();
    let bearer = h
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let custom = h.get("x-leangraph-token").and_then(|v| v.to_str().ok());
    let query = req.uri().query().and_then(|q| {
        form_urlencoded::parse(q.as_bytes())
            .find(|(k, _)| k == "token")
            .map(|(_, v)| v.into_owned())
    });

    let given = bearer.or(custom).map(str::to_string).or(query);
    // Constant time: a byte-at-a-time comparison leaks the prefix to anyone
    // willing to time the responses.
    let ok = given.as_deref().is_some_and(|g| {
        g.len() == want.len()
            && g.bytes()
                .zip(want.bytes())
                .fold(0u8, |acc, (a, b)| acc | (a ^ b))
                == 0
    });
    if ok {
        next.run(req).await
    } else {
        ApiError(
            StatusCode::UNAUTHORIZED,
            "this endpoint needs the admin token — see the line printed at startup".into(),
        )
        .into_response()
    }
}

/// What the `secret` subcommand does, without a server or an admin token.
///
/// `POST /secrets` is the right shape for an install running somewhere else,
/// and the wrong one for someone configuring a checkout on their own machine:
/// it wants a listener, a token, and a correctly shaped JSON body before it
/// will store an API key. This reaches the same encrypted store directly.
pub enum SecretOp {
    Set { name: String, value: String },
    List,
    Remove { name: String },
}

pub fn secret_cli(data_dir: &Path, op: SecretOp) -> Result<()> {
    std::fs::create_dir_all(data_dir).ok();
    let db = Db::open(&data_dir.join("leangraph.db"))?;
    let vault = crypto::Vault::open(data_dir)?;
    match op {
        SecretOp::Set { name, value } => {
            if !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
            {
                anyhow::bail!("name must be lowercase letters, digits and underscores");
            }
            if value.trim().is_empty() {
                anyhow::bail!("value is empty — use `secret rm {name}` to clear it");
            }
            let (nonce, ct) = vault.seal(&value)?;
            let hint = crypto::hint(&value);
            db.put_secret(&name, &nonce, &ct, &hint)?;
            // The hint, never the value. Printing back what was just stored is
            // how a key ends up in a terminal log or a screen recording.
            println!("  stored {name} ({hint})");
        }
        SecretOp::List => {
            let items = db.secret_hints()?;
            if items.is_empty() {
                println!("  nothing stored in {}", data_dir.display());
            }
            for (name, hint) in items {
                println!("  {name:<18} {hint}");
            }
        }
        SecretOp::Remove { name } => {
            if db.delete_secret(&name)? {
                println!("  removed {name}");
            } else {
                println!("  {name} was not set");
            }
        }
    }
    Ok(())
}

pub async fn run(cfg: Config) -> Result<()> {
    std::fs::create_dir_all(&cfg.data_dir).ok();
    // Before anything else, and before the first repository is registered:
    // this fails at boot with a sentence the operator can act on, or it fails
    // hours later as a corrupt WAL under load.
    db::preflight(&cfg.data_dir)?;
    let db = Db::open(&cfg.db_path)?;
    let vault = crypto::Vault::open(&cfg.data_dir)?;
    let key_on_disk = vault.key_on_disk;
    let app = App {
        db,
        cfg: Arc::new(cfg),
        vault: Arc::new(vault),
    };

    // Sweep expired leases, at startup and then on a timer. On a timer because
    // a process that dies mid-job should not strand it until someone restarts
    // the server, and by *expiry* rather than by state because a second
    // instance sharing this database is entitled to reclaim nothing that is
    // still being renewed.
    {
        let sweeper = app.clone();
        tokio::spawn(async move {
            loop {
                match sweeper.db.reclaim_expired() {
                    Ok(n) if n > 0 => tracing_line(
                        "warn",
                        &format!("requeued {n} job(s) whose worker stopped renewing"),
                    ),
                    Err(e) => tracing_line("error", &format!("reclaiming jobs: {e}")),
                    _ => {}
                }
                tokio::time::sleep(std::time::Duration::from_secs(
                    (Db::LEASE_SECS / 2).max(1) as u64
                ))
                .await;
            }
        });
    }

    // Re-analysis, for repositories that asked for it. On a timer rather than
    // a cron expression: this has one job and a number of hours says it.
    if app.cfg.reanalyse_hours > 0 {
        let ticker = app.clone();
        tokio::spawn(async move {
            let every = std::time::Duration::from_secs(ticker.cfg.reanalyse_hours as u64 * 3600);
            loop {
                tokio::time::sleep(every).await;
                let Ok(repos) = ticker.db.repos() else {
                    continue;
                };
                for r in repos.iter().filter(|r| r.state == "ready") {
                    if r.setting_usize("reanalyse", 0) == 0 {
                        continue;
                    }
                    let _ = ticker.db.enqueue(
                        "reanalyse",
                        r.id,
                        "{}",
                        Some(&format!("reanalyse:{}", r.id)),
                    );
                }
            }
        });
    }

    for i in 0..app.cfg.workers {
        let w = app.clone();
        tokio::spawn(async move { worker(w, i, &Db::ISSUE_KINDS).await });
    }
    for i in 0..app.cfg.index_workers {
        let w = app.clone();
        tokio::spawn(async move { worker(w, 1000 + i, &Db::HEAVY_KINDS).await });
    }

    // Everything that manages the install. The dashboard is in here too: it
    // lists repository names, error strings and what each answer cost.
    let managed = Router::new()
        .route("/", get(ui::page))
        .route("/metrics", get(metrics))
        .route("/repos", get(list_repos).post(add_repo))
        .route("/repos/{name}", get(get_repo))
        .route("/repos/{name}/sync", post(sync_repo))
        .route("/repos/{name}/backfill", post(backfill_repo))
        .route("/repos/{name}/reanalyse", post(reanalyse_repo))
        .route("/repos/{name}/config", post(set_config))
        .route("/secrets", get(list_secrets))
        .route("/secrets/{name}", post(put_secret).delete(delete_secret))
        .layer(axum::middleware::from_fn_with_state(
            app.clone(),
            require_token,
        ));

    // The webhook carries its own signature and has to be reachable by the
    // provider; the health probe is what the container healthcheck calls and
    // reports counts rather than contents.
    let open = Router::new()
        .route("/health", get(health))
        .route("/webhook/github", post(webhook::github))
        .route("/webhook/gitlab", post(webhook::gitlab));

    let router = managed.merge(open).with_state(app.clone());

    let listener = tokio::net::TcpListener::bind(app.cfg.addr)
        .await
        .with_context(|| format!("bind {}", app.cfg.addr))?;

    println!(
        "\n  \x1b[1mleangraph server\x1b[0m  http://{}\n  data      {}\n  workers   {}\n  webhook   {}\n  trigger   `{}` label\n  fix       `{}` label, and only where the repo has opted in\n",
        app.cfg.addr,
        app.cfg.data_dir.display(),
        app.cfg.workers,
        if app.cfg.webhook_secret.is_some() {
            "\x1b[32mverified\x1b[0m"
        } else {
            "\x1b[33mdisabled — set LEANGRAPH_WEBHOOK_SECRET\x1b[0m"
        },
        app.cfg.trigger_label,
        app.cfg.fix_label
    );
    match app.cfg.admin_token.as_deref() {
        Some(t) => println!(
            "  \x1b[1madmin\x1b[0m     {t}\n            \x1b[2msend it as `Authorization: Bearer …`, or open the \
dashboard at\n            http://{}/?token={t}\x1b[0m\n",
            app.cfg.addr
        ),
        None => tracing_line(
            "warn",
            "--no-auth: anyone who can reach this port can register repositories \
against your API budget, write secrets, and enable the mode that opens pull requests",
        ),
    }
    if key_on_disk {
        tracing_line(
            "warn",
            "master key is stored beside the database — anyone who can read one can \
usually read the other. Set LEANGRAPH_MASTER_KEY to keep it out of the volume.",
        );
    }

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

/// Prometheus text format.
///
/// Behind the token like everything else that manages the install: the cost
/// figures here are the operator's spend, not a liveness signal. A scraper
/// sends the same `Authorization: Bearer` header a person would.
async fn metrics(State(app): State<App>) -> ApiResult<String> {
    let (queued, running) = app.db.queue_depth()?;
    let repos = app.db.repos()?;
    let ready = repos.iter().filter(|r| r.state == "ready").count();
    let failed = repos.iter().filter(|r| r.state == "error").count();
    let nodes: i64 = repos.iter().map(|r| r.node_count).sum();
    let edges: i64 = repos.iter().map(|r| r.edge_count).sum();
    let (runs, cost) = app.db.run_totals()?;

    let mut out = String::new();
    for (name, help, kind, value) in [
        (
            "leangraph_repos",
            "Repositories registered.",
            "gauge",
            repos.len() as f64,
        ),
        (
            "leangraph_repos_ready",
            "Repositories with a usable graph.",
            "gauge",
            ready as f64,
        ),
        (
            "leangraph_repos_failed",
            "Repositories whose last index failed.",
            "gauge",
            failed as f64,
        ),
        (
            "leangraph_jobs_queued",
            "Jobs waiting to be claimed.",
            "gauge",
            queued as f64,
        ),
        (
            "leangraph_jobs_running",
            "Jobs a worker is holding.",
            "gauge",
            running as f64,
        ),
        (
            "leangraph_graph_nodes",
            "Nodes across every graph.",
            "gauge",
            nodes as f64,
        ),
        (
            "leangraph_graph_edges",
            "Edges across every graph.",
            "gauge",
            edges as f64,
        ),
        (
            "leangraph_runs_total",
            "Issues answered.",
            "counter",
            runs as f64,
        ),
        (
            "leangraph_cost_usd_total",
            "Spent on the model API.",
            "counter",
            cost,
        ),
    ] {
        out.push_str(&format!(
            "# HELP {name} {help}\n# TYPE {name} {kind}\n{name} {value}\n"
        ));
    }
    Ok(out)
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
    /// `github` or `gitlab`. Derived from the URL host when omitted, because
    /// getting this wrong means the comment goes to the wrong API and the
    /// webhook is authenticated the wrong way.
    #[serde(default)]
    provider: Option<String>,
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

    // Named, or read off the URL. A repository registered by local path with no
    // provider given is GitHub, which is what it was before this existed.
    let provider = req.provider.clone().unwrap_or_else(|| {
        match url.as_deref() {
            Some(u) if u.contains("gitlab") => "gitlab",
            _ => "github",
        }
        .into()
    });
    if provider != "github" && provider != "gitlab" {
        return Err(bad(format!("unknown provider `{provider}`")));
    }

    let repo = app.db.upsert_repo(
        &name,
        &path.to_string_lossy(),
        &req.branch,
        url.as_deref(),
        &provider,
    )?;
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

/// Names and hints only. An endpoint that can return a secret it was given
/// makes encrypting it pointless.
async fn list_secrets(State(app): State<App>) -> ApiResult<Json<serde_json::Value>> {
    let items: Vec<_> = app
        .db
        .secret_hints()?
        .into_iter()
        .map(|(name, hint)| json!({ "name": name, "hint": hint }))
        .collect();
    Ok(Json(json!({ "secrets": items })))
}

#[derive(Deserialize)]
struct PutSecret {
    value: String,
}

async fn put_secret(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
    Json(req): Json<PutSecret>,
) -> ApiResult<Json<serde_json::Value>> {
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "name must be lowercase letters, digits and underscores".into(),
        ));
    }
    if req.value.trim().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "value is empty".into()));
    }
    let (nonce, ct) = app.vault.seal(&req.value)?;
    let hint = crypto::hint(&req.value);
    app.db.put_secret(&name, &nonce, &ct, &hint)?;
    tracing_line("info", &format!("secret `{name}` stored ({hint})"));
    Ok(Json(json!({ "stored": name, "hint": hint })))
}

async fn delete_secret(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    Ok(Json(json!({ "deleted": app.db.delete_secret(&name)? })))
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

/// Answer the backlog a repository already had.
///
/// Deliberately a request rather than something registration does on its own: a
/// repository with four hundred open issues would otherwise spend the
/// operator's budget the moment it was added.
async fn backfill_repo(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(repo) = app.db.repo_by_name(&name)? else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such repo".into()));
    };
    let fresh = app.db.enqueue(
        "backfill",
        repo.id,
        "{}",
        Some(&format!("backfill:{}", repo.id)),
    )?;
    tracing_line(
        "info",
        &format!(
            "{}: backfill {}",
            repo.full_name,
            if fresh { "queued" } else { "already queued" }
        ),
    );
    Ok(Json(json!({
        "queued": fresh,
        "note": "open issues carrying the trigger label, analysed as one batch at half price"
    })))
}

/// Look now rather than waiting for the timer.
///
/// After a large merge an operator wants this on demand, and it is the same
/// job the schedule enqueues — including the gate, so an issue whose code did
/// not move still costs nothing.
async fn reanalyse_repo(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(repo) = app.db.repo_by_name(&name)? else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such repo".into()));
    };
    let fresh = app.db.enqueue(
        "reanalyse",
        repo.id,
        "{}",
        Some(&format!("reanalyse:{}", repo.id)),
    )?;
    Ok(Json(json!({
        "queued": fresh,
        "note": "answered issues whose graph seeds have changed since"
    })))
}

#[derive(Deserialize)]
struct SetConfig {
    /// Opt in to proposing patches. Off by default, and one of three
    /// independent switches — the issue still needs the fix label and the
    /// server still needs a write token.
    fix_mode: bool,
    /// This repository's own trigger label, where the server-wide one does not
    /// suit it. Absent leaves whatever is set.
    #[serde(default)]
    trigger_label: Option<String>,
    #[serde(default)]
    fix_label: Option<String>,
    /// Context ceiling in graph nodes. The cost of an answer is roughly linear
    /// in this, so it is the knob for a repository whose issues are cheap or
    /// one whose issues are worth spending on.
    #[serde(default)]
    max_nodes: Option<usize>,
    /// Ask a stronger model again when the analysis reports low confidence.
    /// Off by default: it is five times the input price to help the minority
    /// of issues that need it.
    #[serde(default)]
    escalate: Option<bool>,
    /// Shell command that runs this repository's tests, with `{files}`
    /// substituted for the paths a patch touched. Empty means none run, which
    /// is the default and what the pull request will say.
    ///
    /// It executes in the throwaway worktree as the server process. There is
    /// no sandbox here; putting a boundary around the server is the operator's
    /// to do, and setting this is saying so.
    #[serde(default)]
    test_command: Option<String>,
    /// Wrapper that supplies isolation, with `{dir}` the worktree and `{cmd}`
    /// the shell-quoted command — a `docker run --rm --network none …` belongs
    /// here. Empty means the command runs as the server process does.
    #[serde(default)]
    sandbox: Option<String>,
    /// Re-analyse this repository's answered issues when the code beneath them
    /// changes. Off by default — it spends money on a schedule.
    #[serde(default)]
    reanalyse: Option<bool>,
}

async fn set_config(
    State(app): State<App>,
    AxPath(name): AxPath<String>,
    Json(req): Json<SetConfig>,
) -> ApiResult<Json<serde_json::Value>> {
    let Some(repo) = app.db.repo_by_name(&name)? else {
        return Err(ApiError(StatusCode::NOT_FOUND, "no such repo".into()));
    };
    let mut cfg: serde_json::Value =
        serde_json::from_str(&repo.config_json).unwrap_or_else(|_| json!({}));
    cfg["fix_mode"] = json!(req.fix_mode);
    // Only what was sent. A config endpoint that silently resets the keys a
    // caller did not mention is how a label gets turned off by someone toggling
    // fix mode.
    if let Some(v) = &req.trigger_label {
        cfg["trigger_label"] = json!(v);
    }
    if let Some(v) = &req.fix_label {
        cfg["fix_label"] = json!(v);
    }
    if let Some(v) = req.max_nodes {
        cfg["max_nodes"] = json!(v);
    }
    if let Some(v) = req.escalate {
        cfg["escalate"] = json!(u8::from(v));
    }
    if let Some(v) = &req.sandbox {
        cfg["sandbox"] = json!(v);
    }
    if let Some(v) = req.reanalyse {
        cfg["reanalyse"] = json!(u8::from(v));
    }
    if let Some(v) = &req.test_command {
        cfg["test_command"] = json!(v);
        if !v.trim().is_empty() {
            tracing_line(
                "warn",
                &format!(
                    "{}: will run `{v}` in a worktree on every proposed patch",
                    repo.full_name
                ),
            );
        }
    }
    app.db.set_repo_config(repo.id, &cfg.to_string())?;
    tracing_line(
        if req.fix_mode { "warn" } else { "info" },
        &format!(
            "{}: fix mode {}",
            repo.full_name,
            if req.fix_mode {
                "ENABLED — patches will be pushed as draft pull requests"
            } else {
                "disabled"
            }
        ),
    );
    Ok(Json(
        json!({ "repo": repo.full_name, "fix_mode": req.fix_mode }),
    ))
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

/// Who this process is, for the purpose of holding a lease.
///
/// Random per process rather than derived from the host: two containers on one
/// machine sharing a volume is exactly the case this has to tell apart.
fn instance_id() -> &'static str {
    static ID: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    ID.get_or_init(|| {
        use aes_gcm::aead::rand_core::RngCore;
        let mut b = [0u8; 8];
        aes_gcm::aead::OsRng.fill_bytes(&mut b);
        hex::encode(b)
    })
}

async fn worker(app: App, id: usize, kinds: &'static [&'static str]) {
    let owner = format!("{}:{id}", instance_id());
    loop {
        let claimed = match app.db.claim_as(kinds, &owner) {
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

        // Hold the lease while the work runs. An index of a large repository
        // takes minutes, which is many times the lease, and without this the
        // sweeper would reclaim a job that is progressing perfectly well.
        let beat = {
            let db = app.db.clone();
            let (jid, who) = (job.id, owner.clone());
            tokio::spawn(async move {
                let every = std::time::Duration::from_secs((Db::LEASE_SECS / 3).max(1) as u64);
                loop {
                    tokio::time::sleep(every).await;
                    match db.renew_lease(jid, &who) {
                        Ok(true) => {}
                        // Lost it: something reclaimed the job. Stop renewing
                        // rather than fight over a row that is no longer ours.
                        Ok(false) => break,
                        Err(e) => {
                            tracing_line("error", &format!("renewing lease on {jid}: {e}"));
                            break;
                        }
                    }
                }
            })
        };

        // Waiting for the graph is not failing. A job that stands aside is put
        // back without spending an attempt, so a cold index does not exhaust
        // the retry budget of every issue that arrived while it ran.
        let outcome = run_job(&app, &job).await;
        beat.abort();
        if let Ok(Outcome::Waiting(secs, why)) = &outcome {
            if job.defers >= Db::MAX_DEFERS {
                let msg = format!("gave up waiting: {why}");
                tracing_line("error", &format!("job {} ({}) {msg}", job.id, job.kind));
                let _ = app.db.finish_job(job.id, Some(&msg), false);
            } else {
                // Say it once. Repeating it every ten seconds for a
                // twelve-minute index would bury everything else in the log.
                if job.defers == 0 {
                    tracing_line(
                        "info",
                        &format!("job {} ({}) waiting: {why}", job.id, job.kind),
                    );
                }
                if let Err(e) = app.db.defer_job(job.id, *secs) {
                    tracing_line("error", &format!("worker {id}: defer failed: {e}"));
                }
            }
            continue;
        }

        let err = outcome.as_ref().err().map(|e| e.to_string());
        // Ask the error what it is rather than reading its message.
        let retry = outcome
            .as_ref()
            .err()
            .is_some_and(|e| e.chain().any(|c| c.is::<agent::Transient>()))
            && job.attempts < Db::MAX_ATTEMPTS;
        if let Some(e) = &err {
            tracing_line(
                if retry { "warn" } else { "error" },
                &format!(
                    "job {} ({}) failed: {e}{}",
                    job.id,
                    job.kind,
                    if retry {
                        format!(
                            " — attempt {} of {}, will retry",
                            job.attempts,
                            Db::MAX_ATTEMPTS
                        )
                    } else {
                        String::new()
                    }
                ),
            );
        }
        if let Err(e) = app.db.finish_job(job.id, err.as_deref(), retry) {
            tracing_line("error", &format!("worker {id}: finish failed: {e}"));
        }
    }
}

/// What a job did. `Waiting` is not failure and not success: the work has not
/// been attempted yet because something it needs is not ready.
pub enum Outcome {
    Done,
    Waiting(i64, String),
}

async fn run_job(app: &App, job: &db::Job) -> Result<Outcome> {
    match job.kind.as_str() {
        "clone" => clone_repo(app, job.repo_id).await.map(|()| Outcome::Done),
        "index" => index_repo(app, job.repo_id, true)
            .await
            .map(|()| Outcome::Done),
        "sync" => index_repo(app, job.repo_id, false)
            .await
            .map(|()| Outcome::Done),
        "issue" => answer_issue(app, job).await,
        "backfill" => backfill(app, job, false).await,
        "reanalyse" => backfill(app, job, true).await,
        "batch" => collect_batch(app, job).await,
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
    let token = app.secret("github_token");
    let res =
        tokio::task::spawn_blocking(move || clone::fetch(&dir, &url, &branch, token.as_deref()))
            .await?;
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
            app.db
                .set_repo_state(repo_id, "error", Some(&e.to_string()))?;
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
                &format!(
                    "{name}: {} in {ms}ms — {nodes} nodes, {edges} edges",
                    if full { "indexed" } else { "synced" }
                ),
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
async fn answer_issue(app: &App, job: &db::Job) -> Result<Outcome> {
    let payload: serde_json::Value = serde_json::from_str(&job.payload).unwrap_or_default();
    let issue_id = payload
        .get("issue_id")
        .and_then(serde_json::Value::as_i64)
        .context("issue job without an issue_id")?;
    let issue = app.db.issue(issue_id)?.context("issue vanished")?;
    let repo = app.repo(issue.repo_id)?;

    // An issue can arrive before the repository has finished indexing — the
    // first one usually does, since registering a repository and opening an
    // issue about it are the same afternoon. Answering it anyway means
    // answering without the graph, which is the one thing this tool is for.
    // A cold index is seconds to minutes; the issue can wait for it.
    if repo.state != "ready" {
        if repo.state == "error" {
            anyhow::bail!(
                "repository {} could not be indexed: {}",
                repo.full_name,
                repo.error.as_deref().unwrap_or("no reason recorded")
            );
        }
        return Ok(Outcome::Waiting(
            10,
            format!("{} is {}", repo.full_name, repo.state),
        ));
    }

    let run_id = app.db.start_run(issue.id, repo.id)?;
    let started = Instant::now();

    // --- stage 0: is this one we have already answered? ----------------------
    // Before the API client, before triage, before anything is spent. This is
    // the only saving available that is total rather than fractional.
    let repo_path = PathBuf::from(&repo.path);
    let (title, body) = (issue.title.clone(), issue.body.clone());
    let fp = tokio::task::spawn_blocking(move || fingerprint_of(&repo_path, &title, &body)).await?;
    let prior: Vec<(i64, dedup::Fingerprint)> = app
        .db
        .prior_fingerprints(repo.id, issue.id, dedup::LOOKBACK)?
        .into_iter()
        .map(|(n, enc)| (n, dedup::decode(&enc)))
        .collect();

    if let Some((number, score)) = dedup::best_match(&fp, &prior) {
        let comment = format!(
            "This looks like a restatement of #{number} — {:.0}% of the wording and \
{:.0}% of the code it points at are the same, so I have not re-analysed it.\n\n\
If that is wrong, say so on the issue and I will look properly.\n",
            score.text * 100.0,
            score.seed * 100.0
        );
        post_comment(
            &repo.provider,
            &repo.full_name,
            issue.number,
            &comment,
            app.provider_token(&repo).as_deref(),
        )
        .await
        .ok();
        app.db.set_fingerprint(issue.id, &dedup::encode(&fp))?;
        app.db.finish_run(
            run_id,
            "duplicate",
            Some(&format!("duplicate of #{number}")),
            "[]",
            0,
            0.0,
            started.elapsed().as_millis() as i64,
            None,
        )?;
        tracing_line(
            "info",
            &format!(
                "{}#{} — duplicate of #{number} · no model call",
                repo.full_name, issue.number
            ),
        );
        return Ok(Outcome::Done);
    }

    let Some(client) = agent::Client::new(
        app.secret("anthropic_key"),
        app.secret("openai_key"),
        app.secret("provider"),
        app.models(),
    ) else {
        app.db.finish_run(
            run_id,
            "skipped",
            Some("no api key"),
            "",
            0,
            0.0,
            started.elapsed().as_millis() as i64,
            None,
        )?;
        tracing_line("warn", "no LEANGRAPH_ANTHROPIC_KEY — issue skipped");
        return Ok(Outcome::Done);
    };

    // --- stage 1: triage -----------------------------------------------------
    let (triage, r1) = agent::triage(&client, &issue.title, &issue.body).await?;
    let mut tokens = r1.usage.total();
    let mut cached = r1.usage.cache_read;
    let mut cost = agent::record(&app.db, run_id, repo.id, "triage", &r1)?;

    let mut nodes = 0usize;
    let mut files_json = String::from("[]");
    let mut comment;
    let mut built_for_fix: Option<Built> = None;

    if !triage.worth_analysing() {
        comment = agent::kind_note(&triage);
    } else {
        // --- stage 2: analyse ------------------------------------------------
        // Seeds come from both the triage extraction and the raw text: the model
        // is better at spotting names in prose, the graph is better at knowing
        // which of them exist.
        let seed_text = format!(
            "{} {} {}",
            issue.title,
            triage.symbols.join(" "),
            issue.body
        );
        let repo_path = PathBuf::from(&repo.path);
        let max_nodes = repo.setting_usize("max_nodes", crate::query::Budget::default().max_nodes);
        let built =
            tokio::task::spawn_blocking(move || build_context(&repo_path, &seed_text, max_nodes))
                .await??;
        nodes = built.nodes;
        files_json = built.files_json.clone();

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
        let (text, sure) = agent::split_confidence(&r2.text);
        comment = text;

        // --- stage 2b: escalate, where the answer says it is not sure --------
        // Off unless the repository asks for it. Opus is five times the input
        // price of Sonnet, and spending that on every issue to help the few
        // that need it is the opposite of what this project argues for. The
        // ledger records both calls under their own stage, so an operator can
        // see what escalation cost and whether the answer changed.
        let deep = repo.setting_usize("escalate", 0) != 0;
        if deep && sure == agent::Sure::Low {
            tracing_line(
                "info",
                &format!(
                    "{}#{}: analysis reported low confidence, escalating",
                    repo.full_name, issue.number
                ),
            );
            match agent::escalate(
                &client,
                &built.preamble,
                &built.text,
                &issue.title,
                &issue.body,
            )
            .await
            {
                Ok(r3) => {
                    tokens += r3.usage.total();
                    cached += r3.usage.cache_read;
                    cost += agent::record(&app.db, run_id, repo.id, "escalate", &r3)?;
                    comment = agent::split_confidence(&r3.text).0;
                }
                // The Sonnet answer is already in hand. Losing it because the
                // second opinion failed would be worse than not asking.
                Err(e) => tracing_line(
                    "warn",
                    &format!("escalation failed, keeping the first answer: {e}"),
                ),
            }
        }
        built_for_fix = Some(built);
    }

    // --- stage 3: patch, only where all three switches are open --------------
    let want_fix = payload
        .get("fix")
        .and_then(serde_json::Value::as_bool)
        .unwrap_or(false);
    if want_fix && triage.worth_analysing() {
        match propose_fix(app, &client, &repo, &issue, &built_for_fix).await {
            Ok(Some((url, files, r3))) => {
                tokens += r3.usage.total();
                cached += r3.usage.cache_read;
                cost += agent::record(&app.db, run_id, repo.id, "fix", &r3)?;
                comment.push_str(&format!(
                    "\n\n---\n\n**Proposed patch:** {url}\n\nTouches {}. Opened as a draft — \
it has not been run or tested.\n",
                    files.join(", ")
                ));
            }
            Ok(None) => comment.push_str(
                "\n\n---\n\n_A patch was requested, but I could not produce one I was \
willing to propose from the context available._\n",
            ),
            Err(e) => {
                tracing_line(
                    "warn",
                    &format!("{}#{}: fix failed: {e}", repo.full_name, issue.number),
                );
                comment.push_str(&format!(
                    "\n\n---\n\n_A patch was requested but could not be applied: {e}_\n"
                ));
            }
        }
    }

    // Stored only once an answer exists, so a later issue can only be matched
    // against something there is actually a thread to point at.
    app.db.set_fingerprint(issue.id, &dedup::encode(&fp))?;

    comment.push_str(&agent::receipt(
        client.provider(),
        nodes,
        tokens,
        cached,
        cost,
    ));

    let posted = post_comment(
        &repo.provider,
        &repo.full_name,
        issue.number,
        &comment,
        app.provider_token(&repo).as_deref(),
    )
    .await;
    let status = match &posted {
        Ok(true) => "posted",
        Ok(false) => "dry-run",
        Err(_) => "post-failed",
    };
    if let Ok(false) = posted {
        tracing_line(
            "info",
            &format!(
                "{}#{} — no LEANGRAPH_GITHUB_TOKEN, comment not posted:\n{comment}",
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
    Ok(Outcome::Done)
}

#[derive(Clone)]
struct Built {
    text: String,
    /// Stable across issues, so it can sit before the cache breakpoint.
    preamble: String,
    files_json: String,
    nodes: usize,
}

/// Fingerprint an issue. Opens the graph so the seed half is real; a repository
/// without one still gets the text half, which is the stronger signal anyway.
fn fingerprint_of(repo_path: &std::path::Path, title: &str, body: &str) -> dedup::Fingerprint {
    let g = crate::graph::Graph::open(&repo_path.join(".leangraph").join("graph.bin")).ok();
    dedup::fingerprint(g.as_ref(), title, body)
}

/// Analyse every open issue at once, at half price.
///
/// The Batch API is 50% off and asynchronous — usually under an hour, up to 24.
/// That is useless for a webhook, where latency is the product, and exactly
/// right for the work nobody is waiting on: the backlog a repository already
/// had when it was registered.
///
/// Triage is skipped. Its job is to classify and pull seed symbols out of the
/// text, and the context builder already seeds itself from the issue text — so
/// for a bulk run it is a Haiku call per issue that buys nothing.
/// Re-run the answered ones whose code has moved since.
///
/// The gate is the fingerprint, not the clock. An issue's fingerprint includes
/// the `NodeKey`s of the seeds the context builder picks, so if those are
/// unchanged the analysis would be drawn from the same code and would say the
/// same thing — and a bot that posts the same conclusion every night is a bot
/// people mute. Where the seeds *have* changed, the code the issue points at
/// moved, and there is something new to say.
async fn backfill(app: &App, job: &db::Job, only_stale: bool) -> Result<Outcome> {
    let repo = app.repo(job.repo_id)?;
    if repo.state != "ready" {
        return Ok(Outcome::Waiting(
            10,
            format!("{} is {}", repo.full_name, repo.state),
        ));
    }
    let Some(client) = agent::Client::new(
        app.secret("anthropic_key"),
        app.secret("openai_key"),
        app.secret("provider"),
        app.models(),
    ) else {
        anyhow::bail!("backfill needs an API key");
    };
    // Backfill is the batch path, and only one provider has a batch endpoint
    // here. Refusing up front is better than submitting a body in the wrong
    // shape and finding out when the results do not arrive.
    if !client.provider().batch() {
        anyhow::bail!(
            "backfill submits a batch, which {} does not offer here —              set LEANGRAPH_PROVIDER=anthropic for this repository or answer              issues one at a time",
            client.provider().name()
        );
    }

    let open = list_open_issues(app, &repo).await?;
    if open.is_empty() {
        tracing_line(
            "info",
            &format!("{}: no open issues to look at", repo.full_name),
        );
        return Ok(Outcome::Done);
    }

    let mut items = Vec::new();
    let mut runs: Vec<(String, i64, i64)> = Vec::new(); // custom_id, issue id, run id
    for (number, title, body) in open {
        let issue_id = app
            .db
            .upsert_issue(repo.id, number, &title, &body, "MEMBER")?;
        let answered = app.db.has_run(issue_id)?;
        // Backfill is for what has never been answered — it is re-runnable and
        // must not re-bill an issue whose comment is already on the thread.
        // Re-analysis is the mirror: only what has.
        if answered != only_stale {
            continue;
        }
        let repo_path = PathBuf::from(&repo.path);
        let fresh_fp = {
            let (p, t, b) = (repo_path.clone(), title.clone(), body.clone());
            tokio::task::spawn_blocking(move || fingerprint_of(&p, &t, &b)).await?
        };
        if only_stale {
            let before = app
                .db
                .fingerprint(issue_id)?
                .map(|e| dedup::decode(&e).seeds)
                .unwrap_or_default();
            // Same seeds, same code, same answer. A bot that posts the same
            // conclusion every night is a bot people mute.
            if before == fresh_fp.seeds {
                continue;
            }
        }
        let seed = format!("{title}\n\n{body}");
        let max_nodes = repo.setting_usize("max_nodes", crate::query::Budget::default().max_nodes);
        let built =
            tokio::task::spawn_blocking(move || build_context(&repo_path, &seed, max_nodes))
                .await??;
        let run_id = app.db.start_run(issue_id, repo.id)?;
        let cid = format!("issue-{issue_id}");
        items.push(agent::batch_analyse(
            &cid,
            &built.preamble,
            &built.text,
            &title,
            &body,
        ));
        // Recorded now: the answer is going to be drawn from this selection,
        // and the next run compares against it whether or not this one posts.
        app.db
            .set_fingerprint(issue_id, &dedup::encode(&fresh_fp))?;
        runs.push((cid, issue_id, run_id));
    }
    if items.is_empty() {
        tracing_line(
            "info",
            &format!(
                "{}: nothing to {}",
                repo.full_name,
                if only_stale {
                    "re-analyse — no issue's code has moved"
                } else {
                    "backfill — every open issue is already answered"
                }
            ),
        );
        return Ok(Outcome::Done);
    }

    let batch_id = client.batch_submit(&items).await?;
    tracing_line(
        "info",
        &format!(
            "{}: {} issue(s) submitted as batch {batch_id} at half price",
            repo.full_name,
            items.len()
        ),
    );
    let payload = json!({
        "batch": batch_id,
        "stage": if only_stale { "reanalyse" } else { "backfill" },
        "runs": runs.iter().map(|(c, i, r)| json!({ "cid": c, "issue": i, "run": r }))
            .collect::<Vec<_>>(),
    });
    app.db
        .enqueue("batch", repo.id, &payload.to_string(), None)?;
    Ok(Outcome::Done)
}

/// Poll a submitted batch, and post what came back.
///
/// The waiting is the deferral mechanism the issue path already uses: a job
/// standing aside for something it depends on, not an attempt that failed.
async fn collect_batch(app: &App, job: &db::Job) -> Result<Outcome> {
    let payload: serde_json::Value = serde_json::from_str(&job.payload).unwrap_or_default();
    let batch_id = payload
        .get("batch")
        .and_then(|x| x.as_str())
        .context("batch job without a batch id")?;
    let repo = app.repo(job.repo_id)?;
    let Some(client) = agent::Client::new(
        app.secret("anthropic_key"),
        app.secret("openai_key"),
        app.secret("provider"),
        app.models(),
    ) else {
        anyhow::bail!("collecting a batch needs an API key");
    };

    let Some(results) = client.batch_results(batch_id).await? else {
        return Ok(Outcome::Waiting(60, format!("batch {batch_id} is running")));
    };

    let by_cid: std::collections::HashMap<&str, (i64, i64)> = payload
        .get("runs")
        .and_then(|x| x.as_array())
        .map(|rs| {
            rs.iter()
                .filter_map(|r| {
                    Some((
                        r.get("cid")?.as_str()?,
                        (r.get("issue")?.as_i64()?, r.get("run")?.as_i64()?),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();

    let token = app.provider_token(&repo);
    let mut posted = 0usize;
    for (cid, reply) in results {
        let Some(&(issue_id, run_id)) = by_cid.get(cid.as_str()) else {
            continue;
        };
        let Some(issue) = app.db.issue(issue_id)? else {
            continue;
        };
        let cost = agent::record_at(
            &app.db,
            run_id,
            repo.id,
            payload
                .get("stage")
                .and_then(|x| x.as_str())
                .unwrap_or("backfill"),
            &reply,
            agent::BATCH_RATE,
        )?;
        let mut comment = agent::split_confidence(&reply.text).0;
        comment.push_str(&agent::receipt(
            client.provider(),
            0,
            reply.usage.total(),
            reply.usage.cache_read,
            cost,
        ));
        let ok = post_comment(
            &repo.provider,
            &repo.full_name,
            issue.number,
            &comment,
            token.as_deref(),
        )
        .await;
        app.db.finish_run(
            run_id,
            if ok.is_ok() { "posted" } else { "post-failed" },
            Some("backfill"),
            "[]",
            reply.usage.total(),
            cost,
            0,
            None,
        )?;
        posted += 1;
    }
    tracing_line(
        "info",
        &format!(
            "{}: batch {batch_id} answered {posted} issue(s)",
            repo.full_name
        ),
    );
    Ok(Outcome::Done)
}

/// Open issues carrying the trigger label, from whichever host this is.
async fn list_open_issues(app: &App, repo: &db::Repo) -> Result<Vec<(i64, String, String)>> {
    let label = app.trigger_label(repo);
    let token = app.provider_token(repo);
    let client = reqwest::Client::new();
    let req = if repo.provider == "gitlab" {
        client
            .get(format!(
                "{}/api/v4/projects/{}/issues?state=opened&labels={}&per_page=100",
                gitlab_api_base(),
                urlencoding_encode(&repo.full_name),
                urlencoding_encode(&label)
            ))
            .header("private-token", token.unwrap_or_default())
    } else {
        client
            .get(format!(
                "{}/repos/{}/issues?state=open&labels={}&per_page=100",
                fix::api_base(),
                repo.full_name,
                urlencoding_encode(&label)
            ))
            .header(
                "authorization",
                format!("Bearer {}", token.unwrap_or_default()),
            )
            .header("accept", "application/vnd.github+json")
    };
    let res = req.header("user-agent", "leangraph").send().await?;
    if !res.status().is_success() {
        anyhow::bail!("{} returned {}", repo.provider, res.status());
    }
    let v: serde_json::Value = res.json().await?;
    let gitlab = repo.provider == "gitlab";
    Ok(v.as_array()
        .map(|xs| {
            xs.iter()
                // A pull request arrives in GitHub's issue list too, and it is
                // a different thing.
                .filter(|x| x.get("pull_request").is_none())
                .filter_map(|x| {
                    let number = x.get(if gitlab { "iid" } else { "number" })?.as_i64()?;
                    let title = x.get("title")?.as_str()?.to_string();
                    let body = x
                        .get(if gitlab { "description" } else { "body" })
                        .and_then(|b| b.as_str())
                        .unwrap_or_default()
                        .to_string();
                    Some((number, title, body))
                })
                .collect()
        })
        .unwrap_or_default())
}

/// Graph lookup is synchronous and mmap-backed; it belongs on the blocking pool
/// like indexing does.
fn build_context(repo_path: &std::path::Path, text: &str, max_nodes: usize) -> Result<Built> {
    use crate::{graph::Graph, query};
    let g = Graph::open(&repo_path.join(".leangraph").join("graph.bin"))
        .context("repository has no graph yet")?;
    // A repository can ask for a tighter or a wider selection than the default.
    // The cost of an answer is roughly linear in this, so it is the one knob an
    // operator watching a bill actually wants.
    let ctx = query::build_from_text(
        &g,
        text,
        &query::Budget {
            max_nodes,
            ..query::Budget::default()
        },
    );
    let preamble = g.preamble_at_least(agent::CACHE_MIN_CHARS, 60);

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
        // Stale spans slice the wrong bytes; better to send the agent a name
        // and a location than a confidently mislabelled body.
        let fresh = g.file_is_current(f);
        if let Ok(bytes) = std::fs::read(g.abs_path(f)) {
            if !fresh {
                out.push_str("_(file changed since indexing; source omitted)_\n\n");
                continue;
            }
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

/// Ask for a patch, vet it, push it, open a draft pull request.
///
/// `Ok(None)` means the model declined to produce one, which the prompt
/// explicitly permits — an empty answer is correct when the context is not
/// enough, and a guess that edits someone's repository is not.
async fn propose_fix(
    app: &App,
    client: &agent::Client,
    repo: &db::Repo,
    issue: &db::Issue,
    built: &Option<Built>,
) -> Result<Option<(String, Vec<String>, agent::Reply)>> {
    let built = built.as_ref().context("no context was built")?;
    let token = app
        .secret("github_token")
        .context("fix mode needs a token with write access")?;

    let reply = agent::propose_fix(
        client,
        &built.preamble,
        &built.text,
        &issue.title,
        &issue.body,
    )
    .await?;
    let patch = agent::clean_patch(&reply.text);
    if patch.trim().is_empty() {
        return Ok(None);
    }

    let dir = PathBuf::from(&repo.path);
    let branch_base = repo.default_branch.clone();
    let number = issue.number;
    let name = repo.full_name.clone();
    let tok = token.clone();
    // The operator's command, or none. Never inferred from the tree and never
    // asked of the model: what runs here is a decision somebody typed.
    let test_command = repo.setting("test_command", "").into_owned();
    // Where the operator puts real isolation. Empty means the command runs as
    // the server does, which the config field and SERVER.md both say plainly.
    let sandbox = repo.setting("sandbox", "").into_owned();
    let proposal = tokio::task::spawn_blocking(move || {
        fix::propose(
            &dir,
            &branch_base,
            number,
            &patch,
            Some(&tok),
            &name,
            Some(test_command.as_str()).filter(|c| !c.trim().is_empty()),
            &sandbox,
        )
    })
    .await??;

    let url = fix::open_pr(
        &repo.full_name,
        &proposal.branch,
        &repo.default_branch,
        issue.number,
        &token,
        proposal.tests.as_ref(),
    )
    .await?;
    tracing_line(
        "warn",
        &format!("{}#{}: opened draft PR {url}", repo.full_name, issue.number),
    );
    Ok(Some((url, proposal.files, reply)))
}

/// Where GitLab's API lives. Overridable for a self-managed install, and for
/// the test suite, which points it at a stub.
pub fn gitlab_api_base() -> String {
    std::env::var("LEANGRAPH_GITLAB_API")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "https://gitlab.com".into())
}

/// Returns false when no token is configured — a deliberate dry run rather than
/// an error, so the whole pipeline can be exercised without write access.
async fn post_comment(
    provider: &str,
    full_name: &str,
    number: i64,
    body: &str,
    token: Option<&str>,
) -> Result<bool> {
    let Some(token) = token.filter(|t| !t.is_empty()) else {
        return Ok(false);
    };
    let client = reqwest::Client::new();
    // The two differ in three ways and no more: where the note goes, how the
    // project is named in the path, and how the token is presented.
    let req = if provider == "gitlab" {
        let project = urlencoding_encode(full_name);
        client
            .post(format!(
                "{}/api/v4/projects/{project}/issues/{number}/notes",
                gitlab_api_base()
            ))
            .header("private-token", token)
            .json(&serde_json::json!({ "body": body }))
    } else {
        client
            .post(format!(
                "{}/repos/{full_name}/issues/{number}/comments",
                fix::api_base()
            ))
            .header("authorization", format!("Bearer {token}"))
            .header("accept", "application/vnd.github+json")
            .json(&serde_json::json!({ "body": body }))
    };
    let res = req.header("user-agent", "leangraph").send().await?;
    if !res.status().is_success() {
        anyhow::bail!("{provider} returned {}", res.status());
    }
    Ok(true)
}

/// `group/project` -> `group%2Fproject`.
///
/// GitLab addresses a project by its path with the slashes escaped. Sending it
/// raw makes the API read it as three path segments and answer 404.
fn urlencoding_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
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
/// `anthropic_key` -> `LEANGRAPH_ANTHROPIC_KEY`.
fn env_name(name: &str) -> String {
    format!("LEANGRAPH_{}", name.to_ascii_uppercase())
}

pub fn tracing_line(level: &str, msg: &str) {
    let colour = match level {
        "error" => "\x1b[31m",
        "warn" => "\x1b[33m",
        _ => "\x1b[2m",
    };
    eprintln!("{colour}{level:>5}\x1b[0m  {msg}");
}
