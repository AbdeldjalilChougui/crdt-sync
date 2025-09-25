//! A local-first sync engine: CRDTs (LWW register, OR-set, RGA text, LWW map) inside a
//! document with an operation log and vector clocks, a compact binary encoding, a
//! state-vector sync protocol, sqlite storage, an axum websocket server and a CLI client.

pub mod client;
pub mod crdt;
pub mod doc;
pub mod encoding;
pub mod server;
pub mod store;
pub mod sync;
pub mod types;

pub use doc::{Action, Doc, DocError, NewValue, Op};
pub use types::{ObjId, ObjKind, OpId, ReplicaId, Scalar, Value, VectorClock};
