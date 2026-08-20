//! MCP server over stdio (JSON-RPC 2.0).
//!
//! Two design rules are taken directly from CodeGraph's published findings,
//! which are the product of months of A/B testing against real agents and are
//! worth more than any guess we could make:
//!
//! 1. **One primary tool.** Agents reliably call the first tool and under-pick
//!    the rest; CodeGraph *removed* two tools for this reason. So: `explore` is
//!    the tool, `node` is the depth follow-up, and there is nothing else.
//!
//! 2. **Errors teach abandonment.** One or two `isError` responses early in a
//!    session and the agent stops calling the server entirely. So `isError` is
//!    reserved for genuine malfunctions. "Not indexed", "symbol not found" and
//!    every other expected condition return a *successful* response whose text
//!    explains what to do instead.
//!
//! Where we differ structurally: this is a static binary that mmaps its graph,
//! so it is serving within milliseconds of spawn. CodeGraph documents ~2–3s of
//! startup during which the agent gives up and reaches for grep instead.

use crate::graph::Graph;
use crate::query::{self, Budget};
use anyhow::Result;
use serde_json::{json, Value};
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

const SERVER_INSTRUCTIONS: &str = "\
leangraph gives you the structure of this repository: what calls what, what breaks \
if something changes, and the path between two symbols.

Use `leangraph_explore` first, and pass a bag of symbol names you already suspect \
are involved — function, method or class names, `Class.method` to disambiguate \
an overload. It returns the call path between them plus the surrounding code, \
ranked by how strongly the graph supports each connection.

Treat what it returns as already read; do not re-open those files. Every edge \
carries a confidence: 100 means resolved through lexical scope, 95 through an \
explicit import, 45-80 matched by name and therefore a plausible guess. Weigh \
low-confidence results accordingly rather than trusting them equally.

If a symbol is missing, the graph may be stale — say so rather than guessing.";

struct Server {
    root: PathBuf,
    graph: Option<Graph>,
}

impl Server {
    fn new(root: PathBuf) -> Server {
        let p = root.join(".leangraph").join("graph.bin");
        let graph = Graph::open(&p).ok();
        Server { root, graph }
    }

    fn tools() -> Value {
        json!([
            {
                "name": "leangraph_explore",
                "description": "PRIMARY. Given symbol names you suspect are involved, returns \
the call path between them plus the relevant surrounding code, ranked by graph \
confidence. Call this before reading files — its output is the answer to 'how \
does X reach Y', 'what calls this', and 'what would break'. Treat returned \
source as already read.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbols": {
                            "type": "array",
                            "items": {"type": "string"},
                            "description": "Function, method or class names. Use `Class.method` \
to pick a specific overload. Two or more names makes the tool find the path between them."
                        },
                        "max_nodes": {"type": "integer", "description": "Default 25."},
                        "max_bytes": {"type": "integer", "description": "Source budget, default 24000."}
                    },
                    "required": ["symbols"]
                }
            },
            {
                "name": "leangraph_node",
                "description": "SECONDARY, use after explore. Full source of one symbol plus \
everything that calls it and everything it calls, with confidence on each edge.",
                "inputSchema": {
                    "type": "object",
                    "properties": {
                        "symbol": {"type": "string"},
                        "limit": {"type": "integer", "description": "Max edges each way, default 20."}
                    },
                    "required": ["symbol"]
                }
            }
        ])
    }

    /// Expected conditions return success-shaped guidance, never `isError`.
    fn guidance(&self, text: String) -> Value {
        json!({"content": [{"type": "text", "text": text}]})
    }

    fn graph(&self) -> std::result::Result<&Graph, Value> {
        self.graph.as_ref().ok_or_else(|| {
            self.guidance(format!(
                "No graph for {}. Run `leangraph index {}` to build one, then retry. \
Until then, fall back to reading files directly.",
                self.root.display(),
                self.root.display()
            ))
        })
    }

    fn explore(&self, args: &Value) -> Value {
        let g = match self.graph() {
            Ok(g) => g,
            Err(v) => return v,
        };
        let symbols: Vec<String> = args
            .get("symbols")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();
        if symbols.is_empty() {
            return self.guidance(
                "No symbols given. Pass the function, method or class names you suspect \
are involved, e.g. {\"symbols\": [\"QuerySet\", \"SQLCompiler\"]}."
                    .into(),
            );
        }

        let budget = Budget {
            max_nodes: args
                .get("max_nodes")
                .and_then(Value::as_u64)
                .unwrap_or(25) as usize,
            max_bytes: args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(24_000) as usize,
            ..Default::default()
        };
        let ctx = query::build(g, &symbols, &budget);

        if ctx.items.is_empty() {
            return self.guidance(format!(
                "None of {symbols:?} are in the graph. They may be third-party, or the \
index may predate them. Try a different name, or read the files directly."
            ));
        }

        let mut out = String::new();
        if !ctx.flow.is_empty() {
            let (a, b) = if ctx.flow_reversed {
                (&symbols[1], &symbols[0])
            } else {
                (&symbols[0], &symbols[1])
            };
            out.push_str(&format!("## Flow: {a} → {b}\n\n"));
            for (i, n) in ctx.flow.iter().enumerate() {
                out.push_str(&format!("{}{}\n", "  ".repeat(i), describe(g, *n)));
            }
            out.push('\n');
        }

        out.push_str(&format!(
            "## Context ({} nodes, ~{} tokens)\n\n",
            ctx.items.len(),
            ctx.est_tokens
        ));
        for it in &ctx.items {
            out.push_str(&format!("### {} [{}]\n", describe(g, it.node), it.why.label()));
            if let Some(src) = read_span(g, it.node, budget.max_node_bytes) {
                out.push_str("```\n");
                out.push_str(&src);
                out.push_str("\n```\n\n");
            }
        }
        if ctx.dropped > 0 {
            out.push_str(&format!(
                "_{} lower-ranked nodes omitted. Call leangraph_explore again with more \
specific symbol names to see them — do not fall back to reading files._\n",
                ctx.dropped
            ));
        }
        self.guidance(out)
    }

    fn node(&self, args: &Value) -> Value {
        let g = match self.graph() {
            Ok(g) => g,
            Err(v) => return v,
        };
        let Some(symbol) = args.get("symbol").and_then(Value::as_str) else {
            return self.guidance("No symbol given.".into());
        };
        let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(20) as usize;

        let hits = query::seeds(g, &[symbol.to_string()]);
        let Some(&n) = hits.first() else {
            return self.guidance(format!(
                "`{symbol}` is not in the graph. It may be third-party, or the index may \
be stale. Try leangraph_explore with related names."
            ));
        };

        let mut out = format!("## {}\n\n", describe(g, n));
        if let Some(src) = read_span(g, n, 8_000) {
            out.push_str("```\n");
            out.push_str(&src);
            out.push_str("\n```\n\n");
        }
        for (title, mut ns) in [
            ("Called by", g.callers(n)),
            ("Calls", g.callees(n)),
        ] {
            ns.sort_unstable_by_key(|x| std::cmp::Reverse(x.conf));
            out.push_str(&format!("### {title} ({})\n", ns.len()));
            for x in ns.iter().take(limit) {
                out.push_str(&format!("- [conf {}] {}\n", x.conf, describe(g, x.node)));
            }
            out.push('\n');
        }
        self.guidance(out)
    }

    fn handle(&self, req: &Value) -> Option<Value> {
        let method = req.get("method").and_then(Value::as_str).unwrap_or("");
        let id = req.get("id").cloned();
        // Notifications carry no id and must not be answered.
        id.as_ref()?;

        let result = match method {
            "initialize" => json!({
                "protocolVersion": req
                    .get("params")
                    .and_then(|p| p.get("protocolVersion"))
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18"),
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "leangraph", "version": env!("CARGO_PKG_VERSION")},
                "instructions": SERVER_INSTRUCTIONS
            }),
            "tools/list" => json!({"tools": Self::tools()}),
            "tools/call" => {
                let params = req.get("params").cloned().unwrap_or(json!({}));
                let name = params.get("name").and_then(Value::as_str).unwrap_or("");
                let args = params.get("arguments").cloned().unwrap_or(json!({}));
                match name {
                    "leangraph_explore" => self.explore(&args),
                    "leangraph_node" => self.node(&args),
                    other => self.guidance(format!(
                        "No tool named `{other}`. Available: leangraph_explore, leangraph_node."
                    )),
                }
            }
            "ping" => json!({}),
            _ => {
                return Some(json!({
                    "jsonrpc": "2.0", "id": id,
                    "error": {"code": -32601, "message": format!("unknown method: {method}")}
                }))
            }
        };
        Some(json!({"jsonrpc": "2.0", "id": id, "result": result}))
    }
}

pub fn serve(root: &Path) -> Result<()> {
    let server = Server::new(root.to_path_buf());
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout();

    for line in stdin.lock().lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let Ok(req) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        if let Some(resp) = server.handle(&req) {
            writeln!(stdout, "{resp}")?;
            stdout.flush()?;
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ helpers

fn describe(g: &Graph, n: crate::core::NodeId) -> String {
    use crate::core::DefKind;
    let (file, start, _) = g.location(n);
    let path = g.path(file);
    let kind = g.node_kind(n);
    if kind == DefKind::Module as u8 {
        return format!("file {path}");
    }
    let k = match kind {
        x if x == DefKind::Function as u8 => "fn",
        x if x == DefKind::Method as u8 => "method",
        x if x == DefKind::Class as u8 => "class",
        x if x == DefKind::Interface as u8 => "interface",
        x if x == DefKind::Variable as u8 => "var",
        _ => "?",
    };
    format!("{k} `{}` — {path}@{start}", g.name(n))
}

fn read_span(g: &Graph, n: crate::core::NodeId, cap: u32) -> Option<String> {
    let (file, start, end) = g.location(n);
    let bytes = std::fs::read(g.abs_path(file)).ok()?;
    let end = end.min(start.saturating_add(cap));
    let s = bytes.get(start as usize..(end as usize).min(bytes.len()))?;
    let mut text = String::from_utf8_lossy(s).into_owned();
    if (end as usize) < bytes.len() && end < g.location(n).2 {
        text.push_str("\n… (truncated)");
    }
    Some(text)
}
