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

#[cfg(test)]
mod tests {
    use super::*;

    fn key(n: u64) -> NodeKey {
        NodeKey(n)
    }

    #[test]
    fn a_node_that_still_exists_keeps_its_id() {
        // The property the whole incremental path rests on. If an id moves
        // because an unrelated file grew a function, every cached edge and every
        // row of the adjacency structure is wrong.
        let mut t = IdTable::default();
        let (first, _) = t.assign(&[key(10), key(20), key(30)]);
        let id_of_20 = first[1];

        // A new node appears before it and another after it.
        let (second, churn) = t.assign(&[key(5), key(10), key(20), key(30), key(40)]);
        assert_eq!(
            second[2], id_of_20,
            "key 20 must hold the id it was given, not shift for its new neighbours"
        );
        assert_eq!(churn.kept, 3);
        assert_eq!(churn.added, 2);
    }

    #[test]
    fn a_deleted_node_leaves_a_hole_that_is_reused() {
        let mut t = IdTable::default();
        let (ids, _) = t.assign(&[key(1), key(2), key(3)]);
        let freed = ids[1];

        let (_, churn) = t.assign(&[key(1), key(3)]);
        assert_eq!(churn.retired, 1);
        assert_eq!(churn.holes, 1);

        // The next new key takes the vacated id rather than growing the space.
        let before = t.len();
        let (ids, _) = t.assign(&[key(1), key(3), key(4)]);
        assert_eq!(
            ids[2], freed,
            "a new node should reuse the hole a retired one left"
        );
        assert_eq!(t.len(), before, "and not extend the id space to do it");
    }

    #[test]
    fn the_same_keys_in_the_same_order_give_the_same_ids() {
        let mut a = IdTable::default();
        let mut b = IdTable::default();
        let keys = [key(7), key(8), key(9)];
        assert_eq!(a.assign(&keys).0, b.assign(&keys).0);
    }

    #[test]
    fn a_table_survives_a_round_trip_through_its_key_array() {
        // The table is persisted as the graph's key section and rebuilt from it
        // on the next run. An id that changes across that boundary is the same
        // failure as an id that changes across an edit.
        let mut t = IdTable::default();
        let (ids, _) = t.assign(&[key(11), key(22), key(33)]);
        let raw = t.raw_keys();

        let mut reopened = IdTable::from_keys(&raw);
        let (again, churn) = reopened.assign(&[key(11), key(22), key(33)]);
        assert_eq!(again, ids, "ids must survive being written and read back");
        assert_eq!(churn.added, 0, "nothing is new on an unchanged tree");
        assert_eq!(churn.kept, 3);
    }

    #[test]
    fn holes_survive_the_round_trip_as_holes() {
        let mut t = IdTable::default();
        t.assign(&[key(1), key(2), key(3)]);
        t.assign(&[key(1), key(3)]); // 2 retired
        let reopened = IdTable::from_keys(&t.raw_keys());
        assert_eq!(reopened.len(), t.len());

        let mut reopened = reopened;
        let before = reopened.len();
        reopened.assign(&[key(1), key(3), key(9)]);
        assert_eq!(
            reopened.len(),
            before,
            "the hole must still be known after a reload, or the space grows every sync"
        );
    }

    #[test]
    fn an_empty_assignment_retires_everything() {
        let mut t = IdTable::default();
        t.assign(&[key(1), key(2)]);
        let (ids, churn) = t.assign(&[]);
        assert!(ids.is_empty());
        assert_eq!(churn.retired, 2);
    }
}
