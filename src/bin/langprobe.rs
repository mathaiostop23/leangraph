//! Ask each grammar for its own vocabulary, and check a spec against it.
//!
//! Every tree-sitter grammar names its nodes differently — a function is
//! `function_definition` in Python, `function_item` in Rust, `method_declaration`
//! in Java. `Spec` is built with `kinds()` and `fields()`, which **silently drop**
//! a name the grammar does not know, so a spec written from memory produces an
//! extractor that runs happily and extracts nothing.
//!
//!   langprobe                 dump every kind and field, per language
//!   langprobe <lang>          just one
//!   langprobe --check FILE    validate a JSON spec list against the grammars
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

fn vocab(lang: &Language) -> (Vec<String>, Vec<String>) {
    let mut kinds: Vec<String> = (0..lang.node_kind_count())
        .filter(|&i| lang.node_kind_is_named(i as u16))
        .filter_map(|i| lang.node_kind_for_id(i as u16).map(String::from))
        .collect();
    kinds.sort_unstable();
    kinds.dedup();
    let mut fields: Vec<String> = (1..=lang.field_count())
        .filter_map(|i| lang.field_name_for_id(i as u16).map(String::from))
        .collect();
    fields.sort_unstable();
    fields.dedup();
    (kinds, fields)
}

/// Names a spec claims, checked against what the grammar actually has.
///
/// This is the whole point of the tool: an unknown name is not an error
/// anywhere else in the pipeline, it is an empty list and a quiet gap.
fn check(path: &str) {
    let text = std::fs::read_to_string(path).expect("read spec file");
    let specs: Vec<serde_json::Value> = serde_json::from_str(&text).expect("parse spec file");
    let all = langs();
    let mut bad = 0usize;

    for spec in &specs {
        let name = spec["lang"].as_str().unwrap_or("?");
        let Some((_, lang)) = all.iter().find(|(n, _)| *n == name) else {
            println!("  {name}: no grammar");
            continue;
        };
        let (kinds, fields) = vocab(lang);
        let mut missing: Vec<String> = Vec::new();

        let mut want_kind = |v: &serde_json::Value, label: &str| {
            for k in v.as_array().into_iter().flatten() {
                let k = match k {
                    serde_json::Value::String(s) => s.clone(),
                    o => o["kind"].as_str().unwrap_or_default().to_string(),
                };
                if !k.is_empty() && !kinds.contains(&k) {
                    missing.push(format!("{label}:{k}"));
                }
            }
        };
        for f in ["defs", "refs", "imports", "cond_defs", "fn_values", "var_defs",
                  "idents", "heritage", "dotted", "aliased", "namespaced"] {
            want_kind(&spec[f], f);
        }
        for f in ["f_name", "f_callee", "f_member", "f_module", "f_value",
                  "f_var_name", "f_object", "f_alias"] {
            for k in spec[f].as_array().into_iter().flatten() {
                let k = k.as_str().unwrap_or_default().to_string();
                if !k.is_empty() && !fields.contains(&k) {
                    missing.push(format!("{f}:{k}"));
                }
            }
        }

        if missing.is_empty() {
            println!("  \x1b[32m✓\x1b[0m {name:<10} every name resolves");
        } else {
            bad += missing.len();
            println!("  \x1b[31m✗\x1b[0m {name:<10} {} unknown: {}", missing.len(), missing.join(" "));
        }
    }
    println!("\n  {bad} names would have been silently dropped");
    if bad > 0 {
        std::process::exit(1);
    }
}

/// Print the parse tree of a file, with field names.
///
/// The definitive answer to "what is this node called and which field holds the
/// thing I want" — the question a Spec is entirely made of.
fn tree(lang_name: &str, path: &str) {
    let all = langs();
    let (_, lang) = all.iter().find(|(n, _)| *n == lang_name).expect("unknown language");
    let src = std::fs::read_to_string(path).expect("read source");
    let mut parser = tree_sitter::Parser::new();
    parser.set_language(lang).expect("set language");
    let t = parser.parse(&src, None).expect("parse");
    let mut c = t.walk();
    let mut depth = 0usize;
    loop {
        let n = c.node();
        if n.is_named() {
            let field = c.field_name().map(|f| format!("{f}: ")).unwrap_or_default();
            let text = &src[n.start_byte()..n.end_byte().min(n.start_byte() + 42)];
            let text = text.replace('\n', " ");
            println!("{:indent$}{field}{} — {text}", "", n.kind(), indent = depth * 2);
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

fn main() {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("--tree") => {
            let l = args.next().expect("--tree needs a language");
            tree(&l, &args.next().expect("--tree needs a file"));
        }
        Some("--check") => check(&args.next().expect("--check needs a file")),
        only => {
            for (name, lang) in langs() {
                if only.is_some_and(|o| o != name) {
                    continue;
                }
                let (kinds, fields) = vocab(&lang);
                println!("\n########## {name} ##########");
                println!("KINDS: {}", kinds.join(" "));
                println!("FIELDS: {}", fields.join(" "));
            }
        }
    }
}
