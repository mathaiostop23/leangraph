//! Per-file extraction: mmap -> parse -> single cursor walk -> `FileUnit`.
//!
//! Pure with respect to shared state (the interner is internally concurrent),
//! so this parallelises across files with no coordination.

use crate::cache::FileMeta;
use crate::core::{
    Def, DefIdx, DefKind, FileId, FileUnit, Import, Interner, Recv, Ref, RefKind, Span, SymId,
    NO_SCOPE,
};
use crate::lang::{node_text, Lang, Spec};
use memmap2::Mmap;
use rustc_hash::FxHashSet;
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
/// The words of one comment, docstring or string literal, attributed to the
/// definition around it.
///
/// Bounded deliberately. Words shorter than four characters carry no retrieval
/// signal and are most of the volume; anything past 24 is a token, a hash or a
/// base64 blob rather than a word. The per-definition ceiling exists for the
/// file that embeds a fixture or a minified asset as a string literal, where
/// the alternative is one node quietly owning tens of thousands of words.
const PROSE_MIN: usize = 4;
const PROSE_MAX: usize = 24;
const PROSE_PER_DEF: usize = 96;

fn harvest_prose(
    src: &[u8],
    node: &tree_sitter::Node,
    scope: DefIdx,
    interner: &Interner,
    unit: &mut FileUnit,
    seen: &mut FxHashSet<(DefIdx, SymId)>,
) {
    let Ok(text) = std::str::from_utf8(&src[node.byte_range()]) else {
        return;
    };
    let mut kept = 0usize;
    for raw in text.split(|c: char| !c.is_alphanumeric() && c != '_') {
        if kept >= PROSE_PER_DEF {
            break;
        }
        if raw.len() < PROSE_MIN || raw.len() > PROSE_MAX {
            continue;
        }
        // A run of digits, a hex blob or an identifier-shaped token that is all
        // caps-and-numbers is not a word anyone would type into a bug report.
        if !raw.chars().any(|c| c.is_ascii_alphabetic()) {
            continue;
        }
        let lower = raw.to_ascii_lowercase();
        if is_prose_stopword(&lower) {
            continue;
        }
        let sym = interner.get_or_intern(lower.as_str());
        if seen.insert((scope, sym)) {
            unit.prose.push((scope, sym));
            kept += 1;
        }
    }
}

/// Only the words that are common in *every* English text. Anything narrower is
/// left to the ranking, where a word that appears in half the repository earns a
/// low weight on its own rather than by being guessed at here.
#[inline]
fn is_prose_stopword(w: &str) -> bool {
    matches!(
        w,
        "this"
            | "that"
            | "with"
            | "from"
            | "have"
            | "been"
            | "were"
            | "will"
            | "would"
            | "could"
            | "should"
            | "when"
            | "then"
            | "than"
            | "they"
            | "them"
            | "their"
            | "there"
            | "these"
            | "those"
            | "which"
            | "while"
            | "what"
            | "into"
            | "onto"
            | "over"
            | "under"
            | "each"
            | "some"
            | "such"
            | "only"
            | "also"
            | "more"
            | "most"
            | "other"
            | "does"
            | "done"
            | "here"
            | "must"
            | "very"
            | "just"
            | "like"
            | "make"
            | "made"
            | "same"
            | "both"
            | "because"
            | "about"
            | "after"
            | "before"
            | "between"
            | "through"
    )
}

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
    // Prose nodes nest — a `string` holds a `string_content`, and both are
    // prose — so the same word arrives more than once. Deduplication is what
    // makes that harmless, and it is wanted anyway: how often a word repeats
    // inside one function is not evidence about that function.
    let mut prose_seen: FxHashSet<(DefIdx, SymId)> = FxHashSet::default();
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

        if spec.is_prose(kind) {
            harvest_prose(src, &node, scope, interner, unit, &mut prose_seen);
        }

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
                    // A qualified name carries the class the lexical scope
                    // does not: `void DBImpl::Get()` is a method wherever it is
                    // written.
                    let dk = if dk == DefKind::Function && spec.def_name_is_qualified(&node) {
                        DefKind::Method
                    } else {
                        dk
                    };
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
        } else if spec.is_aliased(kind) {
            // `from m import a as b` binds `b` to whatever `a` names. Recorded
            // as a pair rather than resolved here, because the target lives in
            // another file and the extractor does not know about other files.
            if let Some((l, o)) = spec.alias_pair(&node) {
                if let (Some(lt), Some(ot)) = (node_text(&l, src), node_text(&o, src)) {
                    if lt != ot {
                        consumed.insert(l.id());
                        unit.aliases
                            .push((interner.get_or_intern(lt), interner.get_or_intern(ot)));
                    }
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
    scope == NO_SCOPE
        || matches!(
            defs[scope as usize].kind,
            DefKind::Class | DefKind::Interface
        )
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

#[cfg(test)]
mod tests {
    use crate::core::{DefKind, Recv};
    use crate::testkit::Corpus;

    /// Every receiver kind the extractor distinguishes, for the call named.
    fn recv_of(c: &Corpus, file: usize, called: &str) -> Vec<Recv> {
        c.units[file]
            .refs
            .iter()
            .filter(|r| c.interner.resolve(&r.name) == called)
            .map(|r| r.recv)
            .collect()
    }

    #[test]
    fn the_four_receiver_kinds_are_told_apart() {
        // `Recv` is what decides whether the lexical scope chain is evidence.
        // Collapsing any two of these is not a small error: `self.x()` and
        // `other.x()` resolving the same way is how 1,285 django edges became a
        // method calling itself at confidence 100.
        let c = Corpus::build(&[(
            "a.py",
            "class Child(Base):\n\
             \x20   def run(self, other):\n\
             \x20       bare()\n\
             \x20       self.mine()\n\
             \x20       super().theirs()\n\
             \x20       other.unknown()\n",
        )]);
        assert_eq!(recv_of(&c, 0, "bare"), vec![Recv::Bare]);
        assert_eq!(recv_of(&c, 0, "mine"), vec![Recv::SelfObj]);
        assert_eq!(recv_of(&c, 0, "theirs"), vec![Recv::Super]);
        assert_eq!(recv_of(&c, 0, "unknown"), vec![Recv::Other]);
    }

    #[test]
    fn this_is_the_self_receiver_in_the_typescript_family() {
        let c = Corpus::build(&[(
            "a.ts",
            "class C {\n\
             \x20 run(other: any) {\n\
             \x20   this.mine();\n\
             \x20   other.unknown();\n\
             \x20 }\n\
             \x20 mine() {}\n\
             }\n",
        )]);
        assert_eq!(recv_of(&c, 0, "mine"), vec![Recv::SelfObj]);
        assert_eq!(recv_of(&c, 0, "unknown"), vec![Recv::Other]);
    }

    #[test]
    fn a_receiver_keeps_its_own_name() {
        // Without the name, `flask.redirect()` and `client.open()` are the same
        // reference — and one of them has an import behind it while the other
        // has nothing.
        let c = Corpus::build(&[(
            "a.py",
            "import flask\n\ndef go(client):\n    flask.redirect('/')\n    client.open('/')\n",
        )]);
        let named: Vec<(String, Option<String>)> = c.units[0]
            .refs
            .iter()
            .filter(|r| r.recv == Recv::Other)
            .map(|r| {
                (
                    c.interner.resolve(&r.name).to_string(),
                    r.recv_name.map(|s| c.interner.resolve(&s).to_string()),
                )
            })
            .collect();
        assert!(
            named.contains(&("redirect".to_string(), Some("flask".to_string()))),
            "{named:?}"
        );
        assert!(
            named.contains(&("open".to_string(), Some("client".to_string()))),
            "{named:?}"
        );
    }

    #[test]
    fn nesting_is_recorded_as_it_is_written() {
        let c = Corpus::build(&[(
            "a.py",
            "class Outer:\n\
             \x20   class Inner:\n\
             \x20       def deep(self):\n\
             \x20           return 1\n",
        )]);
        let defs = &c.units[0].defs;
        let name = |i: u32| c.interner.resolve(&defs[i as usize].name).to_string();

        let deep = defs
            .iter()
            .position(|d| c.interner.resolve(&d.name) == "deep")
            .expect("deep is extracted");
        let inner = defs[deep].parent;
        assert_eq!(name(inner), "Inner");
        assert_eq!(name(defs[inner as usize].parent), "Outer");
        assert!(matches!(defs[inner as usize].kind, DefKind::Class));
    }

    #[test]
    fn imports_and_aliases_are_both_captured() {
        let c = Corpus::build(&[(
            "a.py",
            "import os\nimport numpy as np\nfrom helper import assist as fn\n",
        )]);
        let modules: Vec<&str> = c.units[0]
            .imports
            .iter()
            .map(|i| c.interner.resolve(&i.module))
            .collect();
        assert!(modules.contains(&"os"));
        assert!(modules.contains(&"numpy"));

        let aliases: Vec<(String, String)> = c.units[0]
            .aliases
            .iter()
            .map(|(l, o)| {
                (
                    c.interner.resolve(l).to_string(),
                    c.interner.resolve(o).to_string(),
                )
            })
            .collect();
        assert!(
            aliases.contains(&("np".to_string(), "numpy".to_string())),
            "{aliases:?}"
        );
        assert!(
            aliases.contains(&("fn".to_string(), "assist".to_string())),
            "{aliases:?}"
        );
    }

    #[test]
    fn a_constant_assignment_is_a_definition_in_every_language_that_has_one() {
        // `var_def_name` carried a hardcoded whitelist of node kinds, which
        // rejected Ruby's `constant` and took that language to 62.2% recall.
        // A whitelist written from memory is exactly the thing that fails
        // silently — the extractor runs, and simply finds less.
        for (file, src, want) in [
            ("a.rb", "MAX_SIZE = 10\n", "MAX_SIZE"),
            ("a.py", "MAX_SIZE = 10\n", "MAX_SIZE"),
            ("a.go", "const MaxSize = 10\n", "MaxSize"),
            ("a.ts", "export const maxSize = 10;\n", "maxSize"),
        ] {
            let c = Corpus::build(&[(file, src)]);
            let names: Vec<&str> = c.units[0]
                .defs
                .iter()
                .map(|d| c.interner.resolve(&d.name))
                .collect();
            assert!(
                names.contains(&want),
                "{file}: {want} was not extracted, got {names:?}"
            );
        }
    }

    #[test]
    fn a_file_that_does_not_parse_still_yields_what_it_can() {
        // Real repositories contain files that do not parse — a syntax error
        // mid-refactor, a template, a dialect the grammar predates. Dropping
        // the file loses every definition in it; the tree-sitter parse is
        // partial by design and the good half is still worth having.
        let c = Corpus::build(&[(
            "a.py",
            "def before():\n    return 1\n\ndef broken(:\n\ndef after():\n    return 2\n",
        )]);
        let names: Vec<&str> = c.units[0]
            .defs
            .iter()
            .map(|d| c.interner.resolve(&d.name))
            .collect();
        assert!(c.units[0].had_parse_error, "the error must be recorded");
        assert!(
            names.contains(&"before"),
            "definitions before the error survive: {names:?}"
        );
    }

    fn names(c: &Corpus) -> Vec<String> {
        c.units[0]
            .defs
            .iter()
            .map(|d| c.interner.resolve(&d.name).to_string())
            .collect()
    }

    #[test]
    fn a_class_property_is_a_definition_where_the_grammar_labels_no_field() {
        // Kotlin puts the name under an unlabelled `variable_declaration`, so
        // requiring a name field dropped every `val` and `var` in the language:
        // 2,056 symbols in okhttp, 73.5% recall to 93.5% when it was fixed.
        let c = Corpus::build(&[(
            "a.kt",
            "class Demo(private val ctor: String) {\n\
             \x20 private val client = build()\n\
             \x20 private lateinit var server: Server\n\
             }\n\
             enum class Color { RED }\n",
        )]);
        let got = names(&c);
        for want in ["client", "server", "ctor", "RED"] {
            assert!(
                got.contains(&want.to_string()),
                "{want} missing from {got:?}"
            );
        }
        assert!(
            !got.contains(&"build".to_string()),
            "the initialiser is not the name: {got:?}"
        );
        assert!(
            !got.contains(&"Server".to_string()),
            "nor is the type: {got:?}"
        );
    }

    #[test]
    fn a_php_property_is_named_without_its_sigil_and_a_const_counts() {
        // PHP labels the name `variable_name`, whose text is `$errorLevelMap`.
        // Rejecting that wrapper lost every class property — all 376 of
        // monolog's missing symbols — and class constants were not in the spec
        // at all.
        let c = Corpus::build(&[(
            "a.php",
            "<?php\nclass H {\n\
             \x20 private const FATAL = 1;\n\
             \x20 private array $errorLevelMap = [];\n\
             \x20 protected $fatalLevel;\n\
             }\n",
        )]);
        let got = names(&c);
        for want in ["FATAL", "errorLevelMap", "fatalLevel"] {
            assert!(
                got.contains(&want.to_string()),
                "{want} missing from {got:?}"
            );
        }
        assert!(
            !got.iter().any(|n| n.starts_with('$')),
            "the sigil belongs to the syntax, not the name: {got:?}"
        );
    }

    #[test]
    fn a_c_typedef_and_union_are_definitions() {
        // `uv.h` is mostly typedefs, and 184 of libuv's missing symbols were
        // the type names the rest of the codebase refers to.
        let c = Corpus::build(&[(
            "a.c",
            "typedef struct uv_loop_s uv_loop_t;\nunion payload { int a; };\n",
        )]);
        let got = names(&c);
        assert!(got.contains(&"uv_loop_t".to_string()), "{got:?}");
        assert!(got.contains(&"payload".to_string()), "{got:?}");
    }

    #[test]
    fn an_out_of_line_definition_is_a_method() {
        // The class is named in the declarator, not in the enclosing scope.
        let c = Corpus::build(&[(
            "a.cc",
            "namespace db {\nclass Impl { public: int Get(int k); };\n\
             int Impl::Get(int k) { return k; }\n}\n",
        )]);
        let get = c.units[0]
            .defs
            .iter()
            .find(|d| c.interner.resolve(&d.name) == "Get" && matches!(d.kind, DefKind::Method));
        assert!(
            get.is_some(),
            "Impl::Get must be a method, got {:?}",
            c.units[0]
                .defs
                .iter()
                .map(|d| (c.interner.resolve(&d.name), d.kind))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_file_is_not_an_error() {
        let c = Corpus::build(&[("empty.py", "")]);
        assert!(c.units[0].defs.is_empty());
        assert!(c.units[0].refs.is_empty());
    }

    // ---- prose ------------------------------------------------------------

    /// Words a user would write live in comments, docstrings and messages, and
    /// they are attributed to the definition around them rather than the file.
    #[test]
    fn prose_is_harvested_and_attributed_to_its_definition() {
        let c = Corpus::build(&[(
            "a.py",
            "def runshell(conn):\n             \x20   \"Open an interactive shell against the configured database.\"\n             \x20   # the password is passed through the environment\n             \x20   raise RuntimeError(\"connection refused\")\n",
        )]);
        let u = &c.units[0];
        let words: Vec<&str> = u
            .prose
            .iter()
            .map(|&(_, w)| c.interner.resolve(&w))
            .collect();
        for want in [
            "interactive",
            "shell",
            "database",
            "password",
            "environment",
            "refused",
        ] {
            assert!(words.contains(&want), "{want:?} missing from {words:?}");
        }
        // Short words and pure punctuation carry nothing and are not stored.
        assert!(!words.iter().any(|w| w.len() < 4), "{words:?}");

        let def = u
            .defs
            .iter()
            .position(|d| c.interner.resolve(&d.name) == "runshell");
        assert!(
            u.prose
                .iter()
                .any(|&(scope, _)| Some(scope as usize) == def),
            "prose must hang off the function it was written inside"
        );
    }

    /// The same word repeated inside one definition says nothing about it, and
    /// prose node kinds nest, so the same text arrives more than once.
    #[test]
    fn a_word_is_stored_once_per_definition() {
        let c = Corpus::build(&[(
            "a.py",
            "def f():\n             \x20   # retry retry retry\n             \x20   return \"retry\"\n",
        )]);
        let n = c.units[0]
            .prose
            .iter()
            .filter(|&&(_, w)| c.interner.resolve(&w) == "retry")
            .count();
        assert_eq!(n, 1, "{:?}", c.units[0].prose.len());
    }

    #[test]
    fn code_without_prose_stores_none() {
        let c = Corpus::build(&[("a.py", "def f(x):\n    return x + 1\n")]);
        assert!(c.units[0].prose.is_empty());
    }
}
