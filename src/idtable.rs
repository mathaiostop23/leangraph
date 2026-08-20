//! Persistent node-id assignment.
//!
//! Node ids must survive edits to unrelated files, or every incremental step
//! downstream is impossible: adding one definition to one file would renumber
//! everything after it and invalidate the entire adjacency structure.
//!
//! So ids come from a table keyed by content-derived `NodeKey`, persisted with
//! the graph. On a sync, a node that still exists keeps the id it had; new nodes
//! take ids from the free list, or extend the space. Deleted nodes leave holes,
//! which cost one empty CSR row each and are reclaimed by the next full index.

use crate::core::{NodeId, NodeKey};
use rustc_hash::FxHashMap;

#[derive(Default)]
pub struct IdTable {
    by_key: FxHashMap<NodeKey, NodeId>,
    /// key at each id, so the table can be written back out
    keys: Vec<NodeKey>,
    free: Vec<u32>,
}

#[derive(Default, Clone, Copy)]
pub struct Churn {
    pub kept: u32,
    pub added: u32,
    pub retired: u32,
    pub holes: u32,
}

impl IdTable {
    /// Rebuild from the key array persisted in a graph file.
    pub fn from_keys(keys: &[u64]) -> IdTable {
        let mut by_key = FxHashMap::default();
        let mut ks = Vec::with_capacity(keys.len());
        let mut free = Vec::new();
        for (i, &k) in keys.iter().enumerate() {
            ks.push(NodeKey(k));
            if k == 0 {
                free.push(i as u32); // hole left by a retired node
            } else {
                by_key.insert(NodeKey(k), NodeId(i as u32));
            }
        }
        IdTable {
            by_key,
            keys: ks,
            free,
        }
    }

    pub fn len(&self) -> u32 {
        self.keys.len() as u32
    }

    /// Assign ids for exactly this set of keys, retiring anything absent.
    ///
    /// Order matters for reproducibility: new keys are taken in the order given,
    /// and the caller passes them in a deterministic order, so two runs over
    /// identical source produce identical ids.
    pub fn assign(&mut self, wanted: &[NodeKey]) -> (Vec<NodeId>, Churn) {
        let mut churn = Churn::default();
        let mut out = Vec::with_capacity(wanted.len());
        let mut live: FxHashMap<NodeKey, NodeId> = FxHashMap::default();

        for &k in wanted {
            // A key already claimed in this pass means the caller produced a
            // duplicate. Fall through to a fresh id rather than handing out the
            // same one twice — two nodes sharing an id would silently overwrite
            // each other's metadata and drop one node's edges.
            if live.contains_key(&k) {
                let id = match self.free.pop() {
                    Some(i) => NodeId(i),
                    None => {
                        self.keys.push(NodeKey(0));
                        NodeId(self.keys.len() as u32 - 1)
                    }
                };
                self.keys[id.0 as usize] = k;
                churn.added += 1;
                out.push(id);
                continue;
            }
            if let Some(&id) = self.by_key.get(&k) {
                churn.kept += 1;
                live.insert(k, id);
                out.push(id);
            } else {
                let id = match self.free.pop() {
                    Some(i) => NodeId(i),
                    None => {
                        self.keys.push(NodeKey(0));
                        NodeId(self.keys.len() as u32 - 1)
                    }
                };
                self.keys[id.0 as usize] = k;
                churn.added += 1;
                live.insert(k, id);
                out.push(id);
            }
        }

        // Anything that had an id and is no longer wanted becomes a hole.
        for (&k, &id) in &self.by_key {
            if !live.contains_key(&k) {
                self.keys[id.0 as usize] = NodeKey(0);
                self.free.push(id.0);
                churn.retired += 1;
            }
        }
        self.by_key = live;
        churn.holes = self.free.len() as u32;
        (out, churn)
    }

    pub fn raw_keys(&self) -> Vec<u64> {
        self.keys.iter().map(|k| k.0).collect()
    }
}
