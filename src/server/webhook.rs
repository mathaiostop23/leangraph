//! Webhook ingress.
//!
//! Everything here exists because the request is hostile until proven
//! otherwise. Anyone can open an issue on a public repository, and its body
//! goes on to be read by a model with tools. That makes this file the security
//! boundary of the whole product, and it is why the gates below are not
//! configurable-off by accident.
//!
//! The handler does the minimum and returns: verify, deduplicate, enqueue. Work
//! happens on a worker, because a provider that does not get a response in a few
//! seconds retries, and a retry that races the first delivery is how you post
//! the same comment twice.

use super::{db, tracing_line, ApiError, ApiResult, App};
use axum::{
    body::Bytes,
    extract::State,
    http::{HeaderMap, StatusCode},
    Json,
};
use hmac::{Hmac, Mac};
use serde_json::{json, Value};
use sha2::Sha256;

/// Who is allowed to make the bot act. A drive-by issue on a public repo must
/// not be able to spend the owner's API budget, let alone steer an agent.
const TRUSTED: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// Constant-time signature check.
///
/// `hmac`'s `verify_slice` is already constant-time; the length guard in front
/// of it is not a security property, just an early out.
pub fn verify(secret: &[u8], body: &[u8], header: &str) -> bool {
    let Some(hex_sig) = header.strip_prefix("sha256=") else {
        return false;
    };
    let Ok(sig) = hex::decode(hex_sig) else {
        return false;
    };
    let Ok(mut mac) = Hmac::<Sha256>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&sig).is_ok()
}

fn header<'a>(h: &'a HeaderMap, name: &str) -> Option<&'a str> {
    h.get(name).and_then(|v| v.to_str().ok())
}

pub async fn github(
    State(app): State<App>,
    headers: HeaderMap,
    // Raw bytes, deliberately. Deserialising and re-serialising to JSON changes
    // the bytes and breaks the MAC — the most common way this check gets
    // silently disabled.
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let secret = app.webhook_secret();
    let Some(secret) = secret else {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no webhook secret configured".into(),
        ));
    };

    let sig = header(&headers, "x-hub-signature-256").unwrap_or_default();
    if !verify(secret.as_bytes(), &body, sig) {
        tracing_line("warn", "webhook: bad signature, rejected");
        return Err(ApiError(StatusCode::UNAUTHORIZED, "bad signature".into()));
    }

    let delivery = header(&headers, "x-github-delivery")
        .unwrap_or_default()
        .to_string();
    if delivery.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "missing delivery id".into(),
        ));
    }
    // A retry carries the same delivery id. Claiming it is what stops a second
    // comment on the same issue.
    if !app.db.claim_delivery(&delivery)? {
        return Ok((StatusCode::OK, Json(json!({ "status": "duplicate" }))));
    }

    let event = header(&headers, "x-github-event")
        .unwrap_or_default()
        .to_string();
    let payload = parse_payload(&headers, &body)
        .ok_or_else(|| ApiError(StatusCode::BAD_REQUEST, "malformed payload".into()))?;

    let outcome = match event.as_str() {
        "ping" => json!({ "status": "pong" }),
        "push" => on_push(&app, &payload)?,
        "issues" | "issue_comment" => on_issue(&app, &payload)?,
        other => json!({ "status": "ignored", "event": other }),
    };
    Ok((StatusCode::ACCEPTED, Json(outcome)))
}

/// The JSON a delivery carries, whichever way it was encoded.
///
/// GitHub's webhook form defaults to `application/x-www-form-urlencoded`, which
/// sends `payload=<urlencoded json>` rather than a JSON body — and every such
/// delivery was rejected as malformed. The default configuration failed one
/// hundred percent of the time, with an error that did not say why.
///
/// The signature is verified over the raw bytes before this runs, and must stay
/// that way: decoding first and hashing the result would verify something the
/// sender never signed.
fn parse_payload(h: &HeaderMap, body: &[u8]) -> Option<Value> {
    let ct = header(h, "content-type").unwrap_or_default();
    if ct.starts_with("application/x-www-form-urlencoded") {
        let field = form_urlencoded::parse(body)
            .find(|(k, _)| k == "payload")
            .map(|(_, v)| v.into_owned())?;
        return serde_json::from_str(&field).ok();
    }
    serde_json::from_slice(body).ok()
}

fn repo_of(app: &App, p: &Value) -> Option<db::Repo> {
    let name = p.get("repository")?.get("full_name")?.as_str()?;
    app.db.repo_by_name(name).ok().flatten()
}

fn on_push(app: &App, p: &Value) -> ApiResult<Value> {
    let Some(repo) = repo_of(app, p) else {
        return Ok(json!({ "status": "unknown repo" }));
    };
    // Only the branch we index. A push to a feature branch must not rewrite the
    // graph the answers are drawn from.
    let branch = p
        .get("ref")
        .and_then(Value::as_str)
        .and_then(|r| r.strip_prefix("refs/heads/"))
        .unwrap_or_default();
    if branch != repo.default_branch {
        return Ok(json!({ "status": "ignored", "reason": "not the indexed branch" }));
    }

    // `before` lets the sync diff two trees instead of walking; on a force-push
    // git will reject the range and the sync falls back to a walk on its own.
    let before = p.get("before").and_then(Value::as_str).unwrap_or_default();
    let payload = json!({ "since": before }).to_string();
    let fresh = app.db.enqueue(
        "sync",
        repo.id,
        &payload,
        Some(&format!("sync:{}", repo.id)),
    )?;
    Ok(json!({ "status": "queued", "collapsed": !fresh }))
}

fn on_issue(app: &App, p: &Value) -> ApiResult<Value> {
    let Some(repo) = repo_of(app, p) else {
        return Ok(json!({ "status": "unknown repo" }));
    };
    let action = p.get("action").and_then(Value::as_str).unwrap_or_default();
    if !matches!(action, "opened" | "labeled" | "reopened") {
        return Ok(json!({ "status": "ignored", "action": action }));
    }
    let Some(issue) = p.get("issue") else {
        return Ok(json!({ "status": "ignored", "reason": "no issue" }));
    };
    // A pull request also arrives as an `issues` event on some paths; it is a
    // different thing and not what this bot is for.
    if issue.get("pull_request").is_some() {
        return Ok(json!({ "status": "ignored", "reason": "pull request" }));
    }

    let assoc = issue
        .get("author_association")
        .and_then(Value::as_str)
        .unwrap_or("NONE");
    let number = issue.get("number").and_then(Value::as_i64).unwrap_or(0);

    // --- gate 1: who ---------------------------------------------------------
    if !TRUSTED.contains(&assoc) {
        tracing_line(
            "info",
            &format!("{}#{number}: ignored, author is {assoc}", repo.full_name),
        );
        return Ok(json!({ "status": "ignored", "reason": "untrusted author" }));
    }

    // --- gate 2: opt-in ------------------------------------------------------
    // Explicit per-issue opt-in rather than blanket. Blanket means every issue
    // on the repository spends money, and it means an attacker only has to get
    // one message past the model rather than past a human first.
    let want = app.trigger_label(&repo);
    let labelled = issue
        .get("labels")
        .and_then(Value::as_array)
        .is_some_and(|ls| {
            ls.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .any(|n| n.eq_ignore_ascii_case(&want))
        });
    if !labelled {
        return Ok(json!({ "status": "ignored", "reason": format!("needs `{want}` label") }));
    }

    if repo.state != "ready" {
        // Queue rather than fail: the answer is worth waiting for, and telling
        // the user "not indexed" is a worse experience than a slightly late
        // comment.
        tracing_line(
            "info",
            &format!(
                "{}#{number}: repo is {}, queueing anyway",
                repo.full_name, repo.state
            ),
        );
    }

    // --- gate 3: fix mode ----------------------------------------------------
    // Three switches, all required. A repository that opted in, an issue
    // labelled for it specifically, and a token to push with. Asking for an
    // explanation and asking for a change to your code are different decisions,
    // so they are different labels.
    let fix_label = app.fix_label_for(&repo);
    let wants_fix = issue
        .get("labels")
        .and_then(Value::as_array)
        .is_some_and(|ls| {
            ls.iter()
                .filter_map(|l| l.get("name").and_then(Value::as_str))
                .any(|n| n.eq_ignore_ascii_case(&fix_label))
        });
    // Fix mode opens a GitHub pull request. The GitLab equivalent is a merge
    // request against a different API and is not written, so it refuses here
    // rather than failing three steps later with a 404 nobody can read.
    let fix = wants_fix && repo.fix_mode() && repo.provider == "github";
    if wants_fix && repo.fix_mode() && repo.provider != "github" {
        tracing_line(
            "warn",
            &format!(
                "{}#{number}: fix mode is GitHub-only; analysing without a patch",
                repo.full_name
            ),
        );
    }
    if wants_fix && !fix {
        tracing_line(
            "info",
            &format!(
                "{}#{number}: `{fix_label}` requested but fix mode is off for this repo",
                repo.full_name
            ),
        );
    }

    let title = issue.get("title").and_then(Value::as_str).unwrap_or("");
    let body = issue.get("body").and_then(Value::as_str).unwrap_or("");
    let issue_id = app.db.upsert_issue(repo.id, number, title, body, assoc)?;

    let payload = json!({ "issue_id": issue_id, "number": number, "fix": fix }).to_string();
    let fresh = app.db.enqueue(
        "issue",
        repo.id,
        &payload,
        Some(&format!("issue:{}:{number}", repo.id)),
    )?;
    Ok(json!({ "status": "queued", "issue": number, "collapsed": !fresh, "fix": fix }))
}

// -------------------------------------------------------------------- gitlab

/// GitLab's issue and push hooks, translated into the shape the gates above
/// already read.
///
/// Translating rather than writing a second set of handlers is the whole point:
/// the author gate, the label gate and the delivery de-duplication are security
/// properties, and a provider with its own copy of them is a provider where one
/// of them silently differs. What genuinely differs between the two is
/// authentication and the field names, and that is all this does.
pub async fn gitlab(
    State(app): State<App>,
    headers: HeaderMap,
    body: Bytes,
) -> ApiResult<(StatusCode, Json<Value>)> {
    let Some(secret) = app.webhook_secret() else {
        return Err(ApiError(
            StatusCode::SERVICE_UNAVAILABLE,
            "no webhook secret configured".into(),
        ));
    };

    // GitLab sends the secret itself rather than a signature over the body.
    // That is weaker — it is a bearer token, and it is why the comparison has
    // to be constant-time even though there is no MAC to forge.
    let sent = header(&headers, "x-gitlab-token").unwrap_or_default();
    if !constant_time_eq(sent.as_bytes(), secret.as_bytes()) {
        tracing_line("warn", "gitlab webhook: bad token, rejected");
        return Err(ApiError(StatusCode::UNAUTHORIZED, "bad token".into()));
    }

    // GitLab's equivalent of a delivery id. Same purpose: a retry must not
    // produce a second comment.
    let delivery = header(&headers, "x-gitlab-event-uuid")
        .unwrap_or_default()
        .to_string();
    if delivery.is_empty() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "missing delivery id".into(),
        ));
    }
    if !app.db.claim_delivery(&delivery)? {
        return Ok((StatusCode::OK, Json(json!({ "status": "duplicate" }))));
    }

    let p: Value = serde_json::from_slice(&body)
        .map_err(|_| ApiError(StatusCode::BAD_REQUEST, "malformed payload".into()))?;
    let kind = p
        .get("object_kind")
        .and_then(Value::as_str)
        .unwrap_or_default();

    let outcome = match kind {
        "push" => on_push(&app, &to_push(&p))?,
        "issue" => {
            let Some(mut norm) = to_issue(&p) else {
                return Ok((StatusCode::OK, Json(json!({ "status": "ignored" }))));
            };
            // GitLab does not put the author's standing in the payload, so the
            // author gate has to ask. Doing it here rather than on a worker
            // keeps every gate in one place, and it is one request against an
            // API the answer already depends on.
            let assoc = author_standing(&app, &p).await;
            norm["issue"]["author_association"] = json!(assoc);
            on_issue(&app, &norm)?
        }
        other => json!({ "status": "ignored", "event": other }),
    };
    Ok((StatusCode::ACCEPTED, Json(outcome)))
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    use subtle::ConstantTimeEq;
    a.len() == b.len() && a.ct_eq(b).into()
}

/// GitLab push hook -> the fields `on_push` reads.
fn to_push(p: &Value) -> Value {
    json!({
        "repository": { "full_name": project_name(p) },
        "ref": p.get("ref").and_then(Value::as_str).unwrap_or_default(),
        "before": p.get("before").and_then(Value::as_str).unwrap_or_default(),
    })
}

/// GitLab issue hook -> the fields `on_issue` reads.
///
/// `iid` rather than `id`: the internal id is what the issue is called in the
/// project and in its URL, and the global one names nothing a person would
/// recognise.
fn to_issue(p: &Value) -> Option<Value> {
    let a = p.get("object_attributes")?;
    let action = match a.get("action").and_then(Value::as_str).unwrap_or_default() {
        "open" => "opened",
        "reopen" => "reopened",
        // A label added to an existing issue arrives as `update`, which is the
        // path the trigger label is normally applied by.
        "update" => "labeled",
        other => other,
    };
    let labels = p
        .get("labels")
        .and_then(Value::as_array)
        .map(|ls| {
            ls.iter()
                .filter_map(|l| l.get("title").and_then(Value::as_str))
                .map(|t| json!({ "name": t }))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    Some(json!({
        "repository": { "full_name": project_name(p) },
        "action": action,
        "issue": {
            "number": a.get("iid").and_then(Value::as_i64).unwrap_or(0),
            "title": a.get("title").and_then(Value::as_str).unwrap_or_default(),
            "body": a.get("description").and_then(Value::as_str).unwrap_or_default(),
            "labels": labels,
            // Filled in by the caller once the API has been asked.
            "author_association": "NONE",
        }
    }))
}

fn project_name(p: &Value) -> String {
    p.get("project")
        .and_then(|x| x.get("path_with_namespace"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// Is this author a member of the project?
///
/// GitLab has no `author_association`, so this asks. Anything short of a
/// definite yes is `NONE` — an API that is down, a token that cannot see the
/// member list, a malformed answer. The gate exists to keep a stranger from
/// spending the owner's budget, and failing it open would defeat it entirely.
async fn author_standing(app: &App, p: &Value) -> &'static str {
    let (Some(user_id), Some(project)) = (
        p.get("user")
            .and_then(|u| u.get("id"))
            .and_then(Value::as_i64),
        p.get("project")
            .and_then(|x| x.get("id"))
            .and_then(Value::as_i64),
    ) else {
        return "NONE";
    };
    let Some(token) = app.secret("gitlab_token") else {
        return "NONE";
    };
    let url = format!(
        "{}/api/v4/projects/{project}/members/all/{user_id}",
        super::gitlab_api_base()
    );
    let Ok(res) = reqwest::Client::new()
        .get(&url)
        .header("private-token", token)
        .send()
        .await
    else {
        return "NONE";
    };
    if !res.status().is_success() {
        return "NONE";
    }
    let Ok(v) = res.json::<Value>().await else {
        return "NONE";
    };
    // 30 is Developer. Below that a member can open issues but not change the
    // code, which is the same standing GitHub calls a non-collaborator.
    match v.get("access_level").and_then(Value::as_i64).unwrap_or(0) {
        l if l >= 50 => "OWNER",
        l if l >= 30 => "MEMBER",
        _ => "NONE",
    }
}
