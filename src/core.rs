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
}

/// A raw import statement. The module string is interned verbatim; turning it
/// into a `FileId` is the resolver's job and is language-specific.
#[derive(Clone, Copy, Debug)]
pub struct Import {
    pub module: SymId,
    pub alias: Option<SymId>,
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
}

/// Global node id: (file, definition index) flattened during graph build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct NodeId(pub u32);

#[derive(Clone, Copy, Debug)]
pub struct Edge {
    pub src: NodeId,
    pub dst: NodeId,
    pub kind: EdgeKind,
    pub conf: u8,
    pub prov: Provenance,
}
