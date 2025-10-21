//! A document: a tree of CRDT objects rooted at a map, plus the operation log.
//!
//! Local edits create operations that are applied immediately and appended to the log.
//! Remote operations go through [`Doc::apply_remote`], which drops duplicates and buffers
//! an operation until everything it depends on has been integrated (the previous op of the
//! same replica, the object it targets, and any element or tag it references). That gives
//! causal delivery on top of any network, including duplicated and reordered messages.

use crate::crdt::{LwwMap, LwwRegister, OrSet, Rga};
use crate::types::{ObjId, ObjKind, OpId, ReplicaId, Scalar, Stamp, Value, VectorClock};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq)]
pub enum Action {
    /// Write a scalar, a fresh nested object, or (with `None`) delete a key.
    MapSet { key: String, value: Option<NewValue> },
    RegSet(Scalar),
    SetAdd(Scalar),
    SetRemove { value: Scalar, observed: Vec<OpId> },
    TextInsert { after: Option<OpId>, ch: char },
    TextDelete { target: OpId },
}

/// The value written by `MapSet`: a scalar, or a new object whose id is this op's id.
#[derive(Debug, Clone, PartialEq)]
pub enum NewValue {
    Scalar(Scalar),
    Object(ObjKind),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Op {
    pub id: OpId,
    pub lamport: u64,
    pub obj: ObjId,
    pub action: Action,
}

impl Op {
    pub fn stamp(&self) -> Stamp {
        Stamp { lamport: self.lamport, replica: self.id.replica }
    }

    /// Operations that must be integrated before this one.
    pub fn deps(&self) -> Vec<OpId> {
        let mut d = Vec::new();
        if self.id.seq > 1 {
            d.push(OpId { replica: self.id.replica, seq: self.id.seq - 1 });
        }
        if let ObjId::Op(o) = self.obj {
            d.push(o);
        }
        match &self.action {
            Action::TextInsert { after: Some(a), .. } => d.push(*a),
            Action::TextDelete { target } => d.push(*target),
            Action::SetRemove { observed, .. } => d.extend(observed),
            _ => {}
        }
        d
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Object {
    Map(LwwMap),
    Register(LwwRegister),
    Set(OrSet),
    Text(Rga),
}

impl Object {
    fn new(kind: ObjKind) -> Object {
        match kind {
            ObjKind::Map => Object::Map(LwwMap::default()),
            ObjKind::Register => Object::Register(LwwRegister::default()),
            ObjKind::Set => Object::Set(OrSet::default()),
            ObjKind::Text => Object::Text(Rga::default()),
        }
    }

    pub fn kind(&self) -> ObjKind {
        match self {
            Object::Map(_) => ObjKind::Map,
            Object::Register(_) => ObjKind::Register,
            Object::Set(_) => ObjKind::Set,
            Object::Text(_) => ObjKind::Text,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DocError {
    NoSuchObject(ObjId),
    WrongKind { expected: ObjKind, found: ObjKind },
    OutOfRange { index: usize, len: usize },
}

impl fmt::Display for DocError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DocError::NoSuchObject(o) => write!(f, "no such object {o:?}"),
            DocError::WrongKind { expected, found } => write!(f, "expected a {expected:?}, found a {found:?}"),
            DocError::OutOfRange { index, len } => write!(f, "index {index} out of range (length {len})"),
        }
    }
}

impl std::error::Error for DocError {}

pub type Result<T> = std::result::Result<T, DocError>;

#[derive(Debug, Clone)]
pub struct Doc {
    replica: ReplicaId,
    lamport: u64,
    clock: VectorClock,
    log: Vec<Op>,
    pending: Vec<Op>,
    objects: HashMap<ObjId, Object>,
}

impl Doc {
    pub fn new(replica: ReplicaId) -> Doc {
        let mut objects = HashMap::new();
        objects.insert(ObjId::Root, Object::Map(LwwMap::default()));
        Doc { replica, lamport: 0, clock: VectorClock::default(), log: Vec::new(), pending: Vec::new(), objects }
    }

    pub fn replica(&self) -> ReplicaId {
        self.replica
    }

    /// The state vector: what this document has integrated, per replica.
    pub fn clock(&self) -> &VectorClock {
        &self.clock
    }

    /// Every integrated operation, in a causal order.
    pub fn log(&self) -> &[Op] {
        &self.log
    }

    /// Operations received but still waiting for their dependencies.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Operations the holder of `sv` has not seen, in causal order.
    pub fn ops_since(&self, sv: &VectorClock) -> Vec<Op> {
        self.log.iter().filter(|op| !sv.contains(op.id)).cloned().collect()
    }

    // ----- reading -----

    pub fn object(&self, obj: ObjId) -> Result<&Object> {
        self.objects.get(&obj).ok_or(DocError::NoSuchObject(obj))
    }

    fn typed<'a, T>(&'a self, obj: ObjId, kind: ObjKind, pick: impl Fn(&'a Object) -> Option<&'a T>) -> Result<&'a T> {
        let o = self.object(obj)?;
        pick(o).ok_or(DocError::WrongKind { expected: kind, found: o.kind() })
    }

    pub fn map(&self, obj: ObjId) -> Result<&LwwMap> {
        self.typed(obj, ObjKind::Map, |o| if let Object::Map(m) = o { Some(m) } else { None })
    }

    pub fn register(&self, obj: ObjId) -> Result<&LwwRegister> {
        self.typed(obj, ObjKind::Register, |o| if let Object::Register(m) = o { Some(m) } else { None })
    }

    pub fn set(&self, obj: ObjId) -> Result<&OrSet> {
        self.typed(obj, ObjKind::Set, |o| if let Object::Set(m) = o { Some(m) } else { None })
    }

    pub fn text(&self, obj: ObjId) -> Result<&Rga> {
        self.typed(obj, ObjKind::Text, |o| if let Object::Text(m) = o { Some(m) } else { None })
    }

    pub fn get(&self, obj: ObjId, key: &str) -> Result<Option<&Value>> {
        Ok(self.map(obj)?.get(key))
    }

    /// The nested object under `key`, if it is one of kind `kind`.
    pub fn child(&self, obj: ObjId, key: &str, kind: ObjKind) -> Result<Option<ObjId>> {
        Ok(match self.get(obj, key)? {
            Some(Value::Obj(id, k)) if *k == kind => Some(*id),
            _ => None,
        })
    }

    /// The whole document as JSON (text → string, set → sorted array, register → value).
    pub fn to_json(&self) -> serde_json::Value {
        self.obj_json(ObjId::Root)
    }

    fn obj_json(&self, obj: ObjId) -> serde_json::Value {
        match self.objects.get(&obj) {
            None => serde_json::Value::Null,
            Some(Object::Map(m)) => {
                let mut out = serde_json::Map::new();
                for k in m.keys() {
                    let v = match m.get(k) {
                        Some(Value::Scalar(s)) => s.to_json(),
                        Some(Value::Obj(id, _)) => self.obj_json(*id),
                        None => continue,
                    };
                    out.insert(k.to_string(), v);
                }
                serde_json::Value::Object(out)
            }
            Some(Object::Register(r)) => r.get().map_or(serde_json::Value::Null, Scalar::to_json),
            Some(Object::Set(s)) => serde_json::Value::Array(s.values().iter().map(Scalar::to_json).collect()),
            Some(Object::Text(t)) => t.text().into(),
        }
    }

    // ----- local edits -----

    fn local(&mut self, obj: ObjId, action: Action) -> Op {
        let op = Op {
            id: OpId { replica: self.replica, seq: self.clock.get(self.replica) + 1 },
            lamport: self.lamport.saturating_add(1),
            obj,
            action,
        };
        self.integrate(op.clone());
        op
    }

    pub fn map_set(&mut self, obj: ObjId, key: &str, value: impl Into<Scalar>) -> Result<Op> {
        self.map(obj)?;
        Ok(self.local(obj, Action::MapSet { key: key.into(), value: Some(NewValue::Scalar(value.into())) }))
    }

    /// Create a nested object under `key`; returns its id.
    pub fn map_new(&mut self, obj: ObjId, key: &str, kind: ObjKind) -> Result<(ObjId, Op)> {
        self.map(obj)?;
        let op = self.local(obj, Action::MapSet { key: key.into(), value: Some(NewValue::Object(kind)) });
        Ok((ObjId::Op(op.id), op))
    }

    /// The child object under `key`, created if missing (or holding something else).
    pub fn map_get_or_create(&mut self, obj: ObjId, key: &str, kind: ObjKind) -> Result<(ObjId, Option<Op>)> {
        match self.child(obj, key, kind)? {
            Some(id) => Ok((id, None)),
            None => self.map_new(obj, key, kind).map(|(id, op)| (id, Some(op))),
        }
    }

    pub fn map_delete(&mut self, obj: ObjId, key: &str) -> Result<Op> {
        self.map(obj)?;
        Ok(self.local(obj, Action::MapSet { key: key.into(), value: None }))
    }

    pub fn reg_set(&mut self, obj: ObjId, value: impl Into<Scalar>) -> Result<Op> {
        self.register(obj)?;
        Ok(self.local(obj, Action::RegSet(value.into())))
    }

    pub fn set_add(&mut self, obj: ObjId, value: impl Into<Scalar>) -> Result<Op> {
        self.set(obj)?;
        Ok(self.local(obj, Action::SetAdd(value.into())))
    }

    /// Remove `value` as observed here; returns `None` if it is not in the set.
    pub fn set_remove(&mut self, obj: ObjId, value: impl Into<Scalar>) -> Result<Option<Op>> {
        let value = value.into();
        let observed = self.set(obj)?.observed(&value);
        if observed.is_empty() {
            return Ok(None);
        }
        Ok(Some(self.local(obj, Action::SetRemove { value, observed })))
    }

    /// Insert `s` so that it starts at visible position `index`.
    pub fn text_insert(&mut self, obj: ObjId, index: usize, s: &str) -> Result<Vec<Op>> {
        let len = self.text(obj)?.len();
        if index > len {
            return Err(DocError::OutOfRange { index, len });
        }
        let mut after = if index == 0 { None } else { self.text(obj)?.id_at(index - 1) };
        let mut ops = Vec::new();
        for ch in s.chars() {
            let op = self.local(obj, Action::TextInsert { after, ch });
            after = Some(op.id);
            ops.push(op);
        }
        Ok(ops)
    }

    pub fn text_delete(&mut self, obj: ObjId, index: usize, count: usize) -> Result<Vec<Op>> {
        let len = self.text(obj)?.len();
        if index + count > len {
            return Err(DocError::OutOfRange { index: index + count, len });
        }
        let targets: Vec<OpId> = (index..index + count).filter_map(|i| self.text(obj).ok()?.id_at(i)).collect();
        Ok(targets.into_iter().map(|target| self.local(obj, Action::TextDelete { target })).collect())
    }

    // ----- remote operations -----

    /// Integrate operations from elsewhere, in any order, with duplicates. Returns the ones
    /// newly integrated (in integration order); the rest are already known or pending.
    pub fn apply_remote(&mut self, ops: impl IntoIterator<Item = Op>) -> Vec<Op> {
        for op in ops {
            if self.clock.contains(op.id) || self.pending.iter().any(|p| p.id == op.id) {
                continue;
            }
            self.pending.push(op);
        }
        let mut applied = Vec::new();
        loop {
            let ready = self.pending.iter().position(|op| {
                self.clock.get(op.id.replica) + 1 == op.id.seq && op.deps().iter().all(|d| self.clock.contains(*d))
            });
            let Some(i) = ready else { break };
            let op = self.pending.swap_remove(i);
            self.integrate(op.clone());
            applied.push(op);
        }
        applied
    }

    fn integrate(&mut self, op: Op) {
        self.lamport = self.lamport.max(op.lamport);
        self.clock.set(op.id.replica, op.id.seq);
        let stamp = op.stamp();
        if let Action::MapSet { value: Some(NewValue::Object(kind)), .. } = &op.action {
            self.objects.insert(ObjId::Op(op.id), Object::new(*kind));
        }
        // An op aimed at a missing object or one of the wrong kind can only come from a
        // buggy peer. It is still logged (sequence numbers must stay contiguous) but ignored.
        match (self.objects.get_mut(&op.obj), &op.action) {
            (Some(Object::Map(m)), Action::MapSet { key, value }) => {
                let v = value.as_ref().map(|nv| match nv {
                    NewValue::Scalar(s) => Value::Scalar(s.clone()),
                    NewValue::Object(k) => Value::Obj(ObjId::Op(op.id), *k),
                });
                m.apply_set(stamp, key, v);
            }
            (Some(Object::Register(r)), Action::RegSet(v)) => r.apply_set(stamp, v.clone()),
            (Some(Object::Set(s)), Action::SetAdd(v)) => s.apply_add(op.id, v.clone()),
            (Some(Object::Set(s)), Action::SetRemove { value, observed }) => s.apply_remove(value, observed),
            (Some(Object::Text(t)), Action::TextInsert { after, ch }) => t.apply_insert(op.id, stamp, *after, *ch),
            (Some(Object::Text(t)), Action::TextDelete { target }) => t.apply_delete(*target),
            _ => {}
        }
        self.log.push(op);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn local_editing_and_json() {
        let mut d = Doc::new(1);
        d.map_set(ObjId::Root, "title", "notes").unwrap();
        let (t, _) = d.map_new(ObjId::Root, "body", ObjKind::Text).unwrap();
        d.text_insert(t, 0, "helo").unwrap();
        d.text_insert(t, 3, "l").unwrap();
        d.text_insert(t, 5, " world").unwrap();
        d.text_delete(t, 0, 1).unwrap();
        d.text_insert(t, 0, "H").unwrap();
        let (s, _) = d.map_new(ObjId::Root, "tags", ObjKind::Set).unwrap();
        d.set_add(s, "b").unwrap();
        d.set_add(s, "a").unwrap();
        d.set_remove(s, "b").unwrap();
        let (r, _) = d.map_new(ObjId::Root, "n", ObjKind::Register).unwrap();
        d.reg_set(r, 5).unwrap();
        d.reg_set(r, 6).unwrap();
        assert_eq!(d.to_json(), json!({"title": "notes", "body": "Hello world", "tags": ["a"], "n": 6}));
        assert_eq!(d.clock().get(1), d.log().len() as u64);
        assert_eq!(d.text_insert(t, 99, "x"), Err(DocError::OutOfRange { index: 99, len: 11 }));
        assert!(matches!(d.reg_set(t, 1), Err(DocError::WrongKind { .. })));
    }

    #[test]
    fn out_of_order_and_duplicate_delivery_is_buffered() {
        let mut a = Doc::new(1);
        let (t, _) = a.map_new(ObjId::Root, "t", ObjKind::Text).unwrap();
        a.text_insert(t, 0, "abc").unwrap();
        let mut ops = a.log().to_vec();
        ops.reverse();
        ops.extend(a.log().iter().cloned()); // and everything twice
        let mut b = Doc::new(2);
        let first = b.apply_remote(ops[..2].to_vec()); // 'c' and 'b' only: not ready
        assert!(first.is_empty());
        assert_eq!(b.pending_len(), 2);
        let rest = b.apply_remote(ops);
        assert_eq!(rest.len(), 4);
        assert_eq!(b.pending_len(), 0);
        assert_eq!(b.to_json(), a.to_json());
        assert_eq!(b.clock(), a.clock());
    }

    #[test]
    fn concurrent_edits_converge_and_lamport_advances() {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        let (t, op) = a.map_new(ObjId::Root, "t", ObjKind::Text).unwrap();
        b.apply_remote([op]);
        a.text_insert(t, 0, "AAA").unwrap();
        b.text_insert(t, 0, "BBB").unwrap();
        a.map_set(ObjId::Root, "k", "from a").unwrap();
        b.map_set(ObjId::Root, "k", "from b").unwrap();
        let (fa, fb) = (a.ops_since(b.clock()), b.ops_since(a.clock()));
        a.apply_remote(fb);
        b.apply_remote(fa);
        assert_eq!(a.to_json(), b.to_json());
        let text = a.to_json()["t"].as_str().unwrap().to_string();
        assert!(text == "AAABBB" || text == "BBBAAA", "runs interleaved: {text}");
        // A new op from a is stamped after everything it has seen.
        let op = a.map_set(ObjId::Root, "x", 1).unwrap();
        assert!(b.log().iter().all(|o| o.lamport < op.lamport));
    }
}
