use crate::gossip::DistributedRegistry;
use crate::registry::ProcessRegistry;
use crate::runtime::invoke::invoke_actor;
use crate::wire::{deserialize, serialize, Request, Response, Target};
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::Endpoint;
use std::sync::Arc;
use wasmtime::Engine;

/// Protocol handler for plasmoid actor traffic.
///
/// Implements iroh's `ProtocolHandler` trait to handle incoming QUIC
/// connections routed by the Router based on ALPN.
#[derive(Debug, Clone)]
pub struct PlasmoidProtocol {
    registry: Arc<ProcessRegistry>,
    engine: Engine,
    endpoint: Endpoint,
    distributed: Option<Arc<DistributedRegistry>>,
}

impl PlasmoidProtocol {
    pub fn new(
        registry: Arc<ProcessRegistry>,
        engine: Engine,
        endpoint: Endpoint,
        distributed: Option<Arc<DistributedRegistry>>,
    ) -> Self {
        Self {
            registry,
            engine,
            endpoint,
            distributed,
        }
    }
}

impl iroh::protocol::ProtocolHandler for PlasmoidProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();
        let remote_node_id = remote.to_string();

        tracing::debug!(remote = %remote, "Plasmoid connection accepted");

        loop {
            let (send, recv) = match connection.accept_bi().await {
                Ok(stream) => stream,
                Err(e) => {
                    tracing::debug!(error = %e, "Connection closed");
                    break;
                }
            };

            let registry = self.registry.clone();
            let engine = self.engine.clone();
            let remote_node_id = remote_node_id.clone();
            let endpoint = self.endpoint.clone();
            let distributed = self.distributed.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_stream(
                    send,
                    recv,
                    registry,
                    engine,
                    remote_node_id,
                    endpoint,
                    distributed,
                )
                .await
                {
                    tracing::error!(error = %e, "Stream handler error");
                }
            });
        }

        Ok(())
    }
}

async fn handle_stream(
    mut send: iroh::endpoint::SendStream,
    mut recv: iroh::endpoint::RecvStream,
    registry: Arc<ProcessRegistry>,
    engine: Engine,
    remote_node_id: String,
    endpoint: Endpoint,
    distributed: Option<Arc<DistributedRegistry>>,
) -> anyhow::Result<()> {
    let request_bytes = recv.read_to_end(1024 * 1024).await?;
    let request: Request = deserialize(&request_bytes)?;

    tracing::debug!(target = ?request.target, function = %request.function, "Received request");

    // Resolve the target — check local registry first
    let process = match &request.target {
        Target::Pid(pid) => registry.get_by_pid(pid).await,
        Target::Name(name) => {
            if let Some(pid) = registry.get_by_name(name).await {
                registry.get_by_pid(&pid).await
            } else {
                None
            }
        }
    };

    let process = match process {
        Some(p) => p,
        None => {
            // If we have a distributed registry, check for remote processes
            // and forward the call. For now, return an error since the request
            // was sent to this specific node.
            let response = Response {
                id: request.id,
                result: Err(format!("no process found for target {:?}", request.target)),
            };
            let response_bytes = serialize(&response)?;
            send.write_all(&response_bytes).await?;
            send.finish()?;
            return Ok(());
        }
    };

    let component = process.component.clone();
    let capabilities = process.capabilities.clone();
    let actor_id = process.name.unwrap_or_else(|| process.pid.to_string());
    let pid = process.pid.clone();
    let remote = remote_node_id.clone();
    let function = request.function.clone();
    let args = request.args.clone();

    let result = tokio::task::spawn_blocking(move || {
        invoke_actor(
            &engine,
            &component,
            &capabilities,
            &actor_id,
            Some(pid),
            Some(remote),
            &function,
            &args,
            Some(&endpoint),
            Some(registry),
            distributed,
        )
    })
    .await?;

    let response = match result {
        Ok(wave_results) => Response {
            id: request.id,
            result: Ok(wave_results),
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
