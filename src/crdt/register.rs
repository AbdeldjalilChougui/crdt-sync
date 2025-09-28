//! Last-writer-wins register: keep the value with the highest (Lamport, replica) stamp.

use crate::types::{Scalar, Stamp};

#[derive(Debug, Clone, Default, PartialEq)]
pub struct LwwRegister {
    stamp: Stamp,
    value: Option<Scalar>,
}

impl LwwRegister {
    pub fn apply_set(&mut self, stamp: Stamp, value: Scalar) {
        if self.value.is_none() || stamp > self.stamp {
            self.stamp = stamp;
            self.value = Some(value);
        }
    }

    pub fn get(&self) -> Option<&Scalar> {
        self.value.as_ref()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn highest_stamp_wins_in_any_order() {
        let a = (Stamp { lamport: 3, replica: 1 }, Scalar::from("a"));
        let b = (Stamp { lamport: 3, replica: 2 }, Scalar::from("b"));
        let c = (Stamp { lamport: 1, replica: 9 }, Scalar::from("c"));
        let mut r1 = LwwRegister::default();
        let mut r2 = LwwRegister::default();
        for (s, v) in [a.clone(), b.clone(), c.clone()] {
            r1.apply_set(s, v);
        }
        for (s, v) in [c, b, a] {
            r2.apply_set(s, v);
        }
        assert_eq!(r1, r2);
        assert_eq!(r1.get(), Some(&Scalar::from("b")));
    }
}
