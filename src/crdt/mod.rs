//! The four CRDTs. Each is a plain state machine: `apply_*` methods integrate an operation
//! and must give the same state for any delivery order that respects causality (which the
//! document guarantees). They know nothing about networking or storage.

pub mod map;
pub mod orset;
pub mod register;
pub mod rga;

pub use map::LwwMap;
pub use orset::OrSet;
pub use register::LwwRegister;
pub use rga::Rga;
