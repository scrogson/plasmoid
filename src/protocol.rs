use crate::doc_registry::DocRegistry;
use crate::registry::ParticleRegistry;
use crate::runtime::start_process;
use crate::wire::{
    deserialize, serialize, Command, CommandResponse, SendRequest, SendResponse,
    SpawnRequest, SpawnResponse, SpawnResult, Target,
};
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
use iroh::Endpoint;
use std::sync::Arc;
use wasmtime::Engine;

/// Protocol handler for plasmoid traffic.
///
/// Implements iroh's `ProtocolHandler` trait to handle incoming QUIC
/// connections routed by the Router based on ALPN.
#[derive(Debug, Clone)]
pub struct PlasmoidProtocol {
    registry: Arc<ParticleRegistry>,
    engine: Engine,
    endpoint: Endpoint,
    doc_registry: Option<Arc<DocRegistry>>,
}

impl PlasmoidProtocol {
    pub fn new(
        registry: Arc<ParticleRegistry>,
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
            let endpoint = self.endpoint.clone();
            let doc_registry = self.doc_registry.clone();

            tokio::spawn(async move {
                if let Err(e) = handle_stream(
                    send,
                    recv,
                    registry,
                    engine,
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
    registry: Arc<ParticleRegistry>,
    engine: Engine,
    endpoint: Endpoint,
    doc_registry: Option<Arc<DocRegistry>>,
) -> anyhow::Result<()> {
    let request_bytes = recv.read_to_end(1024 * 1024).await?;

    let command: Command = match deserialize(&request_bytes) {
        Ok(cmd) => cmd,
        Err(e) => {
            tracing::error!(error = %e, "Failed to deserialize command");
            send.finish()?;
            return Ok(());
        }
    };

    let result = match command {
        Command::Send(request) => {
            handle_send(request, registry).await
        }
        Command::Spawn(request) => {
            handle_spawn(request, registry, engine, endpoint, doc_registry).await
        }
    };

    let response_bytes = serialize(&result)?;
    send.write_all(&response_bytes).await?;
    send.finish()?;

    Ok(())
}

async fn handle_send(
    request: SendRequest,
    registry: Arc<ParticleRegistry>,
) -> CommandResponse {
    tracing::debug!(target = ?request.target, "Received send request");

    // Resolve the target
    let pid = match &request.target {
        Target::Pid(pid) => Some(pid.clone()),
        Target::Name(name) => registry.get_by_name(name).await,
    };

    let pid = match pid {
        Some(p) => p,
        None => {
            return CommandResponse::Send(SendResponse {
                result: Err(format!("no particle found for target {:?}", request.target)),
            });
        }
    };

    // Send the message to the process mailbox
    match registry.send_to_pid(&pid, request.msg).await {
        Ok(()) => CommandResponse::Send(SendResponse {
            result: Ok(()),
        }),
        Err(e) => CommandResponse::Send(SendResponse {
            result: Err(format!("{}", e)),
        }),
    }
}

async fn handle_spawn(
    request: SpawnRequest,
    registry: Arc<ParticleRegistry>,
    engine: Engine,
    endpoint: Endpoint,
    doc_registry: Option<Arc<DocRegistry>>,
) -> CommandResponse {
    tracing::debug!(
        component = %request.component,
        name = ?request.name,
        "Received spawn request"
    );

    // Look up the component template
    let (component, caps) = match registry.get_component(&request.component).await {
        Some(result) => result,
        None => {
            return CommandResponse::Spawn(SpawnResponse {
                result: Err(format!("component '{}' not registered", request.component)),
            });
        }
    };

    // Spawn in registry
    let (pid, receivers) = match registry
        .spawn(&request.component, request.name.as_deref(), Some(caps.clone()))
        .await
    {
        Ok(result) => result,
        Err(e) => {
            return CommandResponse::Spawn(SpawnResponse {
                result: Err(e.to_string()),
            });
        }
    };

    // Start the process (init + message loop)
    if let Err(e) = start_process(
        &engine,
        &component,
        &caps,
        pid.clone(),
        request.name.clone(),
        &request.init_msg,
        receivers,
        Some(endpoint),
        registry.clone(),
        doc_registry.clone(),
    )
    .await
    {
        return CommandResponse::Spawn(SpawnResponse {
            result: Err(format!("failed to start process: {}", e)),
        });
    }

    // Announce to doc registry for cross-node discovery
    if let Some(ref doc_reg) = doc_registry {
        if let Err(e) = doc_reg
            .announce_spawn(&pid, &request.component, request.name.as_deref())
            .await
        {
            tracing::debug!(error = %e, "Failed to announce spawn (no peers yet?)");
        }
    }

    CommandResponse::Spawn(SpawnResponse {
        result: Ok(SpawnResult {
            pid,
            component: request.component,
            name: request.name,
        }),
    })
}
