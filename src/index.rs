//! Indexing pipeline: discover -> extract -> resolve -> persist.

use crate::core::{FileUnit, Interner};
use crate::extract::{extract_file, Timings};
use crate::cache;
use crate::cochange;
use crate::graph;
use crate::idtable::IdTable;
use crate::lang::{spec_for, Lang, Spec, ALL_LANGS};
use crate::resolve;
use anyhow::{Context, Result};
use ignore::WalkBuilder;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;
use tree_sitter::Parser as TsParser;

const MAX_FILE_BYTES: u64 = 1024 * 1024; // matches CodeGraph's skip, for fair comparison

/// What an index run produced, for callers that need the numbers rather than
/// the printout.
#[derive(Debug, Clone, Copy, Default)]
pub struct Summary {
    pub files: usize,
    pub nodes: u32,
    pub edges: usize,
}

pub struct Config {
    pub path: PathBuf,
    pub threads: Option<usize>,
    pub by_lang: bool,
    pub top: Option<usize>,
    pub no_resolve: bool,
    pub no_cochange: bool,
    /// Reuse cached extraction for files whose size and mtime are unchanged.
    pub incremental: bool,
    /// Commit to diff against, tree-to-tree. For a push webhook that knows the
    /// previous head; skips discovery almost entirely.
    pub since: Option<String>,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
    /// Suppress the human-facing report. The server wants the numbers, not the
    /// terminal output.
    pub quiet: bool,
}

thread_local! {
    static PARSERS: RefCell<FxHashMap<Lang, TsParser>> = RefCell::new(FxHashMap::default());
}

fn with_parser<R>(lang: Lang, f: impl FnOnce(&mut TsParser) -> R) -> R {
    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let p = map.entry(lang).or_insert_with(|| {
            let mut p = TsParser::new();
            p.set_language(&lang.ts_language())
                .expect("grammar/ABI mismatch");
            p
        });
        f(p)
    })
}

fn git(root: &std::path::Path, args: &[&str]) -> Option<String> {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Files in the HEAD tree. A tree read, not a filesystem walk.
fn git_list_tree(root: &std::path::Path) -> Option<Vec<PathBuf>> {
    Some(
        git(root, &["ls-tree", "-r", "--name-only", "-z", "HEAD"])?
            .split('\0')
            .filter(|l| !l.is_empty())
            .map(|l| root.join(l))
            .collect(),
    )
}

/// Paths differing between two commits — tree against tree, so git never
/// touches the working copy. This is what makes the server path cheap, and why
/// it is not used for a local sync where the working tree may have edits git
/// would have to stat to find.
fn git_changed_between(
    root: &std::path::Path,
    since: &str,
) -> Option<rustc_hash::FxHashSet<PathBuf>> {
    Some(
        git(root, &["diff", "--name-only", "-z", since, "HEAD"])?
            .split('\0')
            .filter(|l| !l.is_empty())
            .map(|l| root.join(l))
            .collect(),
    )
}

fn out_path_exists(cfg: &Config, root: &std::path::Path) -> bool {
    out_dir(cfg, root).join("graph.bin").exists()
}

fn out_dir(cfg: &Config, root: &std::path::Path) -> PathBuf {
    cfg.out
        .as_ref()
        .and_then(|p| p.parent().map(std::path::Path::to_path_buf))
        .unwrap_or_else(|| root.join(".leangraph"))
}

pub fn run(cfg: &Config) -> Result<Summary> {

        let threads = cfg
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let root = cfg
        .path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", cfg.path.display()))?;

    // ---- discover ----------------------------------------------------------
    // If the cache knows which commit it was built at and git can tell us what
    // has moved since, we can skip the tree walk entirely. On a server this is
    // the normal case: a push tells you exactly what changed, and stat-ing 3,000
    // files to rediscover it is pure cost.
    let cache_path_early = out_dir(cfg, &root).join("units.bin");
    let git_since = cfg
        .incremental
        .then(|| cache::read_head(&cache_path_early))
        .flatten();


    // Parallel walk, and size+mtime captured in the same `stat` the walker
    // already performs. Doing it twice — once for the size limit, once for the
    // cache check — was costing 3,000 extra syscalls on django.
    let t0 = Instant::now();

    // Server fast path, and only that.
    //
    // Measured: asking git what changed against the *working tree* is slower
    // than walking it (140 ms vs 37 ms on django), because git has to stat every
    // tracked file to answer. Comparing two commits is a tree read and costs
    // almost nothing — which is exactly what a push webhook can supply. So this
    // path is opt-in via `--since <sha>` rather than inferred, and everything
    // else walks.
    let git_touched: Option<rustc_hash::FxHashSet<PathBuf>> = cfg
        .since
        .as_deref()
        .and_then(|since| git_changed_between(&root, since));
    let _ = &git_since;
    let git_listing = git_touched
        .as_ref()
        .and_then(|_| git_list_tree(&root))
        .map(|all| {
            all.into_iter()
                .filter_map(|p| {
                    // The walker runs with `hidden(true)`, so `.github/` and
                    // friends are excluded. git does not know that, and a
                    // listing that disagrees with the walk produces a different
                    // graph depending on which path ran — which the convergence
                    // test caught immediately.
                    if p.strip_prefix(&root).unwrap_or(&p).components().any(|c| {
                        c.as_os_str().to_str().is_some_and(|s| s.starts_with('.'))
                    }) {
                        return None;
                    }
                    let lang = p
                        .extension()
                        .and_then(|e| e.to_str())
                        .and_then(Lang::from_ext)?;
                    Some((p, lang))
                })
                .collect::<Vec<_>>()
        })
        .filter(|v: &Vec<(PathBuf, Lang)>| !v.is_empty());

    let (tx, rx) = std::sync::mpsc::channel::<(PathBuf, Lang, u64, i64)>();
    let oversized = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));

    if git_listing.is_none() {
        let oversized = oversized.clone();
        WalkBuilder::new(&root)
            .hidden(true)
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .follow_links(false)
            .threads(threads)
            .build_parallel()
            .run(|| {
                let tx = tx.clone();
                let oversized = oversized.clone();
                Box::new(move |res| {
                    use ignore::WalkState;
                    let Ok(entry) = res else {
                        return WalkState::Continue;
                    };
                    if !entry.file_type().is_some_and(|t| t.is_file()) {
                        return WalkState::Continue;
                    }
                    let path = entry.path();
                    let Some(lang) = path
                        .extension()
                        .and_then(|e| e.to_str())
                        .and_then(Lang::from_ext)
                    else {
                        return WalkState::Continue;
                    };
                    let Ok(md) = entry.metadata() else {
                        return WalkState::Continue;
                    };
                    if md.len() > MAX_FILE_BYTES {
                        oversized.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return WalkState::Continue;
                    }
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    let _ = tx.send((path.to_path_buf(), lang, md.len(), mtime));
                    WalkState::Continue
                })
            });
    }
    drop(tx);

    let mut found: Vec<(PathBuf, Lang, u64, i64)> = match &git_listing {
        // Unchanged files need no `stat`: git says they did not move, and the
        // cache already holds their size and mtime. Only touched files are
        // stat-ed, and one that no longer exists is a deletion.
        // Only touched files are stat-ed; the rest are known unchanged from the
        // tree diff, and their size and mtime come from the cache. On a push of
        // a handful of files this is a handful of syscalls instead of 3,000.
        Some(list) => {
            let touched = git_touched.as_ref().expect("listing implies touched");
            list.par_iter()
                .filter_map(|(p, lang)| {
                    if !touched.contains(p) {
                        return Some((p.clone(), *lang, 0, 0));
                    }
                    let md = std::fs::metadata(p).ok()?;
                    if md.len() > MAX_FILE_BYTES {
                        oversized.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                        return None;
                    }
                    let mtime = md
                        .modified()
                        .ok()
                        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
                        .map(|d| d.as_secs() as i64)
                        .unwrap_or(0);
                    Some((p.clone(), *lang, md.len(), mtime))
                })
                .collect()
        }
        None => rx.into_iter().collect(),
    };
    // The parallel walker yields in completion order; sort so a given repo
    // always produces the same FileIds and therefore a byte-identical graph.
    found.sort_unstable_by(|a, b| a.0.cmp(&b.0));
    let oversized = oversized.load(std::sync::atomic::Ordering::Relaxed);
    let d_discover = t0.elapsed();

    if found.is_empty() {
        if !cfg.quiet {
            println!("no Python/TypeScript files found under {}", root.display());
        }
        return Ok(Summary::default());
    }

    // ---- extract (cache-aware) ---------------------------------------------
    let specs: FxHashMap<Lang, Spec> = ALL_LANGS.into_iter().map(|l| (l, spec_for(l))).collect();
    let interner = Interner::new();
    let cache_path = cache_path_early;

    // Prior extraction, keyed by path. Anything whose size and mtime still
    // match is reused without touching the parser — which is 75-82% of our CPU.
    let mut prior: FxHashMap<PathBuf, cache::Entry> = FxHashMap::default();
    if cfg.incremental {
        if let Ok(entries) = cache::read(&cache_path, &interner, &root) {
            prior = entries.into_iter().map(|e| (e.path.clone(), e)).collect();
        }
    }

    let t1 = Instant::now();
    let reused_flag: Vec<bool> = found
        .iter()
        .map(|(path, _, size, mtime)| {
            let Some(e) = prior.get(path) else { return false };
            match &git_touched {
                Some(touched) => !touched.contains(path),
                None => e.meta.size == *size && e.meta.mtime == *mtime,
            }
        })
        .collect();

    let raw: Vec<Option<(FileUnit, Timings, cache::FileMeta)>> = found
        .par_iter()
        .enumerate()
        .map(|(i, (path, lang, _, _))| {
            if reused_flag[i] {
                return None; // filled from cache below
            }
            with_parser(*lang, |p| {
                extract_file(path, i as u32, *lang, &specs[lang], p, &interner)
            })
        })
        .collect();

    // Compact into aligned vectors so that vector index == FileId. Files that
    // failed to open are dropped, so positions must be reassigned.
    let mut units = Vec::with_capacity(raw.len());
    let mut paths = Vec::with_capacity(raw.len());
    let mut langs = Vec::with_capacity(raw.len());
    let mut metas: Vec<cache::FileMeta> = Vec::with_capacity(raw.len());
    let mut agg = Timings::default();
    let mut errors = 0u64;
    let mut reused = 0u64;
    let mut by_lang: FxHashMap<Lang, (u64, u64, u64, u64)> = FxHashMap::default();

    for (i, ((path, lang, _, _), slot)) in found.iter().zip(raw).enumerate() {
        let (mut unit, bytes, meta) = if reused_flag[i] {
            let e = prior.remove(path).expect("checked above");
            reused += 1;
            (e.unit, e.meta.size, e.meta)
        } else {
            let Some((unit, t, meta)) = slot else { continue };
            agg.ns_read += t.ns_read;
            agg.ns_hash += t.ns_hash;
            agg.ns_parse += t.ns_parse;
            agg.ns_walk += t.ns_walk;
            agg.ast_nodes += t.ast_nodes;
            (unit, t.bytes, meta)
        };
        agg.bytes += bytes;
        unit.file = units.len() as u32;
        errors += unit.had_parse_error as u64;
        let e = by_lang.entry(*lang).or_default();
        e.0 += 1;
        e.1 += bytes;
        e.2 += unit.defs.len() as u64;
        e.3 += unit.refs.len() as u64;
        units.push(unit);
        paths.push(path.clone());
        langs.push(*lang);
        metas.push(meta);
    }
    let d_extract = t1.elapsed();

    let defs: u64 = units.iter().map(|u| u.defs.len() as u64).sum();
    let refs: u64 = units.iter().map(|u| u.refs.len() as u64).sum();
    let imports: u64 = units.iter().map(|u| u.imports.len() as u64).sum();

    // ---- resolve -----------------------------------------------------------
    let t2 = Instant::now();
    let out_path = out_dir(cfg, &root).join("graph.bin");

    // Previous id assignment, so surviving nodes keep the ids they had. Absent
    // or from an older format, we start clean and assign everything fresh.
    let mut ids = if cfg.incremental {
        graph::read_keys(&out_path)
            .map(|k| IdTable::from_keys(&k))
            .unwrap_or_default()
    } else {
        IdTable::default()
    };
    let mut resolved = (!cfg.no_resolve)
        .then(|| resolve::resolve(&units, &paths, &langs, &root, &interner, &mut ids));
    let d_resolve = t2.elapsed();

    // ---- co-change ---------------------------------------------------------
    // Correlation from git history, tagged as such. This is the signal the cost
    // benchmark identified as the recall ceiling: files a fix touches but never
    // names.
    let t_cc = Instant::now();
    let mut cochange_stats = cochange::Stats::default();
    if let Some(r) = resolved.as_mut() {
        if !cfg.no_cochange {
            let by_path: rustc_hash::FxHashMap<String, u32> = paths
                .iter()
                .enumerate()
                .map(|(i, p)| {
                    (
                        p.strip_prefix(&root)
                            .unwrap_or(p)
                            .to_string_lossy()
                            .replace('\\', "/"),
                        i as u32,
                    )
                })
                .collect();
            let cc_path = out_dir(cfg, &root).join("cochange.bin");
            let cached = cfg
                .incremental
                .then(|| cochange::load(&cc_path))
                .flatten()
                .filter(|(head, _)| {
                    // Reuse while HEAD has not moved far. `drift` returns None
                    // on a diverged history — force-push or branch switch —
                    // where the cache is not stale but wrong.
                    cochange::drift(&root, head).is_some_and(|d| d <= cochange::MAX_DRIFT)
                });

            let extra = match cached {
                Some((_, pairs)) => {
                    cochange_stats.edges = pairs.len();
                    cochange_stats.reused = true;
                    cochange::pairs_to_edges(&pairs, &by_path, &r.space)
                }
                None => {
                    let (extra, pairs, st) =
                        cochange::edges(&root, &by_path, &r.space, &cochange::Opts::default());
                    cochange_stats = st;
                    if let Some(h) = cochange::head(&root) {
                        cochange::save(&cc_path, &h, &pairs).ok();
                    }
                    extra
                }
            };
            r.edges.extend(extra);
        }
    }
    let d_cochange = t_cc.elapsed();

    // ---- persist -----------------------------------------------------------
    // Nothing changed and a graph already exists: the bytes on disk are already
    // correct, and rewriting 7.7 MB to say so is pure cost.
    let nothing_changed = cfg.incremental
        && reused == units.len() as u64
        && prior.is_empty()
        && out_path_exists(cfg, &root);

    let t3 = Instant::now();
    let mut graph_bytes = 0u64;
    if let (Some(r), false, false) = (&resolved, cfg.dry_run, nothing_changed) {
        // Symbol table indexed by SymId, so the on-disk graph is self-contained
        // and a reader needs no interner.
        let mut syms = vec![String::new(); interner.len()];
        for (k, v) in interner.iter() {
            let i = lasso::Key::into_usize(k);
            if i < syms.len() {
                syms[i] = v.to_string();
            }
        }
        let tg = Instant::now();
        graph::write(
            &out_path,
            r,
            &syms,
            &paths,
            &root,
            &ids.raw_keys(),
            &metas.iter().map(|m| (m.size, m.mtime)).collect::<Vec<_>>(),
        )
            .context("writing graph")?;
        let ms_graph = tg.elapsed().as_secs_f64() * 1e3;
        graph_bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
        let tc = Instant::now();
        let head = cochange::head(&root).unwrap_or_default();
        cache::write(&cache_path, &units, &paths, &langs, &metas, &interner, &root, &head)
            .context("writing extraction cache")?;
        if std::env::var_os("LEANGRAPH_PROFILE").is_some() {
            eprintln!(
                "      persist: graph {ms_graph:.0}ms · unit cache {:.0}ms",
                tc.elapsed().as_secs_f64() * 1e3
            );
        }
    }
    let d_persist = t3.elapsed();
    if nothing_changed {
        graph_bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    }

    let wall = d_discover + d_extract + d_resolve + d_cochange + d_persist;
    let mb = agg.bytes as f64 / 1_048_576.0;
    let n_files = units.len() as u64;

    if !cfg.quiet {
        // ---- report ------------------------------------------------------------
        println!("\n\x1b[1mleangraph\x1b[0m  {}", root.display());
        println!("  threads          {threads}");
        println!(
            "  files            {n_files}  ({mb:.1} MB{})",
            if oversized > 0 {
                format!(", {oversized} skipped >1MB")
            } else {
                String::new()
            }
        );
        if git_listing.is_some() {
            println!(
                "  discovery        git tree diff ({} files, {} touched)",
                found.len(),
                git_touched.as_ref().map_or(0, |t| t.len())
            );
        }
        if reused > 0 {
            println!(
                "  \x1b[1mreused           {reused} of {} files from cache\x1b[0m  ({} re-parsed)",
                units.len(),
                units.len() as u64 - reused
            );
        }
        println!(
            "  extracted        {defs} defs · {refs} refs · {imports} imports · {} unique symbols",
            interner.len()
        );
        if errors > 0 {
            println!(
                "  parse errors     {errors} ({:.1}%)",
                100.0 * errors as f64 / n_files as f64
            );
        }

        if let Some(r) = &resolved {
            if r.churn.retired > 0 || (r.churn.added > 0 && r.churn.kept > 0) {
                println!(
                    "  node ids         {} kept · {} new · {} retired · {} holes",
                    r.churn.kept, r.churn.added, r.churn.retired, r.churn.holes
                );
            }
            let s = &r.stats;
            let tot = s.total_refs().max(1) as f64;
            println!(
                "\n  \x1b[1mgraph\x1b[0m           {} nodes · {} edges",
                r.space.total,
                r.edges.len()
            );
            let in_repo = s.in_repo().max(1) as f64;
            println!(
                "  resolution       \x1b[1m{:.1}%\x1b[0m of in-repo refs   ({} of {} whose target exists here)",
                100.0 * s.resolved() as f64 / in_repo,
                s.resolved(),
                s.in_repo()
            );
            for (label, n, conf) in [
                ("scope", s.scope, "100"),
                ("import", s.import, " 95"),
                ("name (unique)", s.name_unique, " 80"),
                ("name (ambig)", s.name_ambiguous, "45-60"),
            ] {
                println!(
                    "    {label:<16} {n:>9}  {:>5.1}%   conf {conf}",
                    100.0 * n as f64 / tot
                );
            }
            for (label, n) in [
                ("too ambiguous", s.too_ambiguous),
                ("weak read", s.weak_read),
                ("builtin (runtime)", s.builtin),
                ("external (deps)", s.external),
            ] {
                println!(
                    "    \x1b[2m{label:<16} {n:>9}  {:>5.1}%\x1b[0m",
                    100.0 * n as f64 / tot
                );
            }
        }

        if cochange_stats.edges > 0 {
            if cochange_stats.reused {
                println!("  co-change        {} edges (cached)", cochange_stats.edges);
            } else {
                println!(
                    "  co-change        {} edges from {} of {} commits",
                    cochange_stats.edges, cochange_stats.commits_used, cochange_stats.commits_scanned
                );
            }
        }

        println!("\n  \x1b[1mwall clock\x1b[0m");
        println!("    discover       {:>8.0} ms", d_discover.as_secs_f64() * 1e3);
        println!("    extract        {:>8.0} ms", d_extract.as_secs_f64() * 1e3);
        if resolved.is_some() {
            println!("    resolve        {:>8.0} ms", d_resolve.as_secs_f64() * 1e3);
        }
        if cochange_stats.edges > 0 || d_cochange.as_millis() > 0 {
            println!("    co-change      {:>8.0} ms", d_cochange.as_secs_f64() * 1e3);
        }
        if graph_bytes > 0 {
            println!(
                "    persist        {:>8.0} ms   ({:.1} MB{})",
                d_persist.as_secs_f64() * 1e3,
                graph_bytes as f64 / 1_048_576.0,
                if nothing_changed { ", unchanged — not rewritten" } else { "" }
            );
        }
        println!(
            "    \x1b[1mtotal          {:>8.0} ms\x1b[0m   ({:.0} files/s, {:.0} MB/s)",
            wall.as_secs_f64() * 1e3,
            n_files as f64 / wall.as_secs_f64(),
            mb / wall.as_secs_f64()
        );

        println!("\n  \x1b[1mcpu time by stage\x1b[0m (summed over threads)");
        let cpu = (agg.ns_read + agg.ns_hash + agg.ns_parse + agg.ns_walk) as f64;
        for (label, ns) in [
            ("mmap", agg.ns_read),
            ("blake3", agg.ns_hash),
            ("parse", agg.ns_parse),
            ("walk+intern", agg.ns_walk),
        ] {
            println!(
                "    {label:<14} {:>8.0} ms  {:>5.1}%",
                ns as f64 / 1e6,
                100.0 * ns as f64 / cpu
            );
        }

        if cfg.by_lang {
            println!("\n  \x1b[1mby language\x1b[0m");
            let mut rows: Vec<_> = by_lang.iter().collect();
            rows.sort_by_key(|(_, v)| std::cmp::Reverse(v.0));
            for (lang, (f, b, d, r)) in rows {
                println!(
                    "    {:<12} {f:>6} files  {:>7.1} MB  {d:>8} defs  {r:>8} refs",
                    lang.name(),
                    *b as f64 / 1_048_576.0
                );
            }
        }

        if let Some(n) = cfg.top {
            let mut counts: FxHashMap<crate::core::SymId, u32> = FxHashMap::default();
            for unit in &units {
                for r in &unit.refs {
                    *counts.entry(r.name).or_default() += 1;
                }
            }
            let mut top: Vec<_> = counts.into_iter().collect();
            top.sort_unstable_by_key(|(_, c)| std::cmp::Reverse(*c));
            println!("\n  \x1b[1mtop {n} referenced symbols\x1b[0m");
            for (sym, c) in top.into_iter().take(n) {
                println!("    {:>8}  {}", c, interner.resolve(&sym));
            }
        }
        println!();

    }

    Ok(Summary {
        files: units.len(),
        nodes: resolved.as_ref().map_or(0, |r| r.space.total),
        edges: resolved.as_ref().map_or(0, |r| r.edges.len()),
    })
}
