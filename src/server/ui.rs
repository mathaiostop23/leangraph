//! Dashboard.
//!
//! One self-contained page, no build step, no CDN. This is a self-hosted binary
//! that people run behind a firewall; a dashboard that needs network access to
//! render is a dashboard that does not render.
//!
//! What it shows is chosen from what an operator actually needs to answer:
//! is it indexed, is the queue moving, what did the last answers cost. Cost is
//! first-class rather than buried — it is the number the whole design exists to
//! keep small, and hiding it would be strange.

use super::{db, ApiResult, App};
use axum::{extract::State, response::Html};

pub async fn page(State(app): State<App>) -> ApiResult<Html<String>> {
    let repos = app.db.repos()?;
    let (queued, running) = app.db.queue_depth()?;
    let runs = app.db.recent_runs(25)?;
    let day = db::now() - 86_400;

    let mut rows = String::new();
    for r in &repos {
        let (tok, cost) = app.db.spend(r.id, day).unwrap_or((0, 0.0));
        rows.push_str(&format!(
            r#"<tr>
  <td><b>{name}</b><div class=sub>{path}</div></td>
  <td><span class="pill {state}">{state}</span>{err}</td>
  <td class=n>{files}</td><td class=n>{nodes}</td><td class=n>{edges}</td>
  <td class=n>{ms} ms</td>
  <td class=n>{tok}</td><td class=n>${cost:.4}</td>
</tr>"#,
            name = esc(&r.full_name),
            path = esc(r.url.as_deref().unwrap_or(&r.path)),
            state = esc(&r.state),
            err = r
                .error
                .as_deref()
                .map(|e| format!("<div class=err>{}</div>", esc(e)))
                .unwrap_or_default(),
            files = r.file_count,
            nodes = r.node_count,
            edges = r.edge_count,
            ms = r.index_ms,
        ));
    }
    if repos.is_empty() {
        rows.push_str(
            r#"<tr><td colspan=8 class=empty>No repositories yet.<br>
<code>curl -X POST localhost:7777/repos -H 'content-type: application/json' \
-d '{"url":"https://github.com/owner/name"}'</code></td></tr>"#,
        );
    }

    let mut run_rows = String::new();
    for r in &runs {
        run_rows.push_str(&format!(
            r#"<tr><td>{repo} <b>#{num}</b></td><td><span class="pill {status}">{status}</span></td>
<td>{outcome}</td><td class=n>{tokens}</td><td class=n>${cost:.4}</td><td class=n>{ms} ms</td>
<td class=sub>{when}</td></tr>"#,
            repo = esc(&r.repo),
            num = r.number,
            status = esc(&r.status),
            outcome = esc(r.outcome.as_deref().unwrap_or("—")),
            tokens = r.total_tokens,
            cost = r.cost_usd,
            ms = r.duration_ms,
            when = ago(db::now() - r.created_at),
        ));
    }
    if runs.is_empty() {
        run_rows.push_str("<tr><td colspan=7 class=empty>No issues answered yet.</td></tr>");
    }

    let total_cost: f64 = runs.iter().map(|r| r.cost_usd).sum();
    let ready = repos.iter().filter(|r| r.state == "ready").count();

    Ok(Html(
        TEMPLATE
            .replace("{{REPOS}}", &rows)
            .replace("{{RUNS}}", &run_rows)
            .replace("{{QUEUED}}", &queued.to_string())
            .replace("{{RUNNING}}", &running.to_string())
            .replace("{{READY}}", &format!("{ready}/{}", repos.len()))
            .replace("{{COST}}", &format!("{total_cost:.4}"))
            .replace("{{VERSION}}", env!("CARGO_PKG_VERSION")),
    ))
}

/// Repository names, branches and error strings all reach this page from
/// outside. Escaping is not optional just because the audience is an operator.
fn esc(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

fn ago(secs: i64) -> String {
    match secs {
        s if s < 60 => "just now".into(),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

const TEMPLATE: &str = r#"<!doctype html>
<html lang=en><head><meta charset=utf-8>
<meta name=viewport content="width=device-width,initial-scale=1">
<title>arbor</title>
<style>
:root{--bg:#fbfbfa;--fg:#1a1a19;--dim:#6b6b66;--line:#e6e5e1;--card:#fff;--acc:#3d6b4f}
@media(prefers-color-scheme:dark){:root{--bg:#131312;--fg:#e8e8e4;--dim:#8a8a84;--line:#2a2a27;--card:#1b1b19;--acc:#7fb894}}
*{box-sizing:border-box}
body{margin:0;padding:2rem 1.25rem;background:var(--bg);color:var(--fg);
 font:15px/1.55 ui-sans-serif,-apple-system,"Segoe UI",system-ui,sans-serif}
main{max-width:1080px;margin:0 auto}
h1{font-size:1.35rem;margin:0;letter-spacing:-.01em}
h1 span{color:var(--dim);font-weight:400;font-size:.8rem;margin-left:.5rem}
h2{font-size:.8rem;text-transform:uppercase;letter-spacing:.08em;color:var(--dim);
 margin:2.5rem 0 .75rem;font-weight:600}
header{display:flex;align-items:baseline;justify-content:space-between;gap:1rem;flex-wrap:wrap}
.stats{display:flex;gap:.5rem;flex-wrap:wrap}
.stat{background:var(--card);border:1px solid var(--line);border-radius:8px;padding:.5rem .8rem}
.stat b{display:block;font-size:1.15rem;font-variant-numeric:tabular-nums}
.stat span{font-size:.72rem;color:var(--dim);text-transform:uppercase;letter-spacing:.05em}
.wrap{overflow-x:auto;background:var(--card);border:1px solid var(--line);border-radius:10px}
table{border-collapse:collapse;width:100%;font-size:.88rem}
th{text-align:left;font-size:.7rem;text-transform:uppercase;letter-spacing:.06em;
 color:var(--dim);padding:.6rem .8rem;border-bottom:1px solid var(--line);font-weight:600;white-space:nowrap}
td{padding:.6rem .8rem;border-bottom:1px solid var(--line);vertical-align:top}
tr:last-child td{border-bottom:0}
td.n{text-align:right;font-variant-numeric:tabular-nums;white-space:nowrap}
.sub{color:var(--dim);font-size:.76rem;margin-top:.15rem;word-break:break-all}
.err{color:#c0392b;font-size:.76rem;margin-top:.25rem}
.empty{text-align:center;color:var(--dim);padding:2rem 1rem}
.empty code{display:block;margin-top:.75rem;font-size:.75rem;white-space:pre-wrap;text-align:left}
.pill{display:inline-block;padding:.1rem .5rem;border-radius:999px;font-size:.72rem;
 border:1px solid var(--line);white-space:nowrap}
.pill.ready,.pill.posted{color:var(--acc);border-color:currentColor}
.pill.error,.pill.failed,.pill\.post-failed{color:#c0392b;border-color:currentColor}
.pill.indexing,.pill.cloning,.pill.syncing,.pill.running{color:#b8860b;border-color:currentColor}
footer{margin-top:2.5rem;color:var(--dim);font-size:.76rem}
code{font-family:ui-monospace,SFMono-Regular,Menlo,monospace}
</style></head><body><main>

<header>
  <h1>arbor <span>v{{VERSION}}</span></h1>
  <div class=stats>
    <div class=stat><b>{{READY}}</b><span>indexed</span></div>
    <div class=stat><b>{{QUEUED}}</b><span>queued</span></div>
    <div class=stat><b>{{RUNNING}}</b><span>running</span></div>
    <div class=stat><b>${{COST}}</b><span>recent spend</span></div>
  </div>
</header>

<h2>Repositories</h2>
<div class=wrap><table>
<tr><th>repository</th><th>state</th><th>files</th><th>nodes</th><th>edges</th>
    <th>index</th><th>tokens 24h</th><th>cost 24h</th></tr>
{{REPOS}}
</table></div>

<h2>Recent answers</h2>
<div class=wrap><table>
<tr><th>issue</th><th>status</th><th>kind</th><th>tokens</th><th>cost</th><th>time</th><th></th></tr>
{{RUNS}}
</table></div>

<footer>Refreshes every 10s.</footer>
</main>
<script>setTimeout(()=>location.reload(),10000)</script>
</body></html>"#;
