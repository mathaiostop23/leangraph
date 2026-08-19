//! Cloning and updating repositories.
//!
//! The token is never written to disk. `git clone https://token@host/...` puts
//! the credential in `.git/config`, where it survives the process, gets copied
//! by backups, and shows up in any bug report that includes the config. Passing
//! it per-invocation via `http.extraHeader` keeps it in the process environment
//! and nowhere else.
//!
//! It is still visible in the child's argv for the life of the call, which is a
//! real if narrow exposure on a shared host. The alternative — a credential
//! helper — needs a second executable and a socket; noted rather than pretended
//! away.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};
use std::process::Command;

/// Where a repository lives under the data directory. Derived from the name
/// rather than stored, so the layout is inspectable and a stale row cannot point
/// somewhere unexpected.
pub fn work_dir(data_dir: &Path, full_name: &str) -> PathBuf {
    let safe: String = full_name
        .chars()
        .map(|c| if c.is_alphanumeric() || c == '-' || c == '_' || c == '/' { c } else { '-' })
        .collect();
    data_dir.join("repos").join(safe)
}

/// Reject anything that is not a plain https git URL.
///
/// `file://` would read the host filesystem, `ssh://` and the scp-like
/// `git@host:path` form would use ambient key material, and `ext::` runs an
/// arbitrary command. A URL arrives over the API, so none of those may be
/// reachable from it.
pub fn check_url(url: &str) -> Result<()> {
    let u = url.trim();
    if !u.starts_with("https://") {
        bail!("only https:// URLs are accepted");
    }
    if u.contains('@') {
        bail!("credentials in the URL are not accepted; configure a token instead");
    }
    if u.contains("..") || u.contains('\n') || u.contains('\r') {
        bail!("malformed URL");
    }
    Ok(())
}

fn run(dir: Option<&Path>, token: Option<&str>, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    // Never prompt: a hung credential prompt inside a worker is a job that
    // never finishes and a queue that never drains.
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    if let Some(t) = token {
        cmd.arg("-c")
            .arg(format!("http.extraHeader=Authorization: Bearer {t}"));
    }
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    let out = cmd.output().context("running git")?;
    if !out.status.success() {
        // The token cannot appear here — it is in a `-c` argument, not the
        // output — but scrub anyway rather than rely on that staying true.
        let err = String::from_utf8_lossy(&out.stderr);
        let err = match token {
            Some(t) => err.replace(t, "***"),
            None => err.into_owned(),
        };
        bail!("git {}: {}", args.first().unwrap_or(&""), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Clone if absent, otherwise fast-forward to the remote branch.
///
/// Returns the HEAD before and after, so a sync can diff two trees instead of
/// walking — the cheap path measured in BENCH.md.
pub fn fetch(
    dir: &Path,
    url: &str,
    branch: &str,
    token: Option<&str>,
) -> Result<(Option<String>, String)> {
    check_url(url)?;

    if !dir.join(".git").is_dir() {
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent)?;
        }
        // Blobless rather than shallow: co-change needs history, and `--depth`
        // would make `git log` useless while a filtered clone keeps it and
        // fetches file contents lazily.
        run(
            None,
            token,
            &[
                "clone",
                "--filter=blob:none",
                "--branch",
                branch,
                url,
                &dir.to_string_lossy(),
            ],
        )?;
        // A blobless clone fetches history lazily, and the first `git log
        // --name-only` — which co-change runs — triggers those fetches. Measured
        // on a 677-commit repo: the first index took 6.8s where every later one
        // took 44ms. Doing it here moves that cost into the clone, where a user
        // expects to wait, instead of into the first answer, where they do not.
        warm_history(dir, token);
        let head = head(dir, token).context("cloned repository has no HEAD")?;
        return Ok((None, head));
    }

    let before = head(dir, token);
    run(Some(dir), token, &["fetch", "--prune", "origin", branch])?;
    // Reset rather than merge: this is a mirror of the remote, and a merge
    // conflict in a directory nobody edits is a job that fails forever.
    run(Some(dir), token, &["reset", "--hard", &format!("origin/{branch}")])?;
    let after = head(dir, token).context("repository has no HEAD after fetch")?;
    Ok((before, after))
}

/// Force any lazy object fetches a partial clone deferred. Best effort: a
/// failure here costs latency later, not correctness.
///
/// `--no-renames` matters more than it looks. Rename detection compares blob
/// contents, and on a blobless clone every comparison is a lazy fetch — one
/// network round trip per historical blob. Measured on pallets/click: 23.8s
/// with it, 0.04s without, against a clone that itself took 2.2s. The argument
/// list here must stay in step with `cochange::edges`, since the point is to
/// warm exactly what that call will need.
fn warm_history(dir: &Path, token: Option<&str>) {
    let _ = run(
        Some(dir),
        token,
        &[
            "log",
            "--no-merges",
            "--no-renames",
            "-n",
            "3000",
            "--name-only",
            "--format=%H",
        ],
    );
}

pub fn head(dir: &Path, token: Option<&str>) -> Option<String> {
    run(Some(dir), token, &["rev-parse", "HEAD"])
        .ok()
        .map(|s| s.trim().to_string())
}

/// `https://github.com/owner/repo(.git)` -> `owner/repo`.
pub fn name_from_url(url: &str) -> Option<String> {
    let rest = url.trim().trim_end_matches('/').strip_prefix("https://")?;
    let mut parts = rest.splitn(2, '/');
    let _host = parts.next()?;
    let path = parts.next()?.trim_end_matches(".git");
    (path.matches('/').count() == 1 && !path.is_empty()).then(|| path.to_string())
}
