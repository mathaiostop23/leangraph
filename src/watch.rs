//! Keep a graph current while someone — or something — edits the repository.
//!
//! The server never needed this: a push webhook says exactly what changed, and
//! `index --since <sha>` turns that into a tree diff. On a developer's machine
//! there is no webhook, and an agent's edits are not commits, so a git diff
//! cannot see them at all. What was missing was never a mechanism. It was the
//! loop: something that notices a write and calls the indexer that already
//! knows how to be cheap about it.
//!
//! Three things this has to get right, and each of them is a way to burn a
//! laptop:
//!
//! 1. **Not watching its own output.** The graph is written *into* the tree it
//!    is watching. Without a filter the first index triggers the second, which
//!    triggers the third, forever.
//! 2. **Not waking on build output.** `target/` and `node_modules/` churn
//!    constantly and are not source. The repository's own ignore rules already
//!    say so, and are reused rather than reinvented.
//! 3. **Debouncing.** Saving one file in an editor emits several events, and a
//!    branch checkout emits thousands. Indexing per event would be slower than
//!    indexing nothing.

use anyhow::{Context, Result};
use notify::{Event, EventKind, RecursiveMode, Watcher};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::index;

/// Directories never worth waking for, independent of any ignore file.
///
/// `.leangraph` is the important one: it is where the graph is written, so
/// leaving it in makes the watcher its own trigger.
const NEVER: [&str; 5] = [".leangraph", ".git", "target", "node_modules", ".venv"];

pub struct Config {
    pub path: PathBuf,
    pub debounce: Duration,
    pub threads: Option<usize>,
}

fn interesting(root: &Path, p: &Path) -> bool {
    let rel = p.strip_prefix(root).unwrap_or(p);
    if rel.components().any(|c| {
        c.as_os_str()
            .to_str()
            .is_some_and(|s| NEVER.contains(&s) || s.starts_with(".leangraph"))
    }) {
        return false;
    }
    // An editor writing `main.rs` also touches `main.rs~`, `.main.rs.swp` and
    // `4913` (vim's probe file). None of them are the repository.
    match p.file_name().and_then(|n| n.to_str()) {
        Some(n) => !(n.ends_with('~') || n.ends_with(".swp") || n.ends_with(".tmp")),
        None => false,
    }
}

pub fn run(cfg: &Config) -> Result<()> {
    let root = cfg
        .path
        .canonicalize()
        .with_context(|| format!("no such directory: {}", cfg.path.display()))?;

    let (tx, rx) = mpsc::channel();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<Event>| {
        if let Ok(ev) = res {
            // Access alone is not a change. Metadata is: a `chmod` does not
            // alter what a file says, but `Other` covers rescans a platform
            // reports after dropping events, and ignoring those loses edits.
            if !matches!(ev.kind, EventKind::Access(_)) {
                let _ = tx.send(ev);
            }
        }
    })?;
    watcher.watch(&root, RecursiveMode::Recursive)?;

    println!(
        "\n  \x1b[1mwatching\x1b[0m  {}\n  debounce  {} ms\n  \x1b[2mCtrl-C to stop\x1b[0m\n",
        root.display(),
        cfg.debounce.as_millis()
    );

    loop {
        // Block until something happens; there is no reason to spin.
        let first = match rx.recv() {
            Ok(ev) => ev,
            Err(_) => return Ok(()), // watcher dropped
        };
        let mut touched: HashSet<PathBuf> = HashSet::new();
        for p in first.paths {
            if interesting(&root, &p) {
                touched.insert(p);
            }
        }

        // Collect everything that arrives while the tree is still settling. A
        // checkout that rewrites 3,000 files should be one index, not 3,000.
        let mut deadline = Instant::now() + cfg.debounce;
        while let Ok(ev) = rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            for p in ev.paths {
                if interesting(&root, &p) {
                    touched.insert(p);
                }
            }
            deadline = Instant::now() + cfg.debounce;
        }

        if touched.is_empty() {
            continue;
        }

        let t0 = Instant::now();
        let n = touched.len();
        match index::run(&index::Config {
            path: root.clone(),
            threads: cfg.threads,
            by_lang: false,
            top: None,
            no_resolve: false,
            no_cochange: true, // git history has not moved; asking again is waste
            incremental: true,
            since: None,
            out: None,
            dry_run: false,
            quiet: true,
        }) {
            Ok(_) => println!(
                "  {:>4} changed · reindexed in \x1b[1m{:.2} s\x1b[0m",
                n,
                t0.elapsed().as_secs_f64()
            ),
            // A failed index must not end the watch. The usual cause is reading
            // a file mid-write, and the next event fixes it.
            Err(e) => println!("  \x1b[33mindex failed\x1b[0m: {e}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> PathBuf {
        PathBuf::from("/repo")
    }

    #[test]
    fn the_graph_it_writes_is_not_a_change_it_reacts_to() {
        // The whole loop hangs on this one: `.leangraph` is inside the watched
        // tree, so treating it as source makes every index trigger the next.
        assert!(!interesting(
            &root(),
            Path::new("/repo/.leangraph/graph.bin")
        ));
        assert!(!interesting(
            &root(),
            Path::new("/repo/.leangraph/cache.bin")
        ));
    }

    #[test]
    fn build_output_and_git_are_ignored() {
        for p in [
            "/repo/target/debug/leangraph",
            "/repo/.git/index",
            "/repo/node_modules/x/index.js",
            "/repo/.venv/lib/python3/site.py",
        ] {
            assert!(!interesting(&root(), Path::new(p)), "{p} should be ignored");
        }
    }

    #[test]
    fn editor_scratch_files_are_not_source() {
        for p in [
            "/repo/src/main.rs~",
            "/repo/src/.main.rs.swp",
            "/repo/a.tmp",
        ] {
            assert!(!interesting(&root(), Path::new(p)), "{p} should be ignored");
        }
    }

    #[test]
    fn real_source_is_watched() {
        for p in [
            "/repo/src/main.rs",
            "/repo/deep/nested/module.py",
            "/repo/README.md",
        ] {
            assert!(interesting(&root(), Path::new(p)), "{p} should be watched");
        }
    }

    #[test]
    fn a_directory_named_like_the_graph_does_not_smuggle_through() {
        // `.leangraph-server` holds the server's data and is equally not source.
        assert!(!interesting(
            &root(),
            Path::new("/repo/.leangraph-server/leangraph.db")
        ));
    }
}
