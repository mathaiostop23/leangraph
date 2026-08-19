//! Language specifications.
//!
//! Everything a language needs is reduced to `u16` node-kind and field IDs,
//! resolved once at startup. The extraction hot loop then compares integers and
//! never touches a string until it has an identifier worth interning.
//!
//! These are hardcoded for Phase 1 (Python + TypeScript, tier 4). Phase 2
//! replaces the bodies of `spec_for` with a TOML loader — the `Spec` shape is
//! already the thing a config file would describe, so nothing above this module
//! changes when that lands.

use crate::core::{DefKind, RefKind};
use tree_sitter::{Language, Node};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang {
    Python,
    TypeScript,
    Tsx,
}

pub const ALL_LANGS: [Lang; 3] = [Lang::Python, Lang::TypeScript, Lang::Tsx];

impl Lang {
    pub fn from_ext(ext: &str) -> Option<Lang> {
        match ext {
            "py" | "pyi" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Lang::Tsx),
            _ => None,
        }
    }

    pub fn ts_language(self) -> Language {
        match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx/jsx",
        }
    }
}

pub struct Spec {
    pub lang: Lang,
    /// node-kind id -> our definition kind
    defs: Vec<(u16, DefKind)>,
    /// node-kind id -> our reference kind
    refs: Vec<(u16, RefKind)>,
    imports: Vec<u16>,
    /// field ids holding a definition's name
    f_name: Vec<u16>,
    /// field ids holding a call's callee expression
    f_callee: Vec<u16>,
    /// field ids for the rightmost segment of a member/attribute access
    f_member: Vec<u16>,
    /// field ids holding an import's module path
    f_module: Vec<u16>,
}

fn kinds(l: &Language, names: &[&str]) -> Vec<u16> {
    names
        .iter()
        .filter_map(|n| match l.id_for_node_kind(n, true) {
            0 => None,
            id => Some(id),
        })
        .collect()
}

fn tagged<T: Copy>(l: &Language, pairs: &[(&str, T)]) -> Vec<(u16, T)> {
    pairs
        .iter()
        .filter_map(|(n, t)| match l.id_for_node_kind(n, true) {
            0 => None,
            id => Some((id, *t)),
        })
        .collect()
}

fn fields(l: &Language, names: &[&str]) -> Vec<u16> {
    names
        .iter()
        .filter_map(|n| l.field_id_for_name(n).map(|f| f.get()))
        .collect()
}

pub fn spec_for(lang: Lang) -> Spec {
    let l = lang.ts_language();
    match lang {
        Lang::Python => Spec {
            lang,
            defs: tagged(
                &l,
                &[
                    ("function_definition", DefKind::Function),
                    ("class_definition", DefKind::Class),
                ],
            ),
            refs: tagged(&l, &[("call", RefKind::Call)]),
            imports: kinds(&l, &["import_statement", "import_from_statement"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["function"]),
            f_member: fields(&l, &["attribute"]),
            f_module: fields(&l, &["module_name", "name"]),
        },
        Lang::TypeScript | Lang::Tsx => Spec {
            lang,
            defs: tagged(
                &l,
                &[
                    ("function_declaration", DefKind::Function),
                    ("generator_function_declaration", DefKind::Function),
                    ("class_declaration", DefKind::Class),
                    ("interface_declaration", DefKind::Interface),
                    ("method_definition", DefKind::Method),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("call_expression", RefKind::Call),
                    ("new_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["import_statement"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["function", "constructor"]),
            f_member: fields(&l, &["property"]),
            f_module: fields(&l, &["source"]),
        },
    }
}

impl Spec {
    #[inline]
    pub fn def_kind(&self, k: u16) -> Option<DefKind> {
        self.defs.iter().find(|(id, _)| *id == k).map(|(_, d)| *d)
    }

    #[inline]
    pub fn ref_kind(&self, k: u16) -> Option<RefKind> {
        self.refs.iter().find(|(id, _)| *id == k).map(|(_, r)| *r)
    }

    #[inline]
    pub fn is_import(&self, k: u16) -> bool {
        self.imports.contains(&k)
    }

    /// The identifier node naming a definition.
    #[inline]
    pub fn def_name_node<'t>(&self, def: &Node<'t>) -> Option<Node<'t>> {
        first_field(def, &self.f_name)
    }

    /// The identifier node naming a call's target.
    ///
    /// For a member/attribute access we take the **rightmost** segment:
    /// `a.b.c()` resolves on `c`. That loses the receiver, which tier-3
    /// resolution needs — but at tier 2 (global name match) the method name is
    /// the discriminating token, and keeping the whole expression would make
    /// every call unique and therefore unmatchable.
    #[inline]
    pub fn ref_name_node<'t>(&self, call: &Node<'t>) -> Option<Node<'t>> {
        let callee = first_field(call, &self.f_callee)?;
        if let Some(seg) = first_field(&callee, &self.f_member) {
            return Some(seg);
        }
        Some(callee)
    }

    /// The node holding an import's module path.
    #[inline]
    pub fn import_module_node<'t>(&self, imp: &Node<'t>) -> Option<Node<'t>> {
        first_field(imp, &self.f_module)
    }
}

#[inline]
fn first_field<'t>(node: &Node<'t>, field_ids: &[u16]) -> Option<Node<'t>> {
    field_ids.iter().find_map(|f| node.child_by_field_id(*f))
}

/// Source text of a node, quote-stripped so TS `import "./x"` and Python
/// `import x` both yield a bare module path.
#[inline]
pub fn node_text<'a>(node: &Node, src: &'a [u8]) -> Option<&'a str> {
    let raw = src.get(node.start_byte()..node.end_byte())?;
    let s = std::str::from_utf8(raw).ok()?;
    Some(s.trim_matches(|c| c == '"' || c == '\'' || c == '`'))
}
