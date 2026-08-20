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
use std::sync::Mutex;
use tree_sitter::{Language, Node};

#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub enum Lang {
    Python,
    TypeScript,
    Tsx,
    Rust,
    Go,
    Java,
    C,
    Cpp,
    CSharp,
    Ruby,
    Php,
    Kotlin,
    Swift,
    Scala,
}

pub const ALL_LANGS: [Lang; 14] = [
    Lang::Python,
    Lang::TypeScript,
    Lang::Tsx,
    Lang::Rust,
    Lang::Go,
    Lang::Java,
    Lang::C,
    Lang::Cpp,
    Lang::CSharp,
    Lang::Ruby,
    Lang::Php,
    Lang::Kotlin,
    Lang::Swift,
    Lang::Scala,
];

/// Extensions that are source code in a language this does not parse.
///
/// The distinction matters for what gets reported. A repository full of `.po`
/// and `.rst` is not a repository we failed on — those are translations and
/// documentation, and saying "3,976 files skipped" about them reads as a
/// malfunction. A repository full of `.ex` or `.hs` *is* one we do not cover,
/// and the user should be told plainly rather than left with a two-node graph
/// and a success message.
pub const UNPARSED_SOURCE: &[&str] = &[
    "ex", "exs", "erl", "hrl", "hs", "lhs", "ml", "mli", "lua", "pl", "pm", "r", "jl", "dart",
    "groovy", "gradle", "clj", "cljs", "cljc", "edn", "f90", "f95", "f03", "for", "vb", "m", "mm",
    "zig", "nim", "cr", "sh", "bash", "zsh", "fish", "ps1", "sql", "v", "sv", "elm", "purs", "rkt",
    "scm", "lisp", "el", "pas", "ada", "adb", "cob", "asm", "s", "d", "tcl", "awk", "vue",
    "svelte", "astro", "coffee", "hx", "pony", "sol", "move", "wat", "wasm",
];

impl Lang {
    /// Is this extension code we simply do not parse, as opposed to data?
    pub fn is_unparsed_source(ext: &str) -> bool {
        UNPARSED_SOURCE.contains(&ext)
    }

    pub fn from_ext(ext: &str) -> Option<Lang> {
        match ext {
            "py" | "pyi" => Some(Lang::Python),
            "ts" | "mts" | "cts" => Some(Lang::TypeScript),
            "tsx" | "jsx" | "js" | "mjs" | "cjs" => Some(Lang::Tsx),
            "rs" => Some(Lang::Rust),
            "go" => Some(Lang::Go),
            "java" => Some(Lang::Java),
            // `.h` is C here. It is ambiguous — most headers in a C++ project are
            // C++ — but the C grammar parses the common subset without failing,
            // where the reverse is not true.
            "c" | "h" => Some(Lang::C),
            "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" => Some(Lang::Cpp),
            "cs" => Some(Lang::CSharp),
            "rb" | "rake" => Some(Lang::Ruby),
            "php" => Some(Lang::Php),
            "kt" | "kts" => Some(Lang::Kotlin),
            "swift" => Some(Lang::Swift),
            "scala" | "sc" => Some(Lang::Scala),
            _ => None,
        }
    }

    pub fn ts_language(self) -> Language {
        match self {
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
            Lang::Java => tree_sitter_java::LANGUAGE.into(),
            Lang::C => tree_sitter_c::LANGUAGE.into(),
            Lang::Cpp => tree_sitter_cpp::LANGUAGE.into(),
            Lang::CSharp => tree_sitter_c_sharp::LANGUAGE.into(),
            Lang::Ruby => tree_sitter_ruby::LANGUAGE.into(),
            Lang::Php => tree_sitter_php::LANGUAGE_PHP.into(),
            Lang::Kotlin => tree_sitter_kotlin_ng::LANGUAGE.into(),
            Lang::Swift => tree_sitter_swift::LANGUAGE.into(),
            Lang::Scala => tree_sitter_scala::LANGUAGE.into(),
        }
    }

    /// How this language spells the join between module path segments.
    ///
    /// Getting it wrong does not error — it produces a module table nothing
    /// matches, so every import falls through to a name match and the tier
    /// silently reports zero.
    pub fn module_sep(self) -> &'static str {
        match self {
            Lang::Python | Lang::Java | Lang::Kotlin | Lang::Scala | Lang::CSharp => ".",
            Lang::Rust => "::",
            _ => "/",
        }
    }

    /// Does an import path here name a symbol at the end, rather than only a
    /// module?
    ///
    /// `use crate::core::Def` and `import java.util.List` both do; Python's
    /// `from a.b import c` does not, because the module field stops at `a.b`.
    /// Dropping the last segment for a language that does not do this binds
    /// `import a.b` to the package `a`, which is a different file.
    pub fn import_ends_in_symbol(self) -> bool {
        matches!(
            self,
            Lang::Rust | Lang::Java | Lang::CSharp | Lang::Kotlin | Lang::Scala
        )
    }

    /// Prefixes a language puts on an import path that name the current crate
    /// or package rather than a directory. `use crate::core::Def` addresses the
    /// same file as `use core::Def` from the root.
    pub fn strip_import_prefix(self, path: &str) -> &str {
        match self {
            Lang::Rust => path
                .strip_prefix("crate::")
                .or_else(|| path.strip_prefix("self::"))
                .or_else(|| path.strip_prefix("super::"))
                .unwrap_or(path),
            _ => path,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Lang::Python => "python",
            Lang::TypeScript => "typescript",
            Lang::Tsx => "tsx/jsx",
            Lang::Rust => "rust",
            Lang::Go => "go",
            Lang::Java => "java",
            Lang::C => "c",
            Lang::Cpp => "c++",
            Lang::CSharp => "c#",
            Lang::Ruby => "ruby",
            Lang::Php => "php",
            Lang::Kotlin => "kotlin",
            Lang::Swift => "swift",
            Lang::Scala => "scala",
        }
    }
}

pub struct Spec {
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
    /// nodes that bind a local name to something imported under another name
    aliased: Vec<u16>,
    /// nodes that bind a whole module to one local name (`import * as ns`)
    namespaced: Vec<u16>,
    /// field id holding that local name
    f_alias: Vec<u16>,
}

/// Names a spec asked for that its grammar does not have.
///
/// A dropped name is not an error anywhere — it is an empty list and a feature
/// that quietly stops working, which is exactly how a grammar upgrade would
/// break a language without failing a build. Recording the misses lets a check
/// assert there are none, against the compiled specs rather than against a copy
/// of them in a file.
static MISSES: Mutex<Vec<(&'static str, String)>> = Mutex::new(Vec::new());
/// Which language `spec_for` is currently assembling, so a miss can name it.
static BUILDING: Mutex<&'static str> = Mutex::new("?");

fn miss(what: &str, name: &str) {
    let lang = BUILDING.lock().map(|g| *g).unwrap_or("?");
    if let Ok(mut m) = MISSES.lock() {
        m.push((lang, format!("{what}:{name}")));
    }
}

/// Every name the last `spec_for` calls could not resolve.
#[cfg(test)]
pub fn unresolved_names() -> Vec<(&'static str, String)> {
    MISSES.lock().map(|m| m.clone()).unwrap_or_default()
}

fn kinds(l: &Language, names: &[&str]) -> Vec<u16> {
    names
        .iter()
        .filter_map(|n| match l.id_for_node_kind(n, true) {
            0 => {
                miss("kind", n);
                None
            }
            id => Some(id),
        })
        .collect()
}

fn tagged<T: Copy>(l: &Language, pairs: &[(&str, T)]) -> Vec<(u16, T)> {
    pairs
        .iter()
        .filter_map(|(n, t)| match l.id_for_node_kind(n, true) {
            0 => {
                miss("kind", n);
                None
            }
            id => Some((id, *t)),
        })
        .collect()
}

fn fields(l: &Language, names: &[&str]) -> Vec<u16> {
    names
        .iter()
        .filter_map(|n| match l.field_id_for_name(n) {
            Some(f) => Some(f.get()),
            None => {
                miss("field", n);
                None
            }
        })
        .collect()
}

pub fn spec_for(lang: Lang) -> Spec {
    let l = lang.ts_language();
    if let Ok(mut b) = BUILDING.lock() {
        *b = lang.name();
    }
    let spec = match lang {
        Lang::Python => Spec {
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
            // Covers both `import numpy as np` and `from m import a as b`;
            // Python spells them with the same node.
            aliased: kinds(&l, &["aliased_import"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["alias"]),
        },
        Lang::TypeScript | Lang::Tsx => Spec {
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
            cond_defs: kinds(
                &l,
                &["variable_declarator", "public_field_definition", "pair"],
            ),
            f_value: fields(&l, &["value"]),
            fn_values: kinds(
                &l,
                // No `"function"`: it is the anonymous keyword token, not a
                // named node, so it never resolved. `function_expression` and
                // `arrow_function` are the real shapes.
                &[
                    "arrow_function",
                    "function_expression",
                    "generator_function",
                    "class",
                ],
            ),
            var_defs: kinds(&l, &["variable_declarator", "public_field_definition"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier", "type_identifier"]),
            heritage: kinds(
                &l,
                &["class_heritage", "extends_clause", "implements_clause"],
            ),
            dotted: kinds(&l, &["member_expression", "nested_type_identifier"]),
            f_object: fields(&l, &["object", "module"]),
            aliased: kinds(&l, &["import_specifier", "namespace_import"]),
            namespaced: kinds(&l, &["namespace_import"]),
            f_alias: fields(&l, &["alias"]),
        },

        // rust — high confidence. Verified against the real grammar by building a scratch dumper (tree-sitter 0.
        Lang::Rust => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_item", DefKind::Function),
                    ("function_signature_item", DefKind::Function),
                    ("struct_item", DefKind::Class),
                    ("enum_item", DefKind::Class),
                    ("trait_item", DefKind::Interface),
                    // `type SymId = lasso::Spur;` names something people
                    // reference, and CodeGraph counts it too.
                    ("type_item", DefKind::Interface),
                    ("impl_item", DefKind::Class),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("call_expression", RefKind::Call),
                    ("macro_invocation", RefKind::Call),
                    ("struct_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["use_declaration"]),
            f_name: fields(&l, &["name", "path", "type"]),
            f_callee: fields(&l, &["function", "macro", "name"]),
            f_member: fields(&l, &["name", "field", "function"]),
            f_module: fields(&l, &["argument"]),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            // Module-level constants are API surface people reference by name;
            // they were missing because the probe's keyword filter hid
            // `const_item` and `static_item` from the vocabulary the spec was
            // written against. 72 of them in this project alone.
            var_defs: kinds(&l, &["const_item", "static_item", "enum_variant", "field_declaration"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier", "type_identifier", "field_identifier"]),
            heritage: kinds(&l, &["trait_bounds"]),
            dotted: kinds(
                &l,
                &[
                    "scoped_identifier",
                    "scoped_type_identifier",
                    "field_expression",
                ],
            ),
            f_object: fields(&l, &["value", "path"]),
            aliased: kinds(&l, &["use_as_clause"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["alias"]),
        },
        // go — high confidence. Verified by dumping real trees with tree-sitter-go 0.
        Lang::Go => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_declaration", DefKind::Function),
                    ("method_declaration", DefKind::Method),
                    ("method_elem", DefKind::Method),
                    ("type_spec", DefKind::Class),
                    ("type_alias", DefKind::Interface),
                ],
            ),
            refs: tagged(&l, &[("call_expression", RefKind::Call)]),
            imports: kinds(&l, &["import_spec"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["function"]),
            f_member: fields(&l, &["field", "name"]),
            f_module: fields(&l, &["path"]),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(&l, &["const_spec", "var_spec"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier", "type_identifier"]),
            heritage: Vec::new(),
            dotted: kinds(&l, &["selector_expression", "qualified_type"]),
            f_object: fields(&l, &["operand", "package"]),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // java — high confidence. Verified by parsing sample Java and re-running extract.
        Lang::Java => Spec {
            defs: tagged(
                &l,
                &[
                    ("class_declaration", DefKind::Class),
                    ("interface_declaration", DefKind::Interface),
                    ("enum_declaration", DefKind::Class),
                    ("method_declaration", DefKind::Method),
                    ("constructor_declaration", DefKind::Method),
                    ("compact_constructor_declaration", DefKind::Method),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("method_invocation", RefKind::Call),
                    ("object_creation_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["import_declaration"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["name", "type"]),
            f_member: fields(&l, &["field", "name"]),
            f_module: Vec::new(),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(&l, &["variable_declarator", "enum_constant"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier", "type_identifier"]),
            heritage: kinds(
                &l,
                &["superclass", "super_interfaces", "extends_interfaces"],
            ),
            dotted: kinds(
                &l,
                &[
                    "field_access",
                    "scoped_identifier",
                    "scoped_type_identifier",
                ],
            ),
            f_object: fields(&l, &["object", "scope"]),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // c — medium confidence. Verified against the real tree (scratch tree-sitter-c dumper) and by building leangraph with three candidate C specs and indexing tree-sitter's own C .
        Lang::C => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_definition", DefKind::Function),
                    ("preproc_function_def", DefKind::Function),
                    ("struct_specifier", DefKind::Class),
                    ("enum_specifier", DefKind::Class),
                ],
            ),
            refs: tagged(&l, &[("call_expression", RefKind::Call)]),
            imports: kinds(&l, &["preproc_include"]),
            f_name: fields(&l, &["name", "declarator"]),
            f_callee: fields(&l, &["function"]),
            f_member: fields(&l, &["field"]),
            f_module: fields(&l, &["path"]),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(&l, &["init_declarator", "enumerator", "field_declaration"]),
            f_var_name: fields(&l, &["declarator", "name"]),
            idents: kinds(&l, &["identifier", "type_identifier", "field_identifier"]),
            heritage: Vec::new(),
            dotted: kinds(&l, &["field_expression"]),
            f_object: fields(&l, &["argument"]),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // cpp — high confidence. Definitions: C++ hangs the name off a declarator chain, not a `name` field.
        Lang::Cpp => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_definition", DefKind::Function),
                    ("class_specifier", DefKind::Class),
                    ("struct_specifier", DefKind::Class),
                    ("enum_specifier", DefKind::Class),
                    ("alias_declaration", DefKind::Interface),
                    ("preproc_function_def", DefKind::Function),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("call_expression", RefKind::Call),
                    ("new_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["preproc_include"]),
            f_name: fields(&l, &["name", "declarator"]),
            f_callee: fields(&l, &["function", "type"]),
            f_member: fields(&l, &["field", "name"]),
            f_module: fields(&l, &["path"]),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(&l, &["enumerator"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(
                &l,
                &["identifier", "type_identifier", "namespace_identifier"],
            ),
            heritage: kinds(&l, &["base_class_clause"]),
            dotted: kinds(&l, &["field_expression", "qualified_identifier"]),
            f_object: fields(&l, &["argument", "scope"]),
            aliased: kinds(&l, &["namespace_alias_definition"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["name"]),
        },
        // csharp — high confidence. Verified by parsing C# samples with tree-sitter-c-sharp 0.
        Lang::CSharp => Spec {
            defs: tagged(
                &l,
                &[
                    ("class_declaration", DefKind::Class),
                    ("record_declaration", DefKind::Class),
                    ("struct_declaration", DefKind::Class),
                    ("enum_declaration", DefKind::Class),
                    ("interface_declaration", DefKind::Interface),
                    ("delegate_declaration", DefKind::Interface),
                    ("method_declaration", DefKind::Method),
                    ("constructor_declaration", DefKind::Method),
                    ("destructor_declaration", DefKind::Method),
                    ("local_function_statement", DefKind::Function),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("invocation_expression", RefKind::Call),
                    ("object_creation_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["using_directive"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["function", "type"]),
            f_member: fields(&l, &["name"]),
            f_module: Vec::new(),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(
                &l,
                &[
                    "variable_declarator",
                    "property_declaration",
                    "enum_member_declaration",
                ],
            ),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["identifier"]),
            heritage: kinds(&l, &["base_list"]),
            dotted: kinds(
                &l,
                &[
                    "member_access_expression",
                    "qualified_name",
                    "member_binding_expression",
                    "alias_qualified_name",
                ],
            ),
            f_object: fields(&l, &["expression", "qualifier"]),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // ruby — high confidence. Non-obvious choices, all checked against a real parse and a real extraction run.
        Lang::Ruby => Spec {
            defs: tagged(
                &l,
                &[
                    ("method", DefKind::Function),
                    ("singleton_method", DefKind::Method),
                    ("class", DefKind::Class),
                    ("module", DefKind::Class),
                    ("alias", DefKind::Function),
                ],
            ),
            refs: tagged(&l, &[("call", RefKind::Call)]),
            imports: Vec::new(),
            f_name: fields(&l, &["name", "left"]),
            f_callee: fields(&l, &["method"]),
            f_member: fields(&l, &["name"]),
            f_module: Vec::new(),
            cond_defs: kinds(&l, &["assignment"]),
            f_value: fields(&l, &["right"]),
            fn_values: kinds(&l, &["lambda"]),
            var_defs: kinds(&l, &["assignment"]),
            f_var_name: fields(&l, &["left"]),
            idents: kinds(&l, &["identifier", "constant"]),
            heritage: kinds(&l, &["superclass"]),
            dotted: kinds(&l, &["scope_resolution"]),
            f_object: fields(&l, &["scope"]),
            aliased: kinds(&l, &["alias"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["alias"]),
        },
        // php — medium confidence. Verified by parsing a feature-heavy sample with tree-sitter-php 0.
        Lang::Php => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_definition", DefKind::Function),
                    ("method_declaration", DefKind::Method),
                    ("class_declaration", DefKind::Class),
                    ("anonymous_class", DefKind::Class),
                    ("enum_declaration", DefKind::Class),
                    ("interface_declaration", DefKind::Interface),
                    ("trait_declaration", DefKind::Interface),
                    ("enum_case", DefKind::Variable),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("function_call_expression", RefKind::Call),
                    ("member_call_expression", RefKind::Call),
                    ("scoped_call_expression", RefKind::Call),
                    ("nullsafe_member_call_expression", RefKind::Call),
                    ("object_creation_expression", RefKind::New),
                ],
            ),
            imports: kinds(&l, &["namespace_use_declaration"]),
            f_name: fields(&l, &["name", "left"]),
            f_callee: fields(&l, &["function", "name"]),
            f_member: fields(&l, &["name"]),
            f_module: Vec::new(),
            cond_defs: kinds(&l, &["assignment_expression"]),
            f_value: fields(&l, &["right"]),
            fn_values: kinds(&l, &["anonymous_function", "arrow_function"]),
            var_defs: kinds(&l, &["property_element"]),
            f_var_name: fields(&l, &["name"]),
            idents: kinds(&l, &["name"]),
            heritage: kinds(&l, &["base_clause", "class_interface_clause"]),
            dotted: kinds(
                &l,
                &[
                    "member_access_expression",
                    "nullsafe_member_access_expression",
                    "scoped_property_access_expression",
                    "qualified_name",
                ],
            ),
            f_object: fields(&l, &["object", "scope", "prefix"]),
            aliased: kinds(&l, &["namespace_use_clause"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["alias"]),
        },
        // kotlin — high confidence. The decisive fact: tree-sitter-kotlin-ng has only EIGHT fields in the entire grammar — argument, condition, label, left, name, operator, right, type —.
        Lang::Kotlin => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_declaration", DefKind::Function),
                    ("class_declaration", DefKind::Class),
                    ("object_declaration", DefKind::Class),
                    ("companion_object", DefKind::Class),
                    ("type_alias", DefKind::Interface),
                ],
            ),
            refs: tagged(&l, &[("call_expression", RefKind::Call)]),
            imports: kinds(&l, &["import"]),
            f_name: fields(&l, &["name", "type"]),
            f_callee: Vec::new(),
            f_member: Vec::new(),
            f_module: Vec::new(),
            cond_defs: Vec::new(),
            f_value: Vec::new(),
            fn_values: Vec::new(),
            var_defs: kinds(
                &l,
                &["property_declaration", "class_parameter", "enum_entry"],
            ),
            f_var_name: Vec::new(),
            idents: kinds(&l, &["identifier"]),
            heritage: kinds(&l, &["delegation_specifiers"]),
            dotted: kinds(&l, &["navigation_expression", "qualified_identifier"]),
            f_object: Vec::new(),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // swift — medium confidence. Verified against tree-sitter-swift 0.
        Lang::Swift => Spec {
            defs: tagged(
                &l,
                &[
                    ("function_declaration", DefKind::Function),
                    ("class_declaration", DefKind::Class),
                    ("protocol_declaration", DefKind::Interface),
                    ("protocol_function_declaration", DefKind::Method),
                    ("typealias_declaration", DefKind::Interface),
                    ("enum_entry", DefKind::Variable),
                ],
            ),
            refs: tagged(
                &l,
                &[
                    ("navigation_expression", RefKind::Call),
                    ("constructor_expression", RefKind::New),
                    ("call_expression", RefKind::Call),
                ],
            ),
            imports: kinds(&l, &["import_declaration"]),
            f_name: fields(&l, &["name"]),
            f_callee: fields(&l, &["constructed_type", "suffix"]),
            f_member: fields(&l, &["suffix"]),
            f_module: Vec::new(),
            cond_defs: kinds(&l, &["property_declaration"]),
            f_value: fields(&l, &["value"]),
            fn_values: kinds(&l, &["lambda_literal"]),
            var_defs: kinds(&l, &["pattern"]),
            f_var_name: fields(&l, &["bound_identifier"]),
            idents: kinds(&l, &["simple_identifier", "type_identifier"]),
            heritage: kinds(&l, &["inheritance_specifier"]),
            dotted: kinds(&l, &["navigation_expression"]),
            f_object: fields(&l, &["target"]),
            aliased: Vec::new(),
            namespaced: Vec::new(),
            f_alias: Vec::new(),
        },
        // scala — high confidence. Verified by parsing Scala samples with tree-sitter-scala 0.
        Lang::Scala => Spec {
            defs: tagged(
                &l,
                &[
                    ("class_definition", DefKind::Class),
                    ("object_definition", DefKind::Class),
                    ("package_object", DefKind::Class),
                    ("enum_definition", DefKind::Class),
                    ("full_enum_case", DefKind::Class),
                    ("trait_definition", DefKind::Interface),
                    ("type_definition", DefKind::Interface),
                    ("function_definition", DefKind::Function),
                    ("function_declaration", DefKind::Function),
                    ("given_definition", DefKind::Variable),
                    ("simple_enum_case", DefKind::Variable),
                ],
            ),
            refs: tagged(&l, &[("call_expression", RefKind::Call)]),
            imports: kinds(&l, &["import_declaration", "export_declaration"]),
            f_name: fields(&l, &["name", "pattern"]),
            f_callee: fields(&l, &["function"]),
            f_member: fields(&l, &["field", "function"]),
            f_module: fields(&l, &["path"]),
            cond_defs: kinds(&l, &["val_definition", "var_definition"]),
            f_value: fields(&l, &["value"]),
            fn_values: kinds(&l, &["lambda_expression"]),
            var_defs: kinds(
                &l,
                &[
                    "val_definition",
                    "var_definition",
                    "val_declaration",
                    "var_declaration",
                ],
            ),
            f_var_name: fields(&l, &["pattern", "name"]),
            idents: kinds(&l, &["identifier", "type_identifier"]),
            heritage: kinds(&l, &["extends_clause", "derives_clause", "uses_clause"]),
            dotted: kinds(
                &l,
                &[
                    "field_expression",
                    "stable_identifier",
                    "stable_type_identifier",
                ],
            ),
            f_object: fields(&l, &["value"]),
            aliased: kinds(&l, &["arrow_renamed_identifier", "as_renamed_identifier"]),
            namespaced: Vec::new(),
            f_alias: fields(&l, &["alias"]),
        },
    };
    // `kinds` drops names the grammar does not know, so a typo here produces an
    // empty list and a feature that silently does nothing. Fail at startup
    // instead, where it is one line to find.
    // What this guards is a spec that *names* alias nodes but typed one wrong,
    // where `kinds()` drops it and the feature quietly does nothing. Go is
    // legitimately empty: it puts the local name and the module path on the same
    // `import_spec`, so there is no separate alias node to name.
    debug_assert!(
        spec.aliased.is_empty() || !spec.f_alias.is_empty(),
        "{:?}: alias nodes named but no alias field resolved",
        lang
    );
    spec
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
        // The spec already says which node kinds name things in this language;
        // a hardcoded list here contradicted it. Ruby spells a constant
        // `constant`, so every module-level `ANSI_COLORS` was rejected by a
        // whitelist written for Python and TypeScript — 13 of 37 symbols in one
        // small library.
        //
        // `property_identifier` stays as an addition rather than a member of
        // `idents`: TypeScript needs it to name a class field, and putting it in
        // `idents` would turn every property *access* into a reference.
        (self.is_ident(n.kind_id()) || n.kind() == "property_identifier").then_some(n)
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

    /// The identifier naming a dotted base class.
    ///
    /// `a.B` names `B`. But `BaseManager.from_queryset(QuerySet)` is a factory:
    /// the base is whatever it returns, which is unknowable, and the most
    /// informative thing in the expression is the class the factory hangs off.
    /// Naming `from_queryset` — a method — is the same defect this rule was
    /// written to remove, one level along.
    pub fn base_name_node<'t>(&self, dotted: &Node<'t>, is_callee: bool) -> Option<Node<'t>> {
        if is_callee {
            let obj = first_field(dotted, &self.f_object)?;
            return if self.is_dotted(obj.kind_id()) {
                first_field(&obj, &self.f_member)
            } else if self.is_ident(obj.kind_id()) {
                Some(obj)
            } else {
                None
            };
        }
        first_field(dotted, &self.f_member)
    }

    /// Is this node the thing being called by its parent?
    pub fn is_callee_of_parent(&self, node: &Node) -> bool {
        node.parent()
            .and_then(|p| first_field(&p, &self.f_callee))
            .is_some_and(|c| c.id() == node.id())
    }

    #[inline]
    pub fn is_import(&self, k: u16) -> bool {
        self.imports.contains(&k)
    }

    /// The identifier node naming a definition.
    #[inline]
    pub fn def_name_node<'t>(&self, def: &Node<'t>) -> Option<Node<'t>> {
        let mut n = first_field(def, &self.f_name)?;
        // C nests the name: a `function_definition`'s `declarator` is a
        // `function_declarator`, whose own `declarator` is finally the
        // identifier. Taking the first hop names the function `square(int v)`
        // — the whole signature — and mapping the inner node as a definition
        // too produces a second, duplicate node for every function.
        //
        // So descend while the field keeps pointing at something that is not an
        // identifier. Bounded, because a grammar with a cycle here would
        // otherwise hang the extractor.
        for _ in 0..4 {
            if self.is_ident(n.kind_id()) {
                break;
            }
            match first_field(&n, &self.f_name) {
                Some(inner) if inner.id() != n.id() => n = inner,
                _ => break,
            }
        }
        Some(n)
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
        // Kotlin and Swift give the callee no field name at all — `step()` is a
        // `call_expression` whose first named child is the identifier. Falling
        // back to that child is what makes those grammars produce call edges;
        // without it the field lookup misses and the language extracts
        // definitions but no calls, which looks like it works.
        let callee = first_field(call, &self.f_callee).or_else(|| call.named_child(0))?;
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

    #[inline]
    pub fn is_aliased(&self, k: u16) -> bool {
        self.aliased.contains(&k)
    }

    /// `(local, original)` for an aliasing import node.
    ///
    /// `import numpy as np` gives `(np, numpy)`; `from m import a as b` gives
    /// `(b, a)`. The resolver treats both the same, because either way a local
    /// name has to be pointed at whatever the original names.
    ///
    /// `import * as ns from "m"` names no symbol, so the module path stands in
    /// as the original — which is what a receiver check needs anyway.
    pub fn alias_pair<'t>(&self, node: &Node<'t>) -> Option<(Node<'t>, Node<'t>)> {
        // A rename: `as` is present and names both ends.
        if let (Some(l), Some(o)) = (
            first_field(node, &self.f_alias),
            first_field(node, &self.f_name),
        ) {
            return Some((l, o));
        }
        // A whole-module binding: `import * as ns from "m"` names no symbol, so
        // the module path stands in as the original.
        if !self.namespaced.contains(&node.kind_id()) {
            // Anything else reaching here is a plain `import { a }` specifier.
            // It introduces no second name, and treating it as one would both
            // invent a binding and swallow a real reference: 2,926 edges in
            // excalidraw when this branch was not guarded.
            return None;
        }
        let l = node.named_child(0)?;
        let mut cur = node.parent();
        for _ in 0..4 {
            let n = cur?;
            if let Some(m) = first_field(&n, &self.f_module) {
                return Some((l, m));
            }
            cur = n.parent();
        }
        None
    }

    /// The receiver expression of a call, if it has one.
    pub fn recv_node<'t>(&self, call: &Node<'t>) -> Option<Node<'t>> {
        let callee = first_field(call, &self.f_callee)?;
        if !self.is_dotted(callee.kind_id()) {
            return None;
        }
        first_field(&callee, &self.f_object)
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Every name every spec asks for must exist in its grammar.
    ///
    /// `kinds()` and `fields()` drop a name the grammar does not know, so a spec
    /// with a typo — or a spec that was right until a grammar upgrade renamed a
    /// node — still compiles, still runs, and quietly stops finding whatever
    /// that node was for. Nothing else in the pipeline notices: the language
    /// keeps producing a graph, just a smaller one.
    ///
    /// This is the check that makes a `cargo update` on a tree-sitter crate
    /// safe to do.
    #[test]
    fn every_spec_name_exists_in_its_grammar() {
        for lang in ALL_LANGS {
            let _ = spec_for(lang);
        }
        let misses = unresolved_names();
        assert!(
            misses.is_empty(),
            "{} name(s) silently dropped: {}",
            misses.len(),
            misses
                .iter()
                .map(|(l, w)| format!("{l}/{w}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    /// A language that has a spec must extract something. An empty `defs` list
    /// is the shape a half-written spec takes, and it is invisible at runtime.
    #[test]
    fn every_language_defines_and_calls_something() {
        for lang in ALL_LANGS {
            let s = spec_for(lang);
            assert!(!s.defs.is_empty(), "{:?}: no definition kinds", lang);
            assert!(!s.refs.is_empty(), "{:?}: no reference kinds", lang);
            assert!(!s.idents.is_empty(), "{:?}: no identifier kinds", lang);
        }
    }
}
