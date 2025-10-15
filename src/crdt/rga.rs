//! RGA (Replicated Growable Array) for text, one element per character.
//!
//! Each character has an id and is inserted *after* a reference element (or at the start).
//! Concurrent inserts after the same reference are ordered by stamp, highest first: the new
//! element skips every element to its right whose stamp is higher. Those are concurrent
//! siblings that win, or descendants of such siblings, which always have even higher stamps
//! because Lamport time grows along causal chains. Deletes leave tombstones.

use crate::types::{OpId, Stamp};

#[derive(Debug, Clone, PartialEq)]
struct Elem {
    id: OpId,
    stamp: Stamp,
    ch: char,
    deleted: bool,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Rga {
    elems: Vec<Elem>,
}

impl Rga {
    fn position(&self, id: OpId) -> Option<usize> {
        self.elems.iter().position(|e| e.id == id)
    }

    /// Integrate an insert. `after` must already be present (causal delivery).
    pub fn apply_insert(&mut self, id: OpId, stamp: Stamp, after: Option<OpId>, ch: char) {
        if self.position(id).is_some() {
            return; // duplicate
        }
        let mut pos = match after {
            None => 0,
            Some(a) => self.position(a).map_or(self.elems.len(), |p| p + 1),
        };
        while pos < self.elems.len() && self.elems[pos].stamp > stamp {
            pos += 1;
        }
        self.elems.insert(pos, Elem { id, stamp, ch, deleted: false });
    }

    pub fn apply_delete(&mut self, target: OpId) {
        if let Some(p) = self.position(target) {
            self.elems[p].deleted = true;
        }
    }

    /// Id of the visible character at `index`.
    pub fn id_at(&self, index: usize) -> Option<OpId> {
        self.elems.iter().filter(|e| !e.deleted).nth(index).map(|e| e.id)
    }

    pub fn len(&self) -> usize {
        self.elems.iter().filter(|e| !e.deleted).count()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Elements including tombstones, for curiosity and tests.
    pub fn len_with_tombstones(&self) -> usize {
        self.elems.len()
    }

    pub fn text(&self) -> String {
        self.elems.iter().filter(|e| !e.deleted).map(|e| e.ch).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(r: u64, s: u64) -> OpId {
        OpId { replica: r, seq: s }
    }
    fn st(l: u64, r: u64) -> Stamp {
        Stamp { lamport: l, replica: r }
    }

    #[test]
    fn concurrent_inserts_at_same_place_converge() {
        // Base "ac" from replica 1; replicas 2 and 3 both insert after 'a'.
        let base = |r: &mut Rga| {
            r.apply_insert(id(1, 1), st(1, 1), None, 'a');
            r.apply_insert(id(1, 2), st(2, 1), Some(id(1, 1)), 'c');
        };
        let ops = [(id(2, 1), st(3, 2), 'X'), (id(3, 1), st(3, 3), 'Y')];
        let mut x = Rga::default();
        let mut y = Rga::default();
        base(&mut x);
        base(&mut y);
        for (i, s, c) in ops {
            x.apply_insert(i, s, Some(id(1, 1)), c);
        }
        for (i, s, c) in ops.iter().rev() {
            y.apply_insert(*i, *s, Some(id(1, 1)), *c);
        }
        assert_eq!(x.text(), y.text());
        assert_eq!(x.text(), "aYXc"); // higher stamp (replica 3) first
    }

    #[test]
    fn typing_runs_stay_together() {
        // Replica 1 types "hi" at the start, replica 2 concurrently types "yo" at the start.
        let mut r = Rga::default();
        r.apply_insert(id(1, 1), st(1, 1), None, 'h');
        r.apply_insert(id(1, 2), st(2, 1), Some(id(1, 1)), 'i');
        r.apply_insert(id(2, 1), st(1, 2), None, 'y');
        r.apply_insert(id(2, 2), st(2, 2), Some(id(2, 1)), 'o');
        assert_eq!(r.text(), "yohi");
        r.apply_delete(id(2, 2));
        assert_eq!(r.text(), "yhi");
        assert_eq!(r.len_with_tombstones(), 4);
        assert_eq!(r.id_at(1), Some(id(1, 1)));
    }
}
