//! Websocket client side of the sync protocol.

use crate::doc::{Doc, Op};
use crate::sync::{self, Message};
use futures_util::{SinkExt, StreamExt};
use std::time::Duration;
use tokio_tungstenite::tungstenite::Message as WsMessage;

pub type Error = Box<dyn std::error::Error + Send + Sync>;

/// Connect to `url` (e.g. `ws://127.0.0.1:3030/sync/notes`), exchange state vectors and
/// missing operations, then disconnect. Returns the operations received from the server.
pub async fn sync_once(url: &str, doc: &mut Doc) -> Result<Vec<Op>, Error> {
    let (mut ws, _) = tokio_tungstenite::connect_async(url).await?;
    ws.send(WsMessage::Binary(Message::StateVector(doc.clock().clone()).encode().into())).await?;
    let mut received = Vec::new();
    // Done once we have answered the server's state vector and hold everything it listed.
    let mut server_sv = None;
    while !server_sv.as_ref().is_some_and(|sv| doc.clock().dominates(sv)) {
        let frame = tokio::time::timeout(Duration::from_secs(10), ws.next()).await?.ok_or("server closed the connection")??;
        let WsMessage::Binary(b) = frame else { continue };
        let msg = Message::decode(&b)?;
        if let Message::StateVector(sv) = &msg {
            server_sv = Some(sv.clone());
        }
        let (reply, applied) = sync::handle(doc, msg);
        received.extend(applied);
        if let Some(r) = reply {
            // The answer to the server's state vector: what it is missing from us.
            ws.send(WsMessage::Binary(r.encode().into())).await?;
        }
    }
    ws.close(None).await?;
    // Drain until the server acknowledges the close, so our ops are processed first.
    while let Ok(Some(Ok(_))) = tokio::time::timeout(Duration::from_secs(2), ws.next()).await {}
    Ok(received)
}

/// Stay connected: send local operations from `local`, integrate remote ones, and call
/// `on_remote` after each batch received. Ends when the server goes away.
pub async fn live(
    url: &str,
    mut doc: Doc,
    mut local: tokio::sync::mpsc::Receiver<Vec<Op>>,
    mut on_remote: impl FnMut(&Doc, &[Op]),
) -> Result<Doc, Error> {
    let (ws, _) = tokio_tungstenite::connect_async(url).await?;
    let (mut sink, mut stream) = ws.split();
    sink.send(WsMessage::Binary(Message::StateVector(doc.clock().clone()).encode().into())).await?;
    loop {
        tokio::select! {
            frame = stream.next() => {
                let Some(frame) = frame else { break };
                let WsMessage::Binary(b) = frame? else { continue };
                let (reply, applied) = sync::handle(&mut doc, Message::decode(&b)?);
                if let Some(r) = reply {
                    sink.send(WsMessage::Binary(r.encode().into())).await?;
                }
                if !applied.is_empty() {
                    on_remote(&doc, &applied);
                }
            }
            ops = local.recv() => {
                let Some(ops) = ops else { break };
                doc.apply_remote(ops.clone()); // no-op if they were made on `doc` already
                sink.send(WsMessage::Binary(Message::Ops(ops).encode().into())).await?;
            }
        }
    }
    Ok(doc)
}
