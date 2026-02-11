use crate::doc_registry::{DocRegistry, ResolvedProcess};
use crate::registry::ProcessRegistry;
use crate::runtime::invoke::invoke_actor;
use crate::runtime::PLASMOID_ALPN;
use crate::wire::{
    self, deserialize, serialize, CallRequest, CallResponse, Command, CommandResponse,
    SpawnRequest, SpawnResponse, SpawnResult, Target,
};
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
    doc_registry: Option<Arc<DocRegistry>>,
}

impl PlasmoidProtocol {
    pub fn new(
        registry: Arc<ProcessRegistry>,
        engine: Engine,
        endpoint: Endpoint,
        doc_registry: Option<Arc<DocRegistry>>,
    ) -> Self {
        Self {
            registry,
            engine,
            endpoint,
            doc_registry,
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
            let doc_registry = self.doc_registry.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_stream(
                    send,
                    recv,
                    registry,
                    engine,
                    remote_node_id,
                    endpoint,
                    doc_registry,
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
    doc_registry: Option<Arc<DocRegistry>>,
) -> anyhow::Result<()> {
    let request_bytes = recv.read_to_end(1024 * 1024).await?;

    let command: Command = match deserialize(&request_bytes) {
        Ok(cmd) => cmd,
        Err(e) => {
            tracing::error!(error = %e, "Failed to deserialize command");
            // Can't send a typed error response since we don't know the command type.
            // Just close the stream cleanly.
            send.finish()?;
            return Ok(());
        }
    };

    let result = match command {
        Command::Call(request) => {
            handle_call(request, registry, engine, remote_node_id, endpoint, doc_registry).await
        }
        Command::Spawn(request) => handle_spawn(request, registry, doc_registry).await,
    };

    let response_bytes = serialize(&result)?;
    send.write_all(&response_bytes).await?;
    send.finish()?;

    Ok(())
}

async fn handle_call(
    request: CallRequest,
    registry: Arc<ProcessRegistry>,
    engine: Engine,
    remote_node_id: String,
    endpoint: Endpoint,
    doc_registry: Option<Arc<DocRegistry>>,
) -> CommandResponse {
    tracing::debug!(target = ?request.target, function = %request.function, "Received call request");

    // Resolve the target -- check local registry first
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
            // Check doc registry for remote processes and forward
            if let Some(ref doc_reg) = doc_registry {
                let resolved = match &request.target {
                    Target::Name(name) => doc_reg.resolve_name(name).await,
                    Target::Pid(pid) => doc_reg.resolve_pid(pid).await,
                };

                if let Some(ResolvedProcess::Remote(remote)) = resolved {
                    return forward_to_remote(&endpoint, &remote, &request).await;
                }
            }

            return CommandResponse::Call(CallResponse {
                id: request.id,
                result: Err(format!("no process found for target {:?}", request.target)),
            });
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
            doc_registry,
        )
    })
    .await;

    // Flatten: JoinError (panic) or invocation error -> error response
    let result = match result {
        Ok(Ok(wave_results)) => Ok(wave_results),
        Ok(Err(e)) => Err(e.to_string()),
        Err(join_err) => Err(format!("invocation panicked: {}", join_err)),
    };

    CommandResponse::Call(CallResponse {
        id: request.id,
        result,
    })
}

async fn handle_spawn(
    request: SpawnRequest,
    registry: Arc<ProcessRegistry>,
    doc_registry: Option<Arc<DocRegistry>>,
) -> CommandResponse {
    tracing::debug!(component = %request.component, name = ?request.name, "Received spawn request");

    let result = registry
        .spawn(&request.component, request.name.as_deref(), None)
        .await;

    CommandResponse::Spawn(SpawnResponse {
        result: match result {
            Ok(pid) => {
                // Announce to doc registry for cross-node discovery
                if let Some(ref doc_reg) = doc_registry {
                    if let Err(e) = doc_reg
                        .announce_spawn(&pid, &request.component, request.name.as_deref())
                        .await
                    {
                        tracing::debug!(error = %e, "Failed to announce spawn (no peers yet?)");
                    }
                }
                Ok(SpawnResult {
                    pid,
                    component: request.component,
                    name: request.name,
                })
            }
            Err(e) => Err(e.to_string()),
        },
    })
}

/// Forward a request to a remote node and return the response.
async fn forward_to_remote(
    endpoint: &Endpoint,
    remote: &crate::doc_registry::RemoteProcess,
    request: &CallRequest,
) -> CommandResponse {
    tracing::debug!(
        target = ?request.target,
        node = %remote.node.fmt_short(),
        "Forwarding request to remote node"
    );

    let result = async {
        let conn = endpoint
            .connect(remote.addr.clone(), PLASMOID_ALPN)
            .await?;
        let (mut remote_send, mut remote_recv) = conn.open_bi().await?;

        let command = Command::Call(request.clone());
        let request_bytes = wire::serialize(&command)?;
        remote_send.write_all(&request_bytes).await?;
        remote_send.finish()?;

        let response_bytes = remote_recv.read_to_end(1024 * 1024).await?;
        let response: CommandResponse = wire::deserialize(&response_bytes)?;
        Ok::<_, anyhow::Error>(response)
    }
    .await;

    match result {
        Ok(response) => response,
        Err(e) => CommandResponse::Call(CallResponse {
            id: request.id,
            result: Err(format!("forwarding failed: {}", e)),
        }),
    }
}
