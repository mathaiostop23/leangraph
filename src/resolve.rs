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
    Def, DefIdx, DefKind, Edge, EdgeKind, FileId, FileUnit, Interner, NodeId, NodeKey, Provenance,
    Recv, RefKind, SymId, NO_SCOPE,
};
use crate::idtable::IdTable;
use crate::lang::Lang;
use lasso::Key;
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
    "len",
    "str",
    "int",
    "float",
    "bool",
    "list",
    "dict",
    "set",
    "tuple",
    "print",
    "isinstance",
    "issubclass",
    "super",
    "type",
    "range",
    "enumerate",
    "zip",
    "map",
    "filter",
    "sorted",
    "sum",
    "min",
    "max",
    "abs",
    "any",
    "all",
    "open",
    "getattr",
    "setattr",
    "hasattr",
    "delattr",
    "repr",
    "id",
    "hash",
    "iter",
    "next",
    "format",
    "bytes",
    "bytearray",
    "object",
    "property",
    "staticmethod",
    "classmethod",
    "callable",
    "vars",
    "dir",
    "round",
    "divmod",
    "pow",
    "chr",
    "ord",
    "hex",
    "oct",
    "bin",
    "frozenset",
    "complex",
    "slice",
    "reversed",
    "input",
    "eval",
    "exec",
    "compile",
    "globals",
    "locals",
    "Exception",
    "BaseException",
    "ValueError",
    "TypeError",
    "KeyError",
    "IndexError",
    "AttributeError",
    "RuntimeError",
    "NotImplementedError",
    "StopIteration",
    "ImportError",
    "OSError",
    "IOError",
    "ZeroDivisionError",
    "AssertionError",
    "KeyboardInterrupt",
    "SystemExit",
    "UnicodeDecodeError",
    "UnicodeEncodeError",
];

const JS_BUILTINS: &[&str] = &[
    "console",
    "Object",
    "Array",
    "String",
    "Number",
    "Boolean",
    "Promise",
    "Math",
    "JSON",
    "Date",
    "Map",
    "Set",
    "WeakMap",
    "WeakSet",
    "Error",
    "TypeError",
    "RangeError",
    "SyntaxError",
    "RegExp",
    "Symbol",
    "Proxy",
    "Reflect",
    "BigInt",
    "parseInt",
    "parseFloat",
    "isNaN",
    "isFinite",
    "encodeURIComponent",
    "decodeURIComponent",
    "encodeURI",
    "decodeURI",
    "require",
    "setTimeout",
    "setInterval",
    "clearTimeout",
    "clearInterval",
    "queueMicrotask",
    "structuredClone",
    "fetch",
    "URL",
    "URLSearchParams",
    "Buffer",
    "process",
    "globalThis",
    "Function",
    "ArrayBuffer",
    "Uint8Array",
    "Int32Array",
    "Float64Array",
    "DataView",
    "Intl",
    "AbortController",
    "TextEncoder",
    "TextDecoder",
    "Headers",
    "Request",
    "Response",
    "FormData",
    "Blob",
    "File",
    "Event",
];

/// Mapping from (file, definition) to global node id.
///
/// Ids used to be positional — `n_files + running_offset` — which is simpler
/// and fatal to incremental work: one added definition renumbers everything
/// after it. They now come from a persistent key table, so a node keeps its id
/// across syncs and the adjacency structure stays valid.
pub struct NodeSpace {
    pub n_files: u32,
    file_ids: Vec<NodeId>,
    def_base: Vec<u32>,
    def_ids: Vec<NodeId>,
    /// Size of the id space, holes included.
    pub total: u32,
}

impl NodeSpace {
    #[inline]
    pub fn file_node(&self, f: FileId) -> NodeId {
        self.file_ids
            .get(f as usize)
            .copied()
            .unwrap_or(NodeId(u32::MAX))
    }
    #[inline]
    pub fn def_node(&self, f: FileId, d: DefIdx) -> NodeId {
        self.def_ids
            .get(self.def_base[f as usize] as usize + d as usize)
            .copied()
            .unwrap_or(NodeId(u32::MAX))
    }
}

/// Identity of every node, in a fixed order: files by index, then each file's
/// definitions in extraction order. Deterministic, so two runs over identical
/// source assign identical ids.
fn node_keys(units: &[FileUnit], rel_paths: &[String], interner: &Interner) -> Vec<NodeKey> {
    let mut keys = Vec::new();
    for p in rel_paths {
        keys.push(NodeKey::of_file(p));
    }
    for (f, unit) in units.iter().enumerate() {
        // A qualified name can legitimately repeat within one file — two `if`
        // branches each defining `handler`, or two same-named classes each with
        // a `run` method. Counting occurrences keeps their identities distinct.
        //
        // The counter must key on exactly what the id keys on. Keying it on the
        // parent *index* instead let two defs with different parents but the
        // same qualified name both take occurrence 0, so they collided onto one
        // id and one of them silently overwrote the other's metadata.
        // Counter keyed on the identity-without-occurrence, so it costs one
        // hash rather than a string.
        let mut seen: FxHashMap<u64, u32> = FxHashMap::default();
        let mut chain: Vec<&str> = Vec::with_capacity(8);
        for def in &unit.defs {
            qualified_into(unit, def, interner, &mut chain);
            let base = NodeKey::of_parts(
                &rel_paths[f],
                chain.iter().copied(),
                def.kind as u8,
                u32::MAX,
            );
            let n = seen.entry(base.0).or_default();
            let occ = *n;
            *n += 1;
            keys.push(NodeKey::of_parts(
                &rel_paths[f],
                chain.iter().copied(),
                def.kind as u8,
                occ,
            ));
        }
    }
    debug_assert_eq!(
        {
            let mut u: Vec<u64> = keys.iter().map(|k| k.0).collect();
            u.sort_unstable();
            u.dedup();
            u.len()
        },
        keys.len(),
        "node keys must be unique: a collision silently merges two nodes"
    );
    keys
}

/// `QuerySet.filter` rather than bare `filter`: the enclosing chain is what
/// makes two same-named methods in one file distinguishable.
///
/// Writes into a reused buffer instead of returning a `String`. The guard
/// bounds a cycle that a malformed parent chain could otherwise turn into a
/// hang.
fn qualified_into<'a>(unit: &FileUnit, def: &Def, interner: &'a Interner, out: &mut Vec<&'a str>) {
    out.clear();
    out.push(interner.resolve(&def.name));
    let mut p = def.parent;
    let mut guard = 0;
    while p != NO_SCOPE && guard < 32 {
        let d = &unit.defs[p as usize];
        out.push(interner.resolve(&d.name));
        p = d.parent;
        guard += 1;
    }
    out.reverse();
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
    // Every suffix, because the source root is not known — `src/flask/app.py`
    // has to be reachable as `flask.app`, and `examples/tutorial/flaskr/` is a
    // real package three levels down that legitimately answers to `flaskr`.
    //
    // The exception is a single-segment key that collides with a standard
    // library module. `django/core/serializers/json.py` was reachable as plain
    // `json`, so `import json` — the standard library — bound to it: a false
    // edge at confidence 95, the tier documented as the strongest AST evidence,
    // dragging that module's whole export list into scope. `import io` landed
    // in `django/contrib/gis/geos/io.py` the same way.
    //
    // Depth is the wrong test for this; the collision is.
    let sep = lang.module_sep();
    (0..parts.len())
        .map(|i| parts[i..].join(sep))
        .filter(|k| !(k == &parts[parts.len() - 1] && lang.shadows_stdlib(k)))
        .collect()
}

/// Every `package.json` in the tree, as `(declared name, its directory)`.
///
/// A monorepo addresses its own packages by name, and the name lives in a file
/// the indexer otherwise has no reason to read. Only directories that actually
/// contain indexed source are considered, so a stray manifest under
/// `node_modules` — already excluded from the walk — cannot introduce a mapping.
fn workspace_packages(root: &Path, paths: &[PathBuf]) -> Vec<(String, PathBuf, Vec<String>)> {
    let mut seen: FxHashSet<PathBuf> = FxHashSet::default();
    let mut out = Vec::new();
    for d in paths.iter().filter_map(|p| p.parent()) {
        let mut cur = Some(d);
        // Walk up, so `packages/core/src/x.ts` also considers `packages/core`.
        while let Some(dir) = cur {
            if !dir.starts_with(root) || !seen.insert(dir.to_path_buf()) {
                break;
            }
            if let Ok(text) = std::fs::read_to_string(dir.join("package.json")) {
                if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                    if let Some(name) = v.get("name").and_then(|n| n.as_str()) {
                        // Where a bare `import "@acme/core"` lands: what the
                        // manifest declares, plus the conventional entries for
                        // the common case where it declares nothing.
                        let mut entries: Vec<String> = ["main", "module", "types"]
                            .iter()
                            .filter_map(|k| v.get(*k).and_then(|m| m.as_str()))
                            .map(|m| m.trim_start_matches("./").to_string())
                            .collect();
                        entries.extend(
                            ["index", "src/index", "lib/index"]
                                .iter()
                                .map(|x| (*x).to_string()),
                        );
                        out.push((name.to_string(), dir.to_path_buf(), entries));
                    }
                }
            }
            cur = dir.parent();
        }
    }
    out
}

/// Resolve one import statement to a file.
///
/// Relative TypeScript specifiers are normalised against the importing file's
/// directory; everything else is matched against the suffix keys above.
fn resolve_import(
    spec: &str,
    importer: &Path,
    root: &Path,
    lang: Lang,
    by_module: &FxHashMap<String, FileId>,
) -> Option<FileId> {
    // `use crate::core::Def` names a crate root that is not a directory, and it
    // ends in a *symbol* rather than a module. Strip the one, then try the path
    // both with and without the other.
    let spec = lang.strip_import_prefix(spec);
    let sep = lang.module_sep();
    if sep != "/" && !spec.starts_with('.') {
        if let Some(f) = by_module.get(spec) {
            return Some(*f);
        }
        if lang.import_ends_in_symbol() {
            if let Some(cut) = spec.rfind(sep) {
                if let Some(f) = by_module.get(&spec[..cut]) {
                    return Some(*f);
                }
            }
        }
    }
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
    /// `(node, word)` — a definition's prose, mapped out of per-file indices
    /// into the graph's own node ids. Done here because this is the only place
    /// that holds both the units and the node space; the writer would otherwise
    /// have to be handed the units and rediscover the mapping.
    ///
    /// File-level prose — a module docstring, a licence header — hangs off the
    /// file node, which is a node like any other.
    pub prose: Vec<(NodeId, u32)>,
    /// Indexed by node id, so it has holes where nodes were retired.
    pub nodes: Vec<NodeMeta>,
    pub edges: Vec<Edge>,
    pub stats: ResolveStats,
    pub churn: crate::idtable::Churn,
}

pub fn resolve(
    units: &[FileUnit],
    paths: &[PathBuf],
    langs: &[Lang],
    root: &Path,
    interner: &Interner,
    ids: &mut IdTable,
) -> Resolved {
    let rel_paths: Vec<String> = paths
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .replace('\\', "/")
        })
        .collect();

    let t_keys = std::time::Instant::now();
    let keys = node_keys(units, &rel_paths, interner);
    let (assigned, churn) = ids.assign(&keys);
    let ms_keys = t_keys.elapsed().as_secs_f64() * 1e3;

    let n_files = units.len() as u32;
    let file_ids: Vec<NodeId> = assigned[..n_files as usize].to_vec();
    let def_ids: Vec<NodeId> = assigned[n_files as usize..].to_vec();
    let mut def_base = Vec::with_capacity(units.len() + 1);
    let mut acc = 0u32;
    for u in units {
        def_base.push(acc);
        acc += u.defs.len() as u32;
    }
    def_base.push(acc);

    let space = NodeSpace {
        n_files,
        file_ids,
        def_base,
        def_ids,
        total: ids.len(),
    };

    let t_index = std::time::Instant::now();
    // ---- phase A: global index (sequential barrier) ------------------------
    let mut by_name: FxHashMap<SymId, Vec<(FileId, DefIdx)>> = FxHashMap::default();
    let mut by_module: FxHashMap<String, FileId> = FxHashMap::default();
    let mut exports: Vec<FxHashMap<SymId, DefIdx>> = Vec::with_capacity(units.len());

    for (f, unit) in units.iter().enumerate() {
        let fid = f as FileId;
        let mut top: FxHashMap<SymId, DefIdx> = FxHashMap::default();
        for (d, def) in unit.defs.iter().enumerate() {
            by_name
                .entry(def.name)
                .or_default()
                .push((fid, d as DefIdx));
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

    // Workspace package names. In a monorepo, `import { x } from "@acme/core"`
    // names a directory of this same repository, and nothing in the path
    // suffixes above says so — the import resolved to nothing and the call fell
    // through to a name match at confidence 80, alongside every other function
    // in the tree with that name.
    for (name, dir, entries) in workspace_packages(root, paths) {
        for (f, path) in paths.iter().enumerate() {
            let Ok(rel) = path.strip_prefix(&dir) else {
                continue;
            };
            let mut sub = rel.to_string_lossy().replace('\\', "/");
            if let Some(dot) = sub.rfind('.') {
                sub.truncate(dot);
            }
            // The bare package name lands on whatever the manifest points at,
            // or on a conventional entry when it points at nothing.
            if entries.iter().any(|e| {
                let e = e
                    .trim_end_matches(".ts")
                    .trim_end_matches(".js")
                    .trim_end_matches(".tsx");
                e == sub
            }) {
                by_module.entry(name.clone()).or_insert(f as FileId);
            }
            // And every file under it, addressed as a subpath.
            by_module
                .entry(format!("{name}/{sub}"))
                .or_insert(f as FileId);
            if let Some(stripped) = sub.strip_suffix("/index") {
                by_module
                    .entry(format!("{name}/{stripped}"))
                    .or_insert(f as FileId);
            }
        }
    }

    // Intern builtin names once so the hot path compares u32s, not strings.
    //
    // Kept apart by language, and consulted *before* the global name index
    // rather than only when the index misses. The old order meant the filter
    // was unreachable whenever a repository defined the name anywhere at all:
    // django ships a minified JavaScript bundle containing a function called
    // `len`, so every Python `len()` in the tree — 2,878 of them — resolved
    // into vendored JavaScript.
    let py_builtins: FxHashSet<SymId> =
        PY_BUILTINS.iter().filter_map(|n| interner.get(n)).collect();
    let js_builtins: FxHashSet<SymId> =
        JS_BUILTINS.iter().filter_map(|n| interner.get(n)).collect();

    let dirs: Vec<&Path> = paths.iter().map(|p| p.parent().unwrap_or(root)).collect();

    let ms_index = t_index.elapsed().as_secs_f64() * 1e3;
    let t_par = std::time::Instant::now();
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
                if let Some(tf) = resolve_import(spec, &paths[f], root, langs[f], &by_module) {
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

            // `from m import a as b` puts `a` in scope above, under its own
            // name. The local binding is `b`, which is not a name the module
            // exports, so without this the reference resolves to nothing and
            // falls through to a global name match. 305 of these in django.
            //
            // Sorted, because the map above is a hash map and an alias pointing
            // at a name that two modules both export would otherwise bind in
            // whichever order iteration happened to take.
            let mut aliases: Vec<(SymId, SymId)> = unit.aliases.clone();
            aliases.sort_unstable_by_key(|(l, o)| (l.into_usize(), o.into_usize()));
            for (local, original) in aliases {
                if let Some(&dst) = imported.get(&original) {
                    imported.entry(local).or_insert(dst);
                }
            }

            // Module paths this file imports, so a dotted call through one can
            // be told apart from a dotted call through an object.
            let mut import_names: FxHashSet<SymId> =
                unit.imports.iter().map(|i| i.module).collect();
            // `import numpy as np` makes `np` a module receiver too. Without
            // this every `np.array()` looks like a call through an object.
            for &(local, original) in &unit.aliases {
                if import_names.contains(&original) {
                    import_names.insert(local);
                }
            }

            // What each class in this file declares as a base. The heritage
            // list is inside the class node, so its Extends refs carry that
            // class as their scope — the information is already here, it was
            // simply never used.
            let mut bases: FxHashMap<DefIdx, Vec<SymId>> = FxHashMap::default();
            for r in &unit.refs {
                if r.kind == RefKind::Extends && r.scope != NO_SCOPE {
                    bases.entry(r.scope).or_default().push(r.name);
                }
            }

            for r in &unit.refs {
                let src = enclosing(&space, fid, r.scope);

                // Tiers 1 and 2 are both statements about the text around the
                // reference: which names are in scope, which names were
                // imported. Neither says anything about `other.foo()` — the
                // receiver decides that, and we do not know what it is. Binding
                // it through the scope chain anyway is what made a method call
                // itself: `super().x()` inside `x` resolved to `x`.
                // A call through an imported module is still an import: the
                // module was named explicitly and its exports are known.
                // `flask.redirect()` loses nothing by being dotted. Only a call
                // through an *object* is opaque, because nothing here says what
                // the object is.
                let via_module = r.recv_name.is_some_and(|n| import_names.contains(&n));

                if r.recv.lexical() || via_module {
                    // tier 1 — lexical scope chain. Never for a module
                    // receiver: `flask.redirect` is not whatever `redirect`
                    // happens to mean in this file.
                    if let Some(hit) = r
                        .recv
                        .lexical()
                        .then(|| walk_scopes(&unit.defs, &scoped, r.scope, r.name))
                        .flatten()
                    {
                        // A class is on the scope stack while its own base list
                        // is being read, so `class Migration(migrations.Migration)`
                        // finds itself. Recursion makes a self-call legitimate;
                        // nothing makes a class its own superclass.
                        if r.kind == RefKind::Extends && space.def_node(fid, hit) == src {
                            st.too_ambiguous += 1;
                            continue;
                        }
                        out.push(Edge {
                            src,
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
                            src,
                            dst,
                            kind: r.kind.edge_kind(),
                            conf: Provenance::Import.base_conf(),
                            prov: Provenance::Import,
                        });
                        st.import += 1;
                        continue;
                    }
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
                // The language runtime owns this name — but only when nothing
                // else is being asked. `open(path)` is the builtin;
                // `client.open(url)` is a method on someone's object that
                // happens to share its name. Consulting the builtin list
                // without looking at the receiver deleted every edge into
                // flask's `FlaskClient.open`, all 198 of which the test suite
                // actually executes, and 8,867 python-to-python edges in
                // django. PY_BUILTINS contains `open`, `set`, `list`,
                // `filter`, `compile` and `type`; the names collide constantly.
                if r.recv == Recv::Bare {
                    let mine = if langs[fid as usize] == Lang::Python {
                        &py_builtins
                    } else {
                        &js_builtins
                    };
                    if mine.contains(&r.name) {
                        st.builtin += 1;
                        continue;
                    }
                }
                let Some(cands) = by_name.get(&r.name) else {
                    // defined nowhere in the repo: third-party dependency
                    st.external += 1;
                    continue;
                };
                // A name with thousands of definitions costs a pass over all of
                // them for every reference to it, and the answer is thrown away
                // anyway once the ambiguity cap fires. So find the best locality
                // tier and count how many sit at it in ONE pass, allocating
                // nothing — and give up before materialising anything when the
                // count is hopeless.
                //
                // Measured on two 50k-file repositories identical except that
                // one names a method `render` everywhere: the old shape spent
                // 27 seconds in resolve against 41 ms.
                //
                // A Python function cannot call a JavaScript one, so the family
                // filter rides along in the same pass.
                let my_lang = langs[fid as usize];
                let mut best = 0u8;
                let mut n_best = 0usize;
                let mut any = false;
                for &(cf, _) in cands.iter() {
                    if !same_family(my_lang, langs[cf as usize]) {
                        continue;
                    }
                    any = true;
                    let loc = locality(cf, fid, &dirs);
                    if loc > best {
                        best = loc;
                        n_best = 1;
                    } else if loc == best {
                        n_best += 1;
                    }
                }
                if !any {
                    st.external += 1;
                    continue;
                }
                // `super()` narrowing can rescue an over-ambiguous set, so it
                // keeps the old path; nothing else does.
                if n_best > MAX_AMBIGUITY && r.recv != Recv::Super {
                    st.too_ambiguous += 1;
                    continue;
                }
                let cands: Vec<&(FileId, DefIdx)> = cands
                    .iter()
                    .filter(|&&(cf, _)| same_family(my_lang, langs[cf as usize]))
                    .collect();
                // `super().x()` means one of the classes this one declares.
                // That has to narrow the candidates *before* locality does, or
                // the answer is thrown away first: `Flask` extends `App`, which
                // lives in a different directory, so the same-directory bucket
                // wins and never contains the base at all.
                let mut cands = cands;
                if r.recv == Recv::Super {
                    if let Some(names) =
                        enclosing_class(&unit.defs, r.scope).and_then(|c| bases.get(&c))
                    {
                        let narrowed: Vec<&(FileId, DefIdx)> = cands
                            .iter()
                            .filter(|&&&(cf, cd)| {
                                let owner = units[cf as usize].defs[cd as usize].parent;
                                owner != NO_SCOPE
                                    && names.contains(&units[cf as usize].defs[owner as usize].name)
                            })
                            .copied()
                            .collect();
                        if !narrowed.is_empty() {
                            cands = narrowed;
                        }
                    }
                }

                let best = cands
                    .iter()
                    .map(|&&(cf, _)| locality(cf, fid, &dirs))
                    .max()
                    .unwrap_or(0);
                let mut top: Vec<_> = cands
                    .iter()
                    .filter(|&&&(cf, _)| locality(cf, fid, &dirs) == best)
                    .copied()
                    .collect();

                // The cap is applied before narrowing for everything except
                // `super()`, whose narrowing is real evidence and is allowed to
                // rescue a reference the cap would otherwise have dropped.
                if top.len() > MAX_AMBIGUITY && r.recv != Recv::Super {
                    st.too_ambiguous += 1;
                    continue;
                }

                // `super().x()` means the base class's implementation, and the
                // one thing it can never mean is this one.
                if r.recv == Recv::Super || r.kind == RefKind::Extends {
                    top.retain(|&&(cf, cd)| space.def_node(cf, cd) != src);
                    if top.is_empty() {
                        st.too_ambiguous += 1;
                        continue;
                    }
                }

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
                        src,
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

    let ms_par = t_par.elapsed().as_secs_f64() * 1e3;
    if std::env::var_os("LEANGRAPH_PROFILE").is_some() {
        eprintln!(
            "      resolve: keys+ids {ms_keys:.0}ms · global index {ms_index:.0}ms · parallel {ms_par:.0}ms"
        );
    }

    // Collapse duplicates: a function referencing the same target three times
    // is one graph edge. Keep the highest-confidence evidence for it.
    let mut edges = edges;
    edges.par_sort_unstable_by_key(|e| (e.src.0, e.dst.0, e.kind as u8, std::cmp::Reverse(e.conf)));
    edges.dedup_by(|a, b| a.src == b.src && a.dst == b.dst && a.kind == b.kind);

    // ---- node metadata -----------------------------------------------------
    // Files occupy the low ids; their "name" is the interned path so a file
    // node is searchable like any other.
    let mut nodes = vec![NodeMeta::default(); space.total as usize];
    for (f, rel) in rel_paths.iter().enumerate() {
        let key = interner.get_or_intern(rel.as_str());
        let n = space.file_node(f as FileId).0 as usize;
        nodes[n] = NodeMeta {
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

    let mut prose: Vec<(NodeId, u32)> = Vec::new();
    for (f, u) in units.iter().enumerate() {
        let fid = f as FileId;
        for &(scope, word) in &u.prose {
            let n = if scope == NO_SCOPE {
                space.file_node(fid)
            } else {
                space.def_node(fid, scope)
            };
            if n.0 != u32::MAX {
                prose.push((n, word.into_usize() as u32));
            }
        }
    }
    prose.sort_unstable();
    prose.dedup();

    Resolved {
        space,
        prose,
        nodes,
        edges,
        stats,
        churn,
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
/// Can a reference in one language name a definition in the other?
///
/// TypeScript, TSX and JavaScript share a module system and routinely reference
/// each other. Python and that family do not, in either direction.
fn same_family(a: Lang, b: Lang) -> bool {
    // TypeScript, TSX and JavaScript share a module system and reference each
    // other constantly. C and C++ share headers the same way. Everything else
    // only matches itself.
    //
    // This listed the two families explicitly and returned false for every
    // other pair, which silently disabled tier-3 name matching for all eleven
    // languages added later — they extracted definitions and calls and resolved
    // nothing beyond the current file.
    use Lang::*;
    matches!(
        (a, b),
        (TypeScript | Tsx, TypeScript | Tsx) | (C | Cpp, C | Cpp)
    ) || a == b
}

/// The class a reference sits inside, if any.
#[inline]
fn enclosing_class(defs: &[Def], mut scope: DefIdx) -> Option<DefIdx> {
    for _ in 0..32 {
        if scope == NO_SCOPE {
            return None;
        }
        let d = defs.get(scope as usize)?;
        if matches!(d.kind, DefKind::Class | DefKind::Interface) {
            return Some(scope);
        }
        scope = d.parent;
    }
    None
}

fn locality(cand: FileId, from: FileId, dirs: &[&Path]) -> u8 {
    if cand == from {
        3
    } else if dirs[cand as usize] == dirs[from as usize] {
        2
    } else {
        1
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::Provenance;
    use crate::lang::ALL_LANGS;
    use crate::testkit::Corpus;

    // ---- the three tiers ---------------------------------------------------
    //
    // The tiers are the product. Each one is a different claim about how much
    // the evidence is worth, and the confidence attached to it is what lets a
    // caller rank. A change that quietly moves an edge between tiers is not
    // visible in any total.

    #[test]
    fn each_tier_resolves_what_it_is_for_and_says_so() {
        let c = Corpus::build(&[
            (
                "app.py",
                "from helper import assist\n\
                 \n\
                 class Client:\n\
                 \x20   def open(self, url):\n\
                 \x20       return self.send(url)\n\
                 \x20   def send(self, url):\n\
                 \x20       return assist(url)\n\
                 \n\
                 def main():\n\
                 \x20   c = Client()\n\
                 \x20   return c.open('/')\n",
            ),
            ("helper.py", "def assist(u):\n    return u\n"),
        ]);

        // `self.send()` — the enclosing object, so the lexical chain answers it.
        let e = c.call("Client.open", "Client.send").expect("self call");
        assert_eq!((e.prov, e.conf), (Provenance::Scope, 100));

        // `assist()` — a bare name the file explicitly imported.
        let e = c.call("Client.send", "assist").expect("imported call");
        assert_eq!((e.prov, e.conf), (Provenance::Import, 95));

        // `c.open()` — nothing here says what `c` is. One method in the repo
        // carries the name, so it is a guess, and it is labelled as one.
        let e = c.call("main", "Client.open").expect("name match");
        assert_eq!(e.prov, Provenance::NameMatch);
        assert!(
            e.conf < 95,
            "a guess must not outrank an import: {}",
            e.conf
        );
    }

    #[test]
    fn an_unknown_receiver_is_not_lexical_evidence() {
        // The bug this guards: `other.send()` binding to the *enclosing* class's
        // `send` at confidence 100. Nothing in the surrounding scopes says what
        // `other` is. On django 1,285 edges at confidence 100 were a method
        // calling itself for this reason.
        let c = Corpus::build(&[(
            "a.py",
            "class Session:\n\
             \x20   def send(self, r):\n\
             \x20       return r\n\
             \x20   def forward(self, other, r):\n\
             \x20       return other.send(r)\n",
        )]);

        let e = c
            .call("Session.forward", "Session.send")
            .expect("the edge is still worth having — it is the confidence that must be honest");
        assert_ne!(
            e.prov,
            Provenance::Scope,
            "a call through an unknown object is not scope evidence"
        );
        assert!(
            e.conf <= 80,
            "and must not carry proof-level confidence, got {}",
            e.conf
        );
    }

    #[test]
    fn super_is_not_a_call_to_the_same_implementation() {
        // `super().run()` means explicitly *not* this class's `run`. Resolving
        // it to the enclosing class inverts the meaning of the code.
        let c = Corpus::build(&[(
            "a.py",
            "class Base:\n\
             \x20   def run(self):\n\
             \x20       return 1\n\
             \n\
             class Child(Base):\n\
             \x20   def run(self):\n\
             \x20       return super().run() + 1\n",
        )]);

        let from_child = c.calls_from("Child.run");
        assert!(
            from_child.iter().all(|e| !e.dst.ends_with("Child.run")),
            "super().run() must not resolve to Child.run: {from_child:?}"
        );
        let base = from_child
            .iter()
            .find(|e| e.dst.ends_with("Base.run"))
            .expect("it should reach the base implementation, which is what it means");
        assert!(base.conf <= 80, "reached by name, not by proof");
    }

    // ---- language families -------------------------------------------------

    #[test]
    fn every_language_matches_itself() {
        // `same_family` listed two families explicitly and returned false for
        // everything else, so eleven languages resolved nothing beyond the
        // current file. Nothing failed; tier 3 was simply never reached. The
        // aggregate that would have shown it was never computed per language.
        for l in ALL_LANGS {
            assert!(
                same_family(l, l),
                "{l:?} does not match itself, which disables cross-file name matching for it"
            );
        }
    }

    #[test]
    fn families_are_the_ones_that_share_a_module_system() {
        use Lang::*;
        assert!(same_family(TypeScript, Tsx));
        assert!(same_family(Tsx, TypeScript));
        assert!(same_family(C, Cpp));
        assert!(same_family(Cpp, C));
        assert!(!same_family(Python, Go));
        assert!(!same_family(TypeScript, Python));
        assert!(!same_family(Java, Kotlin), "different name resolution");
    }

    // ---- module keys -------------------------------------------------------

    #[test]
    fn a_file_answers_to_every_suffix_of_its_path() {
        // The source root is not known, so `src/flask/app.py` has to be
        // reachable as `flask.app` — and `examples/tutorial/flaskr/` is a real
        // package three levels down that legitimately answers to `flaskr`.
        let root = Path::new("/repo");
        let keys = module_keys(root, Path::new("/repo/src/flask/app.py"), Lang::Python);
        assert!(keys.contains(&"src.flask.app".to_string()));
        assert!(keys.contains(&"flask.app".to_string()));
        assert!(keys.contains(&"app".to_string()));
    }

    #[test]
    fn a_package_entry_point_addresses_its_directory() {
        let root = Path::new("/repo");
        for (file, want) in [
            ("/repo/flask/__init__.py", "flask"),
            ("/repo/pkg/index.ts", "pkg"),
            ("/repo/pkg/mod.rs", "pkg"),
        ] {
            let lang = match Path::new(file).extension().unwrap().to_str().unwrap() {
                "py" => Lang::Python,
                "ts" => Lang::TypeScript,
                _ => Lang::Rust,
            };
            let keys = module_keys(root, Path::new(file), lang);
            assert!(
                keys.contains(&want.to_string()),
                "{file} should answer to {want}, got {keys:?}"
            );
        }
    }

    #[test]
    fn a_module_named_after_the_standard_library_does_not_capture_its_import() {
        // `django/core/serializers/json.py` was reachable as plain `json`, so
        // `import json` — the standard library — bound to it at confidence 95,
        // the tier documented as the strongest AST evidence, dragging that
        // module's whole export list into scope. `import io` landed in
        // `django/contrib/gis/geos/io.py` the same way.
        let root = Path::new("/repo");
        let keys = module_keys(
            root,
            Path::new("/repo/django/core/serializers/json.py"),
            Lang::Python,
        );
        assert!(
            !keys.contains(&"json".to_string()),
            "bare `json` must not be a key: {keys:?}"
        );
        // ...but the qualified paths still are, because those are unambiguous.
        assert!(keys.contains(&"django.core.serializers.json".to_string()));
        assert!(keys.contains(&"serializers.json".to_string()));
    }

    #[test]
    fn the_stdlib_rule_applies_only_to_the_bare_name() {
        // A repo module genuinely called `parser` at the top level is still
        // shadowed, but one called `mytool` never was — the filter must not
        // reach beyond the collision it exists for.
        let root = Path::new("/repo");
        let keys = module_keys(root, Path::new("/repo/pkg/mytool.py"), Lang::Python);
        assert!(keys.contains(&"mytool".to_string()));
        assert!(keys.contains(&"pkg.mytool".to_string()));
    }

    // ---- locality ----------------------------------------------------------

    #[test]
    fn locality_prefers_near_over_far() {
        let a = Path::new("/repo/pkg");
        let b = Path::new("/repo/pkg");
        let c = Path::new("/repo/other");
        let dirs = [a, b, c];
        assert!(
            locality(0, 0, &dirs) > locality(1, 0, &dirs),
            "the same file beats a sibling"
        );
        assert!(
            locality(1, 0, &dirs) > locality(2, 0, &dirs),
            "a sibling beats an unrelated directory"
        );
    }

    // ---- imports and aliases ----------------------------------------------

    #[test]
    fn an_alias_binds_the_local_name() {
        let c = Corpus::build(&[
            (
                "app.py",
                "from helper import assist as helper_fn\n\
                 \n\
                 def main():\n\
                 \x20   return helper_fn(1)\n",
            ),
            ("helper.py", "def assist(u):\n    return u\n"),
        ]);
        let e = c
            .call("main", "assist")
            .expect("`assist as helper_fn` must bind helper_fn to assist");
        assert_eq!(e.prov, Provenance::Import);
    }

    #[test]
    fn a_plain_named_import_is_not_an_alias() {
        // `import { a }` has the same shape as `import * as a` to a careless
        // reader of the tree. Treating it as an alias removed 2,926 edges from
        // one repository — every plain named import stopped binding.
        let c = Corpus::build(&[
            (
                "app.ts",
                "import { assist } from './helper';\n\
                 export function main() { return assist(1); }\n",
            ),
            (
                "helper.ts",
                "export function assist(u: number) { return u; }\n",
            ),
        ]);
        let e = c
            .call("main", "assist")
            .expect("a plain named import must still bind");
        assert_eq!(e.prov, Provenance::Import);
    }

    #[test]
    fn a_call_through_a_module_keeps_its_import_evidence() {
        // `helper.assist()` is a call through a *module*, and the import is
        // real evidence about what `helper` is — unlike `obj.assist()`. A fix
        // for the object case that does not distinguish these demotes every
        // module-qualified call in the repository.
        let c = Corpus::build(&[
            (
                "app.py",
                "import helper\n\
                 \n\
                 def main():\n\
                 \x20   return helper.assist(1)\n",
            ),
            ("helper.py", "def assist(u):\n    return u\n"),
        ]);
        let e = c.call("main", "assist").expect("module-qualified call");
        assert_eq!(
            e.prov,
            Provenance::Import,
            "a module receiver is evidence; an object receiver is not"
        );
    }

    // ---- ambiguity ---------------------------------------------------------

    #[test]
    fn a_name_too_many_files_define_resolves_to_nothing() {
        // Past the cap the answer is not "pick one with lower confidence", it
        // is "this is not evidence". Emitting nine guesses would put nine
        // wrong edges in the graph to bury one right one.
        let mut files: Vec<(String, String)> = (0..MAX_AMBIGUITY + 3)
            .map(|i| {
                (
                    format!("m{i}.py"),
                    "class T:\n    def handle(self):\n        return 1\n".to_string(),
                )
            })
            .collect();
        files.push((
            "caller.py".to_string(),
            "def go(x):\n    return x.handle()\n".to_string(),
        ));
        let refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(a, b)| (a.as_str(), b.as_str()))
            .collect();

        let c = Corpus::build(&refs);
        assert!(
            c.calls_from("go").is_empty(),
            "{} candidates is past the cap and must resolve to nothing, got {:?}",
            MAX_AMBIGUITY + 3,
            c.calls_from("go")
        );
        assert!(c.resolved.stats.too_ambiguous > 0, "and must be counted");
    }

    #[test]
    fn a_name_only_one_file_defines_is_taken() {
        let c = Corpus::build(&[
            (
                "m.py",
                "class T:\n    def handle(self):\n        return 1\n",
            ),
            ("caller.py", "def go(x):\n    return x.handle()\n"),
        ]);
        let e = c.call("go", "T.handle").expect("a unique name resolves");
        assert_eq!(e.prov, Provenance::NameMatch);
        assert_eq!(c.resolved.stats.name_unique, 1);
    }

    // ---- determinism -------------------------------------------------------

    #[test]
    fn the_same_source_resolves_identically_twice() {
        // Ids come from a persistent key table rather than from position, and
        // the whole incremental story rests on two runs over identical source
        // producing identical output.
        let files: &[(&str, &str)] = &[
            (
                "a.py",
                "class C:\n    def x(self):\n        return self.y()\n    def y(self):\n        return 1\n",
            ),
            ("b.py", "from a import C\n\ndef go():\n    return C().x()\n"),
        ];
        let one = Corpus::build(files);
        let two = Corpus::build(files);

        let mut a: Vec<_> = one.calls();
        let mut b: Vec<_> = two.calls();
        a.sort_by(|x, y| (&x.src, &x.dst).cmp(&(&y.src, &y.dst)));
        b.sort_by(|x, y| (&x.src, &x.dst).cmp(&(&y.src, &y.dst)));
        assert_eq!(a, b);
        assert!(!a.is_empty(), "the fixture must actually produce edges");
    }
}
