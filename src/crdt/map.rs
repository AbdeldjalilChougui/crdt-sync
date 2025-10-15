//! Map with a last-writer-wins register per key. Deletes are writes of "nothing", so a
//! delete and a concurrent set are ordered by stamp like any two writes.

use crate::types::{Stamp, Value};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LwwMap {
    entries: BTreeMap<String, (Stamp, Option<Value>)>,
}

impl LwwMap {
    pub fn apply_set(&mut self, stamp: Stamp, key: &str, value: Option<Value>) {
        match self.entries.get(key) {
            Some((s, _)) if *s >= stamp => {}
            _ => {
                self.entries.insert(key.to_string(), (stamp, value));
            }
        }
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.entries.get(key).and_then(|(_, v)| v.as_ref())
    }

    /// Live keys in sorted order.
    pub fn keys(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().filter(|(_, (_, v))| v.is_some()).map(|(k, _)| k.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::Scalar;

    #[test]
    fn delete_vs_set_ordered_by_stamp() {
        let s = |l| Stamp { lamport: l, replica: 1 };
        let one = Some(Value::Scalar(Scalar::Int(1)));
        let mut a = LwwMap::default();
        a.apply_set(s(2), "k", None);
        a.apply_set(s(1), "k", one.clone());
        assert_eq!(a.get("k"), None);
        let mut b = LwwMap::default();
        b.apply_set(s(1), "k", one);
        b.apply_set(s(2), "k", None);
        assert_eq!(a, b);
        assert_eq!(b.keys().count(), 0);
    }
}
