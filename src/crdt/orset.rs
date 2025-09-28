//! Observed-remove set (add-wins). Every add creates a unique tag (its op id); a remove
//! deletes only the tags its author had observed, so a concurrent add survives.

use crate::types::{OpId, Scalar};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct OrSet {
    tags: BTreeMap<Scalar, BTreeSet<OpId>>,
}

impl OrSet {
    pub fn apply_add(&mut self, tag: OpId, value: Scalar) {
        self.tags.entry(value).or_default().insert(tag);
    }

    pub fn apply_remove(&mut self, value: &Scalar, observed: &[OpId]) {
        if let Some(t) = self.tags.get_mut(value) {
            for o in observed {
                t.remove(o);
            }
            if t.is_empty() {
                self.tags.remove(value);
            }
        }
    }

    /// Tags a local remove of `value` must cite.
    pub fn observed(&self, value: &Scalar) -> Vec<OpId> {
        self.tags.get(value).map(|t| t.iter().copied().collect()).unwrap_or_default()
    }

    pub fn contains(&self, value: &Scalar) -> bool {
        self.tags.contains_key(value)
    }

    /// Members in sorted order.
    pub fn values(&self) -> Vec<Scalar> {
        self.tags.keys().cloned().collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn id(r: u64, s: u64) -> OpId {
        OpId { replica: r, seq: s }
    }

    #[test]
    fn concurrent_add_wins_over_remove() {
        let x = Scalar::from("x");
        let mut a = OrSet::default();
        a.apply_add(id(1, 1), x.clone());
        let seen = a.observed(&x);
        // Replica 2 adds x concurrently while replica 1 removes the x it saw.
        a.apply_add(id(2, 1), x.clone());
        a.apply_remove(&x, &seen);
        assert!(a.contains(&x));
        a.apply_remove(&x, &[id(2, 1)]);
        assert!(!a.contains(&x));
    }
}
