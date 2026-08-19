//! arbor — native code-graph indexer.
//!
//! Phase 0 measured the parse+extract floor (see BENCH.md).
//! Phase 1 (here) turns counting into real symbol extraction: interned names,
//! containment scopes, and import statements — the inputs resolution needs.

mod core;
mod extract;
mod lang;

use crate::core::{FileUnit, Interner};
use crate::extract::{extract_file, Timings};
use crate::lang::{spec_for, Lang, Spec, ALL_LANGS};
use anyhow::{Context, Result};
use clap::Parser as ClapParser;
use ignore::WalkBuilder;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::path::PathBuf;
use std::time::Instant;
use tree_sitter::Parser as TsParser;

const MAX_FILE_BYTES: u64 = 1024 * 1024; // matches CodeGraph's skip, for fair comparison

#[derive(ClapParser, Debug)]
#[command(name = "arbor", about = "Native code-graph indexer")]
struct Cli {
    /// Repository root to index
    path: PathBuf,
    /// Worker threads (default: all cores)
    #[arg(short, long)]
    threads: Option<usize>,
    /// Per-language breakdown
    #[arg(long)]
    by_lang: bool,
    /// Show the N most-referenced symbols (extraction sanity check)
    #[arg(long, value_name = "N")]
    top: Option<usize>,
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

fn main() -> Result<()> {
    let cli = Cli::parse();
    let threads = cli
        .threads
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(4, |n| n.get()));
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let root = cli
        .path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", cli.path.display()))?;

    // ---- discover ----------------------------------------------------------
    let t0 = Instant::now();
    let mut files: Vec<(PathBuf, Lang)> = Vec::with_capacity(4096);
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
        files.push((path.to_path_buf(), lang));
    }
    let d_discover = t0.elapsed();

    if files.is_empty() {
        println!("no Python/TypeScript files found under {}", root.display());
        return Ok(());
    }

    // ---- extract -----------------------------------------------------------
    let specs: FxHashMap<Lang, Spec> = ALL_LANGS.into_iter().map(|l| (l, spec_for(l))).collect();
    let interner = Interner::new();

    let t1 = Instant::now();
    let results: Vec<(Lang, FileUnit, Timings)> = files
        .par_iter()
        .enumerate()
        .filter_map(|(i, (path, lang))| {
            with_parser(*lang, |parser| {
                extract_file(path, i as u32, *lang, &specs[lang], parser, &interner)
                    .map(|(u, t)| (*lang, u, t))
            })
        })
        .collect();
    let d_extract = t1.elapsed();
    let wall = d_discover + d_extract;

    // ---- aggregate ---------------------------------------------------------
    let mut agg = Timings::default();
    let mut defs = 0u64;
    let mut refs = 0u64;
    let mut imports = 0u64;
    let mut errors = 0u64;
    let mut by_lang: FxHashMap<Lang, (u64, u64, u64, u64)> = FxHashMap::default();

    for (lang, unit, t) in &results {
        agg.ns_read += t.ns_read;
        agg.ns_hash += t.ns_hash;
        agg.ns_parse += t.ns_parse;
        agg.ns_walk += t.ns_walk;
        agg.bytes += t.bytes;
        agg.ast_nodes += t.ast_nodes;
        defs += unit.defs.len() as u64;
        refs += unit.refs.len() as u64;
        imports += unit.imports.len() as u64;
        errors += unit.had_parse_error as u64;
        let e = by_lang.entry(*lang).or_default();
        e.0 += 1;
        e.1 += t.bytes;
        e.2 += unit.defs.len() as u64;
        e.3 += unit.refs.len() as u64;
    }

    let mb = agg.bytes as f64 / 1_048_576.0;
    let n_files = results.len() as u64;

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

    println!("\n  \x1b[1mwall clock\x1b[0m");
    println!("    discover       {:>8.0} ms", d_discover.as_secs_f64() * 1e3);
    println!("    extract        {:>8.0} ms", d_extract.as_secs_f64() * 1e3);
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

    if cli.by_lang {
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

    // Sanity check: are the names we interned actually plausible identifiers?
    if let Some(n) = cli.top {
        let mut counts: FxHashMap<core::SymId, u32> = FxHashMap::default();
        for (_, unit, _) in &results {
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
