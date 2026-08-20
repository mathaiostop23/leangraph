//! leangraph — native code-graph indexer and query engine.

mod cache;
mod cochange;
mod core;
mod extract;
mod graph;
mod idtable;
mod index;
mod install;
mod lang;
mod mcp;
mod query;
mod resolve;
mod server;

use crate::core::{DefKind, EdgeKind, NodeId, Provenance};
use crate::graph::{Graph, Neighbor};
use anyhow::{bail, Context, Result};
use clap::{Parser, Subcommand};
use std::path::{Path, PathBuf};
use std::time::Instant;

#[derive(Parser, Debug)]
#[command(name = "leangraph", version, about = "Native code-graph indexer")]
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
        /// Skip git co-change edges
        #[arg(long)]
        no_cochange: bool,
        /// Full reindex, ignoring any cached extraction
        #[arg(long)]
        force: bool,
        /// Diff against this commit instead of walking the tree (server path)
        #[arg(long, value_name = "SHA")]
        since: Option<String>,
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
    /// Wire leangraph into your editors as an MCP server
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
    /// Build context from free text — an issue, a commit message, a stack trace
    Context {
        text: String,
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        #[arg(long, default_value_t = 25)]
        max_nodes: usize,
        #[arg(long, default_value_t = 24_000)]
        max_bytes: usize,
        /// Emit the selected file set as JSON, for the cost benchmark
        #[arg(long)]
        files_json: bool,
    },
    /// Dump nodes and edges as JSONL, for differential verification
    Dump {
        #[arg(short, long, default_value = ".")]
        path: PathBuf,
        /// nodes | edges | semantic
        #[arg(long, default_value = "nodes")]
        what: String,
    },
    /// Run the self-hosted HTTP server
    Server {
        /// Address to bind
        #[arg(long, default_value = "127.0.0.1:7777")]
        addr: String,
        /// Where repositories, graphs and the database live
        #[arg(long, default_value = ".leangraph-server")]
        data: PathBuf,
        /// Concurrent workers. Indexing already saturates cores.
        #[arg(long, default_value_t = 2)]
        workers: usize,
        /// Label an issue must carry before the bot acts
        #[arg(long, default_value = "leangraph")]
        trigger_label: String,
        /// Additional label that requests a patch, where fix mode is enabled
        #[arg(long, default_value = "leangraph-fix")]
        fix_label: String,
    },
    /// Probe a running server. Exits non-zero when it is not serving, so it
    /// works as a container healthcheck.
    Health {
        #[arg(long, default_value = "127.0.0.1:7777")]
        addr: String,
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
            no_cochange,
            force,
            since,
            out,
            dry_run,
        } => index::run(&index::Config {
            path,
            threads,
            by_lang,
            top,
            no_resolve,
            no_cochange,
            incremental: !force,
            since,
            out,
            dry_run,
            quiet: false,
        })
        .map(|_| ()),

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

        Cmd::Context {
            text,
            path,
            max_nodes,
            max_bytes,
            files_json,
        } => {
            let (g, _) = load(&path)?;
            let ctx = query::build_from_text(
                &g,
                &text,
                &query::Budget {
                    max_nodes,
                    max_bytes,
                    ..Default::default()
                },
            );
            if files_json {
                let mut files: Vec<&str> = ctx
                    .items
                    .iter()
                    .map(|it| g.path(g.location(it.node).0))
                    .collect();
                files.sort_unstable();
                files.dedup();
                let quoted: Vec<String> = files.iter().map(|f| json_str(f)).collect();
                println!(
                    r#"{{"files":[{}],"nodes":{},"est_tokens":{}}}"#,
                    quoted.join(","),
                    ctx.items.len(),
                    ctx.est_tokens
                );
                return Ok(());
            }
            println!(
                "\n\x1b[1mcontext\x1b[0m  {} nodes · ~{} tokens\n",
                ctx.items.len(),
                ctx.est_tokens
            );
            for it in &ctx.items {
                println!("  {:<8} {:>4.2}  {}", it.why.label(), it.score, describe(&g, it.node));
            }
            println!();
            Ok(())
        }

        Cmd::Dump { path, what } => {
            use std::io::Write as _;
            let (g, _) = load(&path)?;
            let out = std::io::stdout();
            let mut w = std::io::BufWriter::new(out.lock());
            match what.as_str() {
                "nodes" => {
                    for i in 0..g.n_nodes() {
                        let n = NodeId(i);
                        if g.node_key(n) == 0 {
                            continue; // hole left by a retired node
                        }
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
                // Identity-keyed, so it is comparable across runs that assigned
                // different ids — which is exactly what an incremental sync does.
                "semantic" => {
                    let mut lines: Vec<String> = Vec::new();
                    for i in 0..g.n_nodes() {
                        let n = NodeId(i);
                        let k = g.node_key(n);
                        if k == 0 {
                            continue;
                        }
                        let (f, a, b) = g.location(n);
                        lines.push(format!(
                            "N {k:016x} {} {} {a} {b} {}",
                            kind_name(g.node_kind(n)),
                            g.path(f),
                            g.name(n)
                        ));
                        for nb in g.callees(n) {
                            let dk = g.node_key(nb.node);
                            if dk == 0 {
                                continue;
                            }
                            lines.push(format!(
                                "E {k:016x} {dk:016x} {} {}",
                                nb.kind, nb.conf
                            ));
                        }
                    }
                    lines.sort_unstable();
                    for l in lines {
                        writeln!(w, "{l}")?;
                    }
                }
                // Everything an external oracle needs to join against: a
                // qualified name and a byte offset on each end, the edge kind by
                // name, and — the point of the whole exercise — the confidence
                // and the provenance that produced it. Without provenance you
                // cannot ask whether confidence predicts correctness, which is
                // the claim the ranking rests on.
                "edges" => {
                    for i in 0..g.n_nodes() {
                        let src = NodeId(i);
                        if g.node_key(src) == 0 {
                            continue;
                        }
                        let (sf, sb, _) = g.location(src);
                        let sq = g.qualified(src);
                        for nb in g.callees(src) {
                            if g.node_key(nb.node) == 0 {
                                continue;
                            }
                            let (df, db, _) = g.location(nb.node);
                            writeln!(
                                w,
                                r#"{{"sq":{},"sf":{},"sb":{},"sk":"{}","dq":{},"df":{},"db":{},"dk":"{}","ek":"{}","conf":{},"prov":"{}"}}"#,
                                json_str(&sq),
                                json_str(g.path(sf)),
                                sb,
                                kind_name(g.node_kind(src)),
                                json_str(&g.qualified(nb.node)),
                                json_str(g.path(df)),
                                db,
                                kind_name(g.node_kind(nb.node)),
                                edge_kind_name(nb.kind),
                                nb.conf,
                                prov_name(nb.prov)
                            )?;
                        }
                    }
                }
                other => bail!("unknown dump target `{other}` (nodes | edges | semantic)"),
            }
            Ok(())
        }

        Cmd::Server {
            addr,
            data,
            workers,
            trigger_label,
            fix_label,
        } => {
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            rt.block_on(server::run(server::Config {
                addr: addr.parse().context("parsing --addr")?,
                db_path: data.join("leangraph.db"),
                data_dir: data,
                workers: workers.max(1),
                // From the environment, never a flag: a secret in argv is
                // visible in `ps` to every user on the box.
                webhook_secret: std::env::var("LEANGRAPH_WEBHOOK_SECRET")
                    .ok()
                    .filter(|s| !s.is_empty()),
                trigger_label,
                fix_label,
            }))
        }

        Cmd::Health { addr } => {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()?;
            rt.block_on(async move {
                let url = format!("http://{addr}/health");
                let res = reqwest::Client::new()
                    .get(&url)
                    .timeout(std::time::Duration::from_secs(3))
                    .send()
                    .await
                    .with_context(|| format!("connecting to {url}"))?;
                if !res.status().is_success() {
                    bail!("{url} returned {}", res.status());
                }
                println!("{}", res.text().await.unwrap_or_default());
                Ok(())
            })
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
    if !g.file_is_current(file) {
        return None;
    }
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
        repo.join(".leangraph").join("graph.bin")
    }
}

fn load(repo: &Path) -> Result<(Graph, u128)> {
    let p = graph_path(repo);
    if !p.exists() {
        bail!(
            "no graph at {} — run `leangraph index {}` first",
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

/// Matched on the enum rather than on literals: these values are also the
/// on-disk encoding, and a reordered discriminant would silently relabel every
/// edge in the dump the benchmark reads.
fn prov_name(p: u8) -> &'static str {
    match p {
        x if x == Provenance::Scope as u8 => "scope",
        x if x == Provenance::Import as u8 => "import",
        x if x == Provenance::NameMatch as u8 => "name",
        x if x == Provenance::CoChange as u8 => "co-change",
        x if x == Provenance::Framework as u8 => "framework",
        _ => "?",
    }
}

fn edge_kind_name(k: u8) -> &'static str {
    match k {
        x if x == EdgeKind::Contains as u8 => "contains",
        x if x == EdgeKind::Calls as u8 => "calls",
        x if x == EdgeKind::Imports as u8 => "imports",
        x if x == EdgeKind::Extends as u8 => "extends",
        x if x == EdgeKind::References as u8 => "references",
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
