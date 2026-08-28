//! Core types.
//!
//! Two decisions are load-bearing here and both are made at the type level so
//! they cannot be skipped later:
//!
//! 1. **Every identifier is a `SymId` (u32), never a `String`.** Resolution is
//!    fundamentally name-matching; on `String` that is hashing and memcmp in the
//!    inner loop, on `u32` it is integer equality.
//!
//! 2. **Every edge carries `conf` + `prov`.** This is not decoration. Ranking by
//!    confidence is how the context builder returns *fewer* tokens at equal
//!    recall — i.e. it is the cost mechanism. An edge without provenance cannot
//!    be ranked, and an agent cannot tell a proven call from a guess.

use lasso::ThreadedRodeo;

pub type SymId = lasso::Spur; // u32 newtype
pub type FileId = u32;
pub type DefIdx = u32; // index into a FileUnit's `defs`

/// Sentinel for "no enclosing definition" (file top level).
pub const NO_SCOPE: DefIdx = u32::MAX;

pub type Interner = ThreadedRodeo;

/// Byte range within the source file. Text is recovered by slicing the mmap at
/// query time, so nothing here owns a string.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Span {
    pub start: u32,
    pub end: u32,
}

impl Span {
    #[inline]
    pub fn of(node: &tree_sitter::Node) -> Span {
        Span {
            start: node.start_byte() as u32,
            end: node.end_byte() as u32,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum DefKind {
    Function,
    Method,
    Class,
    Interface,
    Module,
    Variable,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum RefKind {
    Call,
    New,
    /// Base class / implemented interface.
    Extends,
    /// Any other mention of a name: inheritance lists, type annotations,
    /// decorators, arguments. Without these a class that is subclassed but
    /// never *called* looks unused, which is exactly backwards.
    Read,
}

impl RefKind {
    #[inline]
    pub fn edge_kind(self) -> EdgeKind {
        match self {
            RefKind::Call | RefKind::New => EdgeKind::Calls,
            RefKind::Extends => EdgeKind::Extends,
            RefKind::Read => EdgeKind::References,
        }
    }
}

/// How an edge came to exist. Carried on every edge so consumers — human or
/// agent — can weigh it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum Provenance {
    /// Resolved through the lexical scope chain inside one file.
    Scope,
    /// Resolved through an explicit import.
    Import,
    /// Matched by name across the repo. Ambiguous by construction.
    NameMatch,
    /// Derived from git history co-change, not from the AST.
    CoChange,
    /// Framework convention (route -> handler).
    Framework,
}

impl Provenance {
    /// Default confidence for a tier. Resolvers may lower it, never raise it.
    #[inline]
    pub fn base_conf(self) -> u8 {
        match self {
            Provenance::Scope => 100,
            Provenance::Import => 95,
            Provenance::Framework => 85,
            Provenance::NameMatch => 55,
            Provenance::CoChange => 40,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum EdgeKind {
    Contains,
    Calls,
    Imports,
    Extends,
    References,
}

/// A definition site. `parent` is the index of the enclosing definition within
/// the same file (`NO_SCOPE` at file top level), which gives us the containment
/// tree for free during the single extraction pass.
#[derive(Clone, Copy, Debug)]
pub struct Def {
    pub name: SymId,
    pub kind: DefKind,
    pub span: Span,
    pub name_span: Span,
    pub parent: DefIdx,
}

/// A reference site, tagged with the definition that lexically encloses it.
#[derive(Clone, Copy, Debug)]
pub struct Ref {
    pub name: SymId,
    pub kind: RefKind,
    pub span: Span,
    pub scope: DefIdx,
    /// What the call was made through.
    pub recv: Recv,
    /// The receiver's own name, where it had one. `flask.redirect()` is a call
    /// through a module and an import is real evidence about it;
    /// `client.open()` is a call through an object and it is not. Without the
    /// name the two are the same reference.
    pub recv_name: Option<SymId>,
}

/// How a reference reached its name.
///
/// The lexical scope chain is evidence about a bare name, and about a name
/// reached through `self` — both are questions about the text around the call.
/// It is not evidence about `other.foo()`: nothing in the surrounding scopes
/// says what `other` is, and binding that to the nearest same-named method is a
/// guess wearing the confidence of a proof. Measured on django, 1,285 edges at
/// confidence 100 were a method calling *itself* because `super().x()` and
/// `x()` were indistinguishable by the time the resolver saw them.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
#[repr(u8)]
pub enum Recv {
    /// `foo()`
    #[default]
    Bare = 0,
    /// `self.foo()`, `this.foo()` — the enclosing object, so scope still holds
    SelfObj = 1,
    /// `super().foo()` — explicitly *not* this class's implementation
    Super = 2,
    /// `anything_else.foo()` — the receiver decides, and we do not know it
    Other = 3,
}

impl Recv {
    /// Is the lexical scope chain valid evidence for this reference?
    #[inline]
    pub fn lexical(self) -> bool {
        matches!(self, Recv::Bare | Recv::SelfObj)
    }

    #[inline]
    pub fn from_u8(v: u8) -> Recv {
        match v {
            1 => Recv::SelfObj,
            2 => Recv::Super,
            3 => Recv::Other,
            _ => Recv::Bare,
        }
    }
}

/// A local name bound to something imported under a different name.
///
/// `from M import a as b` binds `b`; `import numpy as np` binds `np`. Without
/// this the local name resolves to nothing, because the resolver only knows the
/// names the target module exports — and `b` is not one of them.
pub type Alias = (SymId, SymId); // (local, original)

/// A raw import statement. The module string is interned verbatim; turning it
/// into a `FileId` is the resolver's job and is language-specific.
#[derive(Clone, Copy, Debug)]
pub struct Import {
    pub module: SymId,
    pub span: Span,
}

/// Everything extracted from one file. Produced by a pure function of the file
/// bytes, so extraction parallelises with no shared mutable state.
#[derive(Debug, Default)]
pub struct FileUnit {
    pub file: FileId,
    pub defs: Vec<Def>,
    pub refs: Vec<Ref>,
    pub imports: Vec<Import>,
    pub had_parse_error: bool,
    /// Local bindings introduced by `as`.
    pub aliases: Vec<Alias>,
    /// Words lifted from comments, docstrings and string literals, each
    /// attributed to the definition that encloses it (`NO_SCOPE` for file
    /// level). Interned like every other name, so what is carried around is a
    /// `u32` and matching a query word against a repository is integer work.
    ///
    /// This is the only part of a file written in the language its *users*
    /// speak. A report saying "the run never continued after approval" names
    /// no symbol that resolves, and matches the sentence above the function
    /// that handles it. Deduplicated per definition: term frequency inside one
    /// function says little, and dropping it keeps this to a bounded size.
    pub prose: Vec<(DefIdx, SymId)>,
}

/// Global node id.
///
/// Assigned through a persistent key table rather than by position. Positional
/// ids are simpler but fatal to incremental work: adding one definition shifts
/// every id after it, which invalidates the whole adjacency structure and every
/// cached edge. A stable id survives edits to unrelated files.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

/// Content-derived identity of a node, independent of where it happens to sit
/// in any array: the file it lives in, its qualified name, its kind, and which
/// occurrence it is when a name repeats at the same scope.
///
/// 64 bits over ~10^5 nodes puts collision probability around 10^-10 — far
/// below the rate at which we would get the semantics wrong by other means.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct NodeKey(pub u64);

impl NodeKey {
    pub fn of(file_path: &str, qualified: &str, kind: u8, occurrence: u32) -> NodeKey {
        NodeKey::of_parts(file_path, qualified.split('.'), kind, occurrence)
    }

    /// Same identity, built from the enclosing chain without materialising it.
    ///
    /// Joining the chain into a `String` per definition cost one allocation per
    /// node — 65,000 of them on django, and 16 ms of a sync spent producing
    /// strings nothing ever reads.
    pub fn of_parts<'a>(
        file_path: &str,
        qualified: impl Iterator<Item = &'a str>,
        kind: u8,
        occurrence: u32,
    ) -> NodeKey {
        use std::hash::{Hash, Hasher};
        let mut h = rustc_hash::FxHasher::default();
        file_path.hash(&mut h);
        0xffu8.hash(&mut h); // separator, so ("a","bc") and ("ab","c") differ
        for part in qualified {
            part.hash(&mut h);
            0xfeu8.hash(&mut h);
        }
        kind.hash(&mut h);
        occurrence.hash(&mut h);
        NodeKey(h.finish())
    }

    /// Files are nodes too, and their identity is just the path.
    pub fn of_file(file_path: &str) -> NodeKey {
        NodeKey::of(file_path, "", u8::MAX, 0)
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub conf: u8,
    pub prov: Provenance,
}
