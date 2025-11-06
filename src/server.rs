//! axum server: one websocket endpoint per document, plus read-only JSON views.
//!
//! - `GET /sync/{doc}` (websocket, binary frames): the sync protocol from [`crate::sync`].
//!   Operations a client sends are integrated, saved to sqlite and relayed to every other
//!   client connected to the same document.
//! - `GET /docs` lists documents, `GET /docs/{doc}` returns a document as JSON.

use crate::doc::Doc;
use crate::store::Store;
use crate::sync::{self, Message};
use axum::extract::ws::{self, WebSocket, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use futures_util::{SinkExt, StreamExt};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Arc, Mutex};
use tokio::sync::broadcast;

struct Room {
    doc: Mutex<Doc>,
    /// (sender connection id, encoded `Ops` message)
    tx: broadcast::Sender<(u64, Arc<Vec<u8>>)>,
}

pub struct AppState {
    store: Mutex<Store>,
    rooms: Mutex<HashMap<String, Arc<Room>>>,
    next_conn: AtomicU64,
}

impl AppState {
    pub fn new(store: Store) -> Arc<AppState> {
        Arc::new(AppState { store: Mutex::new(store), rooms: Mutex::new(HashMap::new()), next_conn: AtomicU64::new(1) })
    }

    fn room(&self, name: &str) -> Result<Arc<Room>, String> {
        let mut rooms = self.rooms.lock().unwrap();
        if let Some(r) = rooms.get(name) {
            return Ok(r.clone());
        }
        // The server never authors operations; its replica id is never used in an op.
        let doc = self.store.lock().unwrap().load_doc(name, 0).map_err(|e| e.to_string())?;
        let (tx, _) = broadcast::channel(1024);
        let room = Arc::new(Room { doc: Mutex::new(doc), tx });
        rooms.insert(name.to_string(), room.clone());
        Ok(room)
    }
}

pub fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/sync/{doc}", get(ws_handler))
        .route("/docs", get(list_docs))
        .route("/docs/{doc}", get(show_doc))
        .with_state(state)
}

fn valid_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && name.chars().all(|c| c.is_ascii_alphanumeric() || "-_.".contains(c))
}

async fn list_docs(State(st): State<Arc<AppState>>) -> Response {
    match st.store.lock().unwrap().docs() {
        Ok(d) => Json(d).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e.to_string()).into_response(),
    }
}

async fn show_doc(State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    if !valid_name(&name) {
        return (axum::http::StatusCode::BAD_REQUEST, "bad document name").into_response();
    }
    match st.room(&name) {
        Ok(room) => Json(room.doc.lock().unwrap().to_json()).into_response(),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn ws_handler(ws: WebSocketUpgrade, State(st): State<Arc<AppState>>, Path(name): Path<String>) -> Response {
    if !valid_name(&name) {
        return (axum::http::StatusCode::BAD_REQUEST, "bad document name").into_response();
    }
    match st.room(&name) {
        Ok(room) => ws.on_upgrade(move |socket| session(socket, st, name, room)),
        Err(e) => (axum::http::StatusCode::INTERNAL_SERVER_ERROR, e).into_response(),
    }
}

async fn session(socket: WebSocket, st: Arc<AppState>, name: String, room: Arc<Room>) {
    let conn = st.next_conn.fetch_add(1, Relaxed);
    let mut rx = room.tx.subscribe();
    let (mut sink, mut stream) = socket.split();
    let hello = Message::StateVector(room.doc.lock().unwrap().clock().clone()).encode();
    if sink.send(ws::Message::Binary(hello.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            incoming = stream.next() => {
                let bytes = match incoming {
                    Some(Ok(ws::Message::Binary(b))) => b,
                    Some(Ok(ws::Message::Close(_))) | None | Some(Err(_)) => break,
                    Some(Ok(_)) => continue,
                };
                let Ok(msg) = Message::decode(&bytes) else { break };
                let (reply, applied) = {
                    let mut doc = room.doc.lock().unwrap();
                    sync::handle(&mut doc, msg)
                };
                if !applied.is_empty() {
                    if let Err(e) = st.store.lock().unwrap().append(&name, &applied) {
                        eprintln!("store: {e}");
                    }
                    let _ = room.tx.send((conn, Arc::new(Message::Ops(applied).encode())));
                }
                if let Some(r) = reply {
                    if sink.send(ws::Message::Binary(r.encode().into())).await.is_err() {
                        break;
                    }
                }
            }
            relayed = rx.recv() => {
                let bytes = match relayed {
                    Ok((from, _)) if from == conn => continue,
                    Ok((_, b)) => b.as_ref().clone(),
                    // We fell behind: resend the whole log; the client drops duplicates.
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        Message::Ops(room.doc.lock().unwrap().log().to_vec()).encode()
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                };
                if sink.send(ws::Message::Binary(bytes.into())).await.is_err() {
                    break;
                }
            }
        }
    }
}

/// Serve until the listener fails.
pub async fn serve(listener: tokio::net::TcpListener, store: Store) -> std::io::Result<()> {
    axum::serve(listener, router(AppState::new(store))).await
}
