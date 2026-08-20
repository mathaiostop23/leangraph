//! Per-file extraction: mmap -> parse -> single cursor walk -> `FileUnit`.
//!
//! Pure with respect to shared state (the interner is internally concurrent),
//! so this parallelises across files with no coordination.

use crate::cache::FileMeta;
use crate::core::{
    Def, DefIdx, DefKind, FileId, FileUnit, Import, Interner, Recv, Ref, RefKind, Span,
    NO_SCOPE,
};
use rustc_hash::FxHashSet;
use crate::lang::{node_text, Lang, Spec};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use tree_sitter::{Node, Parser as TsParser};

#[derive(Default, Clone, Copy)]
pub struct Timings {
    pub ns_read: u64,
    pub ns_hash: u64,
    pub ns_parse: u64,
    pub ns_walk: u64,
    pub bytes: u64,
    pub ast_nodes: u64,
}

/// Walk the whole tree once, emitting definitions, references and imports.
///
/// A stack of `(definition index, tree depth)` gives every node its enclosing
/// definition as we go, so the containment tree falls out of the same pass
/// rather than needing a second traversal.
fn walk(
    tree: &tree_sitter::Tree,
    src: &[u8],
    spec: &Spec,
    interner: &Interner,
    unit: &mut FileUnit,
    t: &mut Timings,
) {
    let mut cursor = tree.walk();
    let mut stack: Vec<(DefIdx, i32)> = Vec::with_capacity(16);
    // Identifier nodes already recorded as a definition name or a call target.
    // Without this, `foo()` would yield both a Call and a Read reference.
    let mut consumed: FxHashSet<usize> = FxHashSet::default();
    // Depths at which we entered a base-class / implements list.
    let mut heritage: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;

    loop {
        while let Some(&(_, d)) = stack.last() {
            if d >= depth {
                stack.pop();
            } else {
                break;
            }
        }
        while let Some(&d) = heritage.last() {
            if d >= depth {
                heritage.pop();
            } else {
                break;
            }
        }

        let node = cursor.node();
        let kind = node.kind_id();
        let scope = stack.last().map(|&(i, _)| i).unwrap_or(NO_SCOPE);
        t.ast_nodes += 1;

        // A heritage list only counts as one when it actually hangs off a class:
        // Python reuses `argument_list` for ordinary call arguments.
        if spec.is_heritage(kind)
            && node
                .parent()
                .is_some_and(|p| spec.is_class_node(p.kind_id()))
        {
            heritage.push(depth);
        }

        // A dotted base class names exactly one thing: the last segment. The
        // segments before it are a module or a namespace, and recording them
        // as superclasses is not a near miss — `migrations` is not a class.
        if !heritage.is_empty() && spec.is_dotted(kind) {
            let is_callee = spec.is_callee_of_parent(&node);
            if let Some(seg) = spec.base_name_node(&node, is_callee) {
                if let Some(txt) = node_text(&seg, src) {
                    consume_idents(&node, spec, &mut consumed);
                    unit.refs.push(Ref {
                        name: interner.get_or_intern(txt),
                        kind: RefKind::Extends,
                        span: Span::of(&node),
                        scope,
                        recv: Recv::Bare,
                        recv_name: None,
                    });
                }
            }
        }

        if let Some(dk) = spec.def_kind_of(&node) {
            if let Some(nn) = spec.def_name_node(&node) {
                if let Some(txt) = node_text(&nn, src) {
                    let idx = unit.defs.len() as DefIdx;
                    consumed.insert(nn.id());
                    unit.defs.push(Def {
                        name: interner.get_or_intern(txt),
                        kind: reclassify(dk, &unit.defs, scope),
                        span: Span::of(&node),
                        name_span: Span::of(&nn),
                        parent: scope,
                    });
                    stack.push((idx, depth));
                }
            }
        } else if let Some(nn) = spec
            .var_def_name(&node)
            .filter(|_| at_container_scope(&unit.defs, scope))
        {
            if let Some(txt) = node_text(&nn, src) {
                consumed.insert(nn.id());
                unit.defs.push(Def {
                    name: interner.get_or_intern(txt),
                    kind: DefKind::Variable,
                    span: Span::of(&node),
                    name_span: Span::of(&nn),
                    parent: scope,
                });
                // not pushed onto the scope stack: a value does not contain code
            }
        } else if let Some(rk) = spec.ref_kind(kind) {
            if let Some(nn) = spec.ref_name_node(&node) {
                if let Some(txt) = node_text(&nn, src) {
                    consumed.insert(nn.id());
                    unit.refs.push(Ref {
                        name: interner.get_or_intern(txt),
                        kind: rk,
                        span: Span::of(&node),
                        scope,
                        recv: spec.recv_of(&node, src),
                        recv_name: spec
                            .recv_node(&node)
                            .and_then(|n| node_text(&n, src))
                            .map(|t| interner.get_or_intern(t)),
                    });
                }
            }
        } else if spec.is_import(kind) {
            if let Some(mn) = spec.import_module_node(&node) {
                if let Some(txt) = node_text(&mn, src) {
                    unit.imports.push(Import {
                        module: interner.get_or_intern(txt),
                        span: Span::of(&node),
                    });
                }
            }
        } else if spec.is_ident(kind) && !consumed.contains(&node.id()) {
            // Everything else that names something: superclasses, type
            // annotations, decorators, arguments. A class that is subclassed
            // but never called is invisible without these.
            if let Some(txt) = node_text(&node, src) {
                unit.refs.push(Ref {
                    name: interner.get_or_intern(txt),
                    kind: if heritage.is_empty() {
                        RefKind::Read
                    } else {
                        RefKind::Extends
                    },
                    span: Span::of(&node),
                    scope,
                    recv: Recv::Bare,
                    recv_name: None,
                });
            }
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

/// A function declared inside a class is a method, whatever the grammar calls
/// it. Python has no separate node kind for this — `def` is `function_definition`
/// at every level — so the distinction has to come from the enclosing scope.
#[inline]
/// Mark every identifier in a subtree as already accounted for.
///
/// Used where one node speaks for its whole subtree — a dotted base class —
/// so the generic identifier branch does not also record the parts.
fn consume_idents(node: &Node, spec: &Spec, consumed: &mut FxHashSet<usize>) {
    let mut c = node.walk();
    let mut depth = 0i32;
    loop {
        let n = c.node();
        if spec.is_ident(n.kind_id()) {
            consumed.insert(n.id());
        }
        if c.goto_first_child() {
            depth += 1;
            continue;
        }
        loop {
            if c.goto_next_sibling() {
                break;
            }
            if depth == 0 {
                return;
            }
            c.goto_parent();
            depth -= 1;
        }
    }
}

fn reclassify(kind: DefKind, defs: &[Def], scope: DefIdx) -> DefKind {
    if kind == DefKind::Function
        && scope != NO_SCOPE
        && matches!(
            defs[scope as usize].kind,
            DefKind::Class | DefKind::Interface
        )
    {
        return DefKind::Method;
    }
    kind
}

/// True at module level or directly inside a class — the scopes where a named
/// value is part of the API surface rather than a local temporary.
#[inline]
fn at_container_scope(defs: &[Def], scope: DefIdx) -> bool {
    scope == NO_SCOPE || matches!(defs[scope as usize].kind, DefKind::Class | DefKind::Interface)
}

pub fn extract_file(
    path: &Path,
    file: FileId,
    _lang: Lang,
    spec: &Spec,
    parser: &mut TsParser,
    interner: &Interner,
) -> Option<(FileUnit, Timings, FileMeta)> {
    let mut t = Timings::default();
    let mut unit = FileUnit {
        file,
        ..Default::default()
    };

    let clock = Instant::now();
    let f = File::open(path).ok()?;
    // SAFETY: read-only mapping; the indexer holds no concurrent writer to the
    // working tree during a run.
    let mmap = unsafe { Mmap::map(&f) }.ok()?;
    t.ns_read = clock.elapsed().as_nanos() as u64;

    let src: &[u8] = &mmap;
    t.bytes = src.len() as u64;

    let clock = Instant::now();
    let digest = blake3::hash(src);
    t.ns_hash = clock.elapsed().as_nanos() as u64;

    let md = f.metadata().ok();
    let meta = FileMeta {
        hash: *digest.as_bytes(),
        size: src.len() as u64,
        mtime: md
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    };

    let clock = Instant::now();
    let tree = parser.parse(src, None);
    t.ns_parse = clock.elapsed().as_nanos() as u64;

    let Some(tree) = tree else {
        unit.had_parse_error = true;
        return Some((unit, t, meta));
    };
    unit.had_parse_error = tree.root_node().has_error();

    let clock = Instant::now();
    walk(&tree, src, spec, interner, &mut unit, &mut t);
    t.ns_walk = clock.elapsed().as_nanos() as u64;

    Some((unit, t, meta))
}
