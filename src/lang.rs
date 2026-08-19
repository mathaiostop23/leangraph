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

use crate::core::{DefKind, Recv, RefKind};
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
    /// nodes that are definitions only when they *hold* a function
    /// (`const foo = () => {}` — the dominant TS idiom, invisible to plain
    /// node-kind matching because the name lives on the declarator, not the
    /// function)
    cond_defs: Vec<u16>,
    /// field ids holding a conditional definition's value
    f_value: Vec<u16>,
    /// value kinds that make a conditional definition count as a function
    fn_values: Vec<u16>,
    /// nodes that define a named value. Only recorded at module or class
    /// scope: a local inside a function body is noise in a code graph, but a
    /// module constant or a class attribute is a real thing people reference.
    var_defs: Vec<u16>,
    /// field ids holding a variable definition's name
    f_var_name: Vec<u16>,
    /// bare identifier node kinds, captured as generic references
    idents: Vec<u16>,
    /// nodes listing base classes / implemented interfaces
    heritage: Vec<u16>,
    /// dotted access — `a.B`. Only the last segment names anything.
    dotted: Vec<u16>,
    /// field ids holding the receiver of a dotted access
    f_object: Vec<u16>,
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
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(&l, &["assignment"]),
            f_var_name: fields(&l, &["left"]),
            idents: kinds(&l, &["identifier"]),
            heritage: kinds(&l, &["argument_list"]),
            dotted: kinds(&l, &["attribute"]),
            f_object: fields(&l, &["object"]),
        },
        Lang::TypeScript | Lang::Tsx => Spec {
            lang,
            defs: tagged(
                &l,
                &[
                    ("function_declaration", DefKind::Function),
                    ("generator_function_declaration", DefKind::Function),
                    ("class_declaration", DefKind::Class),
                    ("abstract_class_declaration", DefKind::Class),
                    ("interface_declaration", DefKind::Interface),
                    ("type_alias_declaration", DefKind::Interface),
                    ("enum_declaration", DefKind::Class),
                    ("method_definition", DefKind::Method),
                    ("method_signature", DefKind::Method),
                    ("abstract_method_signature", DefKind::Method),
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
            // `key` is for object-literal entries: `{ handleEvent: () => {} }`
            // is a method by every meaning that matters, and there are hundreds
            // of them in real React code.
            f_name: fields(&l, &["name", "key"]),
            f_callee: fields(&l, &["function", "constructor"]),
            f_member: fields(&l, &["property"]),
            f_module: fields(&l, &["source"]),
            cond_defs: kinds(&l, &["variable_declarator", "public_field_definition", "pair"]),
            f_value: fields(&l, &["value"]),
            fn_values: kinds(
                &l,
                &["arrow_function", "function_expression", "function", "class"],
            ),
            var_defs: kinds(&l, &["variable_declarator", "public_field_definition"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier", "type_identifier"]),
            heritage: kinds(&l, &["class_heritage", "extends_clause", "implements_clause"]),
            dotted: kinds(&l, &["member_expression", "nested_type_identifier"]),
            f_object: fields(&l, &["object", "module"]),
        },
    }
}

impl Spec {
    /// Definition kind for a node, including the conditional forms.
    ///
    /// Plain node-kind matching misses `const Foo = () => {}` entirely: the
    /// `arrow_function` node has no name, and the `variable_declarator` that
    /// does have one is not a definition in general. So for those we look at
    /// what the declarator holds.
    #[inline]
    pub fn def_kind_of(&self, node: &Node) -> Option<DefKind> {
        let k = node.kind_id();
        if let Some((_, d)) = self.defs.iter().find(|(id, _)| *id == k) {
            return Some(*d);
        }
        if self.cond_defs.contains(&k) {
            let v = first_field(node, &self.f_value)?;
            if self.fn_values.contains(&v.kind_id()) {
                return Some(DefKind::Function);
            }
        }
        None
    }

    /// Name node for a variable-style definition, if this node is one.
    /// Destructuring patterns are skipped: `const {a, b} = x` binds several
    /// names and none of them is *the* definition of that statement.
    #[inline]
    pub fn var_def_name<'t>(&self, node: &Node<'t>) -> Option<Node<'t>> {
        if !self.var_defs.contains(&node.kind_id()) {
            return None;
        }
        let n = first_field(node, &self.f_var_name)?;
        matches!(n.kind(), "identifier" | "property_identifier" | "type_identifier").then_some(n)
    }

    #[inline]
    pub fn ref_kind(&self, k: u16) -> Option<RefKind> {
        self.refs.iter().find(|(id, _)| *id == k).map(|(_, r)| *r)
    }

    /// Node kinds that define a class — needed to disambiguate Python's
    /// `argument_list`, which serves both call arguments and superclasses.
    #[inline]
    pub fn is_class_node(&self, k: u16) -> bool {
        self.defs
            .iter()
            .any(|(id, d)| *id == k && matches!(d, DefKind::Class))
    }

    #[inline]
    pub fn is_ident(&self, k: u16) -> bool {
        self.idents.contains(&k)
    }

    /// True inside a base-class / implements list. Python reuses
    /// `argument_list` for both call arguments and superclasses, so the caller
    /// must check the enclosing node kind, not just this one.
    #[inline]
    pub fn is_heritage(&self, k: u16) -> bool {
        self.heritage.contains(&k)
    }

    /// The named segment of a dotted access, and nothing else.
    ///
    /// `class Migration(migrations.Migration)` has two identifiers in its
    /// heritage list and only one of them is a base class. Treating both as
    /// bases is how a module alias ends up recorded as a superclass — 8,655
    /// edges in django, 47.4% of every `extends` edge it had.
    #[inline]
    pub fn is_dotted(&self, k: u16) -> bool {
        self.dotted.contains(&k)
    }

    #[inline]
    pub fn dotted_member<'t>(&self, node: &Node<'t>) -> Option<Node<'t>> {
        first_field(node, &self.f_member)
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

    /// What a call was made through.
    ///
    /// This is the piece of evidence the resolver was missing. `ref_name_node`
    /// already reduces `app_config.get_models()` to `get_models`, which is
    /// right — but it discards the receiver in doing so, and the receiver is
    /// the whole reason the surrounding scopes cannot answer the question.
    pub fn recv_of(&self, call: &Node, src: &[u8]) -> Recv {
        let Some(callee) = first_field(call, &self.f_callee) else {
            return Recv::Bare;
        };
        if !self.is_dotted(callee.kind_id()) {
            return Recv::Bare;
        }
        let Some(obj) = first_field(&callee, &self.f_object) else {
            return Recv::Other;
        };
        match node_text(&obj, src) {
            Some("self") | Some("cls") | Some("this") => Recv::SelfObj,
            // Python spells it `super()`, TypeScript spells it `super`.
            Some(t) if t == "super" || t.starts_with("super(") => Recv::Super,
            _ => Recv::Other,
        }
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
