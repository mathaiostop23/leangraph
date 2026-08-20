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
use rustc_hash::FxHashMap;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

const MAGIC: [u8; 8] = *b"LGRPHG\x00\x04";
const N_SECTIONS: usize = 25;

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
const S_ROOT: usize = 19;
const S_NODE_KEY: usize = 20;
const S_FILE_STAMP: usize = 21;

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
    root: &'static str,
    node_key: &'static [u64],
    /// `(size, mtime)` per file, as of indexing. The only thing that can tell a
    /// query that the bytes it is about to slice are no longer the bytes that
    /// were parsed.
    file_stamp: &'static [u64],
    /// Built on first lookup, not at open time — keeping `open` a pure mmap is
    /// the point of the format, and many callers never search by name.
    name_index: OnceLock<FxHashMap<&'static str, Vec<NodeId>>>,
}

// ------------------------------------------------------------------- build

/// Sort edges into CSR form. `par_sort_unstable_by_key` on a few hundred
/// thousand edges is a handful of milliseconds; the resulting arrays are the
/// on-disk format verbatim, so there is no separate serialization step.
fn build_dir(edges: &mut [Edge], n_nodes: u32, by_src: bool) -> (Vec<u32>, Vec<u32>, Vec<u8>, Vec<u8>, Vec<u8>) {
    // Total order, not just by the CSR key: an unstable sort leaves ties in
    // arbitrary order, which would make the file differ run to run.
    if by_src {
        edges.par_sort_unstable_by_key(|e| (e.src.0, e.dst.0, e.kind as u8, e.conf));
    } else {
        edges.par_sort_unstable_by_key(|e| (e.dst.0, e.src.0, e.kind as u8, e.conf));
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

pub fn write(
    path: &Path,
    r: &Resolved,
    syms: &[String],
    paths: &[PathBuf],
    root: &Path,
    node_keys: &[u64],
    stamps: &[(u64, i64)],
) -> Result<()> {
    // Canonicalise the symbol table before writing. Two things force this:
    //
    //   * the interner is concurrent, so ids are assigned in whatever order
    //     threads happen to reach a string;
    //   * on an incremental sync the interner also holds strings revived from
    //     the cache for files that have since been deleted.
    //
    // Either one makes two indexes of identical source produce different bytes.
    // Emitting only the symbols the graph actually references, in sorted order,
    // makes the file a pure function of the graph — which is what lets a cache
    // be trusted and a divergence be a real signal rather than noise.
    let mut used: Vec<u32> = r.nodes.iter().map(|m| m.name).collect();
    used.sort_unstable();
    used.dedup();
    used.sort_unstable_by(|&a, &b| {
        syms.get(a as usize)
            .map(String::as_str)
            .unwrap_or("")
            .cmp(syms.get(b as usize).map(String::as_str).unwrap_or(""))
    });
    let mut remap: FxHashMap<u32, u32> = FxHashMap::default();
    for (new, &old) in used.iter().enumerate() {
        remap.insert(old, new as u32);
    }
    let syms: Vec<String> = used
        .iter()
        .map(|&i| syms.get(i as usize).cloned().unwrap_or_default())
        .collect();
    let syms = &syms[..];
    let remap_sym = |id: u32| remap.get(&id).copied().unwrap_or(0);

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
        node_name[i] = remap_sym(m.name);
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
    let path_strings: Vec<String> = paths
        .iter()
        .map(|p| {
            p.strip_prefix(root)
                .unwrap_or(p)
                .to_string_lossy()
                .into_owned()
        })
        .collect();
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
    let root_bytes = root.to_string_lossy().into_owned();
    section!(S_ROOT, root_bytes.as_bytes());
    section!(S_NODE_KEY, node_keys);
    // Two u64s per file: size, and mtime reinterpreted. A query that is about to
    // slice bytes out of a file can then check that they are still the bytes
    // that were parsed.
    let stamp_flat: Vec<u64> = stamps
        .iter()
        .flat_map(|&(size, mtime)| [size, mtime as u64])
        .collect();
    section!(S_FILE_STAMP, &stamp_flat[..]);

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

/// Read just the node-key table, without building a whole `Graph`.
///
/// A sync needs the previous id assignment before it can resolve anything, and
/// paying for the full structure to get one array would be wasteful — this is
/// an mmap and one slice.
pub fn read_keys(path: &Path) -> Option<Vec<u64>> {
    let f = File::open(path).ok()?;
    let mmap = unsafe { Mmap::map(&f) }.ok()?;
    if mmap.len() < std::mem::size_of::<Header>() {
        return None;
    }
    let h: Header = *bytemuck::from_bytes(&mmap[..std::mem::size_of::<Header>()]);
    if h.magic != MAGIC {
        return None; // written by an older format: fall back to a full assignment
    }
    let (o, l) = (h.off[S_NODE_KEY] as usize, h.len[S_NODE_KEY] as usize);
    let slice: &[u64] = bytemuck::try_cast_slice(mmap.get(o..o + l)?).ok()?;
    Some(slice.to_vec())
}

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
                "{}: not an leangraph graph, or written by an incompatible version",
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
            root: std::str::from_utf8(sec_u8(S_ROOT)).unwrap_or(""),
            node_key: {
                let (o, l) = (header.off[S_NODE_KEY] as usize, header.len[S_NODE_KEY] as usize);
                if l == 0 {
                    &[]
                } else {
                    bytemuck::cast_slice(&base[o..o + l])
                }
            },
            file_stamp: {
                let (o, l) = (
                    header.off[S_FILE_STAMP] as usize,
                    header.len[S_FILE_STAMP] as usize,
                );
                if l == 0 {
                    &[]
                } else {
                    bytemuck::cast_slice(&base[o..o + l])
                }
            },
            name_index: OnceLock::new(),
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

    /// Repository root the graph was built from. Paths are stored relative to
    /// it; join through here to touch the working tree.
    /// Is the file still what it was when the graph was built?
    ///
    /// The graph stores byte offsets. If the file has changed since, those
    /// offsets point at whatever now occupies those positions — which is how a
    /// query for one function returned a fragment of a different one, under the
    /// right name, at confidence 100. Size and mtime are a cheap check and the
    /// same one the extraction cache already trusts.
    ///
    /// A graph written before this section existed has no stamps, and reports
    /// everything fresh rather than everything stale.
    pub fn file_is_current(&self, f: u32) -> bool {
        let i = f as usize * 2;
        let (Some(&size), Some(&mtime)) = (self.file_stamp.get(i), self.file_stamp.get(i + 1))
        else {
            return true;
        };
        let Ok(md) = std::fs::metadata(self.abs_path(f)) else {
            return false;
        };
        let now = md
            .modified()
            .ok()
            .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        md.len() == size && now == mtime as i64
    }

    /// Absolute path for a file id.
    pub fn abs_path(&self, f: u32) -> PathBuf {
        Path::new(self.root).join(self.path(f))
    }

    /// Content-derived identity of a node — stable across syncs, unlike the id,
    /// which is an allocation detail.
    #[inline]
    pub fn node_key(&self, n: NodeId) -> u64 {
        self.node_key.get(n.0 as usize).copied().unwrap_or(0)
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

    /// Stable orientation for a repository: the same bytes for every issue, so
    /// it can sit before a cache breakpoint and be paid for once.
    ///
    /// Ranked by how much of the graph each file carries — definition count and
    /// inbound edges — which is a reasonable proxy for "where the important code
    /// lives" and costs one pass.
    /// Grow the file list until the text is worth caching.
    ///
    /// Prompt caching has a minimum block size — below it the breakpoint is
    /// ignored and the whole cost argument quietly stops applying. A short
    /// preamble is not a saving, it is a cache that never engages, so where
    /// there is more of the repository to describe, describe more of it.
    pub fn preamble_at_least(&self, min_chars: usize, from_files: usize) -> String {
        let mut n = from_files;
        loop {
            let out = self.preamble(n);
            if out.len() >= min_chars || n >= self.n_files as usize {
                return out;
            }
            n = (n * 2).min(self.n_files as usize);
        }
    }

    pub fn preamble(&self, max_files: usize) -> String {
        let mut per_file: FxHashMap<u32, (u32, u32)> = FxHashMap::default();
        for i in 0..self.n_nodes {
            let n = NodeId(i);
            if self.node_key(n) == 0 || self.node_kind(n) == 4 {
                continue; // hole, or the file node itself
            }
            let (f, _, _) = self.location(n);
            let e = per_file.entry(f).or_default();
            e.0 += 1;
            e.1 += self.rev.range(n).len() as u32;
        }
        let mut ranked: Vec<(u32, u32, u32)> =
            per_file.into_iter().map(|(f, (d, r))| (d + r / 4, f, d)).collect();
        ranked.sort_unstable_by_key(|&(score, f, _)| (std::cmp::Reverse(score), f));

        let mut out = String::with_capacity(8192);
        out.push_str(&format!(
            "# Repository structure\n\n{} files · {} definitions · {} relationships.\n\n\
The files below carry most of the graph, ordered by how much of it they hold.\n\n",
            self.n_files,
            self.n_nodes as usize - self.n_files as usize,
            self.n_edges()
        ));
        for &(_, f, defs) in ranked.iter().take(max_files) {
            out.push_str(&format!("- `{}` — {defs} definitions\n", self.path(f)));
        }
        out
    }

    /// Dotted path from the enclosing file down to this node — `Flask.send_file`.
    ///
    /// Reconstructed from `Contains` edges rather than stored, because the
    /// containment tree is already in the graph and a second copy of the same
    /// fact is a second thing that can be wrong. Walking upward means following
    /// the reverse direction, which is the one CSR stores for exactly this kind
    /// of question.
    ///
    /// This is what makes a graph node comparable to a Python `co_qualname` or a
    /// stack frame, so it is the join key for any external oracle.
    pub fn qualified(&self, n: NodeId) -> String {
        let mut parts: Vec<&str> = vec![self.name(n)];
        let mut cur = n;
        // Bounded: a cycle in the containment tree would otherwise hang the
        // dump, and nothing guarantees the tree is acyclic after a bad merge.
        for _ in 0..32 {
            let Some(parent) = self
                .callers(cur)
                .into_iter()
                .find(|nb| nb.kind == EdgeKind::Contains as u8)
                .map(|nb| nb.node)
            else {
                break;
            };
            if parent == cur || self.node_kind(parent) == 4 {
                break; // reached the file node; the path is carried separately
            }
            parts.push(self.name(parent));
            cur = parent;
        }
        parts.reverse();
        parts.join(".")
    }

    /// Nodes whose name matches exactly.
    pub fn find(&self, name: &str) -> Vec<NodeId> {
        self.name_index
            .get_or_init(|| {
                let mut m: FxHashMap<&'static str, Vec<NodeId>> = FxHashMap::default();
                for i in 0..self.n_nodes {
                    let s = self.name(NodeId(i));
                    // SAFETY: names are slices of the mmap, which `self` owns
                    // for its whole lifetime.
                    let s: &'static str = unsafe { std::mem::transmute::<&str, &'static str>(s) };
                    m.entry(s).or_default().push(NodeId(i));
                }
                m
            })
            .get(name)
            .cloned()
            .unwrap_or_default()
    }
}
