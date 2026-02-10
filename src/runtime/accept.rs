use crate::host::Database;
use crate::policy::PolicySet;
use crate::runtime::invoke::{invoke_actor, ActorLike};
use crate::runtime::WasmActor;
use crate::wire::{deserialize, serialize, Request, Response};
use anyhow::Result;
use iroh::endpoint::Incoming;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use wasmtime::component::Component;
use wasmtime::Engine;

/// Handle an incoming QUIC connection.
pub async fn handle_incoming(
    incoming: Incoming,
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
    engine: Engine,
    database: Arc<Database>,
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

    let remote_node_id = remote.to_string();

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
        let engine = engine.clone();
        let database = database.clone();
        let remote_node_id = remote_node_id.clone();

        tokio::spawn(async move {
            if let Err(e) = handle_stream(stream, &alpn, actors, engine, database, remote_node_id).await {
                tracing::error!(error = %e, "Stream handler error");
            }
        });
    }

    Ok(())
}

async fn handle_stream(
    (mut send, mut recv): (iroh::endpoint::SendStream, iroh::endpoint::RecvStream),
    alpn: &[u8],
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
    engine: Engine,
    database: Arc<Database>,
    remote_node_id: String,
) -> Result<()> {
    // Read the request (1MB limit)
    let request_bytes = recv.read_to_end(1024 * 1024).await?;
    let request: Request = deserialize(&request_bytes)?;

    let alpn_str = String::from_utf8_lossy(alpn).to_string();
    tracing::debug!(alpn = %alpn_str, function = %request.function, "Received request");

    // Get the actor
    let actors_guard = actors.read().await;
    let actor = match actors_guard.get(alpn) {
        Some(a) => a,
        None => {
            return Ok(());
        }
    };

    // Invoke the actor on a blocking thread since wasmtime is sync
    let result = {
        let engine = engine.clone();
        let database = database.clone();
        let actor_id = alpn_str.clone();
        let remote = remote_node_id.clone();
        let payload = request.args.join(",").into_bytes();

        // We need to invoke synchronously but we're in async context
        // Use tokio::task::spawn_blocking for CPU-bound work
        let component = actor.component().clone();
        let capabilities = actor.capabilities().clone();
        drop(actors_guard); // Release the lock before blocking

        tokio::task::spawn_blocking(move || {
            // Create a temporary actor for invocation
            let temp_actor = TempActor {
                component,
                capabilities,
            };
            invoke_actor(
                &engine,
                &database,
                &temp_actor,
                &actor_id,
                Some(remote),
                payload,
            )
        })
        .await?
    };

    let response = match result {
        Ok(response_payload) => Response {
            id: request.id,
            result: Ok(vec![String::from_utf8_lossy(&response_payload).to_string()]),
        },
        Err(e) => Response {
            id: request.id,
            result: Err(e.to_string()),
        },
    };

    let response_bytes = serialize(&response)?;
    send.write_all(&response_bytes).await?;
    send.finish()?;

    Ok(())
}

/// Temporary actor wrapper for invocation.
/// This exists because we need to invoke with a cloned component.
struct TempActor {
    component: Component,
    capabilities: PolicySet,
}

impl ActorLike for TempActor {
    fn component(&self) -> &Component {
        &self.component
    }

    fn capabilities(&self) -> &PolicySet {
        &self.capabilities
    }
}
