//! A corpus in a test, so resolution can be asserted on directly.
//!
//! The engine had no unit tests at all. What guarded it was `bench/` against
//! three cloned repositories — which does catch regressions, but only the ones
//! large enough to move an aggregate, only when someone runs it, and never in
//! CI, where the corpora do not exist. Three separate regressions reached
//! `master` that way: a receiver-blind filter that deleted 198 real edges, a
//! `same_family` that returned false for eleven languages and silently disabled
//! tier 3, and an alias binding that removed 2,926 edges from one repository.
//! Every one of them is a handful of lines of source and one assertion.
//!
//! The obstacle was that extraction mmaps a real file, so a test needs files.
//! This writes them, runs the real pipeline — the same `extract_file` and the
//! same `resolve` the indexer calls, not a reimplementation — and hands back
//! the edges keyed by readable names.

use crate::core::{Edge, EdgeKind, FileUnit, Interner, NodeId, Provenance};
use crate::extract::extract_file;
use crate::idtable::IdTable;
use crate::lang::spec_for;
use crate::lang::Lang;
use crate::resolve::{resolve, Resolved};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use tree_sitter::Parser as TsParser;

/// A directory that removes itself. Tests run in parallel in one process, so
/// the name carries a counter as well as the pid.
pub struct TempTree(PathBuf);

impl TempTree {
    pub fn new(tag: &str) -> TempTree {
        static N: AtomicU32 = AtomicU32::new(0);
        let dir = std::env::temp_dir().join(format!(
            "leangraph-{tag}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp tree");
        TempTree(dir)
    }

    pub fn path(&self) -> &Path {
        &self.0
    }

    /// Write one file, creating parents. Returns its absolute path.
    pub fn write(&self, rel: &str, source: &str) -> PathBuf {
        let p = self.0.join(rel);
        if let Some(d) = p.parent() {
            std::fs::create_dir_all(d).expect("create parent");
        }
        std::fs::write(&p, source).expect("write file");
        p
    }
}

impl Drop for TempTree {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// One resolved edge, with both ends named the way a person would write them:
/// `"app.py:Flask.run"`, or `"app.py"` for the file node itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EdgeView {
    pub src: String,
    pub dst: String,
    pub kind: EdgeKind,
    pub conf: u8,
    pub prov: Provenance,
}

pub struct Corpus {
    tree: TempTree,
    pub root: PathBuf,
    pub resolved: Resolved,
    names: Vec<Option<String>>,
    pub interner: Interner,
    pub paths: Vec<PathBuf>,
    pub langs: Vec<Lang>,
    pub units: Vec<FileUnit>,
    ids: IdTable,
    pub metas: Vec<crate::cache::FileMeta>,
}

impl Corpus {
    /// Extract and resolve `files`, given as `(relative path, source)`.
    ///
    /// The language comes from the extension, exactly as the indexer decides it.
    pub fn build(files: &[(&str, &str)]) -> Corpus {
        Corpus::build_tagged("corpus", files)
    }

    pub fn build_tagged(tag: &str, files: &[(&str, &str)]) -> Corpus {
        let tree = TempTree::new(tag);
        let root = tree.path().to_path_buf();

        let mut paths = Vec::new();
        let mut langs = Vec::new();
        for (rel, src) in files {
            let p = tree.write(rel, src);
            let ext = Path::new(rel).extension().unwrap_or_default();
            let lang = Lang::from_ext(&ext.to_string_lossy())
                .unwrap_or_else(|| panic!("no language for {rel}"));
            paths.push(p);
            langs.push(lang);
        }

        let interner = Interner::new();
        let mut units: Vec<FileUnit> = Vec::new();
        let mut metas = Vec::new();
        for (i, (p, lang)) in paths.iter().zip(&langs).enumerate() {
            let spec = spec_for(*lang);
            let mut parser = TsParser::new();
            parser
                .set_language(&lang.ts_language())
                .expect("grammar/ABI mismatch");
            let (unit, _, meta) = extract_file(p, i as u32, *lang, &spec, &mut parser, &interner)
                .unwrap_or_else(|| panic!("extraction failed for {}", p.display()));
            units.push(unit);
            metas.push(meta);
        }

        let mut ids = IdTable::default();
        let resolved = resolve(&units, &paths, &langs, &root, &interner, &mut ids);

        // NodeId -> "file.py:Outer.inner", built from the containment chain the
        // extractor already produced.
        let mut names: Vec<Option<String>> = vec![None; resolved.space.total as usize];
        for (f, unit) in units.iter().enumerate() {
            let rel = paths[f]
                .strip_prefix(&root)
                .unwrap_or(&paths[f])
                .to_string_lossy()
                .replace('\\', "/");
            let id = resolved.space.file_node(f as u32);
            if let Some(slot) = names.get_mut(id.0 as usize) {
                *slot = Some(rel.clone());
            }
            for (d, def) in unit.defs.iter().enumerate() {
                let mut chain = vec![interner.resolve(&def.name).to_string()];
                let mut parent = def.parent;
                for _ in 0..32 {
                    let Some(p) = unit.defs.get(parent as usize) else {
                        break;
                    };
                    chain.push(interner.resolve(&p.name).to_string());
                    if parent == p.parent {
                        break;
                    }
                    parent = p.parent;
                }
                chain.reverse();
                let id = resolved.space.def_node(f as u32, d as u32);
                if let Some(slot) = names.get_mut(id.0 as usize) {
                    *slot = Some(format!("{rel}:{}", chain.join(".")));
                }
            }
        }

        Corpus {
            tree,
            root,
            resolved,
            names,
            interner,
            paths,
            langs,
            units,
            ids,
            metas,
        }
    }

    /// Persist and reopen, so a test can assert on what a *query* sees rather
    /// than on the in-memory result. Everything a caller touches goes through
    /// this format, and the format is where a field can be written and never
    /// read back.
    pub fn graph(&self) -> crate::graph::Graph {
        let out = self.tree.path().join("graph.bin");
        let mut syms = vec![String::new(); self.interner.len()];
        for (k, v) in self.interner.iter() {
            let i = lasso::Key::into_usize(k);
            if i < syms.len() {
                syms[i] = v.to_string();
            }
        }
        let stamps: Vec<(u64, i64)> = self.metas.iter().map(|m| (m.size, m.mtime)).collect();
        crate::graph::write(
            &out,
            &self.resolved,
            &syms,
            &self.paths,
            &self.root,
            &self.ids.raw_keys(),
            &stamps,
        )
        .expect("writing the graph");
        crate::graph::Graph::open(&out).expect("reopening the graph")
    }

    /// The temp tree, so a test can touch a file after indexing it.
    pub fn tree(&self) -> &TempTree {
        &self.tree
    }

    pub fn name(&self, n: NodeId) -> &str {
        self.names
            .get(n.0 as usize)
            .and_then(|o| o.as_deref())
            .unwrap_or("<unknown>")
    }

    pub fn view(&self, e: &Edge) -> EdgeView {
        EdgeView {
            src: self.name(e.src).to_string(),
            dst: self.name(e.dst).to_string(),
            kind: e.kind,
            conf: e.conf,
            prov: e.prov,
        }
    }

    /// Every edge of one kind, as names.
    pub fn edges(&self, kind: EdgeKind) -> Vec<EdgeView> {
        self.resolved
            .edges
            .iter()
            .filter(|e| e.kind == kind)
            .map(|e| self.view(e))
            .collect()
    }

    pub fn calls(&self) -> Vec<EdgeView> {
        self.edges(EdgeKind::Calls)
    }

    /// The call edge between two symbols, matched on the tail of each name so a
    /// test can say `"Client.open"` without repeating the file.
    pub fn call(&self, src: &str, dst: &str) -> Option<EdgeView> {
        self.calls()
            .into_iter()
            .find(|e| ends_with_symbol(&e.src, src) && ends_with_symbol(&e.dst, dst))
    }

    pub fn calls_from(&self, src: &str) -> Vec<EdgeView> {
        self.calls()
            .into_iter()
            .filter(|e| ends_with_symbol(&e.src, src))
            .collect()
    }
}

/// `"app.py:Flask.run"` matches `"Flask.run"` and `"run"`, but not `"un"`.
fn ends_with_symbol(full: &str, want: &str) -> bool {
    let sym = full.rsplit(':').next().unwrap_or(full);
    sym == want
        || sym
            .strip_suffix(want)
            .is_some_and(|head| head.ends_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_temp_tree_removes_itself() {
        let path = {
            let t = TempTree::new("selftest");
            t.write("a/b.py", "x = 1");
            assert!(t.path().join("a/b.py").is_file());
            t.path().to_path_buf()
        };
        assert!(!path.exists(), "the directory must not outlive the value");
    }

    #[test]
    fn symbol_matching_is_on_boundaries_not_substrings() {
        assert!(ends_with_symbol("app.py:Flask.run", "run"));
        assert!(ends_with_symbol("app.py:Flask.run", "Flask.run"));
        assert!(!ends_with_symbol("app.py:Flask.run", "un"));
        assert!(!ends_with_symbol("app.py:Flask.rerun", "run"));
    }
}
