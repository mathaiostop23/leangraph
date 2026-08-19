//! Per-file extraction: mmap -> parse -> single cursor walk -> `FileUnit`.
//!
//! Pure with respect to shared state (the interner is internally concurrent),
//! so this parallelises across files with no coordination.

use crate::core::{Def, DefIdx, FileId, FileUnit, Import, Interner, Ref, Span, NO_SCOPE};
use crate::lang::{node_text, Lang, Spec};
use memmap2::Mmap;
use std::fs::File;
use std::path::Path;
use std::time::Instant;
use tree_sitter::Parser as TsParser;

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
    let mut depth: i32 = 0;

    loop {
        // Leave any scopes whose subtree we have finished.
        while let Some(&(_, d)) = stack.last() {
            if d >= depth {
                stack.pop();
            } else {
                break;
            }
        }

        let node = cursor.node();
        let kind = node.kind_id();
        let scope = stack.last().map(|&(i, _)| i).unwrap_or(NO_SCOPE);
        t.ast_nodes += 1;

        if let Some(dk) = spec.def_kind(kind) {
            if let Some(nn) = spec.def_name_node(&node) {
                if let Some(txt) = node_text(&nn, src) {
                    let idx = unit.defs.len() as DefIdx;
                    unit.defs.push(Def {
                        name: interner.get_or_intern(txt),
                        kind: dk,
                        span: Span::of(&node),
                        name_span: Span::of(&nn),
                        parent: scope,
                    });
                    stack.push((idx, depth));
                }
            }
        } else if let Some(rk) = spec.ref_kind(kind) {
            if let Some(nn) = spec.ref_name_node(&node) {
                if let Some(txt) = node_text(&nn, src) {
                    unit.refs.push(Ref {
                        name: interner.get_or_intern(txt),
                        kind: rk,
                        span: Span::of(&node),
                        scope,
                    });
                }
            }
        } else if spec.is_import(kind) {
            if let Some(mn) = spec.import_module_node(&node) {
                if let Some(txt) = node_text(&mn, src) {
                    unit.imports.push(Import {
                        module: interner.get_or_intern(txt),
                        alias: None,
                        span: Span::of(&node),
                    });
                }
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

pub fn extract_file(
    path: &Path,
    file: FileId,
    _lang: Lang,
    spec: &Spec,
    parser: &mut TsParser,
    interner: &Interner,
) -> Option<(FileUnit, Timings)> {
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
    let _digest = blake3::hash(src);
    t.ns_hash = clock.elapsed().as_nanos() as u64;

    let clock = Instant::now();
    let tree = parser.parse(src, None);
    t.ns_parse = clock.elapsed().as_nanos() as u64;

    let Some(tree) = tree else {
        unit.had_parse_error = true;
        return Some((unit, t));
    };
    unit.had_parse_error = tree.root_node().has_error();

    let clock = Instant::now();
    walk(&tree, src, spec, interner, &mut unit, &mut t);
    t.ns_walk = clock.elapsed().as_nanos() as u64;

    Some((unit, t))
}
