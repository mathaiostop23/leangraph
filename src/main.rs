//! arbor — Phase 0 skeleton.
//!
//! Purpose: answer ONE question before building anything else —
//! how much of a real indexer's wall-clock is parse + extract?
//!
//! Pipeline measured here: discover -> mmap -> blake3 -> parse -> cursor-walk.
//! Deliberately NO resolution and NO persistence: those are the phases we
//! believe dominate CodeGraph's ~100s/27k-files, and we need the parse+extract
//! floor to prove it.

use anyhow::{Context, Result};
use clap::Parser as ClapParser;
use ignore::WalkBuilder;
use memmap2::Mmap;
use rayon::prelude::*;
use rustc_hash::FxHashMap;
use std::cell::RefCell;
use std::fs::File;
use std::path::PathBuf;
use std::time::Instant;
use tree_sitter::{Language, Parser as TsParser, TreeCursor};

const MAX_FILE_BYTES: u64 = 1024 * 1024; // match CodeGraph's 1MB skip, for a fair comparison

#[derive(ClapParser, Debug)]
#[command(name = "arbor", about = "Phase 0 indexing-speed skeleton")]
struct Cli {
    /// Repository root to index
    path: PathBuf,
    /// Worker threads (default: all cores)
    #[arg(short, long)]
    threads: Option<usize>,
    /// Print per-language breakdown
    #[arg(long)]
    by_lang: bool,
}

// ---------------------------------------------------------------- languages

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
enum Lang {
    Python,
    TypeScript,
    Tsx,
}

impl Lang {
    fn from_ext(ext: &str) -> Option<Lang> {
        match ext {
            "py" | "pyi" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Lang::Tsx),
            _ => None,
        }
    }

    fn ts_language(self) -> Language {
        match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    fn name(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx/jsx",
        }
    }
}

/// Node-kind IDs resolved once per language.
///
/// The hot loop compares `u16` kind IDs, never strings. This is the key
/// difference from a tree-sitter *query*: queries are convenient but run a
/// matching automaton per node. A cursor walk with integer comparison is
/// several times faster and is what a throughput-oriented extractor must do.
struct Spec {
    defs: Vec<u16>,
    calls: Vec<u16>,
    imports: Vec<u16>,
}

fn ids(lang: &Language, names: &[&str]) -> Vec<u16> {
    names
        .iter()
        .filter_map(|n| match lang.id_for_node_kind(n, true) {
            0 => None,
            id => Some(id),
        })
        .collect()
}

fn spec_for(lang: Lang) -> Spec {
    let l = lang.ts_language();
    match lang {
        Lang::Python => Spec {
            defs: ids(&l, &["function_definition", "class_definition"]),
            calls: ids(&l, &["call"]),
            imports: ids(&l, &["import_statement", "import_from_statement"]),
        },
        Lang::TypeScript | Lang::Tsx => Spec {
            defs: ids(
                &l,
                &[
                    "function_declaration",
                    "class_declaration",
                    "method_definition",
                    "arrow_function",
                    "function_expression",
                    "interface_declaration",
                ],
            ),
            calls: ids(&l, &["call_expression", "new_expression"]),
            imports: ids(&l, &["import_statement", "export_statement"]),
        },
    }
}

// -------------------------------------------------------------------- stats

#[derive(Default, Clone, Copy)]
struct Stats {
    files: u64,
    bytes: u64,
    ast_nodes: u64,
    defs: u64,
    calls: u64,
    imports: u64,
    parse_errors: u64,
    skipped: u64,
    // CPU-time accumulators (summed across threads; will exceed wall time)
    ns_read: u64,
    ns_hash: u64,
    ns_parse: u64,
    ns_walk: u64,
}

impl Stats {
    fn merge(mut self, o: Stats) -> Stats {
        self.files += o.files;
        self.bytes += o.bytes;
        self.ast_nodes += o.ast_nodes;
        self.defs += o.defs;
        self.calls += o.calls;
        self.imports += o.imports;
        self.parse_errors += o.parse_errors;
        self.skipped += o.skipped;
        self.ns_read += o.ns_read;
        self.ns_hash += o.ns_hash;
        self.ns_parse += o.ns_parse;
        self.ns_walk += o.ns_walk;
        self
    }
}

// --------------------------------------------------------------------- walk

/// Iterative DFS over the whole tree. One pass, integer comparisons only.
#[inline]
fn walk(cursor: &mut TreeCursor, spec: &Spec, st: &mut Stats) {
    let mut depth: i32 = 0;
    loop {
        let kind = cursor.node().kind_id();
        st.ast_nodes += 1;
        if spec.defs.contains(&kind) {
            st.defs += 1;
        } else if spec.calls.contains(&kind) {
            st.calls += 1;
        } else if spec.imports.contains(&kind) {
            st.imports += 1;
        }

        if cursor.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if cursor.goto_next_sibling() {
                break;
            }
            if depth == 0 {
                return;
            }
            cursor.goto_parent();
            depth -= 1;
        }
    }
}

// ------------------------------------------------------------ per-thread TS

thread_local! {
    static PARSERS: RefCell<FxHashMap<Lang, TsParser>> = RefCell::new(FxHashMap::default());
}

fn process(path: &PathBuf, lang: Lang, spec: &Spec) -> Stats {
    let mut st = Stats::default();

    let t = Instant::now();
    let file = match File::open(path) {
        Ok(f) => f,
        Err(_) => {
            st.skipped = 1;
            return st;
        }
    };
    // SAFETY: we only read the mapping, and the indexer holds no concurrent
    // writer to the working tree during a run.
    let mmap = match unsafe { Mmap::map(&file) } {
        Ok(m) => m,
        Err(_) => {
            st.skipped = 1;
            return st;
        }
    };
    st.ns_read = t.elapsed().as_nanos() as u64;

    let bytes: &[u8] = &mmap;
    st.bytes = bytes.len() as u64;
    st.files = 1;

    let t = Instant::now();
    let _hash = blake3::hash(bytes);
    st.ns_hash = t.elapsed().as_nanos() as u64;

    PARSERS.with(|cell| {
        let mut map = cell.borrow_mut();
        let parser = map.entry(lang).or_insert_with(|| {
            let mut p = TsParser::new();
            p.set_language(&lang.ts_language())
                .expect("grammar/ABI mismatch");
            p
        });

        let t = Instant::now();
        let tree = parser.parse(bytes, None);
        st.ns_parse = t.elapsed().as_nanos() as u64;

        if let Some(tree) = tree {
            if tree.root_node().has_error() {
                st.parse_errors = 1;
            }
            let t = Instant::now();
            let mut cursor = tree.walk();
            walk(&mut cursor, spec, &mut st);
            st.ns_walk = t.elapsed().as_nanos() as u64;
        } else {
            st.parse_errors = 1;
        }
    });

    st
}

// --------------------------------------------------------------------- main

fn main() -> Result<()> {
    let cli = Cli::parse();
    let threads = cli.threads.unwrap_or_else(num_cpus_fallback);
    rayon::ThreadPoolBuilder::new()
        .num_threads(threads)
        .build_global()
        .ok();

    let root = cli
        .path
        .canonicalize()
        .with_context(|| format!("cannot resolve {}", cli.path.display()))?;

    // ---- phase 1: discover -------------------------------------------------
    let t_discover = Instant::now();
    let mut files: Vec<(PathBuf, Lang)> = Vec::with_capacity(4096);
    let mut oversized = 0u64;

    let walker = WalkBuilder::new(&root)
        .hidden(true)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .follow_links(false)
        .build();

    for entry in walker.flatten() {
        let Some(ft) = entry.file_type() else { continue };
        if !ft.is_file() {
            continue;
        }
        let path = entry.path();
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            continue;
        };
        let Some(lang) = Lang::from_ext(ext) else {
            continue;
        };
        if let Ok(md) = entry.metadata() {
            if md.len() > MAX_FILE_BYTES {
                oversized += 1;
                continue;
            }
        }
        files.push((path.to_path_buf(), lang));
    }
    let d_discover = t_discover.elapsed();

    if files.is_empty() {
        println!("no Python/TypeScript files found under {}", root.display());
        return Ok(());
    }

    // ---- phase 2: extract --------------------------------------------------
    let specs: FxHashMap<Lang, Spec> = [Lang::Python, Lang::TypeScript, Lang::Tsx]
        .into_iter()
        .map(|l| (l, spec_for(l)))
        .collect();

    let t_extract = Instant::now();
    let per_lang: FxHashMap<Lang, Stats> = files
        .par_iter()
        .fold(
            FxHashMap::<Lang, Stats>::default,
            |mut acc, (path, lang)| {
                let st = process(path, *lang, &specs[lang]);
                let e = acc.entry(*lang).or_default();
                *e = e.merge(st);
                acc
            },
        )
        .reduce(FxHashMap::<Lang, Stats>::default, |mut a, b| {
            for (k, v) in b {
                let e = a.entry(k).or_default();
                *e = e.merge(v);
            }
            a
        });
    let d_extract = t_extract.elapsed();

    let total = per_lang.values().copied().fold(Stats::default(), Stats::merge);
    let wall = d_discover + d_extract;

    // ---- report ------------------------------------------------------------
    let mb = total.bytes as f64 / 1_048_576.0;
    println!("\n\x1b[1marbor phase-0\x1b[0m  {}", root.display());
    println!("  threads          {threads}");
    println!(
        "  files            {}  ({:.1} MB{})",
        total.files,
        mb,
        if oversized > 0 {
            format!(", {oversized} skipped >1MB")
        } else {
            String::new()
        }
    );
    println!("  ast nodes        {}", total.ast_nodes);
    println!(
        "  extracted        {} defs · {} calls · {} imports",
        total.defs, total.calls, total.imports
    );
    if total.parse_errors > 0 {
        println!(
            "  parse errors     {} ({:.1}%)",
            total.parse_errors,
            100.0 * total.parse_errors as f64 / total.files as f64
        );
    }

    println!("\n  \x1b[1mwall clock\x1b[0m");
    println!("    discover       {:>8.0} ms", d_discover.as_secs_f64() * 1e3);
    println!("    extract        {:>8.0} ms", d_extract.as_secs_f64() * 1e3);
    println!(
        "    \x1b[1mtotal          {:>8.0} ms\x1b[0m   ({:.0} files/s, {:.0} MB/s)",
        wall.as_secs_f64() * 1e3,
        total.files as f64 / wall.as_secs_f64(),
        mb / wall.as_secs_f64()
    );

    println!("\n  \x1b[1mcpu time by stage\x1b[0m (summed over threads)");
    let cpu = (total.ns_read + total.ns_hash + total.ns_parse + total.ns_walk) as f64;
    for (label, ns) in [
        ("mmap", total.ns_read),
        ("blake3", total.ns_hash),
        ("parse", total.ns_parse),
        ("walk", total.ns_walk),
    ] {
        println!(
            "    {label:<14} {:>8.0} ms  {:>5.1}%",
            ns as f64 / 1e6,
            100.0 * ns as f64 / cpu
        );
    }

    if cli.by_lang {
        println!("\n  \x1b[1mby language\x1b[0m");
        let mut rows: Vec<_> = per_lang.iter().collect();
        rows.sort_by_key(|(_, s)| std::cmp::Reverse(s.files));
        for (lang, s) in rows {
            println!(
                "    {:<12} {:>6} files  {:>7.1} MB  {:>8} defs  {:>8} calls",
                lang.name(),
                s.files,
                s.bytes as f64 / 1_048_576.0,
                s.defs,
                s.calls
            );
        }
    }
    println!();

    Ok(())
}

fn num_cpus_fallback() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
}
