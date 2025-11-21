//! The websocket server with sqlite storage, driven by library clients and by the CLI.

use crdt_sync::client::{live, sync_once};
use crdt_sync::store::Store;
use crdt_sync::{Doc, ObjId, ObjKind};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::task::JoinHandle;

fn tmp(name: &str) -> PathBuf {
    let d = std::env::temp_dir().join(format!("crdt-sync-test-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&d);
    std::fs::create_dir_all(&d).unwrap();
    d
}

struct Server {
    addr: SocketAddr,
    task: JoinHandle<()>,
}

impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}

async fn start(db: &PathBuf) -> Server {
    let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = l.local_addr().unwrap();
    let store = Store::open(db).unwrap();
    let task = tokio::spawn(async move {
        let _ = crdt_sync::server::serve(l, store).await;
    });
    Server { addr, task }
}

fn url(s: &Server, doc: &str) -> String {
    format!("ws://{}/sync/{doc}", s.addr)
}

/// Plain HTTP GET, returning (status line, body).
async fn http_get(addr: SocketAddr, path: &str) -> (String, String) {
    let mut s = tokio::net::TcpStream::connect(addr).await.unwrap();
    s.write_all(format!("GET {path} HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n").as_bytes()).await.unwrap();
    let mut buf = String::new();
    s.read_to_string(&mut buf).await.unwrap();
    let (head, body) = buf.split_once("\r\n\r\n").unwrap();
    (head.lines().next().unwrap().to_string(), body.to_string())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn offline_edits_merge_through_the_server_and_persist() {
    let dir = tmp("merge");
    let db = dir.join("server.db");
    let srv = start(&db).await;
    let u = url(&srv, "notes");

    let mut a = Doc::new(1);
    let (t, _) = a.map_new(ObjId::Root, "body", ObjKind::Text).unwrap();
    a.text_insert(t, 0, "shared base").unwrap();
    sync_once(&u, &mut a).await.unwrap();
    let mut b = Doc::new(2);
    let got = sync_once(&u, &mut b).await.unwrap();
    assert_eq!(got.len(), a.log().len());

    // Both edit while "offline".
    a.text_insert(t, 0, "A: ").unwrap();
    a.map_set(ObjId::Root, "owner", "alice").unwrap();
    let (tags, _) = a.map_new(ObjId::Root, "tags", ObjKind::Set).unwrap();
    a.set_add(tags, "draft").unwrap();
    let len = b.text(t).unwrap().len();
    b.text_insert(t, len, " (edited by B)").unwrap();
    b.map_set(ObjId::Root, "reviewed", true).unwrap();

    sync_once(&u, &mut a).await.unwrap();
    sync_once(&u, &mut b).await.unwrap();
    sync_once(&u, &mut a).await.unwrap();
    assert_eq!(a.to_json(), b.to_json());
    assert_eq!(a.to_json()["body"], "A: shared base (edited by B)");
    let (status, body) = http_get(srv.addr, "/docs/notes").await;
    assert!(status.contains("200"), "{status}");
    assert_eq!(serde_json::from_str::<serde_json::Value>(&body).unwrap(), a.to_json());

    // Restart the server on the same database: a fresh client gets the same document.
    drop(srv);
    let srv = start(&db).await;
    let mut c = Doc::new(3);
    sync_once(&url(&srv, "notes"), &mut c).await.unwrap();
    assert_eq!(c.to_json(), a.to_json());
    let (_, list) = http_get(srv.addr, "/docs").await;
    assert_eq!(list, "[\"notes\"]");
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn live_clients_receive_relayed_operations() {
    let dir = tmp("live");
    let srv = start(&dir.join("s.db")).await;
    let u = url(&srv, "board");

    let mut a = Doc::new(10);
    a.map_set(ObjId::Root, "hello", "world").unwrap();
    sync_once(&u, &mut a).await.unwrap();

    // B stays connected and reports every change.
    let (changes_tx, mut changes) = tokio::sync::mpsc::unbounded_channel();
    let (local_tx, local_rx) = tokio::sync::mpsc::channel(8);
    let mut b_seed = Doc::new(20);
    sync_once(&u, &mut b_seed).await.unwrap();
    let b_task = tokio::spawn({
        let u = u.clone();
        let b = b_seed.clone();
        async move {
            live(&u, b, local_rx, move |d, _| {
                let _ = changes_tx.send(d.to_json());
            })
            .await
        }
    });

    a.map_set(ObjId::Root, "counter", 1).unwrap();
    sync_once(&u, &mut a).await.unwrap();
    let seen = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let j = changes.recv().await.unwrap();
            if j["counter"] == 1 {
                return j;
            }
        }
    })
    .await
    .expect("B never saw A's edit");
    assert_eq!(seen["hello"], "world");

    // B pushes a local edit over its live connection; A picks it up on the next sync.
    let op = b_seed.map_set(ObjId::Root, "from_b", "hi").unwrap();
    local_tx.send(vec![op]).await.unwrap();
    let ok = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            sync_once(&u, &mut a).await.unwrap();
            if a.to_json()["from_b"] == "hi" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(ok.is_ok(), "A never saw B's live edit");
    drop(local_tx);
    let _ = tokio::time::timeout(Duration::from_secs(2), b_task).await;
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test]
async fn rejects_bad_document_names() {
    let dir = tmp("names");
    let srv = start(&dir.join("s.db")).await;
    let (status, _) = http_get(srv.addr, "/docs/bad%20name").await;
    assert!(status.contains("400"), "{status}");
    let r = sync_once(&format!("ws://{}/sync/..%2fetc", srv.addr), &mut Doc::new(1)).await;
    assert!(r.is_err());
    let _ = std::fs::remove_dir_all(dir);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cli_clients_edit_offline_and_sync() {
    let dir = tmp("cli");
    let srv = start(&dir.join("server.db")).await;
    let server = format!("ws://{}", srv.addr);
    let cli = env!("CARGO_BIN_EXE_crdt-cli");
    let run = |db: &str, args: &[&str]| {
        let mut full = vec!["--db".to_string(), dir.join(db).display().to_string(), "--server".into(), server.clone(), "--doc".into(), "todo".into()];
        full.extend(args.iter().map(|s| s.to_string()));
        tokio::task::spawn_blocking({
            let cli = cli.to_string();
            move || {
                let out = std::process::Command::new(cli).args(&full).output().unwrap();
                assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
                String::from_utf8(out.stdout).unwrap()
            }
        })
    };
    run("alice.db", &["insert", "title", "0", "Groceries"]).await.unwrap();
    run("alice.db", &["add", "items", "milk"]).await.unwrap();
    run("alice.db", &["sync"]).await.unwrap();
    run("bob.db", &["sync"]).await.unwrap();
    // Offline on both sides.
    run("alice.db", &["add", "items", "eggs"]).await.unwrap();
    run("bob.db", &["add", "items", "bread"]).await.unwrap();
    run("bob.db", &["remove", "items", "milk"]).await.unwrap();
    run("bob.db", &["insert", "title", "9", " for Sunday"]).await.unwrap();
    run("bob.db", &["set", "budget", "25"]).await.unwrap();
    run("alice.db", &["sync"]).await.unwrap();
    run("bob.db", &["sync"]).await.unwrap();
    run("alice.db", &["sync"]).await.unwrap();
    let a: serde_json::Value = serde_json::from_str(&run("alice.db", &["show"]).await.unwrap()).unwrap();
    let b: serde_json::Value = serde_json::from_str(&run("bob.db", &["show"]).await.unwrap()).unwrap();
    assert_eq!(a, b);
    assert_eq!(a, serde_json::json!({"title": "Groceries for Sunday", "items": ["bread", "eggs"], "budget": 25}));
    let info = run("alice.db", &["info"]).await.unwrap();
    assert!(info.contains("operations"));
    let _ = std::fs::remove_dir_all(dir);
}
