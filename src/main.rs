//! arbor — native code-graph indexer and query engine.

mod core;
mod extract;
mod graph;
mod index;
mod install;
mod lang;
mod mcp;
mod query;
mod resolve;

use crate::core::{DefKind, NodeId};
use crate::graph::{Graph, Neighbor};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "arbor", version, about = "Native code-graph indexer")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand, Debug)]
enum Cmd {
    /// Index a repository and write its graph
    Index {
        path: PathBuf,
        #[arg(short, long)]
        threads: Option<usize>,
        #[arg(long)]
        by_lang: bool,
        #[arg(long, value_name = "N")]
        top: Option<usize>,
        #[arg(long)]
        no_resolve: bool,
        #[arg(short, long)]
        out: Option<PathBuf>,
        /// Index only; do not write the graph to disk
        #[arg(long)]
        dry_run: bool,
    },
    /// Look up definitions by name
    Find {
        symbol: String,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// What calls this symbol
    Callers {
        symbol: String,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        #[arg(short, long, default_value_t = 25)]
        limit: usize,
        /// Drop edges below this confidence
        #[arg(long, default_value_t = 0)]
        min_conf: u8,
    },
    /// What this symbol calls
    Callees {
        symbol: String,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        #[arg(short, long, default_value_t = 25)]
        limit: usize,
        #[arg(long, default_value_t = 0)]
        min_conf: u8,
    },
    /// Everything affected by changing this symbol
    Impact {
        symbol: String,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        #[arg(short, long, default_value_t = 3)]
        depth: u32,
        #[arg(long, default_value_t = 80)]
        min_conf: u8,
        #[arg(short, long, default_value_t = 40)]
        limit: usize,
        /// Follow containment edges too (file <-> its definitions)
        #[arg(long)]
        with_contains: bool,
    },
    /// Build agent context for a set of symbols
    Explore {
        /// Symbol names; `Class.method` disambiguates an overload
        #[arg(required = true)]
        symbols: Vec<String>,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 25)]
        max_nodes: usize,
        #[arg(long, default_value_t = 24_000)]
        max_bytes: usize,
        /// Print the source of each selected node
        #[arg(long)]
        source: bool,
    },
    /// Wire arbor into your editors as an MCP server
    Install {
        /// Targets: claude-code, cursor, codex (default: all)
        #[arg(value_name = "TARGET")]
        targets: Vec<String>,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// Remove the wiring instead
        #[arg(long)]
        uninstall: bool,
    },
    /// Run as an MCP server over stdio (for Claude Code, Cursor, Codex, …)
    Serve {
        #[arg(long)]
        mcp: bool,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
    },
    /// Dump nodes and edges as JSONL, for differential verification
    Dump {
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// nodes | edges
        #[arg(long, default_value = "nodes")]
        what: String,
    },
    /// Graph statistics and load time
    Status {
        #[arg(default_value = ".")]
        path: PathBuf,
    },
}

fn main() -> Result<()> {
    match Cli::parse().cmd {
        Cmd::Index {
            path,
            threads,
            by_lang,
            top,
            no_resolve,
            out,
            dry_run,
        } => index::run(&index::Config {
            path,
            threads,
            by_lang,
            top,
            no_resolve,
            out,
            dry_run,
        }),

        Cmd::Find { symbol, path } => {
            let (g, _) = load(&path)?;
            let hits = g.find(&symbol);
            if hits.is_empty() {
                println!("no definition named `{symbol}`");
            }
            for n in hits.iter().take(50) {
                println!("  {}", describe(&g, *n));
            }
            if hits.len() > 50 {
                println!("  … {} more", hits.len() - 50);
            }
            Ok(())
        }

        Cmd::Callers {
            symbol,
            path,
            limit,
            min_conf,
        } => neighbors(&path, &symbol, limit, min_conf, true),

        Cmd::Callees {
            symbol,
            path,
            limit,
            min_conf,
        } => neighbors(&path, &symbol, limit, min_conf, false),

        Cmd::Impact {
            symbol,
            path,
            depth,
            min_conf,
            limit,
            with_contains,
        } => {
            let (g, _) = load(&path)?;
            let target = pick(&g, &symbol)?;
            let t = Instant::now();
            let hit = g.impact(target, depth, min_conf, !with_contains);
            let us = t.elapsed().as_micros();
            println!(
                "\nimpact of \x1b[1m{symbol}\x1b[0m — {} nodes within {depth} hops (conf ≥ {min_conf}) in {us} µs\n",
                hit.len()
            );
            for n in hit.iter().take(limit) {
                println!("  {}", describe(&g, *n));
            }
            if hit.len() > limit {
                println!("  … {} more", hit.len() - limit);
            }
            println!();
            Ok(())
        }

        Cmd::Explore {
            symbols,
            path,
            max_nodes,
            max_bytes,
            source,
        } => {
            let (g, load_us) = load(&path)?;
            let t = Instant::now();
            let ctx = query::build(
                &g,
                &symbols,
                &query::Budget {
                    max_nodes,
                    max_bytes,
                    ..Default::default()
                },
            );
            let us = t.elapsed().as_micros();

            if !ctx.flow.is_empty() {
                println!(
                    "\n\x1b[1mflow\x1b[0m  {} → {}",
                    if ctx.flow_reversed { &symbols[1] } else { &symbols[0] },
                    if ctx.flow_reversed { &symbols[0] } else { &symbols[1] }
                );
                for (i, n) in ctx.flow.iter().enumerate() {
                    println!("  {}{}", "  ".repeat(i), describe(&g, *n));
                }
            }
            println!(
                "\n\x1b[1mcontext\x1b[0m  {} nodes · ~{} tokens · {} dropped   (load {load_us} µs, build {us} µs)\n",
                ctx.items.len(),
                ctx.est_tokens,
                ctx.dropped
            );
            for it in &ctx.items {
                println!(
                    "  {:<8} {:>4.2}  {}",
                    it.why.label(),
                    it.score,
                    describe(&g, it.node)
                );
                if source {
                    if let Some(txt) = read_span(&g, it.node) {
                        for line in txt.lines().take(30) {
                            println!("        │ {line}");
                        }
                    }
                }
            }
            println!();
            Ok(())
        }

        Cmd::Install {
            targets,
            path,
            uninstall,
        } => {
            let sel: Vec<install::Target> = if targets.is_empty() {
                install::ALL.to_vec()
            } else {
                let mut v = Vec::new();
                for t in &targets {
                    match install::Target::parse(t) {
                        Some(x) => v.push(x),
                        None => bail!("unknown target `{t}` (claude-code, cursor, codex)"),
                    }
                }
                v
            };
            println!();
            for (t, o) in install::apply(&sel, &path, uninstall)? {
                let (mark, detail) = match &o {
                    install::Outcome::Installed(p) => ("\x1b[32m+\x1b[0m", p.display().to_string()),
                    install::Outcome::Removed(p) => ("\x1b[33m-\x1b[0m", p.display().to_string()),
                    install::Outcome::Unchanged(p) => ("\x1b[2m=\x1b[0m", p.display().to_string()),
                    install::Outcome::Skipped(why) => ("\x1b[31m!\x1b[0m", (*why).to_string()),
                };
                println!("  {mark} {:<14} {detail}", t.name());
            }
            if !uninstall {
                println!("\n  restart your editor, then ask it to explore a symbol\n");
            } else {
                println!();
            }
            Ok(())
        }

        Cmd::Serve { mcp: _, path } => mcp::serve(&path),

        Cmd::Dump { path, what } => {
            use std::io::Write as _;
            let (g, _) = load(&path)?;
            let out = std::io::stdout();
            let mut w = std::io::BufWriter::new(out.lock());
            match what.as_str() {
                "nodes" => {
                    for i in 0..g.n_nodes() {
                        let n = NodeId(i);
                        let (f, _, _) = g.location(n);
                        if g.node_kind(n) == DefKind::Module as u8 {
                            continue; // files are compared separately
                        }
                        writeln!(
                            w,
                            r#"{{"file":{},"name":{},"kind":"{}"}}"#,
                            json_str(g.path(f)),
                            json_str(g.name(n)),
                            kind_name(g.node_kind(n))
                        )?;
                    }
                }
                "edges" => {
                    for i in 0..g.n_nodes() {
                        let src = NodeId(i);
                        let (sf, _, _) = g.location(src);
                        for nb in g.callees(src) {
                            let (df, _, _) = g.location(nb.node);
                            writeln!(
                                w,
                                r#"{{"sf":{},"sn":{},"df":{},"dn":{},"kind":{},"conf":{}}}"#,
                                json_str(g.path(sf)),
                                json_str(g.name(src)),
                                json_str(g.path(df)),
                                json_str(g.name(nb.node)),
                                nb.kind,
                                nb.conf
                            )?;
                        }
                    }
                }
                other => bail!("unknown dump target `{other}` (nodes | edges)"),
            }
            Ok(())
        }

        Cmd::Status { path } => {
            let (g, load_us) = load(&path)?;
            let bytes = std::fs::metadata(graph_path(&path))
                .map(|m| m.len())
                .unwrap_or(0);
            println!("\n  nodes      {}", g.n_nodes());
            println!("  edges      {}", g.n_edges());
            println!("  files      {}", g.n_files());
            println!("  on disk    {:.1} MB", bytes as f64 / 1_048_576.0);
            println!("  \x1b[1mload       {load_us} µs\x1b[0m   (mmap + header check; nothing is deserialized)\n");
            Ok(())
        }
    }
}

/// Slice a node's source out of its file. The graph stores byte spans, not
/// text, so this is the only place that touches the working tree.
fn read_span(g: &Graph, n: NodeId) -> Option<String> {
    let (file, start, end) = g.location(n);
    let bytes = std::fs::read(g.abs_path(file)).ok()?;
    let s = bytes.get(start as usize..end as usize)?;
    Some(String::from_utf8_lossy(s).into_owned())
}

/// Minimal JSON string escaping — enough for paths and identifiers, and it
/// keeps `dump` free of a serde dependency in the hot loop.
fn json_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\t' => out.push_str("\\t"),
            '\r' => out.push_str("\\r"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn graph_path(repo: &Path) -> PathBuf {
    if repo.extension().is_some_and(|e| e == "bin") {
        repo.to_path_buf()
    } else {
        repo.join(".arbor").join("graph.bin")
    }
}

fn load(repo: &Path) -> Result<(Graph, u128)> {
    let p = graph_path(repo);
    if !p.exists() {
        bail!(
            "no graph at {} — run `arbor index {}` first",
            p.display(),
            repo.display()
        );
    }
    let t = Instant::now();
    let g = Graph::open(&p).with_context(|| format!("loading {}", p.display()))?;
    Ok((g, t.elapsed().as_micros()))
}

/// Pick a target when a name is ambiguous. Reports the ambiguity on stderr
/// rather than silently guessing — a wrong silent pick is worse than a noisy
/// right one.
fn pick(g: &Graph, symbol: &str) -> Result<NodeId> {
    let mut hits = g.find(symbol);
    if hits.is_empty() {
        bail!("no definition named `{symbol}`");
    }
    hits.sort_by_key(|&n| rank_candidate(g, n));
    if hits.len() > 1 {
        eprintln!(
            "note: {} definitions named `{symbol}`; using {}",
            hits.len(),
            describe(g, hits[0])
        );
    }
    Ok(hits[0])
}

fn neighbors(repo: &Path, symbol: &str, limit: usize, min_conf: u8, inbound: bool) -> Result<()> {
    let (g, _) = load(repo)?;
    let target = pick(&g, symbol)?;
    let t = Instant::now();
    let mut ns: Vec<Neighbor> = if inbound {
        g.callers(target)
    } else {
        g.callees(target)
    };
    let us = t.elapsed().as_micros();
    ns.retain(|n| n.conf >= min_conf);
    ns.sort_unstable_by_key(|n| std::cmp::Reverse(n.conf));

    println!(
        "\n{} \x1b[1m{symbol}\x1b[0m — {} edges in {us} µs\n",
        if inbound { "callers of" } else { "callees of" },
        ns.len()
    );
    for n in ns.iter().take(limit) {
        println!(
            "  [{:>3}] {:<10} {}",
            n.conf,
            prov_name(n.prov),
            describe(&g, n.node)
        );
    }
    if ns.len() > limit {
        println!("  … {} more", ns.len() - limit);
    }
    println!();
    Ok(())
}

fn prov_name(p: u8) -> &'static str {
    match p {
        0 => "scope",
        1 => "import",
        2 => "name",
        3 => "co-change",
        4 => "framework",
        _ => "?",
    }
}

fn kind_name(k: u8) -> &'static str {
    match k {
        x if x == DefKind::Function as u8 => "fn",
        x if x == DefKind::Method as u8 => "method",
        x if x == DefKind::Class as u8 => "class",
        x if x == DefKind::Interface as u8 => "iface",
        x if x == DefKind::Module as u8 => "file",
        x if x == DefKind::Variable as u8 => "var",
        _ => "?",
    }
}

fn short_path(path: &str) -> String {
    if path.len() <= 42 {
        return path.to_string();
    }
    let tail: Vec<&str> = path.rsplit('/').take(3).collect();
    format!("…/{}", tail.into_iter().rev().collect::<Vec<_>>().join("/"))
}

fn describe(g: &Graph, n: NodeId) -> String {
    let (file, start, _) = g.location(n);
    let kind = g.node_kind(n);
    let loc = short_path(g.path(file));
    if kind == DefKind::Module as u8 {
        // a file node's name *is* its path — printing both is noise
        return format!("{:<7} {}", "file", loc);
    }
    format!("{:<7} {:<34} {}@{}", kind_name(kind), g.name(n), loc, start)
}

/// Rank candidates when a name is ambiguous: real code over tests, shallower
/// paths over deeper, functions over values. Django has five `get_or_create`
/// definitions and four of them are fixtures.
fn rank_candidate(g: &Graph, n: NodeId) -> (u8, usize) {
    let (file, _, _) = g.location(n);
    let path = g.path(file);
    let is_test = path.contains("/test") || path.contains("_test.") || path.contains(".test.");
    let kind = g.node_kind(n);
    let kind_rank = if kind == DefKind::Variable as u8 { 1 } else { 0 };
    ((is_test as u8) * 2 + kind_rank, path.matches('/').count())
}
