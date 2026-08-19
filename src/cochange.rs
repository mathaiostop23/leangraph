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
    pub reused: bool,
}

/// Returns symmetric file-to-file edges, the path pairs behind them (for the
/// cache), and what it took to find them.
pub fn edges(
    root: &Path,
    by_path: &FxHashMap<String, FileId>,
    space: &NodeSpace,
    opts: &Opts,
) -> (Vec<Edge>, Vec<(String, String, u8)>, Stats) {
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
        return (Vec::new(), Vec::new(), stats);
    };
    if !out.status.success() {
        return (Vec::new(), Vec::new(), stats); // not a git repo, or git unavailable
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

    let id_to_path: FxHashMap<FileId, &String> = by_path.iter().map(|(p, f)| (*f, p)).collect();
    let mut out_edges = Vec::new();
    let mut out_pairs = Vec::new();
    for (file, mut partners) in best {
        partners.sort_unstable_by_key(|&(n, _)| std::cmp::Reverse(n));
        partners.truncate(opts.max_partners);
        for (n, other) in partners {
            // 40 at the threshold, rising with support, never reaching the
            // confidence of an edge the compiler could have told us about.
            let conf = (Provenance::CoChange.base_conf() as u32 + n * 3).min(70) as u8;
            if let (Some(a), Some(b)) = (id_to_path.get(&file), id_to_path.get(&other)) {
                out_pairs.push(((*a).clone(), (*b).clone(), conf));
            }
            out_edges.push(Edge {
                src: space.file_node(file),
                dst: space.file_node(other),
                kind: EdgeKind::References,
                conf,
                prov: Provenance::CoChange,
            });
        }
    }
    // Emitted from a hash map, so impose an order before returning: these are
    // appended after the resolver's sort and would otherwise vary per run.
    out_edges.sort_unstable_by_key(|e| (e.src.0, e.dst.0, e.conf));
    out_pairs.sort_unstable();
    stats.edges = out_edges.len();
    (out_edges, out_pairs, stats)
}


// ------------------------------------------------------------------- cache
//
// `git log -n 3000 --name-only` costs ~150 ms on django — by far the largest
// item in an otherwise-incremental sync, and almost entirely wasted: coupling
// computed over three thousand commits does not meaningfully change because one
// more landed.
//
// So the result is cached against the HEAD it was computed at and reused while
// HEAD has not moved far. Pairs are stored as **paths**, not node ids: a single
// added or deleted file shifts every id after it, and a cache keyed on ids
// would silently describe the wrong files.

use anyhow::Result;
use bytemuck::{Pod, Zeroable};
use std::io::Write;

const CACHE_MAGIC: [u8; 8] = *b"ARBORX\x00\x01";
/// Recompute once HEAD has moved further than this. Small enough that coupling
/// stays current, large enough to make the common sync free.
pub const MAX_DRIFT: u32 = 25;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CPair {
    a: u32,
    b: u32,
    conf: u32,
}

pub fn head(root: &Path) -> Option<String> {
    let out = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// How many commits HEAD has advanced since `since`. `None` when the histories
/// have diverged — a force-push or a branch switch — in which case the cache is
/// not merely stale but wrong, and must be discarded.
pub fn drift(root: &Path, since: &str) -> Option<u32> {
    let out = Command::new("git")
        .args(["rev-list", "--count", &format!("{since}..HEAD")])
        .current_dir(root)
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().parse().ok()
}

pub fn save(path: &Path, head: &str, pairs: &[(String, String, u8)]) -> Result<()> {
    let mut names: Vec<&str> = Vec::new();
    let mut idx: FxHashMap<&str, u32> = FxHashMap::default();
    let mut rows = Vec::with_capacity(pairs.len());
    for (a, b, conf) in pairs {
        for s in [a.as_str(), b.as_str()] {
            if !idx.contains_key(s) {
                idx.insert(s, names.len() as u32);
                names.push(s);
            }
        }
        rows.push(CPair {
            a: idx[a.as_str()],
            b: idx[b.as_str()],
            conf: *conf as u32,
        });
    }

    let mut blob = Vec::new();
    let mut offs = vec![0u32];
    for n in &names {
        blob.extend_from_slice(n.as_bytes());
        offs.push(blob.len() as u32);
    }

    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(&CACHE_MAGIC)?;
        f.write_all(&(head.len() as u32).to_le_bytes())?;
        f.write_all(head.as_bytes())?;
        f.write_all(&(rows.len() as u32).to_le_bytes())?;
        f.write_all(&(offs.len() as u32).to_le_bytes())?;
        f.write_all(&(blob.len() as u32).to_le_bytes())?;
        f.write_all(bytemuck::cast_slice(&rows))?;
        f.write_all(bytemuck::cast_slice(&offs))?;
        f.write_all(&blob)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Returns `(head, pairs)`, or `None` if absent or unreadable.
pub fn load(path: &Path) -> Option<(String, Vec<(String, String, u8)>)> {
    let buf = std::fs::read(path).ok()?;
    let mut p = 0usize;
    let mut take = |n: usize| -> Option<&[u8]> {
        let s = buf.get(p..p + n)?;
        p += n;
        Some(s)
    };
    if take(8)? != CACHE_MAGIC {
        return None;
    }
    let hl = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let head = String::from_utf8(take(hl)?.to_vec()).ok()?;
    let n_rows = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let n_offs = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let n_blob = u32::from_le_bytes(take(4)?.try_into().ok()?) as usize;
    let rows: Vec<CPair> = bytemuck::try_cast_slice(take(n_rows * 12)?).ok()?.to_vec();
    let offs: Vec<u32> = bytemuck::try_cast_slice(take(n_offs * 4)?).ok()?.to_vec();
    let blob = take(n_blob)?.to_vec();

    let name = |i: u32| -> Option<String> {
        let a = *offs.get(i as usize)? as usize;
        let b = *offs.get(i as usize + 1)? as usize;
        String::from_utf8(blob.get(a..b)?.to_vec()).ok()
    };
    let mut out = Vec::with_capacity(rows.len());
    for r in &rows {
        out.push((name(r.a)?, name(r.b)?, r.conf as u8));
    }
    Some((head, out))
}

/// Turn cached path pairs into edges against the current file ids, dropping
/// anything whose file no longer exists.
pub fn pairs_to_edges(
    pairs: &[(String, String, u8)],
    by_path: &FxHashMap<String, FileId>,
    space: &NodeSpace,
) -> Vec<Edge> {
    pairs
        .iter()
        .filter_map(|(a, b, conf)| {
            Some(Edge {
                src: space.file_node(*by_path.get(a)?),
                dst: space.file_node(*by_path.get(b)?),
                kind: EdgeKind::References,
                conf: *conf,
                prov: Provenance::CoChange,
            })
        })
        .collect()
}
