use clap::{Parser, Subcommand};
use crdt_sync::store::Store;
use crdt_sync::{Doc, ObjId, ObjKind, Op, Scalar};

/// Local-first client. Edits go to a local sqlite file and work offline; `sync` exchanges
/// operations with the server; `watch` stays connected and prints every remote change.
#[derive(Parser)]
#[command(version)]
struct Args {
    /// Local sqlite database (holds this replica's id and its copy of each document).
    #[arg(long, default_value = "crdt-client.db")]
    db: std::path::PathBuf,
    /// Server base URL.
    #[arg(long, default_value = "ws://127.0.0.1:3030")]
    server: String,
    /// Document name.
    #[arg(long, default_value = "default")]
    doc: String,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Print the local copy as JSON.
    Show,
    /// Set a top-level key to a scalar (numbers, true/false and null are parsed).
    Set { key: String, value: String },
    /// Delete a top-level key.
    Del { key: String },
    /// Insert text into the text object under KEY (created if missing).
    Insert { key: String, index: usize, text: String },
    /// Delete COUNT characters from the text under KEY.
    Erase { key: String, index: usize, count: usize },
    /// Add a value to the set under KEY (created if missing).
    Add { key: String, value: String },
    /// Remove a value from the set under KEY.
    Remove { key: String, value: String },
    /// Exchange operations with the server once.
    Sync,
    /// Stay connected and print the document whenever a remote change arrives.
    Watch,
    /// Show replica id, clock and log size.
    Info,
}

type R<T> = Result<T, Box<dyn std::error::Error + Send + Sync>>;

fn edit(doc: &mut Doc, cmd: &Cmd) -> R<Vec<Op>> {
    let root = ObjId::Root;
    let mut ops = Vec::new();
    match cmd {
        Cmd::Set { key, value } => ops.push(doc.map_set(root, key, Scalar::parse_loose(value))?),
        Cmd::Del { key } => ops.push(doc.map_delete(root, key)?),
        Cmd::Insert { key, index, text } => {
            let (t, created) = doc.map_get_or_create(root, key, ObjKind::Text)?;
            ops.extend(created);
            ops.extend(doc.text_insert(t, *index, text)?);
        }
        Cmd::Erase { key, index, count } => {
            let t = doc.child(root, key, ObjKind::Text)?.ok_or(format!("{key} is not a text"))?;
            ops.extend(doc.text_delete(t, *index, *count)?);
        }
        Cmd::Add { key, value } => {
            let (s, created) = doc.map_get_or_create(root, key, ObjKind::Set)?;
            ops.extend(created);
            ops.push(doc.set_add(s, Scalar::parse_loose(value))?);
        }
        Cmd::Remove { key, value } => {
            let s = doc.child(root, key, ObjKind::Set)?.ok_or(format!("{key} is not a set"))?;
            ops.extend(doc.set_remove(s, Scalar::parse_loose(value))?);
        }
        _ => {}
    }
    Ok(ops)
}

async fn run(a: Args) -> R<()> {
    let mut store = Store::open(&a.db)?;
    let replica = store.replica_id()?;
    let mut doc = store.load_doc(&a.doc, replica)?;
    let url = format!("{}/sync/{}", a.server.trim_end_matches('/'), a.doc);
    match &a.cmd {
        Cmd::Show => println!("{}", serde_json::to_string_pretty(&doc.to_json())?),
        Cmd::Info => {
            println!("replica {replica:016x}");
            println!("operations {}", doc.log().len());
            for (r, s) in &doc.clock().0 {
                println!("  {r:016x}: {s}");
            }
        }
        Cmd::Sync => {
            let got = crdt_sync::client::sync_once(&url, &mut doc).await?;
            store.append(&a.doc, &got)?;
            println!("synced: received {} operation(s); {} total", got.len(), doc.log().len());
        }
        Cmd::Watch => {
            let (_tx, rx) = tokio::sync::mpsc::channel(1);
            println!("{}", doc.to_json());
            crdt_sync::client::live(&url, doc, rx, |d, ops| {
                if let Err(e) = store.append(&a.doc, ops) {
                    eprintln!("store: {e}");
                }
                println!("{}", d.to_json());
            })
            .await?;
        }
        other => {
            let ops = edit(&mut doc, other)?;
            store.append(&a.doc, &ops)?;
            println!("{}", serde_json::to_string_pretty(&doc.to_json())?);
        }
    }
    Ok(())
}

#[tokio::main]
async fn main() {
    if let Err(e) = run(Args::parse()).await {
        eprintln!("error: {e}");
        std::process::exit(1);
    }
}
