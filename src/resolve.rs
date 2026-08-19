//! Three-tier reference resolution.
//!
//! Phase A builds a read-only global index (one sequential barrier).
//! Phase B resolves every reference in parallel against it — no locking, since
//! nothing is mutated after the barrier.
//!
//! Every edge is tagged with the tier that produced it. Nothing is emitted
//! without provenance: an unlabelled guess is worse than no edge, because a
//! consumer cannot discount it.

use crate::core::{
    Def, DefIdx, DefKind, Edge, EdgeKind, FileId, FileUnit, Interner, NodeId, Provenance, RefKind,
    SymId, NO_SCOPE,
};
use lasso::Key;
use crate::lang::Lang;
use rayon::prelude::*;
use rustc_hash::{FxHashMap, FxHashSet};
use std::path::{Path, PathBuf};

/// Above this many equally-ranked candidates a name carries no information —
/// emitting the edges would explode the graph without making it more useful.
/// (`get` in django has 4,834 call sites; linking each to every `def get` would
/// add ~10^6 edges and zero signal.)
const MAX_AMBIGUITY: usize = 8;

#[derive(Default, Debug, Clone, Copy)]
pub struct ResolveStats {
    pub scope: u64,
    pub import: u64,
    pub name_unique: u64,
    pub name_ambiguous: u64,
    pub too_ambiguous: u64,
    /// Bare identifier reads that reached tier 3, where name matching is not
    /// justified. Reported, not silently dropped.
    pub weak_read: u64,
    pub builtin: u64,
    pub external: u64,
    pub contains: u64,
}

impl ResolveStats {
    fn merge(mut self, o: ResolveStats) -> ResolveStats {
        self.scope += o.scope;
        self.import += o.import;
        self.name_unique += o.name_unique;
        self.name_ambiguous += o.name_ambiguous;
        self.too_ambiguous += o.too_ambiguous;
        self.weak_read += o.weak_read;
        self.builtin += o.builtin;
        self.external += o.external;
        self.contains += o.contains;
        self
    }
    pub fn resolved(&self) -> u64 {
        self.scope + self.import + self.name_unique + self.name_ambiguous
    }
    pub fn total_refs(&self) -> u64 {
        self.resolved() + self.too_ambiguous + self.weak_read + self.builtin + self.external
    }
    /// References whose target actually exists in this repository.
    ///
    /// This is the only honest denominator for a resolution-rate claim. A call
    /// to `len()` or to `react.useState` has no definition in the repo, so
    /// failing to link it is correct behaviour, not a miss — counting those as
    /// failures would understate the resolver, and excluding them silently
    /// would flatter it. We report all three buckets separately.
    pub fn in_repo(&self) -> u64 {
        self.resolved() + self.too_ambiguous
    }
}

/// Names that resolve to the language runtime, not to repository code.
/// Counting these as failures understates resolution; emitting edges for them
/// would be worse — they would point nowhere.
const PY_BUILTINS: &[&str] = &[
    "len", "str", "int", "float", "bool", "list", "dict", "set", "tuple", "print", "isinstance",
    "issubclass", "super", "type", "range", "enumerate", "zip", "map", "filter", "sorted", "sum",
    "min", "max", "abs", "any", "all", "open", "getattr", "setattr", "hasattr", "delattr", "repr",
    "id", "hash", "iter", "next", "format", "bytes", "bytearray", "object", "property",
    "staticmethod", "classmethod", "callable", "vars", "dir", "round", "divmod", "pow", "chr",
    "ord", "hex", "oct", "bin", "frozenset", "complex", "slice", "reversed", "input", "eval",
    "exec", "compile", "globals", "locals", "Exception", "BaseException", "ValueError",
    "TypeError", "KeyError", "IndexError", "AttributeError", "RuntimeError", "NotImplementedError",
    "StopIteration", "ImportError", "OSError", "IOError", "ZeroDivisionError", "AssertionError",
    "KeyboardInterrupt", "SystemExit", "UnicodeDecodeError", "UnicodeEncodeError",
];

const JS_BUILTINS: &[&str] = &[
    "console", "Object", "Array", "String", "Number", "Boolean", "Promise", "Math", "JSON", "Date",
    "Map", "Set", "WeakMap", "WeakSet", "Error", "TypeError", "RangeError", "SyntaxError",
    "RegExp", "Symbol", "Proxy", "Reflect", "BigInt", "parseInt", "parseFloat", "isNaN",
    "isFinite", "encodeURIComponent", "decodeURIComponent", "encodeURI", "decodeURI", "require",
    "setTimeout", "setInterval", "clearTimeout", "clearInterval", "queueMicrotask", "structuredClone",
    "fetch", "URL", "URLSearchParams", "Buffer", "process", "globalThis", "Function", "ArrayBuffer",
    "Uint8Array", "Int32Array", "Float64Array", "DataView", "Intl", "AbortController", "TextEncoder",
    "TextDecoder", "Headers", "Request", "Response", "FormData", "Blob", "File", "Event",
];

/// Flat global node id space: files occupy `[0, n_files)`, then each file's
/// definitions occupy a contiguous run. Keeping it flat is what lets the CSR
/// in the next phase be a pair of `Vec<u32>` with no indirection.
pub struct NodeSpace {
    pub n_files: u32,
    def_base: Vec<u32>,
    pub total: u32,
}

impl NodeSpace {
    fn build(units: &[FileUnit]) -> NodeSpace {
        let n_files = units.len() as u32;
        let mut def_base = Vec::with_capacity(units.len() + 1);
        let mut acc = n_files;
        for u in units {
            def_base.push(acc);
            acc += u.defs.len() as u32;
        }
        def_base.push(acc);
        NodeSpace {
            n_files,
            def_base,
            total: acc,
        }
    }
    #[inline]
    pub fn file_node(&self, f: FileId) -> NodeId {
        NodeId(f)
    }
    #[inline]
    pub fn def_node(&self, f: FileId, d: DefIdx) -> NodeId {
        NodeId(self.def_base[f as usize] + d)
    }
}

// ------------------------------------------------------------ module naming

/// Every key a file can be imported by, longest first.
///
/// `django/db/models/query.py` yields `django.db.models.query`, `db.models.query`,
/// `models.query`, `query`. Emitting suffixes costs little and makes resolution
/// work when the repo root is not the package root — a very common layout.
fn module_keys(root: &Path, path: &Path, lang: Lang) -> Vec<String> {
    let Ok(rel) = path.strip_prefix(root) else {
        return Vec::new();
    };
    let mut parts: Vec<String> = rel
        .components()
        .map(|c| c.as_os_str().to_string_lossy().into_owned())
        .collect();
    if let Some(last) = parts.last_mut() {
        if let Some(dot) = last.rfind('.') {
            last.truncate(dot);
        }
    }
    // package entry points address the directory, not the file
    match parts.last().map(String::as_str) {
        Some("__init__") | Some("index") | Some("mod") => {
            parts.pop();
        }
        _ => {}
    }
    if parts.is_empty() {
        return Vec::new();
    }
    let sep = if lang == Lang::Python { "." } else { "/" };
    (0..parts.len()).map(|i| parts[i..].join(sep)).collect()
}

/// Resolve one import statement to a file.
///
/// Relative TypeScript specifiers are normalised against the importing file's
/// directory; everything else is matched against the suffix keys above.
fn resolve_import(
    spec: &str,
    importer: &Path,
    root: &Path,
    by_module: &FxHashMap<String, FileId>,
) -> Option<FileId> {
    if spec.starts_with('.') {
        let mut base = importer.parent()?.to_path_buf();
        for part in spec.split('/') {
            match part {
                "." | "" => {}
                ".." => {
                    base.pop();
                }
                p => base.push(p),
            }
        }
        let rel = base.strip_prefix(root).ok()?;
        let key = rel.to_string_lossy().replace('\\', "/");
        return by_module
            .get(&key)
            .or_else(|| by_module.get(key.trim_end_matches("/index")))
            .copied();
    }
    // Python dotted, or a bare/aliased TS specifier.
    by_module
        .get(spec)
        .or_else(|| by_module.get(&spec.replace('.', "/")))
        .copied()
}

// ------------------------------------------------------------------ resolve

/// Flat per-node metadata, structure-of-arrays on disk.
#[derive(Clone, Copy, Default)]
pub struct NodeMeta {
    pub name: u32,
    pub kind: u8,
    pub file: u32,
    pub start: u32,
    pub end: u32,
}

pub struct Resolved {
    pub space: NodeSpace,
    pub nodes: Vec<NodeMeta>,
    pub edges: Vec<Edge>,
    pub stats: ResolveStats,
}

pub fn resolve(
    units: &[FileUnit],
    paths: &[PathBuf],
    langs: &[Lang],
    root: &Path,
    interner: &Interner,
) -> Resolved {
    let space = NodeSpace::build(units);

    // ---- phase A: global index (sequential barrier) ------------------------
    let mut by_name: FxHashMap<SymId, Vec<(FileId, DefIdx)>> = FxHashMap::default();
    let mut by_module: FxHashMap<String, FileId> = FxHashMap::default();
    let mut exports: Vec<FxHashMap<SymId, DefIdx>> = Vec::with_capacity(units.len());

    for (f, unit) in units.iter().enumerate() {
        let fid = f as FileId;
        let mut top: FxHashMap<SymId, DefIdx> = FxHashMap::default();
        for (d, def) in unit.defs.iter().enumerate() {
            by_name.entry(def.name).or_default().push((fid, d as DefIdx));
            if def.parent == NO_SCOPE {
                top.insert(def.name, d as DefIdx);
            }
        }
        exports.push(top);

        for key in module_keys(root, &paths[f], langs[f]) {
            // longest key wins; shorter suffixes only fill gaps
            by_module.entry(key).or_insert(fid);
        }
    }

    // Intern builtin names once so the hot path compares u32s, not strings.
    let builtins: FxHashSet<SymId> = PY_BUILTINS
        .iter()
        .chain(JS_BUILTINS.iter())
        .filter_map(|n| interner.get(n))
        .collect();

    let dirs: Vec<&Path> = paths.iter().map(|p| p.parent().unwrap_or(root)).collect();

    // ---- phase B: resolve in parallel (index is read-only from here) -------
    let (edges, stats) = units
        .par_iter()
        .enumerate()
        .map(|(f, unit)| {
            let fid = f as FileId;
            let mut out: Vec<Edge> = Vec::with_capacity(unit.refs.len() + unit.defs.len());
            let mut st = ResolveStats::default();

            // containment: file -> top-level def -> nested def
            for (d, def) in unit.defs.iter().enumerate() {
                let src = if def.parent == NO_SCOPE {
                    space.file_node(fid)
                } else {
                    space.def_node(fid, def.parent)
                };
                out.push(Edge {
                    src,
                    dst: space.def_node(fid, d as DefIdx),
                    kind: EdgeKind::Contains,
                    conf: Provenance::Scope.base_conf(),
                    prov: Provenance::Scope,
                });
                st.contains += 1;
            }

            // (scope, name) -> def, for the lexical walk
            let mut scoped: FxHashMap<(DefIdx, SymId), DefIdx> = FxHashMap::default();
            for (d, def) in unit.defs.iter().enumerate() {
                scoped.insert((def.parent, def.name), d as DefIdx);
            }

            // names this file pulled in, flattened once instead of per-reference
            let mut imported: FxHashMap<SymId, NodeId> = FxHashMap::default();
            for imp in &unit.imports {
                let spec = interner.resolve(&imp.module);
                if let Some(tf) = resolve_import(spec, &paths[f], root, &by_module) {
                    if tf == fid {
                        continue;
                    }
                    out.push(Edge {
                        src: space.file_node(fid),
                        dst: space.file_node(tf),
                        kind: EdgeKind::Imports,
                        conf: Provenance::Import.base_conf(),
                        prov: Provenance::Import,
                    });
                    // `or_insert` means the first writer wins, so iterating a
                    // hash map here let thread-dependent ordering decide which
                    // import shadows another. Sort first.
                    let mut names: Vec<(&SymId, &DefIdx)> = exports[tf as usize].iter().collect();
                    names.sort_unstable_by_key(|(n, _)| n.into_usize());
                    for (name, didx) in names {
                        imported.entry(*name).or_insert(space.def_node(tf, *didx));
                    }
                }
            }

            for r in &unit.refs {
                // tier 1 — lexical scope chain
                if let Some(hit) = walk_scopes(&unit.defs, &scoped, r.scope, r.name) {
                    out.push(Edge {
                        src: enclosing(&space, fid, r.scope),
                        dst: space.def_node(fid, hit),
                        kind: r.kind.edge_kind(),
                        conf: Provenance::Scope.base_conf(),
                        prov: Provenance::Scope,
                    });
                    st.scope += 1;
                    continue;
                }

                // tier 2 — explicit import
                if let Some(&dst) = imported.get(&r.name) {
                    out.push(Edge {
                        src: enclosing(&space, fid, r.scope),
                        dst,
                        kind: r.kind.edge_kind(),
                        conf: Provenance::Import.base_conf(),
                        prov: Provenance::Import,
                    });
                    st.import += 1;
                    continue;
                }

                // tier 3 — global name match, ranked by locality.
                //
                // Only for references whose *syntax* says they name a
                // definition: a call, an instantiation, a superclass. A bare
                // identifier read is too weak a signal — matching a local
                // variable `request` against every repo symbol called
                // `request` is noise, and it is the single largest source of
                // false edges.
                if matches!(r.kind, RefKind::Read) {
                    st.weak_read += 1;
                    continue;
                }
                let Some(cands) = by_name.get(&r.name) else {
                    if builtins.contains(&r.name) {
                        st.builtin += 1;
                    } else {
                        // defined nowhere in the repo: third-party dependency
                        st.external += 1;
                    }
                    continue;
                };
                let best = cands
                    .iter()
                    .map(|&(cf, _)| locality(cf, fid, &dirs))
                    .max()
                    .unwrap_or(0);
                let top: Vec<_> = cands
                    .iter()
                    .filter(|&&(cf, _)| locality(cf, fid, &dirs) == best)
                    .collect();

                if top.len() > MAX_AMBIGUITY {
                    st.too_ambiguous += 1;
                    continue;
                }
                let conf = match top.len() {
                    1 => 80,
                    2..=4 => 60,
                    _ => 45,
                };
                if top.len() == 1 {
                    st.name_unique += 1;
                } else {
                    st.name_ambiguous += 1;
                }
                for &&(cf, cd) in &top {
                    out.push(Edge {
                        src: enclosing(&space, fid, r.scope),
                        dst: space.def_node(cf, cd),
                        kind: r.kind.edge_kind(),
                        conf,
                        prov: Provenance::NameMatch,
                    });
                }
            }

            (out, st)
        })
        .reduce(
            || (Vec::new(), ResolveStats::default()),
            |mut a, b| {
                a.0.extend(b.0);
                (a.0, a.1.merge(b.1))
            },
        );

    // Collapse duplicates: a function referencing the same target three times
    // is one graph edge. Keep the highest-confidence evidence for it.
    let mut edges = edges;
    edges.par_sort_unstable_by_key(|e| (e.src.0, e.dst.0, e.kind as u8, std::cmp::Reverse(e.conf)));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    // ---- node metadata -----------------------------------------------------
    // Files occupy the low ids; their "name" is the interned path so a file
    // node is searchable like any other.
    let mut nodes = vec![NodeMeta::default(); space.total as usize];
    for (f, path) in paths.iter().enumerate() {
        let key = interner.get_or_intern(path.to_string_lossy().as_ref());
        nodes[f] = NodeMeta {
            name: key.into_usize() as u32,
            kind: DefKind::Module as u8,
            file: f as u32,
            start: 0,
            end: 0,
        };
    }
    for (f, unit) in units.iter().enumerate() {
        for (d, def) in unit.defs.iter().enumerate() {
            let n = space.def_node(f as FileId, d as DefIdx).0 as usize;
            nodes[n] = NodeMeta {
                name: def.name.into_usize() as u32,
                kind: def.kind as u8,
                file: f as u32,
                start: def.span.start,
                end: def.span.end,
            };
        }
    }

    Resolved {
        space,
        nodes,
        edges,
        stats,
    }
}

#[inline]
fn enclosing(space: &NodeSpace, f: FileId, scope: DefIdx) -> NodeId {
    if scope == NO_SCOPE {
        space.file_node(f)
    } else {
        space.def_node(f, scope)
    }
}

/// Walk outward through enclosing definitions looking for a visible name.
/// A method calling a sibling method resolves on the second hop (via the class).
#[inline]
fn walk_scopes(
    defs: &[Def],
    scoped: &FxHashMap<(DefIdx, SymId), DefIdx>,
    from: DefIdx,
    name: SymId,
) -> Option<DefIdx> {
    let mut s = from;
    loop {
        if let Some(&d) = scoped.get(&(s, name)) {
            return Some(d);
        }
        if s == NO_SCOPE {
            return None;
        }
        s = defs[s as usize].parent;
    }
}

/// Same file beats same directory beats anywhere. Cheap, and it meaningfully
/// improves tier-3 precision on repos that reuse short method names.
#[inline]
fn locality(cand: FileId, from: FileId, dirs: &[&Path]) -> u8 {
    if cand == from {
        3
    } else if dirs[cand as usize] == dirs[from as usize] {
        2
    } else {
        1
    }
}
