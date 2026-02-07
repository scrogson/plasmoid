use crate::runtime::WasmActor;
use crate::wire::{deserialize, serialize, Message};
use anyhow::Result;
use iroh::endpoint::Incoming;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Handle an incoming QUIC connection.
pub async fn handle_incoming(
    incoming: Incoming,
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
) -> Result<()> {
    // Accept the incoming connection and get the connecting state
    let mut connecting = incoming.accept()?;

    // Get the ALPN protocol before completing the connection
    let alpn = connecting.alpn().await?;

    // Complete the connection handshake
    let conn = connecting.await?;
    let remote = conn.remote_id();

    tracing::debug!(
        alpn = ?String::from_utf8_lossy(&alpn),
        remote = %remote,
        "Connection accepted"
    );

    // Check if we have an actor for this ALPN
    {
        let actors_guard = actors.read().await;
        if !actors_guard.contains_key(&alpn) {
            tracing::warn!(alpn = ?String::from_utf8_lossy(&alpn), "No actor for ALPN");
            return Ok(());
        }
    }

    // Handle bidirectional streams
    loop {
        let stream = match conn.accept_bi().await {
            Ok(stream) => stream,
            Err(e) => {
                tracing::debug!(error = %e, "Connection closed");
                break;
            }
        };

        let actors = actors.clone();
        let alpn = alpn.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, &alpn, actors).await {
                tracing::error!(error = %e, "Stream handler error");
            }
        });
    }

    Ok(())
}

async fn handle_stream(
    (mut send, mut recv): (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
    alpn: &[u8],
    _actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
) -> Result<()> {
    // Read the request (1MB limit)
    let request_bytes = recv.read_to_end(1024 * 1024).await?;
    let request: Message = deserialize(&request_bytes)?;

    tracing::debug!(alpn = ?String::from_utf8_lossy(alpn), "Received request");

    // TODO: Actually invoke the WASM actor
    // For now, echo back an error response
    let response = match request {
        Message::Request { id, payload: _ } => Message::Response {
            id,
            payload: Err("actor invocation not yet implemented".to_string()),
        },
        _ => Message::Response {
            id: 0,
            payload: Err("expected request message".to_string()),
        },
    };

    let response_bytes = serialize(&response)?;
    send.write_all(&response_bytes).await?;
    send.finish()?;

    Ok(())
}
