//! sqlite persistence: an append-only table of encoded operations per document, plus a
//! small key/value table (the local replica id lives there).

use crate::doc::{Doc, Op};
use crate::encoding::{decode_ops, encode_ops};
use crate::types::{random_replica_id, ReplicaId};
use rusqlite::{params, Connection, OptionalExtension};
use std::path::Path;

pub struct Store {
    conn: Connection,
}

pub type Result<T> = rusqlite::Result<T>;

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Store> {
        Store::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Store> {
        Store::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Store> {
        conn.execute_batch(
            "PRAGMA journal_mode = WAL;
             CREATE TABLE IF NOT EXISTS ops (
                 doc     TEXT    NOT NULL,
                 replica INTEGER NOT NULL,
                 seq     INTEGER NOT NULL,
                 body    BLOB    NOT NULL,
                 PRIMARY KEY (doc, replica, seq)
             );
             CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Store { conn })
    }

    /// Append operations (already integrated, in integration order). Re-inserting an op is a
    /// no-op, so callers do not have to track what was saved.
    pub fn append(&mut self, doc: &str, ops: &[Op]) -> Result<()> {
        let tx = self.conn.transaction()?;
        {
            let mut st = tx.prepare_cached("INSERT OR IGNORE INTO ops (doc, replica, seq, body) VALUES (?1, ?2, ?3, ?4)")?;
            for op in ops {
                // sqlite integers are signed 64-bit; store the replica id's bits as i64.
                st.execute(params![doc, op.id.replica as i64, op.id.seq as i64, encode_ops(std::slice::from_ref(op))])?;
            }
        }
        tx.commit()
    }

    /// All operations of a document in the order they were saved.
    pub fn load_ops(&self, doc: &str) -> Result<Vec<Op>> {
        let mut st = self.conn.prepare_cached("SELECT body FROM ops WHERE doc = ?1 ORDER BY rowid")?;
        let rows = st.query_map([doc], |r| r.get::<_, Vec<u8>>(0))?;
        let mut out = Vec::new();
        for body in rows {
            let body = body?;
            let ops = decode_ops(&body).map_err(|e| rusqlite::Error::ToSqlConversionFailure(Box::new(e)))?;
            out.extend(ops);
        }
        Ok(out)
    }

    /// Rebuild a document from its stored operations.
    pub fn load_doc(&self, doc: &str, replica: ReplicaId) -> Result<Doc> {
        let mut d = Doc::new(replica);
        d.apply_remote(self.load_ops(doc)?);
        Ok(d)
    }

    pub fn docs(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT doc FROM ops ORDER BY doc")?;
        let rows = st.query_map([], |r| r.get(0))?;
        rows.collect()
    }

    pub fn meta(&self, key: &str) -> Result<Option<String>> {
        self.conn.query_row("SELECT value FROM meta WHERE key = ?1", [key], |r| r.get(0)).optional()
    }

    pub fn set_meta(&self, key: &str, value: &str) -> Result<()> {
        self.conn.execute("INSERT OR REPLACE INTO meta (key, value) VALUES (?1, ?2)", [key, value])?;
        Ok(())
    }

    /// This database's replica id, created on first use.
    pub fn replica_id(&self) -> Result<ReplicaId> {
        if let Some(v) = self.meta("replica_id")? {
            if let Ok(id) = u64::from_str_radix(&v, 16) {
                return Ok(id);
            }
        }
        let id = random_replica_id();
        self.set_meta("replica_id", &format!("{id:016x}"))?;
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{ObjId, ObjKind};

    #[test]
    fn save_reload_and_dedupe() {
        let mut s = Store::open_in_memory().unwrap();
        let id = s.replica_id().unwrap();
        assert_eq!(s.replica_id().unwrap(), id);
        let mut d = Doc::new(id);
        let (t, _) = d.map_new(ObjId::Root, "t", ObjKind::Text).unwrap();
        d.text_insert(t, 0, "persist me").unwrap();
        s.append("notes", d.log()).unwrap();
        s.append("notes", d.log()).unwrap(); // idempotent
        assert_eq!(s.load_ops("notes").unwrap().len(), d.log().len());
        let back = s.load_doc("notes", id).unwrap();
        assert_eq!(back.to_json(), d.to_json());
        assert_eq!(back.clock(), d.clock());
        assert_eq!(s.docs().unwrap(), vec!["notes".to_string()]);
        assert!(s.load_ops("other").unwrap().is_empty());
    }
}
