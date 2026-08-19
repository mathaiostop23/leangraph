//! Context building — the component that decides what an agent actually sees.
//!
//! This is where "cheaper" is won or lost. CodeGraph's stated mechanism for
//! sufficiency is to return *more* (18K–35K chars scaled by repo size), because
//! an agent that gets an insufficient answer falls back to Read/Grep and that
//! costs far more than a big response. That is reliable and expensive.
//!
//! We aim to return *better*: rank by edge confidence, graph distance and
//! locality, then cut. Fewer tokens at equal answer quality is the only claim
//! worth making, and it is one you can only earn with a measurement — see
//! `bench/cost.sh` (not yet written).

use crate::core::{DefKind, NodeId};
use crate::graph::Graph;
use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::VecDeque;

pub struct Budget {
    pub max_nodes: usize,
    pub max_bytes: usize,
    /// Per-node ceiling. A django class body can be 2,000 lines; including it
    /// whole would blow the entire budget on one node, and it is unnecessary —
    /// its methods are separate nodes that can be selected on their own merits.
    pub max_node_bytes: u32,
}

impl Default for Budget {
    fn default() -> Self {
        Budget {
            max_nodes: 25,
            max_bytes: 24_000,
            max_node_bytes: 2_400,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Why {
    Seed,
    OnPath,
    Caller,
    Callee,
}

impl Why {
    pub fn label(self) -> &'static str {
        match self {
            Why::Seed => "seed",
            Why::OnPath => "on-path",
            Why::Caller => "caller",
            Why::Callee => "callee",
        }
    }
    /// Base weight. A node on the flow between two named symbols is the answer
    /// to "how does X reach Y" and outranks an arbitrary neighbour.
    fn weight(self) -> f32 {
        match self {
            Why::Seed => 1.0,
            Why::OnPath => 0.9,
            Why::Caller => 0.6,
            Why::Callee => 0.5,
        }
    }
}

pub struct Item {
    pub node: NodeId,
    pub why: Why,
    pub score: f32,
    pub bytes: u32,
}

pub struct Context {
    pub items: Vec<Item>,
    /// True when the flow runs from the *second* named symbol to the first.
    pub flow_reversed: bool,
    /// Path between the first two seeds, if one exists.
    pub flow: Vec<NodeId>,
    pub est_tokens: usize,
    pub dropped: usize,
}

/// Rank a name's candidate definitions: real code over tests, functions over
/// values, shallower paths over deeper.
fn candidate_rank(g: &Graph, n: NodeId) -> (u8, usize) {
    let (file, _, _) = g.location(n);
    let path = g.path(file);
    let is_test = path.contains("/test") || path.contains("_test.") || path.contains(".test.");
    let kind = g.node_kind(n);
    let demote = u8::from(kind == DefKind::Variable as u8 || kind == DefKind::Module as u8);
    (u8::from(is_test) * 2 + demote, path.matches('/').count())
}

pub fn seeds(g: &Graph, symbols: &[String]) -> Vec<NodeId> {
    let mut out = Vec::new();
    for s in symbols {
        // `Class.method` — disambiguate an overloaded method by its owner
        let (owner, name) = match s.split_once('.') {
            Some((o, n)) if !o.is_empty() && !n.is_empty() => (Some(o), n),
            _ => (None, s.as_str()),
        };
        let mut hits = g.find(name);
        if let Some(owner) = owner {
            let owned: Vec<NodeId> = hits
                .iter()
                .copied()
                .filter(|&n| g.callers(n).iter().any(|p| g.name(p.node) == owner))
                .collect();
            if !owned.is_empty() {
                hits = owned;
            }
        }
        hits.sort_by_key(|&n| candidate_rank(g, n));
        if let Some(&best) = hits.first() {
            if !out.contains(&best) {
                out.push(best);
            }
        }
    }
    out
}

/// Pull candidate symbol names out of free text — an issue title and body,
/// a commit message, a stack trace.
///
/// This is the entry point the server actually needs: nobody filing a bug
/// hands you a symbol list. We take every token that *is* a symbol in this
/// repo, which is precise by construction — a word that names nothing here
/// contributes nothing — and rank by specificity, because `get` appearing in
/// an issue is noise while `SQLCompiler` is the whole answer.
pub fn seeds_from_text(g: &Graph, text: &str, max: usize) -> Vec<NodeId> {
    let mut hits: Vec<(u8, usize, NodeId)> = Vec::new();
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();

    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')) {
        if raw.len() < 3 {
            continue;
        }
        // `models.query.QuerySet` and `QuerySet` should both find the class
        for tok in raw.split('.').chain(std::iter::once(raw)) {
            if tok.len() < 3 || is_stopword(tok) {
                continue;
            }
            let shape = identifier_shape(tok);
            let found = g.find(tok);
            if found.is_empty() || found.len() > 12 {
                // a name with dozens of definitions is a common word, not a lead
                continue;
            }
            // A plain lowercase word is only a lead if it is also rare here.
            // English is full of words that happen to be method names —
            // `using`, `when`, `raises` all resolve in django and all are noise.
            if shape == 0 && found.len() > 2 {
                continue;
            }
            let mut ranked = found;
            ranked.sort_by_key(|&n| candidate_rank(g, n));
            if let Some(&best) = ranked.first() {
                if seen.insert(best) {
                    hits.push((u8::MAX - shape, ranked.len(), best));
                }
            }
        }
    }
    // identifier-shaped first, then most specific
    hits.sort_by_key(|&(shape, spec, _)| (shape, spec));
    hits.into_iter().map(|(_, _, n)| n).take(max).collect()
}

/// How much a token looks like it was copied out of source rather than typed
/// as prose. This is the cheapest reliable signal available in an issue body.
#[inline]
fn identifier_shape(tok: &str) -> u8 {
    let upper = tok.chars().any(char::is_uppercase);
    let lower = tok.chars().any(char::is_lowercase);
    if tok.contains('_') {
        3 // snake_case
    } else if upper && lower {
        3 // camelCase / PascalCase
    } else if upper {
        2 // CONSTANT
    } else {
        0 // plain lowercase word — could be anything
    }
}

/// Words that are common in English *and* common as method names. Kept short
/// on purpose: the real filter is shape plus specificity, and a long list would
/// start suppressing genuine leads.
const STOPWORDS: &[&str] = &[
    "the", "and", "for", "not", "but", "with", "from", "this", "that", "when", "then", "than",
    "have", "has", "was", "are", "you", "all", "any", "can", "may", "use", "using", "used", "get",
    "set", "add", "new", "one", "two", "out", "off", "its", "our", "how", "why", "who", "what",
    "which", "where", "some", "only", "also", "into", "over", "same", "such", "each", "more",
    "most", "other", "should", "would", "could", "does", "did", "done", "make", "made", "see",
    "seen", "call", "called", "run", "running", "raise", "raises", "raised", "error", "errors",
    "issue", "bug", "fix", "fixed", "test", "tests", "code", "file", "files", "line", "lines",
];

#[inline]
fn is_stopword(tok: &str) -> bool {
    let lower = tok.to_ascii_lowercase();
    STOPWORDS.contains(&lower.as_str())
}

/// Shortest call path between two nodes, following forward edges.
///
/// This is what an agent asking "how does X reach Y" actually wants, and it is
/// the one thing a graph can answer that grep fundamentally cannot.
pub fn path(g: &Graph, from: NodeId, to: NodeId, max_hops: u32) -> Vec<NodeId> {
    if from == to {
        return vec![from];
    }
    let mut prev: FxHashMap<NodeId, NodeId> = FxHashMap::default();
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();
    let mut q = VecDeque::new();
    q.push_back((from, 0u32));
    seen.insert(from);

    while let Some((n, d)) = q.pop_front() {
        if d >= max_hops {
            continue;
        }
        for nb in g.callees(n) {
            if !seen.insert(nb.node) {
                continue;
            }
            prev.insert(nb.node, n);
            if nb.node == to {
                let mut p = vec![to];
                let mut cur = to;
                while let Some(&up) = prev.get(&cur) {
                    p.push(up);
                    cur = up;
                }
                p.reverse();
                return p;
            }
            q.push_back((nb.node, d + 1));
        }
    }
    Vec::new()
}

/// How much of a node we would actually emit, after per-node truncation.
#[inline]
fn charged_bytes(g: &Graph, n: NodeId, budget: &Budget) -> u32 {
    let (_, a, b) = g.location(n);
    b.saturating_sub(a).min(budget.max_node_bytes)
}

pub fn build(g: &Graph, symbols: &[String], budget: &Budget) -> Context {
    build_from(g, seeds(g, symbols), budget)
}

/// Same ranking, but seeded from free text instead of a symbol list.
///
/// Seed count scales with the budget. A fixed cap was silently the binding
/// constraint: at 200 nodes the expansion had only 8 places to expand from, so
/// recall flattened while the budget went unused.
pub fn build_from_text(g: &Graph, text: &str, budget: &Budget) -> Context {
    let max_seeds = (budget.max_nodes / 3).clamp(4, 32);
    let s = seeds_from_text(g, text, max_seeds);
    build_from(g, s, budget)
}

fn build_from(g: &Graph, seed_nodes: Vec<NodeId>, budget: &Budget) -> Context {
    let mut scored: FxHashMap<NodeId, (Why, f32)> = FxHashMap::default();

    for &s in &seed_nodes {
        scored.insert(s, (Why::Seed, Why::Seed.weight()));
    }

    // The flow between the first two named symbols, if there is one.
    let mut flow = Vec::new();
    let mut flow_reversed = false;
    if seed_nodes.len() >= 2 {
        flow = path(g, seed_nodes[0], seed_nodes[1], 8);
        if flow.is_empty() {
            flow = path(g, seed_nodes[1], seed_nodes[0], 8);
            flow_reversed = !flow.is_empty();
        }
        for &n in &flow {
            scored
                .entry(n)
                .or_insert((Why::OnPath, Why::OnPath.weight()));
        }
    }

    // Expand outward, weighted by the confidence of the edge that got us there.
    // A guessed edge contributes proportionally less than a proven one — this
    // is the whole reason `conf` is a core field rather than a decoration.
    //
    // A second hop only pays for itself once the budget is large enough to hold
    // it; below that it just crowds out closer, better-supported nodes.
    let hops = if budget.max_nodes >= 60 { 2 } else { 1 };
    let mut frontier: Vec<NodeId> = seed_nodes.clone();
    let mut decay = 1.0f32;
    for _ in 0..hops {
        let mut next = Vec::new();
        for &s in &frontier {
            for (why, nbs) in [(Why::Caller, g.callers(s)), (Why::Callee, g.callees(s))] {
                for nb in nbs {
                    let w = why.weight() * (nb.conf as f32 / 100.0) * decay;
                    let e = scored.entry(nb.node).or_insert((why, 0.0));
                    if e.1 < w {
                        *e = (why, w);
                        next.push(nb.node);
                    }
                }
            }
        }
        // Each hop is weaker evidence than the last; without decay a distant
        // node with one strong edge outranks a direct neighbour.
        decay *= 0.45;
        frontier = next;
        if frontier.is_empty() {
            break;
        }
    }

    let mut items: Vec<Item> = scored
        .into_iter()
        .map(|(node, (why, score))| Item {
            node,
            why,
            score,
            bytes: charged_bytes(g, node, budget),
        })
        .collect();

    items.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.bytes.cmp(&b.bytes)) // cheaper first at equal score
    });

    // Cut to budget. Seeds are never dropped: returning context that omits what
    // was asked for is the failure mode that sends an agent back to grep.
    let total = items.len();
    let mut kept: Vec<Item> = Vec::new();
    let mut bytes = 0usize;
    for it in items {
        let is_seed = it.why == Why::Seed;
        if !is_seed
            && (kept.len() >= budget.max_nodes || bytes + it.bytes as usize > budget.max_bytes)
        {
            continue;
        }
        bytes += it.bytes as usize;
        kept.push(it);
    }

    Context {
        dropped: total - kept.len(),
        // ~3.5 bytes/token is a reasonable proxy for source code. Only an
        // estimate — the product layer must count for real before billing a
        // budget against it.
        est_tokens: bytes * 2 / 7,
        items: kept,
        flow,
        flow_reversed,
    }
}
