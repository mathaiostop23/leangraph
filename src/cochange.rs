//! Co-change edges from git history.
//!
//! Static analysis sees what the code says. It cannot see that a settings file
//! must always be edited alongside a particular module, that a fix in a parser
//! always needs its fixture updated, or that two modules are coupled by a
//! convention nobody wrote down. Git knows all of it, and `git log` is the
//! cheapest signal in the whole pipeline.
//!
//! This is what the cost benchmark identified as the recall ceiling: the
//! ground-truth files an approach cannot reach are tests, docs and migrations
//! touched by a fix but never named in its message. No amount of budget finds
//! them; a different signal does.
//!
//! Every edge produced here is tagged `Provenance::CoChange` and carries lower
//! confidence than anything derived from the AST. It is correlation, and it is
//! labelled as correlation.

use crate::core::{Edge, EdgeKind, FileId, Provenance};
use crate::resolve::NodeSpace;
use rustc_hash::FxHashMap;
use std::path::Path;
use std::process::Command;

pub struct Opts {
    /// How far back to look.
    pub commits: usize,
    /// Commits touching more than this are ignored. A formatting sweep or a
    /// dependency bump touches hundreds of files and would couple all of them
    /// to each other — pure noise, and quadratically expensive.
    pub max_files_per_commit: usize,
    /// Two files must have changed together at least this often. Below this it
    /// is coincidence.
    pub min_support: u32,
    /// Keep only the strongest partners per file, so one busy file does not
    /// dominate the graph.
    pub max_partners: usize,
}

impl Default for Opts {
    fn default() -> Self {
        Opts {
            commits: 3000,
            max_files_per_commit: 20,
            min_support: 3,
            max_partners: 8,
        }
    }
}

#[derive(Default)]
pub struct Stats {
    pub commits_scanned: usize,
    pub commits_used: usize,
    pub edges: usize,
}

/// Returns symmetric file-to-file edges plus what it took to find them.
pub fn edges(
    root: &Path,
    by_path: &FxHashMap<String, FileId>,
    space: &NodeSpace,
    opts: &Opts,
) -> (Vec<Edge>, Stats) {
    let mut stats = Stats::default();

    let out = Command::new("git")
        .args([
            "log",
            "--no-merges",
            "-n",
            &opts.commits.to_string(),
            "--pretty=format:%x00",
            "--name-only",
        ])
        .current_dir(root)
        .output();
    let Ok(out) = out else {
        return (Vec::new(), stats);
    };
    if !out.status.success() {
        return (Vec::new(), stats); // not a git repo, or git unavailable
    }
    let text = String::from_utf8_lossy(&out.stdout);

    // pair -> how many commits touched both
    let mut support: FxHashMap<(FileId, FileId), u32> = FxHashMap::default();

    for commit in text.split('\0') {
        if commit.trim().is_empty() {
            continue;
        }
        stats.commits_scanned += 1;
        let mut files: Vec<FileId> = commit
            .lines()
            .filter_map(|l| by_path.get(l.trim()).copied())
            .collect();
        files.sort_unstable();
        files.dedup();
        if files.len() < 2 || files.len() > opts.max_files_per_commit {
            continue;
        }
        stats.commits_used += 1;
        for i in 0..files.len() {
            for j in (i + 1)..files.len() {
                *support.entry((files[i], files[j])).or_default() += 1;
            }
        }
    }

    // Keep each file's strongest partners rather than every pair over the
    // threshold: a file touched in every release commit would otherwise couple
    // to half the repository.
    let mut best: FxHashMap<FileId, Vec<(u32, FileId)>> = FxHashMap::default();
    for (&(a, b), &n) in &support {
        if n < opts.min_support {
            continue;
        }
        best.entry(a).or_default().push((n, b));
        best.entry(b).or_default().push((n, a));
    }

    let mut out_edges = Vec::new();
    for (file, mut partners) in best {
        partners.sort_unstable_by_key(|&(n, _)| std::cmp::Reverse(n));
        partners.truncate(opts.max_partners);
        for (n, other) in partners {
            out_edges.push(Edge {
                src: space.file_node(file),
                dst: space.file_node(other),
                kind: EdgeKind::References,
                // 40 at the threshold, rising with support, never reaching the
                // confidence of an edge the compiler could have told us about.
                conf: (Provenance::CoChange.base_conf() as u32 + n * 3).min(70) as u8,
                prov: Provenance::CoChange,
            });
        }
    }
    stats.edges = out_edges.len();
    (out_edges, stats)
}
