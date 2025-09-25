//! Identifiers, clocks and values shared by every CRDT.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fmt;
use std::hash::{Hash, Hasher};

pub type ReplicaId = u64;

/// Unique id of an operation: the replica that made it and its per-replica sequence number
/// (1, 2, 3, ... without gaps). Because sequences are contiguous, "have I seen op X?" is
/// just `clock[X.replica] >= X.seq`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct OpId {
    pub replica: ReplicaId,
    pub seq: u64,
}

impl fmt::Display for OpId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}@{:x}", self.seq, self.replica)
    }
}

/// Total order used to break ties between concurrent operations: Lamport time, then replica.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Default)]
pub struct Stamp {
    pub lamport: u64,
    pub replica: ReplicaId,
}

/// Which object an operation targets. The root map always exists; every other object is
/// named after the operation that created it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ObjId {
    Root,
    Op(OpId),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ObjKind {
    Map,
    Register,
    Set,
    Text,
}

/// `f64` with a total order and bitwise equality, so scalars can be set members and keys.
#[derive(Debug, Clone, Copy)]
pub struct F64(pub f64);

impl PartialEq for F64 {
    fn eq(&self, o: &Self) -> bool {
        self.0.to_bits() == o.0.to_bits()
    }
}
impl Eq for F64 {}
impl Hash for F64 {
    fn hash<H: Hasher>(&self, h: &mut H) {
        self.0.to_bits().hash(h)
    }
}
impl PartialOrd for F64 {
    fn partial_cmp(&self, o: &Self) -> Option<Ordering> {
        Some(self.cmp(o))
    }
}
impl Ord for F64 {
    fn cmp(&self, o: &Self) -> Ordering {
        self.0.total_cmp(&o.0)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum Scalar {
    Null,
    Bool(bool),
    Int(i64),
    Float(F64),
    Str(String),
}

impl Scalar {
    /// Parse CLI input: `null`, `true`/`false`, integers, floats, else a string.
    pub fn parse_loose(s: &str) -> Scalar {
        match s {
            "null" => Scalar::Null,
            "true" => Scalar::Bool(true),
            "false" => Scalar::Bool(false),
            _ => {
                if let Ok(i) = s.parse::<i64>() {
                    Scalar::Int(i)
                } else if let Ok(f) = s.parse::<f64>() {
                    Scalar::Float(F64(f))
                } else {
                    Scalar::Str(s.to_string())
                }
            }
        }
    }

    pub fn to_json(&self) -> serde_json::Value {
        match self {
            Scalar::Null => serde_json::Value::Null,
            Scalar::Bool(b) => (*b).into(),
            Scalar::Int(i) => (*i).into(),
            Scalar::Float(F64(f)) => serde_json::Number::from_f64(*f).map_or(serde_json::Value::Null, Into::into),
            Scalar::Str(s) => s.clone().into(),
        }
    }
}

impl From<&str> for Scalar {
    fn from(s: &str) -> Self {
        Scalar::Str(s.to_string())
    }
}
impl From<i64> for Scalar {
    fn from(i: i64) -> Self {
        Scalar::Int(i)
    }
}
impl From<bool> for Scalar {
    fn from(b: bool) -> Self {
        Scalar::Bool(b)
    }
}

/// What a map key holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Scalar(Scalar),
    /// A nested object, created by the op that wrote this value.
    Obj(ObjId, ObjKind),
}

/// For each replica, the highest contiguous sequence number integrated.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct VectorClock(pub BTreeMap<ReplicaId, u64>);

impl VectorClock {
    pub fn get(&self, r: ReplicaId) -> u64 {
        self.0.get(&r).copied().unwrap_or(0)
    }

    pub fn set(&mut self, r: ReplicaId, seq: u64) {
        self.0.insert(r, seq);
    }

    pub fn contains(&self, id: OpId) -> bool {
        self.get(id.replica) >= id.seq
    }

    /// True if every entry of `other` is <= ours.
    pub fn dominates(&self, other: &VectorClock) -> bool {
        other.0.iter().all(|(r, s)| self.get(*r) >= *s)
    }

    /// Total number of operations this clock covers.
    pub fn total(&self) -> u64 {
        self.0.values().sum()
    }
}

/// A fresh random replica id without pulling in a RNG crate.
pub fn random_replica_id() -> ReplicaId {
    use std::collections::hash_map::RandomState;
    use std::hash::BuildHasher;
    let mut h = RandomState::new().build_hasher();
    std::time::SystemTime::now().hash(&mut h);
    std::process::id().hash(&mut h);
    h.finish() | 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loose_parsing() {
        assert_eq!(Scalar::parse_loose("42"), Scalar::Int(42));
        assert_eq!(Scalar::parse_loose("1.5"), Scalar::Float(F64(1.5)));
        assert_eq!(Scalar::parse_loose("true"), Scalar::Bool(true));
        assert_eq!(Scalar::parse_loose("null"), Scalar::Null);
        assert_eq!(Scalar::parse_loose("hi there"), Scalar::Str("hi there".into()));
    }

    #[test]
    fn clock_queries() {
        let mut a = VectorClock::default();
        a.set(1, 3);
        a.set(2, 1);
        let mut b = VectorClock::default();
        b.set(1, 2);
        assert!(a.dominates(&b) && !b.dominates(&a));
        assert!(a.contains(OpId { replica: 1, seq: 3 }) && !a.contains(OpId { replica: 1, seq: 4 }));
        assert_eq!(a.total(), 4);
    }

    #[test]
    fn nan_is_a_well_behaved_set_member() {
        let n = Scalar::Float(F64(f64::NAN));
        assert_eq!(n, n.clone());
        assert!(Scalar::Float(F64(1.0)) < Scalar::Float(F64(2.0)));
    }
}
