//! Dump each grammar's vocabulary.
//!
//! Every tree-sitter grammar names its nodes differently — a function is
//! `function_definition` in Python, `function_item` in Rust, `method_declaration`
//! in Java. Writing a `Spec` from memory is guesswork that fails silently,
//! because `kinds()` drops names the grammar does not know.
//!
//! So ask the grammar. This prints the node kinds and field names each language
//! actually has, filtered to the shapes a Spec cares about.
use tree_sitter::Language;

fn langs() -> Vec<(&'static str, Language)> {
    vec![
        ("python", tree_sitter_python::LANGUAGE.into()),
        ("typescript", tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        ("rust", tree_sitter_rust::LANGUAGE.into()),
        ("go", tree_sitter_go::LANGUAGE.into()),
        ("java", tree_sitter_java::LANGUAGE.into()),
        ("c", tree_sitter_c::LANGUAGE.into()),
        ("cpp", tree_sitter_cpp::LANGUAGE.into()),
        ("csharp", tree_sitter_c_sharp::LANGUAGE.into()),
        ("ruby", tree_sitter_ruby::LANGUAGE.into()),
        ("php", tree_sitter_php::LANGUAGE_PHP.into()),
        ("kotlin", tree_sitter_kotlin_ng::LANGUAGE.into()),
        ("swift", tree_sitter_swift::LANGUAGE.into()),
        ("scala", tree_sitter_scala::LANGUAGE.into()),
    ]
}

const WANT: &[&str] = &[
    "function", "method", "class", "struct", "enum", "interface", "trait", "impl",
    "call", "invocation", "new", "object_creation", "import", "use", "include",
    "require", "package", "module", "field", "property", "variable", "declarator",
    "assignment", "identifier", "member", "attribute", "selector", "qualified",
    "heritage", "extends", "implements", "superclass", "base", "alias", "namespace",
];

fn main() {
    let only = std::env::args().nth(1);
    for (name, lang) in langs() {
        if only.as_deref().is_some_and(|o| o != name) {
            continue;
        }
        println!("\n########## {name} ##########");
        let mut kinds: Vec<&str> = Vec::new();
        for id in 0..lang.node_kind_count() {
            if let Some(k) = lang.node_kind_for_id(id as u16) {
                if lang.node_kind_is_named(id as u16) && WANT.iter().any(|w| k.contains(w)) {
                    kinds.push(k);
                }
            }
        }
        kinds.sort_unstable();
        kinds.dedup();
        println!("KINDS: {}", kinds.join(" "));

        let mut fields: Vec<&str> = Vec::new();
        for id in 1..=lang.field_count() {
            if let Some(f) = lang.field_name_for_id(id as u16) {
                fields.push(f);
            }
        }
        fields.sort_unstable();
        fields.dedup();
        println!("FIELDS: {}", fields.join(" "));
    }
}
