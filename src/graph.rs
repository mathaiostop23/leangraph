//! CSR graph: build, write, and zero-copy load.
//!
//! The whole point of this module is that **loading is not deserialization**.
//! `Graph::open` is an `mmap` plus a header check plus pointer casts — a
//! multi-million-edge graph becomes queryable in microseconds, which is what
//! lets a daemon answer instantly instead of paying a multi-second warm-up.
//!
//! Adjacency is stored as CSR (compressed sparse row): a `Vec<u32>` of offsets
//! and a `Vec<u32>` of targets. `neighbors(n)` is then two array reads and a
//! slice — no B-tree descent, no row decode, no allocation, and the targets are
//! contiguous so the prefetcher works. Both directions are stored, because the
//! question an issue agent actually asks is "what breaks if this is wrong",
//! which is the reverse edge.

use crate::core::{Edge, EdgeKind, NodeId};
use crate::resolve::Resolved;
use anyhow::{bail, Context, Result};
use bytemuck::{Pod, Zeroable};
use memmap2::Mmap;
use rayon::prelude::*;
use std::fs::File;
use std::io::Write;
use std::path::{Path, PathBuf};

const MAGIC: [u8; 8] = *b"ARBORG\x00\x02";
const N_SECTIONS: usize = 24;

// section ids
const S_NODE_NAME: usize = 0;
const S_NODE_KIND: usize = 1;
const S_NODE_FILE: usize = 2;
const S_NODE_START: usize = 3;
const S_NODE_END: usize = 4;
const S_FWD_OFF: usize = 5;
const S_FWD_TGT: usize = 6;
const S_FWD_KIND: usize = 7;
const S_FWD_CONF: usize = 8;
const S_FWD_PROV: usize = 9;
const S_REV_OFF: usize = 10;
const S_REV_TGT: usize = 11;
const S_REV_KIND: usize = 12;
const S_REV_CONF: usize = 13;
const S_REV_PROV: usize = 14;
const S_SYM_OFF: usize = 15;
const S_SYM_BLOB: usize = 16;
const S_PATH_OFF: usize = 17;
const S_PATH_BLOB: usize = 18;

#[repr(C)]
#[derive(Clone, Copy, Pod, Zeroable)]
struct Header {
    magic: [u8; 8],
    n_nodes: u32,
    n_edges: u32,
    n_files: u32,
    n_syms: u32,
    _pad: [u32; 2],
    off: [u64; N_SECTIONS],
    len: [u64; N_SECTIONS],
}

/// One direction of the adjacency structure.
struct Dir<'a> {
    off: &'a [u32],
    tgt: &'a [u32],
    kind: &'a [u8],
    conf: &'a [u8],
    prov: &'a [u8],
}

impl<'a> Dir<'a> {
    #[inline]
    fn range(&self, n: NodeId) -> std::ops::Range<usize> {
        let i = n.0 as usize;
        if i + 1 >= self.off.len() {
            return 0..0;
        }
        self.off[i] as usize..self.off[i + 1] as usize
    }
}

/// A neighbour, with the evidence that produced the edge.
#[derive(Clone, Copy, Debug)]
pub struct Neighbor {
    pub node: NodeId,
    pub kind: u8,
    pub conf: u8,
    pub prov: u8,
}

pub struct Graph {
    _mmap: Mmap,
    n_nodes: u32,
    n_files: u32,
    node_name: &'static [u32],
    node_kind: &'static [u8],
    node_file: &'static [u32],
    node_start: &'static [u32],
    node_end: &'static [u32],
    fwd: Dir<'static>,
    rev: Dir<'static>,
    sym_off: &'static [u32],
    sym_blob: &'static [u8],
    path_off: &'static [u32],
    path_blob: &'static [u8],
}

// ------------------------------------------------------------------- build

/// Sort edges into CSR form. `par_sort_unstable_by_key` on a few hundred
/// thousand edges is a handful of milliseconds; the resulting arrays are the
/// on-disk format verbatim, so there is no separate serialization step.
fn build_dir(edges: &mut [Edge], n_nodes: u32, by_src: bool) -> (Vec<u32>, Vec<u32>, Vec<u8>, Vec<u8>, Vec<u8>) {
    if by_src {
        edges.par_sort_unstable_by_key(|e| e.src.0);
    } else {
        edges.par_sort_unstable_by_key(|e| e.dst.0);
    }

    let m = edges.len();
    let mut off = vec![0u32; n_nodes as usize + 1];
    let mut tgt = Vec::with_capacity(m);
    let mut kind = Vec::with_capacity(m);
    let mut conf = Vec::with_capacity(m);
    let mut prov = Vec::with_capacity(m);

    for e in edges.iter() {
        let key = if by_src { e.src.0 } else { e.dst.0 };
        if (key as usize) < n_nodes as usize {
            off[key as usize + 1] += 1;
        }
    }
    for i in 0..n_nodes as usize {
        off[i + 1] += off[i];
    }
    for e in edges.iter() {
        let other = if by_src { e.dst.0 } else { e.src.0 };
        tgt.push(other);
        kind.push(e.kind as u8);
        conf.push(e.conf);
        prov.push(e.prov as u8);
    }
    (off, tgt, kind, conf, prov)
}

fn pad_to_8(v: &mut Vec<u8>) {
    while v.len() % 8 != 0 {
        v.push(0);
    }
}

pub fn write(path: &Path, r: &Resolved, syms: &[String], paths: &[PathBuf]) -> Result<()> {
    let n_nodes = r.space.total;
    let mut fwd_edges = r.edges.clone();
    let mut rev_edges = r.edges.clone();
    let (fo, ft, fk, fc, fp) = build_dir(&mut fwd_edges, n_nodes, true);
    let (ro, rt, rk, rc, rp) = build_dir(&mut rev_edges, n_nodes, false);

    // node metadata, structure-of-arrays for cache locality on scans
    let mut node_name = vec![0u32; n_nodes as usize];
    let mut node_kind = vec![0u8; n_nodes as usize];
    let mut node_file = vec![0u32; n_nodes as usize];
    let mut node_start = vec![0u32; n_nodes as usize];
    let mut node_end = vec![0u32; n_nodes as usize];
    for (i, m) in r.nodes.iter().enumerate() {
        node_name[i] = m.name;
        node_kind[i] = m.kind;
        node_file[i] = m.file;
        node_start[i] = m.start;
        node_end[i] = m.end;
    }

    // string tables: offsets + one blob, so lookup is a slice, never an alloc
    let blob = |items: &[String]| -> (Vec<u32>, Vec<u8>) {
        let mut off = Vec::with_capacity(items.len() + 1);
        let mut buf = Vec::new();
        off.push(0u32);
        for s in items {
            buf.extend_from_slice(s.as_bytes());
            off.push(buf.len() as u32);
        }
        (off, buf)
    };
    let (sym_off, sym_blob) = blob(syms);
    let path_strings: Vec<String> = paths.iter().map(|p| p.to_string_lossy().into_owned()).collect();
    let (path_off, path_blob) = blob(&path_strings);

    let mut header = Header {
        magic: MAGIC,
        n_nodes,
        n_edges: r.edges.len() as u32,
        n_files: r.space.n_files,
        n_syms: syms.len() as u32,
        _pad: [0; 2],
        off: [0; N_SECTIONS],
        len: [0; N_SECTIONS],
    };

    let mut body: Vec<u8> = Vec::with_capacity(16 * 1024 * 1024);
    let head_len = std::mem::size_of::<Header>();

    macro_rules! section {
        ($id:expr, $data:expr) => {{
            let bytes: &[u8] = bytemuck::cast_slice($data);
            pad_to_8(&mut body);
            header.off[$id] = (head_len + body.len()) as u64;
            header.len[$id] = bytes.len() as u64;
            body.extend_from_slice(bytes);
        }};
    }

    section!(S_NODE_NAME, &node_name[..]);
    section!(S_NODE_KIND, &node_kind[..]);
    section!(S_NODE_FILE, &node_file[..]);
    section!(S_NODE_START, &node_start[..]);
    section!(S_NODE_END, &node_end[..]);
    section!(S_FWD_OFF, &fo[..]);
    section!(S_FWD_TGT, &ft[..]);
    section!(S_FWD_KIND, &fk[..]);
    section!(S_FWD_CONF, &fc[..]);
    section!(S_FWD_PROV, &fp[..]);
    section!(S_REV_OFF, &ro[..]);
    section!(S_REV_TGT, &rt[..]);
    section!(S_REV_KIND, &rk[..]);
    section!(S_REV_CONF, &rc[..]);
    section!(S_REV_PROV, &rp[..]);
    section!(S_SYM_OFF, &sym_off[..]);
    section!(S_SYM_BLOB, &sym_blob[..]);
    section!(S_PATH_OFF, &path_off[..]);
    section!(S_PATH_BLOB, &path_blob[..]);

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).ok();
    }
    let tmp = path.with_extension("tmp");
    {
        let mut f = File::create(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytemuck::bytes_of(&header))?;
        f.write_all(&body)?;
        f.sync_all()?;
    }
    // atomic swap: a reader either sees the old graph or the new one, never a
    // half-written file
    std::fs::rename(&tmp, path)?;
    Ok(())
}

// -------------------------------------------------------------------- open

impl Graph {
    pub fn open(path: &Path) -> Result<Graph> {
        let f = File::open(path).with_context(|| format!("open {}", path.display()))?;
        // SAFETY: the file is replaced atomically via rename, never mutated in
        // place, so the mapping cannot be torn under us.
        let mmap = unsafe { Mmap::map(&f)? };
        if mmap.len() < std::mem::size_of::<Header>() {
            bail!("{}: truncated graph file", path.display());
        }
        let header: Header = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<Header>()]);
        if header.magic != MAGIC {
            bail!(
                "{}: not an arbor graph, or written by an incompatible version",
                path.display()
            );
        }

        // The mapping outlives every slice because `Graph` owns it and never
        // exposes one beyond its own lifetime.
        let base: &'static [u8] = unsafe { std::mem::transmute::<&[u8], &'static [u8]>(&mmap) };
        let sec_u32 = |id: usize| -> &'static [u32] {
            let (o, l) = (header.off[id] as usize, header.len[id] as usize);
            if l == 0 {
                return &[];
            }
            bytemuck::cast_slice(&base[o..o + l])
        };
        let sec_u8 = |id: usize| -> &'static [u8] {
            let (o, l) = (header.off[id] as usize, header.len[id] as usize);
            &base[o..o + l]
        };

        Ok(Graph {
            n_nodes: header.n_nodes,
            n_files: header.n_files,
            node_name: sec_u32(S_NODE_NAME),
            node_kind: sec_u8(S_NODE_KIND),
            node_file: sec_u32(S_NODE_FILE),
            node_start: sec_u32(S_NODE_START),
            node_end: sec_u32(S_NODE_END),
            fwd: Dir {
                off: sec_u32(S_FWD_OFF),
                tgt: sec_u32(S_FWD_TGT),
                kind: sec_u8(S_FWD_KIND),
                conf: sec_u8(S_FWD_CONF),
                prov: sec_u8(S_FWD_PROV),
            },
            rev: Dir {
                off: sec_u32(S_REV_OFF),
                tgt: sec_u32(S_REV_TGT),
                kind: sec_u8(S_REV_KIND),
                conf: sec_u8(S_REV_CONF),
                prov: sec_u8(S_REV_PROV),
            },
            sym_off: sec_u32(S_SYM_OFF),
            sym_blob: sec_u8(S_SYM_BLOB),
            path_off: sec_u32(S_PATH_OFF),
            path_blob: sec_u8(S_PATH_BLOB),
            _mmap: mmap,
        })
    }

    pub fn n_nodes(&self) -> u32 {
        self.n_nodes
    }
    pub fn n_files(&self) -> u32 {
        self.n_files
    }
    pub fn n_edges(&self) -> usize {
        self.fwd.tgt.len()
    }

    #[inline]
    pub fn name(&self, n: NodeId) -> &str {
        self.sym(self.node_name.get(n.0 as usize).copied().unwrap_or(0))
    }

    #[inline]
    pub fn sym(&self, id: u32) -> &str {
        let i = id as usize;
        if i + 1 >= self.sym_off.len() {
            return "";
        }
        let (a, b) = (self.sym_off[i] as usize, self.sym_off[i + 1] as usize);
        std::str::from_utf8(&self.sym_blob[a..b]).unwrap_or("")
    }

    #[inline]
    pub fn path(&self, f: u32) -> &str {
        let i = f as usize;
        if i + 1 >= self.path_off.len() {
            return "";
        }
        let (a, b) = (self.path_off[i] as usize, self.path_off[i + 1] as usize);
        std::str::from_utf8(&self.path_blob[a..b]).unwrap_or("")
    }

    #[inline]
    pub fn node_kind(&self, n: NodeId) -> u8 {
        self.node_kind.get(n.0 as usize).copied().unwrap_or(0)
    }

    /// File and byte range this node occupies, for slicing source at query time.
    #[inline]
    pub fn location(&self, n: NodeId) -> (u32, u32, u32) {
        let i = n.0 as usize;
        (
            self.node_file.get(i).copied().unwrap_or(0),
            self.node_start.get(i).copied().unwrap_or(0),
            self.node_end.get(i).copied().unwrap_or(0),
        )
    }

    /// What this node points at.
    pub fn callees(&self, n: NodeId) -> Vec<Neighbor> {
        Self::collect(&self.fwd, n)
    }

    /// What points at this node. The direction that matters for
    /// "what breaks if this is wrong".
    pub fn callers(&self, n: NodeId) -> Vec<Neighbor> {
        Self::collect(&self.rev, n)
    }

    fn collect(d: &Dir<'static>, n: NodeId) -> Vec<Neighbor> {
        let r = d.range(n);
        r.map(|i| Neighbor {
            node: NodeId(d.tgt[i]),
            kind: d.kind[i],
            conf: d.conf[i],
            prov: d.prov[i],
        })
        .collect()
    }

    /// Everything reachable within `depth` reverse hops — the blast radius of
    /// changing `n`. Pure slice walking over mmap'd memory; no allocation per
    /// hop beyond the frontier itself.
    /// `skip_contains` excludes structural containment. Without it, one hop
    /// from any definition reaches its own file and the result degenerates into
    /// "every file that imports this file" — true, but not what "what breaks if
    /// I change this" means.
    pub fn impact(&self, n: NodeId, depth: u32, min_conf: u8, skip_contains: bool) -> Vec<NodeId> {
        let mut seen = vec![false; self.n_nodes as usize];
        let mut frontier = vec![n];
        let mut out = Vec::new();
        seen[n.0 as usize] = true;
        for _ in 0..depth {
            let mut next = Vec::new();
            for &node in &frontier {
                let r = self.rev.range(node);
                for i in r {
                    if self.rev.conf[i] < min_conf {
                        continue;
                    }
                    if skip_contains && self.rev.kind[i] == EdgeKind::Contains as u8 {
                        continue;
                    }
                    let t = self.rev.tgt[i] as usize;
                    if t < seen.len() && !seen[t] {
                        seen[t] = true;
                        let id = NodeId(t as u32);
                        out.push(id);
                        next.push(id);
                    }
                }
            }
            if next.is_empty() {
                break;
            }
            frontier = next;
        }
        out
    }

    /// Nodes whose name matches exactly. Linear for now; Phase 3 adds an index.
    pub fn find(&self, name: &str) -> Vec<NodeId> {
        (0..self.n_nodes)
            .filter(|&i| self.name(NodeId(i)) == name)
            .map(NodeId)
            .collect()
    }
}
