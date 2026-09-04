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
    /// A name in the question that resolves in this repository.
    Seed,
    /// A file the question addressed by where it lives rather than by name.
    PathSeed,
    /// A node whose comments, docstring or strings the question describes.
    ProseSeed,
    OnPath,
    Caller,
    Callee,
    /// Reached through git co-change rather than through the AST.
    CoChange,
}

impl Why {
    pub fn label(self) -> &'static str {
        match self {
            Why::Seed => "seed",
            Why::PathSeed => "path",
            Why::ProseSeed => "prose",
            Why::OnPath => "on-path",
            Why::Caller => "caller",
            Why::Callee => "callee",
            Why::CoChange => "co-change",
        }
    }
    /// Base weight. A node on the flow between two named symbols is the answer
    /// to "how does X reach Y" and outranks an arbitrary neighbour.
    fn weight(self) -> f32 {
        match self {
            Why::Seed => 1.0,
            // Deliberately equal to a named seed for now. Whether a hint in a
            // comment should outrank, match or trail a name that resolves is a
            // question for the benchmarks, and changing two things at once
            // would make their answer unreadable.
            Why::PathSeed => 1.0,
            Why::ProseSeed => 1.0,
            Why::OnPath => 0.9,
            Why::Caller => 0.6,
            Why::Callee => 0.5,
            Why::CoChange => 0.4,
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
/// Files whose **path** the query describes.
///
/// "Use subprocess.run and PGPASSWORD for client in postgres backend" names no
/// symbol that exists anywhere, and points squarely at
/// `django/db/backends/postgresql/client.py` — `postgres`, `backend` and
/// `client` are all segments of that path. Seeding from symbol names alone
/// could not see it, and this is not a rare shape: of the 121 SWE-bench
/// instances where the context came back with nothing useful, 57 wanted a file
/// under `django/db`, and those issues describe behaviour rather than name code.
///
/// Two distinct tokens are required. One is a coincidence — every repository
/// has a `utils` and a `core` — and the second is what makes it an address.
fn seeds_from_path(g: &Graph, text: &str, max: usize) -> Vec<NodeId> {
    let mut want: Vec<String> = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        let t = raw.to_ascii_lowercase();
        if t.len() >= 4 && !is_stopword(&t) && !want.contains(&t) {
            want.push(t);
        }
    }
    if want.len() < 2 {
        return Vec::new();
    }

    let mut scored: Vec<(usize, usize, u32)> = Vec::new();
    for f in 0..g.n_files() {
        let path = g.path(f).to_ascii_lowercase();
        let depth = path.matches('/').count();
        let hits = want
            .iter()
            .filter(|t| {
                path.split(['/', '.', '_', '-']).any(|seg| {
                    // `postgres` should reach `postgresql`, and `migration`
                    // `migrations`, without `code` reaching `codecs`.
                    seg == t.as_str() || (t.len() >= 5 && seg.starts_with(t.as_str()))
                })
            })
            .count();
        if hits >= 2 {
            scored.push((hits, depth, f));
        }
    }
    if scored.is_empty() {
        return Vec::new();
    }
    // Most tokens matched, then the shallower path: `db/backends/postgresql`
    // over a test fixture that happens to repeat the same words.
    scored.sort_by_key(|&(hits, depth, f)| (std::cmp::Reverse(hits), depth, f));
    let keep: Vec<u32> = scored.into_iter().take(max).map(|(_, _, f)| f).collect();

    // One pass over the nodes rather than one per file: a repository with
    // 68,000 nodes and eight candidate files would otherwise be eight scans.
    let mut per_file: FxHashMap<u32, Vec<NodeId>> = FxHashMap::default();
    for n in (0..g.n_nodes()).map(NodeId) {
        let f = g.location(n).0;
        if keep.contains(&f) && !g.name(n).is_empty() {
            per_file.entry(f).or_default().push(n);
        }
    }

    let mut out = Vec::new();
    for f in keep {
        let Some(mut ns) = per_file.remove(&f) else {
            continue;
        };
        ns.sort_by_key(|&n| candidate_rank(g, n));
        out.extend(ns.into_iter().take(2));
        if out.len() >= max {
            break;
        }
    }
    out
}

/// Nodes whose *prose* the query describes — comments, docstrings and the
/// strings a program prints.
///
/// This is the only part of a repository written in the language its users
/// speak, and it is the half the seeder was blind to. "The run never continued
/// after the reviewer approved it" resolves no symbol: `run` and `approved` are
/// too common to be leads, and nothing in that sentence is an identifier. It is
/// almost word-for-word the docstring above the function that handles it.
///
/// Scored as BM25 with a binary term frequency. Frequency is binary because a
/// word is stored once per definition — how often a comment repeats itself is
/// not evidence about the code under it — which collapses the usual saturation
/// term into a constant per node, leaving length-normalised IDF. Length
/// normalisation is what stops a 2,000-line module with a long licence header
/// from answering every question.
fn seeds_from_prose(g: &Graph, text: &str, max: usize) -> Vec<NodeId> {
    // Only words this repository actually writes down. One binary search each,
    // and a word nobody here has ever typed costs nothing further.
    let mut want: Vec<u32> = Vec::new();
    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
        if raw.len() < 4 || raw.len() > 24 {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        if let Some(id) = g.sym_id(&lower) {
            if !want.contains(&id) {
                want.push(id);
            }
        }
    }
    if want.is_empty() {
        return Vec::new();
    }
    let wanted: FxHashSet<u32> = want.iter().copied().collect();

    // One pass over the prose of every node: which query words it holds, and
    // how long its prose is. Nodes with none — most of them — cost a bounds
    // check and nothing else.
    let mut hits: Vec<(NodeId, u32)> = Vec::new();
    let mut lens: FxHashMap<NodeId, u32> = FxHashMap::default();
    let mut total_len = 0u64;
    let mut n_docs = 0u64;
    for n in (0..g.n_nodes()).map(NodeId) {
        let words = g.prose(n);
        if words.is_empty() {
            continue;
        }
        n_docs += 1;
        total_len += words.len() as u64;
        let mut any = false;
        for &w in words {
            if wanted.contains(&w) {
                hits.push((n, w));
                any = true;
            }
        }
        if any {
            lens.insert(n, words.len() as u32);
        }
    }
    if hits.is_empty() {
        return Vec::new();
    }

    let mut df: FxHashMap<u32, u32> = FxHashMap::default();
    for &(_, w) in &hits {
        *df.entry(w).or_default() += 1;
    }
    let n_docs = n_docs.max(1) as f32;
    let avg = (total_len as f32 / n_docs).max(1.0);

    const B: f32 = 0.75;
    let mut score: FxHashMap<NodeId, f32> = FxHashMap::default();
    for &(n, w) in &hits {
        let d = *df.get(&w).unwrap_or(&1) as f32;
        let idf = (1.0 + (n_docs - d + 0.5) / (d + 0.5)).ln();
        let len = *lens.get(&n).unwrap_or(&1) as f32;
        *score.entry(n).or_default() += idf / (1.0 - B + B * len / avg);
    }

    let mut ranked: Vec<(NodeId, f32)> = score.into_iter().collect();
    // Ties broken by node id so the same repository and query always answer the
    // same way; a ranking that shuffles is a benchmark that cannot be repeated.
    ranked.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(a.0.cmp(&b.0))
    });
    ranked.into_iter().take(max).map(|(n, _)| n).collect()
}

pub fn seeds_from_text(g: &Graph, text: &str, max: usize) -> Vec<NodeId> {
    seeds_from_text_why(g, text, max)
        .into_iter()
        .map(|(n, _)| n)
        .collect()
}

/// The same seeds, each carrying which signal produced it.
///
/// Provenance is not decoration. It is what lets the ranking treat a name that
/// resolves differently from a word in a comment, what lets the budget give a
/// signal its own slice, and what tells whoever reads the context back why a
/// fragment is in front of them.
pub fn seeds_from_text_why(g: &Graph, text: &str, max: usize) -> Vec<(NodeId, Why)> {
    let mut hits: Vec<(u8, usize, NodeId)> = Vec::new();
    // Tokens rejected only for being stopwords, kept in case nothing else
    // survives. The stopword list is a proxy for "too common to be a lead";
    // where the repository disagrees — one definition of `add` in the whole
    // tree — the repository is the better authority. These are used only when
    // the ordinary path finds nothing, so a large codebase never sees them.
    let mut fallback: Vec<(usize, NodeId)> = Vec::new();
    let mut seen: FxHashSet<NodeId> = FxHashSet::default();

    for raw in text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '.')) {
        if raw.len() < 3 {
            continue;
        }
        // `models.query.QuerySet` and `QuerySet` should both find the class
        for tok in raw.split('.').chain(std::iter::once(raw)) {
            if tok.len() < 3 {
                continue;
            }
            if is_stopword(tok) {
                let found = g.find(tok);
                if !found.is_empty() && found.len() <= 2 {
                    let mut ranked = found;
                    ranked.sort_by_key(|&n| candidate_rank(g, n));
                    if let Some(&best) = ranked.first() {
                        fallback.push((ranked.len(), best));
                    }
                }
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
    if hits.is_empty() {
        // Nothing but common words, and some of them name something that
        // exists exactly once here. Better a narrow lead than no context.
        fallback.sort_by_key(|&(spec, n)| (spec, n));
        fallback.dedup_by_key(|&mut (_, n)| n);
        let mut out: Vec<(NodeId, Why)> =
            fallback.into_iter().map(|(_, n)| (n, Why::Seed)).collect();
        // `contains` rather than `dedup`, which only removes *neighbours*: a
        // fallback lead and a path seed can be the same node without landing
        // next to each other, and a repeated seed silently spends a slot twice.
        for n in seeds_from_path(g, text, max) {
            if !out.iter().any(|&(m, _)| m == n) {
                out.push((n, Why::PathSeed));
            }
        }
        fill_from_prose(g, text, max, &mut out);
        return out.into_iter().take(max).collect();
    }
    // identifier-shaped first, then most specific
    hits.sort_by_key(|&(shape, spec, _)| (shape, spec));
    let mut out: Vec<(NodeId, Why)> = hits.into_iter().map(|(_, _, n)| (n, Why::Seed)).collect();

    // Path evidence *after* symbol evidence, never instead of it: a name that
    // resolves in this repository is the stronger signal, and appending can
    // only fill slots the names left empty rather than take any.
    //
    // Appending rather than falling back is the whole of the gain, and that is
    // not what it looks like it should be. The obvious reading of "24% of
    // contexts come back with nothing useful" is that no name resolved there —
    // so restricting path seeds to that case ought to buy the same rescues
    // without disturbing anything. Measured over the same 500 instances, it
    // does not: the fallback branch fires on only 25 of those 121, rescues 5,
    // and lands at -0.5 points with p = 0.58. In the other 96 the names
    // resolve perfectly well and simply point at the wrong code, and path
    // evidence is worth having precisely where it sits *beside* a name that
    // did not pan out. Appending: +4.0 points, 41 of the 121 rescued, p = 0.03.
    //
    // It is not free. The expansion has a hundred nodes to spend and every
    // extra root spreads them thinner, so 17 contexts that had the right file
    // lost it — django__django-11815 went from 3,904 tokens holding the answer
    // to 10,316 tokens without it. Net of both halves the change is worth
    // making, and the losing half is real.
    //
    // Capping these at a reserved fifth was tried against exactly that half,
    // a reserved slice being what fixed the same shape for co-change edges,
    // and it is worse: rescues fall from 47 to 34 while only 6 of the 17
    // losses are avoided — about two rescues surrendered per loss prevented,
    // -1.6 points. So the tail of the path ranking is not padding. The right
    // file is frequently *not* the best-scoring path match, and the extra
    // roots earn the dilution they cause. Declined, on the measurement.
    // Paths take what the names left, minus a slice held back for prose.
    //
    // Without the reservation prose is not outranked, it is never asked.
    // Measured over a stratified sample of SWE-bench: of 2,015 seeds, 1,145
    // came from paths and 213 from prose, and 78% of instances got no prose
    // seed at all. Path seeding is greedy by construction — it takes two nodes
    // per matching file until the budget is gone — so on any issue whose names
    // resolve thinly it fills every remaining slot before the question of what
    // the comments say is ever put.
    let quota = max / 4;
    let room = max.saturating_sub(quota);
    if out.len() < room {
        for n in seeds_from_path(g, text, room - out.len()) {
            if !out.iter().any(|&(m, _)| m == n) {
                out.push((n, Why::PathSeed));
            }
        }
    }
    fill_from_prose(g, text, max, &mut out);
    // Prose may leave its slice unspent — a question whose words this
    // repository never writes down has nothing to give. Paths get the
    // remainder rather than letting it go to waste.
    if out.len() < max {
        for n in seeds_from_path(g, text, max - out.len()) {
            if !out.iter().any(|&(m, _)| m == n) {
                out.push((n, Why::PathSeed));
            }
        }
    }
    out.into_iter().take(max).collect()
}

/// Prose last, into whatever the names and the paths left empty.
///
/// Last because it is the broadest signal and the easiest to be wrong about: a
/// name that resolves here is a fact, and a word in a comment is a hint. Which
/// of them should give way when the seed budget is full is a question the two
/// benchmarks answer, not this comment.
fn fill_from_prose(g: &Graph, text: &str, max: usize, out: &mut Vec<(NodeId, Why)>) {
    if out.len() >= max {
        return;
    }
    for n in seeds_from_prose(g, text, max - out.len()) {
        if !out.iter().any(|&(m, _)| m == n) {
            out.push((n, Why::ProseSeed));
        }
    }
}

/// How much a token looks like it was copied out of source rather than typed
/// as prose. This is the cheapest reliable signal available in an issue body.
// snake_case and camelCase deliberately score the same: both are strong
// evidence the token was copied from source, and there is no reason to rank one
// above the other. Clippy sees two identical blocks; the comments are the point.
#[allow(clippy::if_same_then_else)]
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
    // Asking for a symbol by name is the one case with no ambiguity about
    // where the seed came from.
    let s = seeds(g, symbols)
        .into_iter()
        .map(|n| (n, Why::Seed))
        .collect();
    build_from(g, s, budget)
}

/// Same ranking, but seeded from free text instead of a symbol list.
///
/// Seed count scales with the budget. A fixed cap was silently the binding
/// constraint: at 200 nodes the expansion had only 8 places to expand from, so
/// recall flattened while the budget went unused.
pub fn build_from_text(g: &Graph, text: &str, budget: &Budget) -> Context {
    let max_seeds = (budget.max_nodes / 3).clamp(4, 32);
    let s = seeds_from_text_why(g, text, max_seeds);
    build_from(g, s, budget)
}

fn build_from(g: &Graph, seed_nodes: Vec<(NodeId, Why)>, budget: &Budget) -> Context {
    let mut scored: FxHashMap<NodeId, (Why, f32)> = FxHashMap::default();

    for &(s, why) in &seed_nodes {
        scored.insert(s, (why, why.weight()));
    }
    let seed_nodes: Vec<NodeId> = seed_nodes.into_iter().map(|(n, _)| n).collect();

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
                    // provenance 3 is CoChange; it is scored on its own scale
                    let (why, w) = if nb.prov == 3 {
                        (
                            Why::CoChange,
                            Why::CoChange.weight() * (nb.conf as f32 / 100.0),
                        )
                    } else {
                        (why, why.weight() * (nb.conf as f32 / 100.0) * decay)
                    };
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
    //
    // Co-change gets a reserved slice rather than competing on the same scale.
    // It is a different *kind* of evidence, not a weaker version of the same
    // one — mixed into one ranking it always loses to structural edges, and
    // measurement confirmed it: it never survived the cut and net recall went
    // down. Historical coupling is exactly what the AST cannot see, so it earns
    // its own quota.
    let quota = budget.max_nodes / 5;
    let total = items.len();
    let mut kept: Vec<Item> = Vec::new();
    let mut bytes = 0usize;
    let mut co_used = 0usize;

    for it in items {
        let is_seed = matches!(it.why, Why::Seed | Why::PathSeed | Why::ProseSeed);
        let is_co = it.why == Why::CoChange;
        let room = if is_co {
            co_used < quota && kept.len() < budget.max_nodes + quota
        } else {
            kept.len() - co_used < budget.max_nodes
        };
        if !is_seed && (!room || bytes + it.bytes as usize > budget.max_bytes) {
            continue;
        }
        bytes += it.bytes as usize;
        co_used += usize::from(is_co);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::Corpus;

    /// A chain deep enough that a budget has to refuse something.
    fn chain(n: usize) -> Corpus {
        let mut src = String::new();
        for i in 0..n {
            src.push_str(&format!(
                "def step{i}(x):\n    # padding to give this node some bytes\n    return step{}(x)\n\n",
                i + 1
            ));
        }
        src.push_str(
            "def step{}(x):\n    return x\n"
                .replace("{}", &n.to_string())
                .as_str(),
        );
        let files: Vec<(String, String)> = vec![("chain.py".to_string(), src)];
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();
        Corpus::build(&refs)
    }

    #[test]
    fn the_budget_is_a_ceiling_not_a_suggestion() {
        // Returning fewer tokens at equal recall is the entire argument of this
        // project. A selection that overruns its budget is not a smaller
        // context, it is a bigger one with a number attached.
        let c = chain(40);
        let g = c.graph();
        let budget = Budget {
            max_nodes: 5,
            max_bytes: 400,
            max_node_bytes: 200,
        };
        let ctx = build(&g, &["step0".to_string()], &budget);
        assert!(
            ctx.items.len() <= budget.max_nodes,
            "{} items exceeds max_nodes {}",
            ctx.items.len(),
            budget.max_nodes
        );
        let total: u32 = ctx.items.iter().map(|i| i.bytes).sum();
        assert!(
            total as usize <= budget.max_bytes,
            "{total} bytes exceeds max_bytes {}",
            budget.max_bytes
        );
        assert!(
            ctx.items.iter().all(|i| i.bytes <= budget.max_node_bytes),
            "no single node may exceed its own ceiling"
        );
    }

    #[test]
    fn a_tighter_budget_never_returns_more() {
        let c = chain(30);
        let g = c.graph();
        let wide = build(
            &g,
            &["step0".to_string()],
            &Budget {
                max_nodes: 20,
                max_bytes: 20_000,
                max_node_bytes: 2_000,
            },
        );
        let tight = build(
            &g,
            &["step0".to_string()],
            &Budget {
                max_nodes: 3,
                max_bytes: 20_000,
                max_node_bytes: 2_000,
            },
        );
        assert!(tight.items.len() <= wide.items.len());
        assert!(tight.items.len() <= 3);
    }

    #[test]
    fn what_was_asked_for_comes_back_first() {
        // A seed the caller named must not be evicted in favour of something
        // the walk found. It is the one node the caller is certain about.
        let c = chain(12);
        let g = c.graph();
        let ctx = build(
            &g,
            &["step0".to_string()],
            &Budget {
                max_nodes: 2,
                max_bytes: 20_000,
                max_node_bytes: 2_000,
            },
        );
        assert!(!ctx.items.is_empty());
        assert_eq!(ctx.items[0].why, Why::Seed);
        assert_eq!(g.name(ctx.items[0].node), "step0");
    }

    #[test]
    fn a_path_is_found_and_respects_its_hop_limit() {
        let c = chain(10);
        let g = c.graph();
        let from = g.find("step0")[0];
        let to = g.find("step4")[0];

        let p = path(&g, from, to, 16);
        assert_eq!(p.first(), Some(&from));
        assert_eq!(p.last(), Some(&to));
        assert!(p.len() >= 2);

        // Every consecutive pair must be a real edge, or the path is fiction.
        for pair in p.windows(2) {
            assert!(
                g.callees(pair[0]).iter().any(|n| n.node == pair[1]),
                "{} -> {} is not an edge",
                g.qualified(pair[0]),
                g.qualified(pair[1])
            );
        }

        assert!(
            path(&g, from, to, 1).is_empty(),
            "a target four hops away must not be reachable in one"
        );
    }

    #[test]
    fn a_node_reaches_itself_in_zero_hops() {
        let g = chain(3).graph();
        let n = g.find("step0")[0];
        assert_eq!(path(&g, n, n, 4), vec![n]);
    }

    #[test]
    fn unconnected_nodes_have_no_path() {
        let c = Corpus::build(&[
            ("a.py", "def alpha():\n    return 1\n"),
            ("b.py", "def beta():\n    return 2\n"),
        ]);
        let g = c.graph();
        let a = g.find("alpha")[0];
        let b = g.find("beta")[0];
        assert!(path(&g, a, b, 8).is_empty());
    }

    // ---- seed selection from prose ----------------------------------------

    #[test]
    fn identifier_shape_ranks_code_above_prose() {
        assert!(identifier_shape("send_file") > identifier_shape("file"));
        assert!(identifier_shape("sendFile") > identifier_shape("file"));
        assert!(identifier_shape("SendFile") > identifier_shape("file"));
        assert!(identifier_shape("MAX_SIZE") > identifier_shape("size"));
        assert_eq!(identifier_shape("crashes"), 0);
    }

    #[test]
    fn words_that_are_both_english_and_method_names_are_not_seeds() {
        for w in ["error", "test", "run", "fix", "the", "file"] {
            assert!(is_stopword(w), "{w} should be a stopword");
        }
        for w in ["send_file", "Flask", "werkzeug", "descriptor"] {
            assert!(!is_stopword(w), "{w} must survive as a lead");
        }
    }

    #[test]
    fn a_bug_report_selects_the_symbol_it_names() {
        let c = Corpus::build(&[(
            "app.py",
            "def send_file(path):\n    return open(path)\n\ndef unrelated():\n    return 0\n",
        )]);
        let g = c.graph();
        let seeds = seeds_from_text(
            &g,
            "The file descriptor leaks when send_file streams a large download.",
            8,
        );
        let names: Vec<&str> = seeds.iter().map(|n| g.name(*n)).collect();
        assert!(
            names.contains(&"send_file"),
            "the named symbol must be picked: {names:?}"
        );
        assert!(
            !names.contains(&"unrelated"),
            "and an unmentioned one must not be"
        );
    }

    #[test]
    fn text_naming_nothing_in_the_repo_selects_nothing() {
        let c = Corpus::build(&[("app.py", "def send_file(path):\n    return path\n")]);
        let g = c.graph();
        let seeds = seeds_from_text(&g, "The documentation could be clearer about this.", 8);
        assert!(
            seeds.is_empty(),
            "prose with no leads must not invent them: {:?}",
            seeds.iter().map(|n| g.name(*n)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn asking_for_a_symbol_that_does_not_exist_is_empty_not_a_panic() {
        let g = Corpus::build(&[("a.py", "def alpha():\n    return 1\n")]).graph();
        let ctx = build(&g, &["no_such_thing".to_string()], &Budget::default());
        assert!(ctx.items.is_empty());
        assert!(seeds(&g, &["no_such_thing".to_string()]).is_empty());
    }
    // ---- seed selection from paths ----------------------------------------

    /// The shape `seeds_from_path` exists for: an issue that describes
    /// behaviour, names no symbol that resolves anywhere, and addresses a file
    /// by where it lives.
    #[test]
    fn an_issue_that_names_only_a_path_still_reaches_the_file() {
        let c = Corpus::build(&[
            (
                "django/db/backends/postgresql/client.py",
                "def runshell(conn):\n    return conn\n",
            ),
            (
                "django/core/mail/message.py",
                "def sanitize(addr):\n    return addr\n",
            ),
        ]);
        let g = c.graph();
        let seeds = seeds_from_text(
            &g,
            "Use subprocess and PGPASSWORD when starting a shell for the postgres backend client.",
            8,
        );
        let names: Vec<&str> = seeds.iter().map(|n| g.name(*n)).collect();
        assert!(
            names.contains(&"runshell"),
            "three path segments name this file and nothing else: {names:?}"
        );
        assert!(
            !names.contains(&"sanitize"),
            "a file the path words do not address must not come along: {names:?}"
        );
    }

    #[test]
    fn a_single_path_word_is_a_coincidence_not_an_address() {
        let c = Corpus::build(&[
            (
                "django/db/backends/postgresql/client.py",
                "def runshell(conn):\n    return conn\n",
            ),
            ("django/utils/http.py", "def urlencode(q):\n    return q\n"),
        ]);
        let g = c.graph();
        let seeds = seeds_from_text(&g, "The client hangs occasionally on startup.", 8);
        assert!(
            seeds.is_empty(),
            "one segment is every repository's `utils`: {:?}",
            seeds.iter().map(|n| g.name(*n)).collect::<Vec<_>>()
        );
    }

    /// Path evidence is the weaker signal and must only ever fill slots the
    /// names left empty — never take one.
    #[test]
    fn path_evidence_fills_slots_that_symbols_left_empty() {
        let c = Corpus::build(&[
            (
                "django/db/backends/postgresql/client.py",
                "def runshell(conn):\n    return conn\n",
            ),
            (
                "django/utils/formats.py",
                "def sanitize_separators(value):\n    return value\n",
            ),
        ]);
        let g = c.graph();
        let text = "sanitize_separators breaks for the postgres backend client.";

        let seeds = seeds_from_text(&g, text, 8);
        let names: Vec<&str> = seeds.iter().map(|n| g.name(*n)).collect();
        assert_eq!(
            names.first(),
            Some(&"sanitize_separators"),
            "a name that resolves here outranks a path that matches: {names:?}"
        );
        assert!(
            names.contains(&"runshell"),
            "and the path still fills the slots left over: {names:?}"
        );

        // With room for one seed, the symbol takes it and the path gets none.
        let tight = seeds_from_text(&g, text, 1);
        let tight: Vec<&str> = tight.iter().map(|n| g.name(*n)).collect();
        assert_eq!(tight, vec!["sanitize_separators"]);
    }
}
