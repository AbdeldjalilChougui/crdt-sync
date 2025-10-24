//! Compact binary encoding for operations and sync messages.
//!
//! Integers are LEB128 varints (signed ones zigzag-encoded). A batch of operations starts
//! with a table of the replica ids it mentions (8 bytes each); every op id afterwards names
//! its replica by table index, usually one byte. Sequence numbers and Lamport times are
//! stored as deltas from the previous op in the batch, and "same object as the previous op"
//! and "insert after the previous op" have one-byte shortcuts, so typing a run of
//! characters costs about 7 bytes per character.
//!
//! ```text
//! ops      := varint(n_replicas) u64le* varint(n_ops) op*
//! op       := varint(ridx) zigzag(seq - prev.seq) zigzag(lamport - prev.lamport) obj action
//! obj      := 0 root | 1 opid | 2 same as previous op
//! opid     := varint(ridx) varint(seq)
//! action   := 0 str (0 | 1 scalar | 2 kind)      map set / delete / new object
//!           | 1 scalar                            register set
//!           | 2 scalar                            set add
//!           | 3 scalar varint(n) opid*            set remove
//!           | 4 (0 | 1 opid | 2) varint(char)     text insert (2 = after previous op)
//!           | 5 opid                              text delete
//! scalar   := 0 null | 1 false | 2 true | 3 zigzag(i64) | 4 f64le | 5 str
//! str      := varint(len) utf8
//! ```

use crate::doc::{Action, NewValue, Op};
use crate::types::{ObjId, ObjKind, OpId, ReplicaId, Scalar, VectorClock, F64};
use std::collections::HashMap;
use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DecodeError {
    Truncated,
    BadTag(&'static str, u8),
    BadReplicaIndex(u64),
    VarintOverflow,
    BadUtf8,
    BadChar(u64),
    TooLong,
    TrailingBytes,
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "decode error: {self:?}")
    }
}

impl std::error::Error for DecodeError {}

type Result<T> = std::result::Result<T, DecodeError>;

// ----- primitives -----

pub fn put_varint(out: &mut Vec<u8>, mut v: u64) {
    while v >= 0x80 {
        out.push((v as u8) | 0x80);
        v >>= 7;
    }
    out.push(v as u8);
}

fn zigzag(v: i64) -> u64 {
    ((v << 1) ^ (v >> 63)) as u64
}

fn unzigzag(v: u64) -> i64 {
    ((v >> 1) as i64) ^ -((v & 1) as i64)
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    put_varint(out, s.len() as u64);
    out.extend_from_slice(s.as_bytes());
}

pub struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    pub fn new(buf: &'a [u8]) -> Self {
        Reader { buf, pos: 0 }
    }

    pub fn is_empty(&self) -> bool {
        self.pos == self.buf.len()
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    pub fn u8(&mut self) -> Result<u8> {
        let b = *self.buf.get(self.pos).ok_or(DecodeError::Truncated)?;
        self.pos += 1;
        Ok(b)
    }

    fn bytes(&mut self, n: usize) -> Result<&'a [u8]> {
        if n > self.remaining() {
            return Err(DecodeError::Truncated);
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    pub fn varint(&mut self) -> Result<u64> {
        let mut v: u64 = 0;
        for shift in (0..64).step_by(7) {
            let b = self.u8()?;
            if shift == 63 && b > 1 {
                return Err(DecodeError::VarintOverflow);
            }
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
        }
        Err(DecodeError::VarintOverflow)
    }

    /// A count of items that each take at least one byte: reject counts the input
    /// cannot possibly hold, so a hostile length cannot trigger a huge allocation.
    pub fn count(&mut self) -> Result<usize> {
        let n = self.varint()?;
        if n > self.remaining() as u64 {
            return Err(DecodeError::TooLong);
        }
        Ok(n as usize)
    }

    fn u64le(&mut self) -> Result<u64> {
        Ok(u64::from_le_bytes(self.bytes(8)?.try_into().unwrap()))
    }

    fn str(&mut self) -> Result<String> {
        let n = self.count()?;
        String::from_utf8(self.bytes(n)?.to_vec()).map_err(|_| DecodeError::BadUtf8)
    }
}

// ----- scalars -----

fn put_scalar(out: &mut Vec<u8>, s: &Scalar) {
    match s {
        Scalar::Null => out.push(0),
        Scalar::Bool(false) => out.push(1),
        Scalar::Bool(true) => out.push(2),
        Scalar::Int(i) => {
            out.push(3);
            put_varint(out, zigzag(*i));
        }
        Scalar::Float(F64(f)) => {
            out.push(4);
            out.extend_from_slice(&f.to_le_bytes());
        }
        Scalar::Str(s) => {
            out.push(5);
            put_str(out, s);
        }
    }
}

fn get_scalar(r: &mut Reader) -> Result<Scalar> {
    Ok(match r.u8()? {
        0 => Scalar::Null,
        1 => Scalar::Bool(false),
        2 => Scalar::Bool(true),
        3 => Scalar::Int(unzigzag(r.varint()?)),
        4 => Scalar::Float(F64(f64::from_bits(r.u64le()?))),
        5 => Scalar::Str(r.str()?),
        t => return Err(DecodeError::BadTag("scalar", t)),
    })
}

fn kind_tag(k: ObjKind) -> u8 {
    match k {
        ObjKind::Map => 0,
        ObjKind::Register => 1,
        ObjKind::Set => 2,
        ObjKind::Text => 3,
    }
}

fn get_kind(r: &mut Reader) -> Result<ObjKind> {
    Ok(match r.u8()? {
        0 => ObjKind::Map,
        1 => ObjKind::Register,
        2 => ObjKind::Set,
        3 => ObjKind::Text,
        t => return Err(DecodeError::BadTag("kind", t)),
    })
}

// ----- operations -----

struct Table {
    ids: Vec<ReplicaId>,
    index: HashMap<ReplicaId, u64>,
}

impl Table {
    fn idx(&self, r: ReplicaId) -> u64 {
        self.index[&r]
    }
}

fn collect_replicas(ops: &[Op]) -> Table {
    let mut t = Table { ids: Vec::new(), index: HashMap::new() };
    let mut add = |r: ReplicaId| {
        if !t.index.contains_key(&r) {
            t.index.insert(r, t.ids.len() as u64);
            t.ids.push(r);
        }
    };
    for op in ops {
        add(op.id.replica);
        if let ObjId::Op(o) = op.obj {
            add(o.replica);
        }
        for d in op.deps() {
            add(d.replica);
        }
    }
    t
}

fn put_opid(out: &mut Vec<u8>, t: &Table, id: OpId) {
    put_varint(out, t.idx(id.replica));
    put_varint(out, id.seq);
}

fn get_opid(r: &mut Reader, t: &[ReplicaId]) -> Result<OpId> {
    let i = r.varint()?;
    let replica = *t.get(i as usize).ok_or(DecodeError::BadReplicaIndex(i))?;
    Ok(OpId { replica, seq: r.varint()? })
}

pub fn encode_ops(ops: &[Op]) -> Vec<u8> {
    let mut out = Vec::new();
    write_ops(&mut out, ops);
    out
}

pub fn write_ops(out: &mut Vec<u8>, ops: &[Op]) {
    let t = collect_replicas(ops);
    put_varint(out, t.ids.len() as u64);
    for r in &t.ids {
        out.extend_from_slice(&r.to_le_bytes());
    }
    put_varint(out, ops.len() as u64);
    let mut prev: Option<&Op> = None;
    for op in ops {
        let (pseq, plam) = prev.map_or((0, 0), |p| (p.id.seq, p.lamport));
        put_varint(out, t.idx(op.id.replica));
        put_varint(out, zigzag(op.id.seq.wrapping_sub(pseq) as i64));
        put_varint(out, zigzag(op.lamport.wrapping_sub(plam) as i64));
        match op.obj {
            _ if prev.is_some_and(|p| p.obj == op.obj) => out.push(2),
            ObjId::Root => out.push(0),
            ObjId::Op(o) => {
                out.push(1);
                put_opid(out, &t, o);
            }
        }
        match &op.action {
            Action::MapSet { key, value } => {
                out.push(0);
                put_str(out, key);
                match value {
                    None => out.push(0),
                    Some(NewValue::Scalar(s)) => {
                        out.push(1);
                        put_scalar(out, s);
                    }
                    Some(NewValue::Object(k)) => {
                        out.push(2);
                        out.push(kind_tag(*k));
                    }
                }
            }
            Action::RegSet(s) => {
                out.push(1);
                put_scalar(out, s);
            }
            Action::SetAdd(s) => {
                out.push(2);
                put_scalar(out, s);
            }
            Action::SetRemove { value, observed } => {
                out.push(3);
                put_scalar(out, value);
                put_varint(out, observed.len() as u64);
                for o in observed {
                    put_opid(out, &t, *o);
                }
            }
            Action::TextInsert { after, ch } => {
                out.push(4);
                match after {
                    None => out.push(0),
                    Some(a) if prev.is_some_and(|p| p.id == *a) => out.push(2),
                    Some(a) => {
                        out.push(1);
                        put_opid(out, &t, *a);
                    }
                }
                put_varint(out, u64::from(u32::from(*ch)));
            }
            Action::TextDelete { target } => {
                out.push(5);
                put_opid(out, &t, *target);
            }
        }
        prev = Some(op);
    }
}

pub fn decode_ops(b: &[u8]) -> Result<Vec<Op>> {
    let mut r = Reader::new(b);
    let ops = read_ops(&mut r)?;
    if !r.is_empty() {
        return Err(DecodeError::TrailingBytes);
    }
    Ok(ops)
}

pub fn read_ops(r: &mut Reader) -> Result<Vec<Op>> {
    let n = r.count()?;
    let mut table = Vec::with_capacity(n);
    for _ in 0..n {
        table.push(r.u64le()?);
    }
    let n = r.count()?;
    let mut ops: Vec<Op> = Vec::with_capacity(n);
    for _ in 0..n {
        let prev = ops.last();
        let (pseq, plam) = prev.map_or((0, 0), |p| (p.id.seq, p.lamport));
        let i = r.varint()?;
        let replica = *table.get(i as usize).ok_or(DecodeError::BadReplicaIndex(i))?;
        let seq = pseq.wrapping_add(unzigzag(r.varint()?) as u64);
        let id = OpId { replica, seq };
        let lamport = plam.wrapping_add(unzigzag(r.varint()?) as u64);
        let obj = match r.u8()? {
            0 => ObjId::Root,
            1 => ObjId::Op(get_opid(r, &table)?),
            2 => prev.ok_or(DecodeError::BadTag("obj", 2))?.obj,
            t => return Err(DecodeError::BadTag("obj", t)),
        };
        let prev_id = prev.map(|p| p.id);
        let action = match r.u8()? {
            0 => {
                let key = r.str()?;
                let value = match r.u8()? {
                    0 => None,
                    1 => Some(NewValue::Scalar(get_scalar(r)?)),
                    2 => Some(NewValue::Object(get_kind(r)?)),
                    t => return Err(DecodeError::BadTag("map value", t)),
                };
                Action::MapSet { key, value }
            }
            1 => Action::RegSet(get_scalar(r)?),
            2 => Action::SetAdd(get_scalar(r)?),
            3 => {
                let value = get_scalar(r)?;
                let k = r.count()?;
                let observed = (0..k).map(|_| get_opid(r, &table)).collect::<Result<_>>()?;
                Action::SetRemove { value, observed }
            }
            4 => {
                let after = match r.u8()? {
                    0 => None,
                    1 => Some(get_opid(r, &table)?),
                    2 => Some(prev_id.ok_or(DecodeError::BadTag("after", 2))?),
                    t => return Err(DecodeError::BadTag("after", t)),
                };
                let c = r.varint()?;
                let ch = u32::try_from(c).ok().and_then(char::from_u32).ok_or(DecodeError::BadChar(c))?;
                Action::TextInsert { after, ch }
            }
            5 => Action::TextDelete { target: get_opid(r, &table)? },
            t => return Err(DecodeError::BadTag("action", t)),
        };
        ops.push(Op { id, lamport, obj, action });
    }
    Ok(ops)
}

// ----- vector clocks -----

pub fn write_clock(out: &mut Vec<u8>, c: &VectorClock) {
    put_varint(out, c.0.len() as u64);
    for (r, s) in &c.0 {
        out.extend_from_slice(&r.to_le_bytes());
        put_varint(out, *s);
    }
}

pub fn read_clock(r: &mut Reader) -> Result<VectorClock> {
    let n = r.count()?;
    let mut c = VectorClock::default();
    for _ in 0..n {
        let rep = r.u64le()?;
        let seq = r.varint()?;
        c.set(rep, seq);
    }
    Ok(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Doc;

    #[test]
    fn varints_and_zigzag() {
        for v in [0u64, 1, 127, 128, 300, u32::MAX as u64, u64::MAX] {
            let mut b = Vec::new();
            put_varint(&mut b, v);
            assert_eq!(Reader::new(&b).varint(), Ok(v));
        }
        for v in [0i64, -1, 1, i64::MIN, i64::MAX] {
            assert_eq!(unzigzag(zigzag(v)), v);
        }
        assert_eq!(Reader::new(&[0xff; 11]).varint(), Err(DecodeError::VarintOverflow));
    }

    #[test]
    fn text_inserts_are_small() {
        let mut d = Doc::new(0x1234_5678_9abc_def0);
        let (t, _) = d.map_new(ObjId::Root, "t", ObjKind::Text).unwrap();
        let ops = d.text_insert(t, 0, &"x".repeat(1000)).unwrap();
        let bytes = encode_ops(&ops);
        assert!(bytes.len() <= 8 * 1000, "{} bytes for 1000 inserts", bytes.len());
        assert_eq!(decode_ops(&bytes).unwrap(), ops);
    }

    #[test]
    fn hostile_lengths_are_rejected_without_allocating() {
        let mut b = Vec::new();
        put_varint(&mut b, u64::MAX >> 1);
        assert_eq!(decode_ops(&b), Err(DecodeError::TooLong));
    }
}
