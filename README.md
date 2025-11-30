# crdt-sync

A small local-first sync engine in Rust. Documents are trees of CRDTs (a last-writer-wins
register, an observed-remove set, RGA text and a last-writer-wins map) backed by an
operation log and vector clocks. Replicas edit offline, store operations in sqlite, and
sync through an axum websocket server with a two-message protocol: exchange state
vectors, then send the missing operations. Property tests check that replicas converge
under random interleavings, duplicates and out-of-order delivery.

## Who it is for

Developers who want to understand how local-first apps (Automerge, Yjs and friends) work
underneath, or who need a compact, readable starting point for syncing small documents
between devices and a server they control.

## Install

```sh
cargo install crdt-sync --locked   # crdt-server and crdt-cli binaries
cargo add crdt-sync                # library
```

## Quick start

```sh
cargo run --release --bin crdt-server -- --listen 127.0.0.1:3030 --db server.db

# two clients, each with its own local database (works offline)
alias c='cargo run -q --release --bin crdt-cli -- --server ws://127.0.0.1:3030 --doc notes'
c --db alice.db insert title 0 "Meeting notes"
c --db alice.db add tags work
c --db alice.db sync
c --db bob.db sync
c --db alice.db set status draft        # concurrent edits while "offline"
c --db bob.db set status final
c --db bob.db add tags urgent
c --db alice.db sync && c --db bob.db sync && c --db alice.db sync
c --db alice.db show
c --db bob.db watch                     # stay connected, print every remote change
curl 127.0.0.1:3030/docs/notes          # the server's copy as JSON
```

A real run of the commands above:

```
synced: received 0 operation(s); 16 total
synced: received 16 operation(s); 16 total
synced: received 0 operation(s); 17 total
synced: received 1 operation(s); 19 total
synced: received 2 operation(s); 19 total
{
  "status": "final",
  "tags": ["urgent", "work"],
  "title": "Meeting notes"
}
```

CLI commands: `show`, `info`, `set KEY VALUE`, `del KEY`, `insert KEY INDEX TEXT`,
`erase KEY INDEX COUNT`, `add KEY VALUE`, `remove KEY VALUE`, `sync`, `watch`.

As a library:

```rust
use crdt_sync::{Doc, ObjId, ObjKind, sync::sync_pair};

let mut a = Doc::new(1);
let (text, _) = a.map_new(ObjId::Root, "body", ObjKind::Text)?;
a.text_insert(text, 0, "hello")?;
let mut b = Doc::new(2);
sync_pair(&mut a, &mut b);            // state vectors, then missing ops
b.text_insert(text, 5, " world")?;
a.map_set(ObjId::Root, "title", "greeting")?;
sync_pair(&mut a, &mut b);
assert_eq!(a.to_json(), b.to_json()); // {"body":"hello world","title":"greeting"}
```

## How it works

**Operations.** Every edit is an `Op` with an id `(replica, seq)` where `seq` counts
1, 2, 3, ... per replica, a Lamport timestamp, a target object and an action. Because
sequence numbers have no gaps, a vector clock `replica → highest seq` says exactly which
operations a replica has, and "have I seen op X" is one comparison.

**Causal delivery.** `Doc::apply_remote` accepts operations in any order, any number of
times. Known ones are dropped; the rest wait in a pending list until their dependencies
are integrated: the previous op of the same replica, the object they target, the
character an insert goes after or a delete removes, and the tags a set-remove observed.

**CRDTs** (`src/crdt/`):

| type | rule |
|---|---|
| `LwwRegister` | highest (Lamport, replica) stamp wins |
| `LwwMap` | an LWW register per key; delete is a write of "nothing"; a value can be a scalar or a nested object created by that write |
| `OrSet` | add-wins: each add makes a unique tag, a remove deletes only the tags it observed |
| `Rga` | each character inserted after a reference character; concurrent inserts at the same spot ordered by stamp (highest first, skipping the higher-stamped subtrees); deletes leave tombstones |

**Encoding** (`src/encoding.rs`): LEB128 varints, a per-batch replica table so op ids
take one or two bytes, delta-coded sequence numbers and Lamport times, and one-byte
shortcuts for "same object as the previous op" and "insert after the previous op". Typing
1000 characters encodes to under 8 KB. The decoder rejects truncated or hostile input
without panicking or over-allocating.

**Sync protocol** (`src/sync.rs`): `StateVector(clock)` → answer with `Ops(missing)`;
`Ops(ops)` → integrate. Both ends send their state vector on connect, so one round trip
each way brings both up to date. The server then relays newly integrated operations to
every other client on the same document.

**Server** (`src/server.rs`): axum, `GET /sync/{doc}` websocket (binary frames),
`GET /docs` and `GET /docs/{doc}` JSON. One in-memory `Doc` per open document, loaded
from sqlite on first use; integrated ops are appended to sqlite before being relayed.
**Storage** (`src/store.rs`): table `ops(doc, replica, seq, body)` with the encoded op,
`INSERT OR IGNORE` so saving is idempotent, plus the local replica id.

## Tests

`cargo test` runs 17 unit tests (each CRDT, document editing, buffering of reordered and
duplicated ops, varints, encoding size, sqlite round trip, protocol messages), 6
proptest properties in `tests/properties.rs` and 4 end-to-end tests in
`tests/server.rs`:

- **Convergence**: three replicas perform random text inserts/deletes, set add/remove,
  register writes and map set/delete, while random subsets of each other's logs are
  delivered shuffled and duplicated; finally everyone receives everything in a scrambled
  order, twice. All replicas must have no pending ops, equal clocks and equal JSON
  (300 cases).
- **Protocol convergence**: random edits, then pairwise state-vector syncs in a ring.
- Single-replica text matches a `Vec<char>` model; encoding round-trips; random bytes
  and bit-flipped valid messages never panic the decoder or a document.
- Server: offline edits from two clients merge, the server's JSON matches, and a
  restarted server serves the same document from sqlite; a live client receives relayed
  edits and pushes its own; bad document names are rejected; two `crdt-cli` processes edit
  offline and converge.

As a sanity check of the properties, breaking the LWW tie-break or the RGA sibling
ordering makes the convergence properties fail within a few cases.

## Status / limitations

- The server has no authentication or authorization; anyone who can reach it can read and
  write every document. Run it on localhost or behind an authenticating proxy.
- Logs grow forever: no compaction, snapshots or tombstone garbage collection, and a
  document is rebuilt by replaying its whole log.
- RGA lookups are linear scans (fine for notes, slow for books); one op per character.
- No rich text, lists of objects, counters or moves. Concurrent creation of a nested
  object under the same key keeps one object (LWW); edits made to the other are hidden.
- Ops from a buggy peer that target a missing object or the wrong kind are logged and
  ignored; there is no validation of who may write which replica id.
- `watch` only prints remote changes; edits are made with separate CLI invocations.

## License

MIT, see [LICENSE](LICENSE).
