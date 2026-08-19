//! Editor wiring.
//!
//! Every target gets an explicit `-p <absolute repo path>`. Cursor is known to
//! launch MCP subprocesses with the wrong working directory and not to pass
//! `rootUri` in `initialize`, and a server that silently indexes the wrong
//! directory is worse than one that fails loudly — so nobody gets to rely on
//! cwd.
//!
//! Edits are merged, not overwritten: these files hold the user's other MCP
//! servers, and clobbering them to install ours would be unforgivable.

use anyhow::{Context, Result};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Target {
    ClaudeCode,
    Cursor,
    Codex,
}

pub const ALL: [Target; 3] = [Target::ClaudeCode, Target::Cursor, Target::Codex];

impl Target {
    pub fn name(self) -> &'static str {
        match self {
            Target::ClaudeCode => "claude-code",
            Target::Cursor => "cursor",
            Target::Codex => "codex",
        }
    }

    pub fn parse(s: &str) -> Option<Target> {
        match s.to_ascii_lowercase().replace('_', "-").as_str() {
            "claude" | "claude-code" => Some(Target::ClaudeCode),
            "cursor" => Some(Target::Cursor),
            "codex" => Some(Target::Codex),
            _ => None,
        }
    }

    /// Config file this target reads. Claude Code and Cursor are per-project;
    /// Codex is user-global.
    fn config(self, repo: &Path) -> Option<PathBuf> {
        match self {
            Target::ClaudeCode => Some(repo.join(".mcp.json")),
            Target::Cursor => Some(repo.join(".cursor").join("mcp.json")),
            Target::Codex => dirs_home().map(|h| h.join(".codex").join("config.toml")),
        }
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

#[derive(Debug)]
pub enum Outcome {
    Installed(PathBuf),
    Unchanged(PathBuf),
    Removed(PathBuf),
    Skipped(&'static str),
}

fn exe() -> String {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.to_str().map(str::to_string))
        .unwrap_or_else(|| "arbor".into())
}

fn server_entry(repo: &Path) -> Value {
    json!({
        "command": exe(),
        "args": ["serve", "--mcp", "-p", repo.to_string_lossy()]
    })
}

fn read_json(path: &Path) -> Value {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}))
}

fn write_json(path: &Path, v: &Value) -> Result<()> {
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let mut s = serde_json::to_string_pretty(v)?;
    s.push('\n');
    std::fs::write(path, s).with_context(|| format!("writing {}", path.display()))
}

fn install_json(path: &Path, repo: &Path) -> Result<Outcome> {
    let mut doc = read_json(path);
    let entry = server_entry(repo);
    let servers = doc
        .as_object_mut()
        .expect("object")
        .entry("mcpServers")
        .or_insert_with(|| Value::Object(Map::new()));
    let Some(map) = servers.as_object_mut() else {
        return Ok(Outcome::Skipped("mcpServers is not an object"));
    };
    if map.get("arbor") == Some(&entry) {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    }
    map.insert("arbor".into(), entry);
    write_json(path, &doc)?;
    Ok(Outcome::Installed(path.to_path_buf()))
}

fn uninstall_json(path: &Path) -> Result<Outcome> {
    if !path.exists() {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    }
    let mut doc = read_json(path);
    let removed = doc
        .get_mut("mcpServers")
        .and_then(Value::as_object_mut)
        .map(|m| m.remove("arbor").is_some())
        .unwrap_or(false);
    if !removed {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    }
    write_json(path, &doc)?;
    Ok(Outcome::Removed(path.to_path_buf()))
}

/// Codex uses TOML. We only touch our own `[mcp_servers.arbor]` table and leave
/// every sibling line byte-identical — a hand-rolled edit rather than a parse
/// and re-emit, so user comments and formatting survive.
fn install_toml(path: &Path, repo: &Path) -> Result<Outcome> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let block = format!(
        "[mcp_servers.arbor]\ncommand = \"{}\"\nargs = [\"serve\", \"--mcp\", \"-p\", \"{}\"]\n",
        exe(),
        repo.to_string_lossy()
    );
    let stripped = strip_toml_block(&existing);
    if stripped.trim() == existing.trim() && existing.contains(&block) {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    }
    let mut out = stripped;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    if !out.is_empty() {
        out.push('\n');
    }
    out.push_str(&block);
    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    std::fs::write(path, out).with_context(|| format!("writing {}", path.display()))?;
    Ok(Outcome::Installed(path.to_path_buf()))
}

fn strip_toml_block(src: &str) -> String {
    let mut out = String::with_capacity(src.len());
    let mut skipping = false;
    for line in src.lines() {
        let t = line.trim_start();
        if t.starts_with('[') {
            skipping = t.starts_with("[mcp_servers.arbor]");
        }
        if !skipping {
            out.push_str(line);
            out.push('\n');
        }
    }
    out
}

fn uninstall_toml(path: &Path) -> Result<Outcome> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    };
    let stripped = strip_toml_block(&existing);
    if stripped == existing {
        return Ok(Outcome::Unchanged(path.to_path_buf()));
    }
    std::fs::write(path, stripped)?;
    Ok(Outcome::Removed(path.to_path_buf()))
}

pub fn apply(targets: &[Target], repo: &Path, remove: bool) -> Result<Vec<(Target, Outcome)>> {
    let repo = repo.canonicalize().unwrap_or_else(|_| repo.to_path_buf());
    let mut out = Vec::new();
    for &t in targets {
        let Some(cfg) = t.config(&repo) else {
            out.push((t, Outcome::Skipped("cannot locate home directory")));
            continue;
        };
        let res = match (t, remove) {
            (Target::Codex, false) => install_toml(&cfg, &repo),
            (Target::Codex, true) => uninstall_toml(&cfg),
            (_, false) => install_json(&cfg, &repo),
            (_, true) => uninstall_json(&cfg),
        }?;
        out.push((t, res));
    }
    Ok(out)
}
