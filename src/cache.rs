//! Extraction cache — what makes sync cheap.
//!
//! Parsing is 75–82% of our CPU. Everything downstream of it (resolve, CSR
//! build, write) costs ~50 ms on django and is cheap to redo; re-parsing 3,000
//! unchanged files to find out that nothing changed is the expensive part.
//!
//! So we persist the extracted `FileUnit`s beside the graph, keyed by content
//! hash. A sync stats every file, hashes only the suspects, re-extracts only
//! what actually changed, and reuses the rest verbatim.
//!
//! Symbol ids are cache-local. On load every string is re-interned into the
//! live interner and the ids are remapped, so a cache written by one run is
//! valid for the next even though the interner is rebuilt from scratch.

use crate::core::{Def, DefKind, FileUnit, Import, Interner, Recv, Ref, RefKind, Span, SymId};
use crate::lang::Lang;
use anyhow::{bail, Context, Result};
use bytemuck::{Pod, Zeroable};
use lasso::Key;
use memmap2::Mmap;
use rustc_hash::FxHashMap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: [u8; 8] = *b"LGRPHC\x00\x06";
const N_SECTIONS: usize = 12;

const S_FILES: usize = 0;
const S_DEFS: usize = 1;
const S_REFS: usize = 2;
const S_IMPORTS: usize = 3;
const S_SYM_OFF: usize = 4;
const S_SYM_BLOB: usize = 5;
const S_PATH_OFF: usize = 6;
const S_PATH_BLOB: usize = 7;
const S_HEAD: usize = 8;
const S_ALIASES: usize = 9;
/// Paths the base still holds but that no longer exist. Only a delta writes
/// these; without them a deleted file would keep coming back from the base, and
/// the "nothing changed" check that skips rewriting the graph would never fire
/// again.
const S_GONE_OFF: usize = 10;
const S_GONE_BLOB: usize = 11;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Header {
    magic: [u8; 8],
    n_files: u32,
    _pad: u32,
    off: [u64; N_SECTIONS],
    len: [u64; N_SECTIONS],
}

/// Identity of a file plus where its extracted data lives.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CFile {
    hash: [u8; 32],
    size: u64,
    mtime: i64,
    def_at: u32,
    def_n: u32,
    ref_at: u32,
    ref_n: u32,
    imp_at: u32,
    imp_n: u32,
    lang: u32,
    had_error: u32,
    alias_at: u32,
    alias_n: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CDef {
    name: u32,
    kind: u32,
    span_s: u32,
    span_e: u32,
    ns_s: u32,
    ns_e: u32,
    parent: u32,
    _pad: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CRef {
    name: u32,
    kind: u32,
    span_s: u32,
    span_e: u32,
    scope: u32,
    /// `Recv`, in the space the padding already occupied. The magic is bumped
    /// anyway: a cache written before the receiver existed holds refs whose
    /// meaning has changed, not merely a missing field.
    recv: u32,
    /// The receiver's interned name; `u32::MAX` for none.
    recv_name: u32,
    _pad: u32,
}

/// A local name and what it was imported as. Two interned ids, nothing else —
/// the resolver reconstructs the binding from the module tables it already has.
#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CAlias {
    local: u32,
    original: u32,
}

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable, Default)]
struct CImport {
    module: u32,
    span_s: u32,
    span_e: u32,
    _pad: u32,
}

/// Identity of a file on disk, cheap end first.
#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub struct FileMeta {
    pub hash: [u8; 32],
    pub size: u64,
    pub mtime: i64,
}

pub struct Entry {
    pub path: PathBuf,
    pub meta: FileMeta,
    pub unit: FileUnit,
}

/// Commit the cache was built at, if any. Lets a sync ask git what changed
/// instead of walking and stat-ing the whole tree.
pub fn read_head(path: &Path) -> Option<String> {
    // The delta is written after the base and carries the commit the cache is
    // actually current at. Reading the base's would send a sync back over work
    // the delta already records.
    head_of(&delta_path(path)).or_else(|| head_of(path))
}

fn head_of(path: &Path) -> Option<String> {
    let buf = std::fs::read(path).ok()?;
    if buf.len() < std::mem::size_of::<Header>() || buf[..8] != MAGIC {
        return None;
    }
    let h: Header = *bytemuck::from_bytes(&buf[..std::mem::size_of::<Header>()]);
    let (o, l) = (h.off[S_HEAD] as usize, h.len[S_HEAD] as usize);
    if l == 0 {
        return None;
    }
    String::from_utf8(buf.get(o..o + l)?.to_vec()).ok()
}

fn kind_of_def(k: u32) -> DefKind {
    match k {
        1 => DefKind::Method,
        2 => DefKind::Class,
        3 => DefKind::Interface,
        4 => DefKind::Module,
        5 => DefKind::Variable,
        _ => DefKind::Function,
    }
}

fn kind_of_ref(k: u32) -> RefKind {
    match k {
        1 => RefKind::New,
        2 => RefKind::Extends,
        3 => RefKind::Read,
        _ => RefKind::Call,
    }
}

/// Position in `ALL_LANGS`, so adding a language does not mean remembering to
/// update a second list. The value is written to the cache and read by nothing
/// today, but it costs four bytes and a wrong one would be a silent mislabel.
fn lang_id(l: Lang) -> u32 {
    crate::lang::ALL_LANGS
        .iter()
        .position(|&x| x == l)
        .unwrap_or(0) as u32
}

fn pad8(v: &mut Vec<u8>) {
    while v.len() % 8 != 0 {
        v.push(0);
    }
}

fn blob(items: impl Iterator<Item = String>) -> (Vec<u32>, Vec<u8>) {
    let mut off = vec![0u32];
    let mut buf = Vec::new();
    for s in items {
        buf.extend_from_slice(s.as_bytes());
        off.push(buf.len() as u32);
    }
    (off, buf)
}

/// Where the appended part of the cache lives, beside the base.
fn delta_path(base: &Path) -> PathBuf {
    base.with_extension("delta")
}

/// Rewriting the whole cache costs more than everything else in a sync put
/// together — 33 MB on django, 56 of the 84 ms that "persist" reports, to record
/// that one file changed. Past this share of the tree it is cheaper to fold the
/// delta back in and start again than to keep reading two files.
const COMPACT_ABOVE: f64 = 0.25;

/// `(relative path, content hash)` for what the base already holds.
///
/// Read straight from the mmap without interning anything: deciding what to
/// write must not cost what writing it would have.
fn base_index(path: &Path) -> Option<FxHashMap<String, [u8; 32]>> {
    let f = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&f).ok()? };
    if mmap.len() < std::mem::size_of::<Header>() || mmap[..8] != MAGIC {
        return None;
    }
    let h: Header = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<Header>()]);
    let sec = |id: usize| -> Option<&[u8]> {
        let (o, l) = (h.off[id] as usize, h.len[id] as usize);
        mmap.get(o..o + l)
    };
    let files: &[CFile] = bytemuck::cast_slice(sec(S_FILES)?);
    let path_off: &[u32] = bytemuck::cast_slice(sec(S_PATH_OFF)?);
    let path_blob = sec(S_PATH_BLOB)?;
    let mut out = FxHashMap::default();
    for (i, cf) in files.iter().enumerate() {
        let (a, b) = (*path_off.get(i)? as usize, *path_off.get(i + 1)? as usize);
        let rel = std::str::from_utf8(path_blob.get(a..b)?).ok()?;
        out.insert(rel.to_string(), cf.hash);
    }
    Some(out)
}

fn rel_of(p: &Path, root: &Path) -> String {
    p.strip_prefix(root)
        .unwrap_or(p)
        .to_string_lossy()
        .into_owned()
}

pub fn write(
    path: &Path,
    units: &[FileUnit],
    paths: &[PathBuf],
    langs: &[Lang],
    metas: &[FileMeta],
    interner: &Interner,
    root: &Path,
    head: &str,
    compact: bool,
) -> Result<()> {
    let delta = delta_path(path);
    let base = if compact { None } else { base_index(path) };
    let Some(base) = base else {
        // No base to append to.
        let all: Vec<usize> = (0..units.len()).collect();
        write_set(
            path,
            &all,
            units,
            paths,
            langs,
            metas,
            interner,
            root,
            head,
            &[],
        )?;
        let _ = std::fs::remove_file(&delta);
        return Ok(());
    };

    // What the base does not already hold, or holds differently.
    let mut changed: Vec<usize> = Vec::new();
    let mut live: Vec<String> = Vec::with_capacity(paths.len());
    for i in 0..units.len() {
        let rel = rel_of(&paths[i], root);
        match base.get(&rel) {
            Some(h) if *h == metas[i].hash => {}
            _ => changed.push(i),
        }
        live.push(rel);
    }
    let live: rustc_hash::FxHashSet<&str> = live.iter().map(String::as_str).collect();
    let gone: Vec<String> = base
        .keys()
        .filter(|k| !live.contains(k.as_str()))
        .cloned()
        .collect();

    // Past the threshold the delta stops paying for itself.
    let churn = (changed.len() + gone.len()) as f64 / units.len().max(1) as f64;
    if churn > COMPACT_ABOVE {
        let all: Vec<usize> = (0..units.len()).collect();
        write_set(
            path,
            &all,
            units,
            paths,
            langs,
            metas,
            interner,
            root,
            head,
            &[],
        )?;
        let _ = std::fs::remove_file(&delta);
        return Ok(());
    }

    write_set(
        &delta, &changed, units, paths, langs, metas, interner, root, head, &gone,
    )
}

/// Serialise exactly the files named by `sel`.
///
/// The symbol table holds only the strings those files reference, remapped to
/// dense local ids. Writing the whole interner instead — which is what this did
/// — put every symbol in the repository into a delta that describes one file.
#[allow(clippy::too_many_arguments)]
fn write_set(
    path: &Path,
    sel: &[usize],
    units: &[FileUnit],
    paths: &[PathBuf],
    langs: &[Lang],
    metas: &[FileMeta],
    interner: &Interner,
    root: &Path,
    head: &str,
    gone: &[String],
) -> Result<()> {
    let mut files = Vec::with_capacity(sel.len());
    let mut defs = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();
    let mut aliases: Vec<CAlias> = Vec::new();

    let mut sym_idx: FxHashMap<u32, u32> = FxHashMap::default();
    let mut sym_strings: Vec<String> = Vec::new();
    macro_rules! sym {
        ($s:expr) => {{
            let id = Key::into_usize($s) as u32;
            match sym_idx.get(&id) {
                Some(&v) => v,
                None => {
                    let v = sym_strings.len() as u32;
                    sym_strings.push(interner.resolve(&$s).to_string());
                    sym_idx.insert(id, v);
                    v
                }
            }
        }};
    }

    for &i in sel {
        let u = &units[i];
        files.push(CFile {
            hash: metas[i].hash,
            size: metas[i].size,
            mtime: metas[i].mtime,
            def_at: defs.len() as u32,
            def_n: u.defs.len() as u32,
            ref_at: refs.len() as u32,
            ref_n: u.refs.len() as u32,
            imp_at: imports.len() as u32,
            imp_n: u.imports.len() as u32,
            alias_at: aliases.len() as u32,
            alias_n: u.aliases.len() as u32,
            lang: lang_id(langs[i]),
            had_error: u32::from(u.had_parse_error),
        });
        for d in &u.defs {
            defs.push(CDef {
                name: sym!(d.name),
                kind: d.kind as u32,
                span_s: d.span.start,
                span_e: d.span.end,
                ns_s: d.name_span.start,
                ns_e: d.name_span.end,
                parent: d.parent,
                _pad: 0,
            });
        }
        for r in &u.refs {
            refs.push(CRef {
                name: sym!(r.name),
                kind: r.kind as u32,
                span_s: r.span.start,
                span_e: r.span.end,
                scope: r.scope,
                recv: r.recv as u32,
                // The sentinel means "no receiver" and is not a symbol id.
                recv_name: match r.recv_name {
                    Some(s) => sym!(s),
                    None => u32::MAX,
                },
                _pad: 0,
            });
        }
        for m in &u.imports {
            imports.push(CImport {
                module: sym!(m.module),
                span_s: m.span.start,
                span_e: m.span.end,
                _pad: 0,
            });
        }
        for (l, o) in &u.aliases {
            aliases.push(CAlias {
                local: sym!(*l),
                original: sym!(*o),
            });
        }
    }

    let (sym_off, sym_blob) = blob(sym_strings.into_iter());
    let (path_off, path_blob) = blob(sel.iter().map(|&i| rel_of(&paths[i], root)));
    let (gone_off, gone_blob) = blob(gone.iter().cloned());

    let mut header = Header {
        magic: MAGIC,
        n_files: sel.len() as u32,
        _pad: 0,
        off: [0; N_SECTIONS],
        len: [0; N_SECTIONS],
    };
    let head_len = std::mem::size_of::<Header>();
    let mut body: Vec<u8> = Vec::with_capacity(8 * 1024 * 1024);

    macro_rules! section {
        ($id:expr, $data:expr) => {{
            let bytes: &[u8] = bytemuck::cast_slice($data);
            pad8(&mut body);
            header.off[$id] = (head_len + body.len()) as u64;
            header.len[$id] = bytes.len() as u64;
            body.extend_from_slice(bytes);
        }};
    }
    section!(S_FILES, &files[..]);
    section!(S_DEFS, &defs[..]);
    section!(S_REFS, &refs[..]);
    section!(S_IMPORTS, &imports[..]);
    section!(S_ALIASES, &aliases[..]);
    section!(S_SYM_OFF, &sym_off[..]);
    section!(S_SYM_BLOB, &sym_blob[..]);
    section!(S_PATH_OFF, &path_off[..]);
    section!(S_PATH_BLOB, &path_blob[..]);
    section!(S_HEAD, head.as_bytes());
    section!(S_GONE_OFF, &gone_off[..]);
    section!(S_GONE_BLOB, &gone_blob[..]);

    if let Some(d) = path.parent() {
        std::fs::create_dir_all(d).ok();
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp)?;
        f.write_all(bytemuck::bytes_of(&header))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

/// Everything the cache knows, base and delta folded together.
///
/// A delta entry replaces the base's entry for the same path, and a path the
/// delta lists as gone is dropped. Order is not part of the contract — the
/// caller keys this by path immediately — which is what lets the merge be a
/// map rather than a splice.
pub fn read(path: &Path, interner: &Interner, root: &Path) -> Result<Vec<Entry>> {
    let (mut entries, _) = read_one(path, interner, root)?;
    if let Ok((delta, gone)) = read_one(&delta_path(path), interner, root) {
        let replaced: rustc_hash::FxHashSet<PathBuf> =
            delta.iter().map(|e| e.path.clone()).collect();
        let gone: rustc_hash::FxHashSet<PathBuf> = gone.iter().map(|g| root.join(g)).collect();
        entries.retain(|e| !replaced.contains(&e.path) && !gone.contains(&e.path));
        entries.extend(delta);
    }
    Ok(entries)
}

/// One cache file, base or delta, with the paths it declares gone.
fn read_one(path: &Path, interner: &Interner, root: &Path) -> Result<(Vec<Entry>, Vec<String>)> {
    let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
    let mmap = unsafe { Mmap::map(&f)? };
    if mmap.len() < std::mem::size_of::<Header>() {
        bail!("truncated cache");
    }
    let h: Header = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<Header>()]);
    if h.magic != MAGIC {
        bail!("cache written by an incompatible version");
    }

    let sec = |id: usize| -> &[u8] {
        let (o, l) = (h.off[id] as usize, h.len[id] as usize);
        &mmap[o..o + l]
    };
    let files: &[CFile] = bytemuck::cast_slice(sec(S_FILES));
    let defs: &[CDef] = bytemuck::cast_slice(sec(S_DEFS));
    let refs: &[CRef] = bytemuck::cast_slice(sec(S_REFS));
    let imports: &[CImport] = bytemuck::cast_slice(sec(S_IMPORTS));
    let aliases: &[CAlias] = bytemuck::cast_slice(sec(S_ALIASES));
    let sym_off: &[u32] = bytemuck::cast_slice(sec(S_SYM_OFF));
    let sym_blob = sec(S_SYM_BLOB);
    let path_off: &[u32] = bytemuck::cast_slice(sec(S_PATH_OFF));
    let path_blob = sec(S_PATH_BLOB);

    // The interner is rebuilt every run, so cached ids mean nothing until they
    // are re-interned. One pass builds the whole remap.
    let mut remap: Vec<SymId> = Vec::with_capacity(sym_off.len().saturating_sub(1));
    for i in 0..sym_off.len().saturating_sub(1) {
        let (a, b) = (sym_off[i] as usize, sym_off[i + 1] as usize);
        let s = std::str::from_utf8(&sym_blob[a..b]).unwrap_or("");
        remap.push(interner.get_or_intern(s));
    }
    let map = |id: u32| -> SymId {
        remap
            .get(id as usize)
            .copied()
            .unwrap_or_else(|| interner.get_or_intern(""))
    };

    let mut out = Vec::with_capacity(files.len());
    for (i, cf) in files.iter().enumerate() {
        let (a, b) = (path_off[i] as usize, path_off[i + 1] as usize);
        let rel = std::str::from_utf8(&path_blob[a..b]).unwrap_or("");
        let unit = FileUnit {
            aliases: aliases[cf.alias_at as usize..(cf.alias_at + cf.alias_n) as usize]
                .iter()
                .map(|a| (map(a.local), map(a.original)))
                .collect(),
            file: i as u32,
            had_parse_error: cf.had_error != 0,
            defs: defs[cf.def_at as usize..(cf.def_at + cf.def_n) as usize]
                .iter()
                .map(|d| Def {
                    name: map(d.name),
                    kind: kind_of_def(d.kind),
                    span: Span {
                        start: d.span_s,
                        end: d.span_e,
                    },
                    name_span: Span {
                        start: d.ns_s,
                        end: d.ns_e,
                    },
                    parent: d.parent,
                })
                .collect(),
            refs: refs[cf.ref_at as usize..(cf.ref_at + cf.ref_n) as usize]
                .iter()
                .map(|r| Ref {
                    name: map(r.name),
                    kind: kind_of_ref(r.kind),
                    span: Span {
                        start: r.span_s,
                        end: r.span_e,
                    },
                    scope: r.scope,
                    recv: Recv::from_u8(r.recv as u8),
                    recv_name: (r.recv_name != u32::MAX).then(|| map(r.recv_name)),
                })
                .collect(),
            imports: imports[cf.imp_at as usize..(cf.imp_at + cf.imp_n) as usize]
                .iter()
                .map(|m| Import {
                    module: map(m.module),
                    span: Span {
                        start: m.span_s,
                        end: m.span_e,
                    },
                })
                .collect(),
        };
        out.push(Entry {
            path: root.join(rel),
            meta: FileMeta {
                hash: cf.hash,
                size: cf.size,
                mtime: cf.mtime,
            },
            unit,
        });
    }

    let gone_off: &[u32] = bytemuck::cast_slice(sec(S_GONE_OFF));
    let gone_blob = sec(S_GONE_BLOB);
    let gone: Vec<String> = (0..gone_off.len().saturating_sub(1))
        .filter_map(|i| {
            let (a, b) = (gone_off[i] as usize, gone_off[i + 1] as usize);
            std::str::from_utf8(gone_blob.get(a..b)?)
                .ok()
                .map(String::from)
        })
        .collect();
    Ok((out, gone))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testkit::{Corpus, TempTree};

    fn corpus() -> Corpus {
        Corpus::build(&[
            (
                "app.py",
                "import helper\n\
                 from helper import assist as helper_fn\n\
                 \n\
                 class Client:\n\
                 \x20   def open(self, url):\n\
                 \x20       return self.send(url)\n\
                 \x20   def send(self, url):\n\
                 \x20       return helper_fn(url)\n",
            ),
            ("helper.py", "def assist(u):\n    return u\n"),
        ])
    }

    fn round_trip(c: &Corpus) -> (TempTree, Vec<Entry>) {
        let tree = TempTree::new("cache");
        let path = tree.path().join("cache.bin");
        write(
            &path,
            &c.units,
            &c.paths,
            &c.langs,
            &c.metas,
            &c.interner,
            &c.root,
            "abc123",
            true,
        )
        .expect("writing the cache");
        let back = read(&path, &c.interner, &c.root).expect("reading the cache");
        (tree, back)
    }

    #[test]
    fn everything_extracted_comes_back() {
        // The cache is what makes a sync cheap: an unchanged file is not
        // re-parsed, its extraction is read back from here. Anything this drops
        // is silently missing from the next incremental graph, and shows up as
        // edges that exist after a full index but not after a sync.
        let c = corpus();
        let (_tree, back) = round_trip(&c);

        assert_eq!(back.len(), c.units.len(), "every file must come back");
        for (entry, original) in back.iter().zip(&c.units) {
            assert_eq!(entry.unit.defs.len(), original.defs.len(), "defs");
            assert_eq!(entry.unit.refs.len(), original.refs.len(), "refs");
            assert_eq!(entry.unit.imports.len(), original.imports.len(), "imports");
            assert_eq!(entry.unit.aliases.len(), original.aliases.len(), "aliases");
        }
    }

    #[test]
    fn a_reference_keeps_the_receiver_it_was_extracted_with() {
        // `Recv` decides which tier a reference is allowed to reach. Losing it
        // across the cache would silently re-tier every call in an unchanged
        // file on the next sync — the graph would differ from a full index
        // without a single error.
        let c = corpus();
        let (_tree, back) = round_trip(&c);

        for (entry, original) in back.iter().zip(&c.units) {
            for (got, want) in entry.unit.refs.iter().zip(&original.refs) {
                assert_eq!(got.recv, want.recv, "receiver kind");
                assert_eq!(
                    got.recv_name.map(|s| c.interner.resolve(&s).to_string()),
                    want.recv_name.map(|s| c.interner.resolve(&s).to_string()),
                    "receiver name"
                );
                assert_eq!(got.kind, want.kind);
                assert_eq!(
                    c.interner.resolve(&got.name),
                    c.interner.resolve(&want.name)
                );
            }
        }
    }

    #[test]
    fn names_and_containment_survive_interning_twice() {
        // Symbol ids are assigned by a concurrent interner, so they are not
        // stable across runs. What has to survive is the *string* and the
        // parent chain, not the number.
        let c = corpus();
        let (_tree, back) = round_trip(&c);

        let app = back
            .iter()
            .find(|e| e.path.ends_with("app.py"))
            .expect("app.py in the cache");
        let names: Vec<&str> = app
            .unit
            .defs
            .iter()
            .map(|d| c.interner.resolve(&d.name))
            .collect();
        assert!(names.contains(&"Client"));
        assert!(names.contains(&"open"));
        assert!(names.contains(&"send"));

        let client = app
            .unit
            .defs
            .iter()
            .position(|d| c.interner.resolve(&d.name) == "Client")
            .unwrap() as u32;
        let open = app
            .unit
            .defs
            .iter()
            .find(|d| c.interner.resolve(&d.name) == "open")
            .unwrap();
        assert_eq!(open.parent, client, "open must still be inside Client");
    }

    #[test]
    fn an_alias_survives_the_round_trip() {
        let c = corpus();
        let (_tree, back) = round_trip(&c);
        let app = back
            .iter()
            .find(|e| e.path.ends_with("app.py"))
            .expect("app.py");
        let pairs: Vec<(String, String)> = app
            .unit
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
            pairs.contains(&("helper_fn".to_string(), "assist".to_string())),
            "the alias binding must persist: {pairs:?}"
        );
    }

    #[test]
    fn the_commit_it_was_built_at_is_recorded() {
        // A sync asks git what changed since this. Without it there is nothing
        // to diff against and the only safe move is walking the whole tree.
        let c = corpus();
        let tree = TempTree::new("cache-head");
        let path = tree.path().join("cache.bin");
        write(
            &path,
            &c.units,
            &c.paths,
            &c.langs,
            &c.metas,
            &c.interner,
            &c.root,
            "deadbeef",
            true,
        )
        .unwrap();
        assert_eq!(read_head(&path).as_deref(), Some("deadbeef"));
    }

    /// Write `sel` of the corpus as the cache's view of the tree.
    fn write_view(path: &Path, c: &Corpus, keep: &[usize], head: &str, compact: bool) {
        let units: Vec<_> = keep.iter().map(|&i| clone_unit(&c.units[i])).collect();
        let paths: Vec<_> = keep.iter().map(|&i| c.paths[i].clone()).collect();
        let langs: Vec<_> = keep.iter().map(|&i| c.langs[i]).collect();
        let metas: Vec<_> = keep.iter().map(|&i| c.metas[i]).collect();
        write(
            path,
            &units,
            &paths,
            &langs,
            &metas,
            &c.interner,
            &c.root,
            head,
            compact,
        )
        .expect("writing the cache");
    }

    fn clone_unit(u: &FileUnit) -> FileUnit {
        FileUnit {
            file: u.file,
            defs: u.defs.clone(),
            refs: u.refs.clone(),
            imports: u.imports.clone(),
            had_parse_error: u.had_parse_error,
            aliases: u.aliases.clone(),
        }
    }

    #[test]
    fn a_delta_holds_only_what_changed_and_still_reads_whole() {
        // Rewriting 33 MB to record that one file moved was the largest single
        // cost in a sync — more than resolution and the graph put together.
        let c = corpus();
        let tree = TempTree::new("cache-delta");
        let path = tree.path().join("cache.bin");

        write_view(&path, &c, &[0, 1], "base", true);
        let base_len = std::fs::metadata(&path).unwrap().len();
        assert!(
            !delta_path(&path).exists(),
            "a compacting write leaves none"
        );

        // The same tree again: nothing changed, so the delta carries no files.
        write_view(&path, &c, &[0, 1], "second", false);
        let delta_len = std::fs::metadata(delta_path(&path)).unwrap().len();
        assert!(
            delta_len * 4 < base_len,
            "a delta describing no change is {delta_len} against a base of {base_len}"
        );

        let back = read(&path, &c.interner, &c.root).expect("read");
        assert_eq!(back.len(), 2, "both files still come back");
        for (entry, original) in back.iter().zip(&c.units) {
            assert_eq!(entry.unit.defs.len(), original.defs.len());
            assert_eq!(entry.unit.refs.len(), original.refs.len());
        }
    }

    #[test]
    fn the_delta_carries_the_newer_commit() {
        let c = corpus();
        let tree = TempTree::new("cache-delta-head");
        let path = tree.path().join("cache.bin");
        write_view(&path, &c, &[0, 1], "old-sha", true);
        write_view(&path, &c, &[0, 1], "new-sha", false);
        assert_eq!(
            read_head(&path).as_deref(),
            Some("new-sha"),
            "reading the base's commit would send a sync back over work already done"
        );
    }

    #[test]
    fn a_file_deleted_after_the_base_does_not_come_back() {
        // The base still holds it. Without a tombstone it would be returned
        // forever — and the check that skips rewriting an unchanged graph looks
        // for exactly this leftover, so it would never fire again either.
        let c = corpus();
        let tree = TempTree::new("cache-gone");
        let path = tree.path().join("cache.bin");
        write_view(&path, &c, &[0, 1], "base", true);
        write_view(&path, &c, &[0], "after-delete", false);

        let back = read(&path, &c.interner, &c.root).expect("read");
        let left: Vec<_> = back.iter().map(|e| e.path.display().to_string()).collect();
        assert_eq!(back.len(), 1, "the deleted file must be gone: {left:?}");
        assert!(back[0].path.ends_with("app.py"));
    }

    #[test]
    fn enough_churn_folds_the_delta_back_into_the_base() {
        // Past a share of the tree, reading two files costs more than writing
        // one. Without this the delta grows without bound.
        let c = corpus();
        let tree = TempTree::new("cache-compact");
        let path = tree.path().join("cache.bin");
        write_view(&path, &c, &[0, 1], "base", true);
        // One of two files deleted is 50% churn, past the threshold.
        write_view(&path, &c, &[0], "churned", false);
        assert!(
            !delta_path(&path).exists(),
            "past the threshold the base is rewritten and the delta dropped"
        );
        assert_eq!(read(&path, &c.interner, &c.root).unwrap().len(), 1);
    }

    #[test]
    fn a_missing_or_corrupt_cache_is_an_error_not_a_panic() {
        let tree = TempTree::new("cache-bad");
        let interner = Interner::new();
        let missing = tree.path().join("nothing.bin");
        assert!(read(&missing, &interner, tree.path()).is_err());

        let garbage = tree.path().join("garbage.bin");
        std::fs::write(&garbage, b"not a cache file at all, just some bytes").unwrap();
        assert!(read(&garbage, &interner, tree.path()).is_err());
        assert!(read_head(&garbage).is_none());
    }
}
