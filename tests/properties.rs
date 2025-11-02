//! Property tests: replicas that receive the same operations converge, whatever the
//! interleaving of edits, delivery order and duplication; the encoding round-trips and
//! never panics on garbage.

use crdt_sync::encoding::{decode_ops, encode_ops};
use crdt_sync::sync::{sync_pair, Message};
use crdt_sync::{Doc, ObjId, ObjKind, Op, Scalar};
use proptest::prelude::*;

const REPLICAS: usize = 3;

/// One step of a random scenario.
#[derive(Debug, Clone)]
enum Step {
    /// Replica `r` makes a local edit chosen by `kind` and `arg`.
    Edit { r: usize, kind: u8, arg: u16, text: String },
    /// Replica `to` receives a random subset of replica `from`'s log, shuffled, some twice.
    Deliver { from: usize, to: usize, pick: Vec<u16> },
}

fn step() -> impl Strategy<Value = Step> {
    prop_oneof![
        3 => (0..REPLICAS, 0u8..9, any::<u16>(), "[a-c]{1,3}").prop_map(|(r, kind, arg, text)| Step::Edit { r, kind, arg, text }),
        2 => (0..REPLICAS, 0..REPLICAS, prop::collection::vec(any::<u16>(), 0..12))
            .prop_map(|(from, to, pick)| Step::Deliver { from, to, pick }),
    ]
}

/// Shared starting point: one replica creates the containers, everyone receives them.
fn setup() -> Vec<Doc> {
    let mut a = Doc::new(1);
    a.map_new(ObjId::Root, "text", ObjKind::Text).unwrap();
    a.map_new(ObjId::Root, "tags", ObjKind::Set).unwrap();
    a.map_new(ObjId::Root, "reg", ObjKind::Register).unwrap();
    let base = a.log().to_vec();
    let mut docs = vec![a];
    for r in 1..REPLICAS {
        let mut d = Doc::new(r as u64 + 1);
        d.apply_remote(base.clone());
        docs.push(d);
    }
    docs
}

fn obj(d: &Doc, key: &str, kind: ObjKind) -> ObjId {
    d.child(ObjId::Root, key, kind).unwrap().expect("container exists")
}

fn edit(d: &mut Doc, kind: u8, arg: u16, text: &str) {
    let a = arg as usize;
    match kind {
        0..=2 => {
            let t = obj(d, "text", ObjKind::Text);
            let len = d.text(t).unwrap().len();
            d.text_insert(t, a % (len + 1), text).unwrap();
        }
        3 => {
            let t = obj(d, "text", ObjKind::Text);
            let len = d.text(t).unwrap().len();
            if len > 0 {
                let at = a % len;
                d.text_delete(t, at, 1 + (a / 7) % (len - at).min(3)).unwrap();
            }
        }
        4 => {
            let s = obj(d, "tags", ObjKind::Set);
            d.set_add(s, text).unwrap();
        }
        5 => {
            let s = obj(d, "tags", ObjKind::Set);
            d.set_remove(s, text).unwrap();
        }
        6 => {
            let r = obj(d, "reg", ObjKind::Register);
            d.reg_set(r, arg as i64).unwrap();
        }
        7 => {
            d.map_set(ObjId::Root, &format!("k{}", a % 4), Scalar::Str(text.into())).unwrap();
        }
        _ => {
            d.map_delete(ObjId::Root, &format!("k{}", a % 4)).unwrap();
        }
    }
}

fn run(steps: &[Step], final_order_seed: u64) -> Vec<Doc> {
    let mut docs = setup();
    for s in steps {
        match s {
            Step::Edit { r, kind, arg, text } => edit(&mut docs[*r], *kind, *arg, text),
            Step::Deliver { from, to, pick } => {
                let log = docs[*from].log().to_vec();
                if log.is_empty() {
                    continue;
                }
                let batch: Vec<Op> = pick.iter().map(|i| log[*i as usize % log.len()].clone()).collect();
                docs[*to].apply_remote(batch);
            }
        }
    }
    // Finally everyone gets everything, in a seed-dependent scrambled order, twice.
    let all: Vec<Op> = docs.iter().flat_map(|d| d.log().to_vec()).collect();
    for (i, d) in docs.iter_mut().enumerate() {
        let mut ops = all.clone();
        let n = ops.len();
        let mut x = final_order_seed ^ (i as u64 + 1).wrapping_mul(0x9E3779B97F4A7C15);
        for k in (1..n).rev() {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            ops.swap(k, (x % (k as u64 + 1)) as usize);
        }
        ops.extend(all.iter().rev().cloned());
        d.apply_remote(ops);
    }
    docs
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(300))]

    #[test]
    fn replicas_converge(steps in prop::collection::vec(step(), 1..60), seed in any::<u64>()) {
        let docs = run(&steps, seed);
        for d in &docs {
            prop_assert_eq!(d.pending_len(), 0);
            prop_assert_eq!(d.clock(), docs[0].clock());
            prop_assert_eq!(d.to_json(), docs[0].to_json());
        }
    }

    #[test]
    fn state_vector_sync_converges(steps in prop::collection::vec(step(), 1..40)) {
        let mut docs = setup();
        for s in &steps {
            if let Step::Edit { r, kind, arg, text } = s {
                edit(&mut docs[*r], *kind, *arg, text);
            }
        }
        // Pairwise syncs in a ring, twice around.
        for _ in 0..2 {
            for i in 0..REPLICAS {
                let j = (i + 1) % REPLICAS;
                let (lo, hi) = docs.split_at_mut(j.max(i));
                let (a, b) = if i < j { (&mut lo[i], &mut hi[0]) } else { (&mut hi[0], &mut lo[j]) };
                sync_pair(a, b);
            }
        }
        for d in &docs {
            prop_assert_eq!(d.to_json(), docs[0].to_json());
            prop_assert_eq!(d.log().len(), docs[0].log().len());
        }
    }

    #[test]
    fn single_replica_text_matches_a_string_model(edits in prop::collection::vec((any::<bool>(), any::<u16>(), "[a-z]{1,4}"), 1..80)) {
        let mut d = Doc::new(1);
        let (t, _) = d.map_new(ObjId::Root, "t", ObjKind::Text).unwrap();
        let mut model: Vec<char> = Vec::new();
        for (ins, pos, s) in edits {
            if ins || model.is_empty() {
                let at = pos as usize % (model.len() + 1);
                d.text_insert(t, at, &s).unwrap();
                for (k, c) in s.chars().enumerate() {
                    model.insert(at + k, c);
                }
            } else {
                let at = pos as usize % model.len();
                d.text_delete(t, at, 1).unwrap();
                model.remove(at);
            }
        }
        prop_assert_eq!(d.text(t).unwrap().text(), model.into_iter().collect::<String>());
    }

    #[test]
    fn encoding_roundtrips(steps in prop::collection::vec(step(), 1..40)) {
        let docs = run(&steps, 1);
        let ops = docs[0].log().to_vec();
        prop_assert_eq!(decode_ops(&encode_ops(&ops)).unwrap(), ops.clone());
        let m = Message::Ops(ops);
        prop_assert_eq!(Message::decode(&m.encode()).unwrap(), m);
        let sv = Message::StateVector(docs[0].clock().clone());
        prop_assert_eq!(Message::decode(&sv.encode()).unwrap(), sv);
    }

    #[test]
    fn decoding_garbage_never_panics(bytes in prop::collection::vec(any::<u8>(), 0..200)) {
        let _ = decode_ops(&bytes);
        let _ = Message::decode(&bytes);
    }

    #[test]
    fn corrupting_a_valid_message_never_panics(steps in prop::collection::vec(step(), 1..20), flips in prop::collection::vec((any::<u16>(), any::<u8>()), 1..5)) {
        let docs = run(&steps, 2);
        let mut b = Message::Ops(docs[0].log().to_vec()).encode();
        for (at, x) in flips {
            let i = at as usize % b.len();
            b[i] ^= x;
        }
        if let Ok(Message::Ops(ops)) = Message::decode(&b) {
            // Even nonsense that decodes must not break a document.
            let mut d = Doc::new(99);
            d.apply_remote(ops);
            let _ = d.to_json();
        }
    }
}
