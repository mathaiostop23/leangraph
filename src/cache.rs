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

use crate::core::{
    Def, DefKind, FileUnit, Import, Interner, Recv, Ref, RefKind, Span, SymId,
};
use crate::lang::Lang;
use anyhow::{bail, Context, Result};
use bytemuck::{Pod, Zeroable};
use lasso::Key;
use memmap2::Mmap;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: [u8; 8] = *b"LGRPHC\x00\x04";
const N_SECTIONS: usize = 10;

const S_FILES: usize = 0;
const S_DEFS: usize = 1;
const S_REFS: usize = 2;
const S_IMPORTS: usize = 3;
const S_SYM_OFF: usize = 4;
const S_SYM_BLOB: usize = 5;
const S_PATH_OFF: usize = 6;
const S_PATH_BLOB: usize = 7;
const S_HEAD: usize = 8;

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

fn lang_id(l: Lang) -> u32 {
    match l {
        Lang::Python => 0,
        Lang::TypeScript => 1,
        Lang::Tsx => 2,
    }
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

pub fn write(
    path: &Path,
    units: &[FileUnit],
    paths: &[PathBuf],
    langs: &[Lang],
    metas: &[FileMeta],
    interner: &Interner,
    root: &Path,
    head: &str,
) -> Result<()> {
    let mut files = Vec::with_capacity(units.len());
    let mut defs = Vec::new();
    let mut refs = Vec::new();
    let mut imports = Vec::new();

    for (i, u) in units.iter().enumerate() {
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
            lang: lang_id(langs[i]),
            had_error: u32::from(u.had_parse_error),
        });
        for d in &u.defs {
            defs.push(CDef {
                name: d.name.into_usize() as u32,
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
                name: r.name.into_usize() as u32,
                kind: r.kind as u32,
                span_s: r.span.start,
                span_e: r.span.end,
                scope: r.scope,
                recv: r.recv as u32,
                recv_name: r.recv_name.map_or(u32::MAX, |n| n.into_usize() as u32),
                _pad: 0,
            });
        }
        for m in &u.imports {
            imports.push(CImport {
                module: m.module.into_usize() as u32,
                span_s: m.span.start,
                span_e: m.span.end,
                _pad: 0,
            });
        }
    }

    let mut syms = vec![String::new(); interner.len()];
    for (k, v) in interner.iter() {
        let i = Key::into_usize(k);
        if i < syms.len() {
            syms[i] = v.to_string();
        }
    }
    let (sym_off, sym_blob) = blob(syms.into_iter());
    let (path_off, path_blob) = blob(paths.iter().map(|p| {
        p.strip_prefix(root)
            .unwrap_or(p)
            .to_string_lossy()
            .into_owned()
    }));

    let mut header = Header {
        magic: MAGIC,
        n_files: units.len() as u32,
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
    section!(S_SYM_OFF, &sym_off[..]);
    section!(S_SYM_BLOB, &sym_blob[..]);
    section!(S_PATH_OFF, &path_off[..]);
    section!(S_PATH_BLOB, &path_blob[..]);
    section!(S_HEAD, head.as_bytes());

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

pub fn read(path: &Path, interner: &Interner, root: &Path) -> Result<Vec<Entry>> {
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
    Ok(out)
}
