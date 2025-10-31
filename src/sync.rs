//! The sync protocol. Two messages, symmetric for both ends:
//!
//! - `StateVector(sv)`: "this is what I have". The receiver answers with `Ops` holding every
//!   operation not covered by `sv`, in causal order.
//! - `Ops(ops)`: operations to integrate. Duplicates and out-of-order arrivals are fine.
//!
//! On connect both sides send their state vector; after the two `Ops` answers each side
//! has everything the other had. From then on new local or relayed operations are pushed
//! as `Ops`. Messages are one byte of tag followed by the binary encoding.

use crate::doc::{Doc, Op};
use crate::encoding::{read_clock, read_ops, write_clock, write_ops, DecodeError, Reader};
use crate::types::VectorClock;

#[derive(Debug, Clone, PartialEq)]
pub enum Message {
    StateVector(VectorClock),
    Ops(Vec<Op>),
}

const TAG_SV: u8 = 1;
const TAG_OPS: u8 = 2;

impl Message {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Message::StateVector(sv) => {
                out.push(TAG_SV);
                write_clock(&mut out, sv);
            }
            Message::Ops(ops) => {
                out.push(TAG_OPS);
                write_ops(&mut out, ops);
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Message, DecodeError> {
        let mut r = Reader::new(b);
        let m = match r.u8()? {
            TAG_SV => Message::StateVector(read_clock(&mut r)?),
            TAG_OPS => Message::Ops(read_ops(&mut r)?),
            t => return Err(DecodeError::BadTag("message", t)),
        };
        if !r.is_empty() {
            return Err(DecodeError::TrailingBytes);
        }
        Ok(m)
    }
}

/// Handle one incoming message. Returns the reply to send (if any) and the operations that
/// were newly integrated (to persist and relay).
pub fn handle(doc: &mut Doc, msg: Message) -> (Option<Message>, Vec<Op>) {
    match msg {
        Message::StateVector(sv) => (Some(Message::Ops(doc.ops_since(&sv))), Vec::new()),
        Message::Ops(ops) => (None, doc.apply_remote(ops)),
    }
}

/// Run the full exchange between two in-memory documents (handy for tests and offline
/// merges): each sends its state vector, each answers with what the other lacks.
pub fn sync_pair(a: &mut Doc, b: &mut Doc) -> usize {
    let (to_b, _) = handle(a, Message::StateVector(b.clock().clone()));
    let (to_a, _) = handle(b, Message::StateVector(a.clock().clone()));
    let mut moved = 0;
    if let Some(Message::Ops(ops)) = to_b {
        moved += handle(b, Message::Ops(ops)).1.len();
    }
    if let Some(Message::Ops(ops)) = to_a {
        moved += handle(a, Message::Ops(ops)).1.len();
    }
    moved
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ObjId, ObjKind};

    #[test]
    fn messages_roundtrip() {
        let mut d = Doc::new(7);
        let (s, _) = d.map_new(ObjId::Root, "s", ObjKind::Set).unwrap();
        d.set_add(s, "x").unwrap();
        d.set_remove(s, "x").unwrap();
        for m in [Message::StateVector(d.clock().clone()), Message::Ops(d.log().to_vec())] {
            assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        }
        assert!(Message::decode(&[9]).is_err());
        assert!(Message::decode(&[]).is_err());
    }

    #[test]
    fn sync_pair_only_sends_the_difference() {
        let mut a = Doc::new(1);
        let mut b = Doc::new(2);
        a.map_set(ObjId::Root, "a", 1).unwrap();
        b.map_set(ObjId::Root, "b", 2).unwrap();
        assert_eq!(sync_pair(&mut a, &mut b), 2);
        assert_eq!(a.to_json(), b.to_json());
        assert_eq!(sync_pair(&mut a, &mut b), 0);
        a.map_set(ObjId::Root, "c", 3).unwrap();
        assert_eq!(sync_pair(&mut a, &mut b), 1);
    }
}
