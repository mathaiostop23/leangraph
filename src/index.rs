//! Indexing pipeline: discover -> extract -> resolve -> persist.

use crate::core::{FileUnit, Interner};
use crate::extract::{extract_file, Timings};
use crate::graph;
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

pub struct Config {
    pub path: PathBuf,
    pub threads: Option<usize>,
    pub by_lang: bool,
    pub top: Option<usize>,
    pub no_resolve: bool,
    pub out: Option<PathBuf>,
    pub dry_run: bool,
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

pub fn run(cfg: &Config) -> Result<()> {

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
    let t0 = Instant::now();
    let mut found: Vec<(PathBuf, Lang)> = Vec::with_capacity(4096);
    let mut oversized = 0u64;

    for entry in WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        let Some(lang) = path
            .extension()
            .and_then(|e| e.to_str())
            .and_then(Lang::from_ext)
        else {
            continue;
        };
        if entry.metadata().is_ok_and(|m| m.len() > MAX_FILE_BYTES) {
            oversized += 1;
            continue;
        }
        found.push((path.to_path_buf(), lang));
    }
    let d_discover = t0.elapsed();

    if found.is_empty() {
        println!("no Python/TypeScript files found under {}", root.display());
        return Ok(());
    }

    // ---- extract -----------------------------------------------------------
    let specs: FxHashMap<Lang, Spec> = ALL_LANGS.into_iter().map(|l| (l, spec_for(l))).collect();
    let interner = Interner::new();

    let t1 = Instant::now();
    let raw: Vec<Option<(FileUnit, Timings)>> = found
        .par_iter()
        .enumerate()
        .map(|(i, (path, lang))| {
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
    let mut agg = Timings::default();
    let mut errors = 0u64;
    let mut by_lang: FxHashMap<Lang, (u64, u64, u64, u64)> = FxHashMap::default();

    for ((path, lang), slot) in found.iter().zip(raw) {
        let Some((mut unit, t)) = slot else { continue };
        unit.file = units.len() as u32;
        agg.ns_read += t.ns_read;
        agg.ns_hash += t.ns_hash;
        agg.ns_parse += t.ns_parse;
        agg.ns_walk += t.ns_walk;
        agg.bytes += t.bytes;
        agg.ast_nodes += t.ast_nodes;
        errors += unit.had_parse_error as u64;
        let e = by_lang.entry(*lang).or_default();
        e.0 += 1;
        e.1 += t.bytes;
        e.2 += unit.defs.len() as u64;
        e.3 += unit.refs.len() as u64;
        units.push(unit);
        paths.push(path.clone());
        langs.push(*lang);
    }
    let d_extract = t1.elapsed();

    let defs: u64 = units.iter().map(|u| u.defs.len() as u64).sum();
    let refs: u64 = units.iter().map(|u| u.refs.len() as u64).sum();
    let imports: u64 = units.iter().map(|u| u.imports.len() as u64).sum();

    // ---- resolve -----------------------------------------------------------
    let t2 = Instant::now();
    let resolved = (!cfg.no_resolve)
        .then(|| resolve::resolve(&units, &paths, &langs, &root, &interner));
    let d_resolve = t2.elapsed();

    // ---- persist -----------------------------------------------------------
    let t3 = Instant::now();
    let mut graph_bytes = 0u64;
    let out_path = cfg
        .out
        .clone()
        .unwrap_or_else(|| root.join(".arbor").join("graph.bin"));
    if let (Some(r), false) = (&resolved, cfg.dry_run) {
        // Symbol table indexed by SymId, so the on-disk graph is self-contained
        // and a reader needs no interner.
        let mut syms = vec![String::new(); interner.len()];
        for (k, v) in interner.iter() {
            let i = lasso::Key::into_usize(k);
            if i < syms.len() {
                syms[i] = v.to_string();
            }
        }
        graph::write(&out_path, r, &syms, &paths, &root).context("writing graph")?;
        graph_bytes = std::fs::metadata(&out_path).map(|m| m.len()).unwrap_or(0);
    }
    let d_persist = t3.elapsed();

    let wall = d_discover + d_extract + d_resolve + d_persist;
    let mb = agg.bytes as f64 / 1_048_576.0;
    let n_files = units.len() as u64;

    // ---- report ------------------------------------------------------------
    println!("\n\x1b[1marbor\x1b[0m  {}", root.display());
    println!("  threads          {threads}");
    println!(
        "  files            {n_files}  ({mb:.1} MB{})",
        if oversized > 0 {
            format!(", {oversized} skipped >1MB")
        } else {
            String::new()
        }
    );
    println!("  ast nodes        {}", agg.ast_nodes);
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

    println!("\n  \x1b[1mwall clock\x1b[0m");
    println!("    discover       {:>8.0} ms", d_discover.as_secs_f64() * 1e3);
    println!("    extract        {:>8.0} ms", d_extract.as_secs_f64() * 1e3);
    if resolved.is_some() {
        println!("    resolve        {:>8.0} ms", d_resolve.as_secs_f64() * 1e3);
    }
    if graph_bytes > 0 {
        println!(
            "    persist        {:>8.0} ms   ({:.1} MB -> {})",
            d_persist.as_secs_f64() * 1e3,
            graph_bytes as f64 / 1_048_576.0,
            out_path.display()
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

    Ok(())
}
