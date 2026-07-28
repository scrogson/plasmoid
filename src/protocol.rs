use crate::message::ExitReason;
use crate::pid::Pid;
use crate::registry::ParticleRegistry;
use crate::runtime::start_particle;
use crate::transport::{Addressee, PeerMessage, read_frame};
use crate::wire::{
    Command, CommandResponse, SendRequest, SendResponse, SpawnRequest, SpawnResponse, SpawnResult,
    Target, deserialize, serialize,
};
use iroh::Endpoint;
use iroh::endpoint::Connection;
use iroh::protocol::AcceptError;
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
    peers: Arc<crate::transport::PeerLinks>,
    cluster: Arc<crate::cluster::Cluster>,
}

impl PlasmoidProtocol {
    pub fn new(
        registry: Arc<ParticleRegistry>,
        engine: Engine,
        endpoint: Endpoint,
        peers: Arc<crate::transport::PeerLinks>,
        cluster: Arc<crate::cluster::Cluster>,
    ) -> Self {
        Self {
            registry,
            engine,
            endpoint,
            peers,
            cluster,
        }
    }
}

impl iroh::protocol::ProtocolHandler for PlasmoidProtocol {
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let remote = connection.remote_id();

        tracing::debug!(remote = %remote, "Plasmoid connection accepted");

        // Connecting is joining (#26). A node that dials us is a member, and we
        // tell it who else we know so the mesh converges from one introduction.
        if !self.cluster.learn([remote]).await.is_empty() {
            announce_to(&self.cluster, &self.peers, &[remote]).await;
        }

        loop {
            tokio::select! {
                // Request/response: external clients doing spawn and send.
                bi = connection.accept_bi() => {
                    let (send, recv) = match bi {
                        Ok(stream) => stream,
                        Err(e) => {
                            tracing::debug!(error = %e, "Connection closed");
                            break;
                        }
                    };

                    let registry = self.registry.clone();
                    let engine = self.engine.clone();
                    let endpoint = self.endpoint.clone();
                    let peers = self.peers.clone();

                    tokio::spawn(async move {
                        if let Err(e) =
                            handle_stream(send, recv, registry, engine, endpoint, peers).await
                        {
                            tracing::error!(error = %e, "Stream handler error");
                        }
                    });
                }
                // Particle messages: one ordered stream per peer node.
                uni = connection.accept_uni() => {
                    let recv = match uni {
                        Ok(stream) => stream,
                        Err(e) => {
                            tracing::debug!(error = %e, "Connection closed");
                            break;
                        }
                    };

                    let registry = self.registry.clone();
                    let peers = self.peers.clone();
                    let cluster = self.cluster.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_message_link(recv, registry, peers, cluster).await {
                            tracing::debug!(error = %e, "Peer link ended");
                        }
                    });
                }
            }
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
    peers: Arc<crate::transport::PeerLinks>,
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
        Command::Send(request) => handle_send(request, registry).await,
        Command::Spawn(request) => handle_spawn(request, registry, engine, endpoint, peers).await,
    };

    let response_bytes = serialize(&result)?;
    send.write_all(&response_bytes).await?;
    send.finish()?;

    Ok(())
}

async fn handle_send(request: SendRequest, registry: Arc<ParticleRegistry>) -> CommandResponse {
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

    // Send the message to the particle mailbox
    match registry.send_to_pid(&pid, request.msg).await {
        Ok(()) => CommandResponse::Send(SendResponse { result: Ok(()) }),
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
    peers: Arc<crate::transport::PeerLinks>,
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
                result: Err(crate::wire::SpawnFailureWire::ComponentNotFound),
            });
        }
    };

    // Spawn in registry
    let (pid, mailbox) = match registry
        .spawn(
            &request.component,
            request.name.as_deref(),
            Some(caps.clone()),
        )
        .await
    {
        Ok(result) => result,
        Err(e) => {
            tracing::debug!(error = %e, "Remote spawn refused");
            return CommandResponse::Spawn(SpawnResponse {
                result: Err(crate::wire::SpawnFailureWire::ResourceLimit),
            });
        }
    };

    // Start the particle (calls component's start function)
    if let Err(e) = start_particle(
        &engine,
        &component,
        &caps,
        pid.clone(),
        request.name.clone(),
        &request.init_args,
        crate::runtime::ParticleContext {
            mailbox,
            registry: registry.clone(),
            endpoint: Some(endpoint),
            peers: Some(peers.clone()),
        },
    )
    .await
    {
        tracing::debug!(error = %e, "Remote particle failed to start");
        return CommandResponse::Spawn(SpawnResponse {
            result: Err(crate::wire::SpawnFailureWire::InitFailed),
        });
    }

    CommandResponse::Spawn(SpawnResponse {
        result: Ok(SpawnResult {
            pid,
            component: request.component,
            name: request.name,
        }),
    })
}

/// Drain a peer's message link, delivering each message as it arrives.
///
/// Messages are handled **sequentially, in arrival order** — deliberately not
/// spawned per message, since spawning would discard the very ordering the
/// single stream exists to provide.
async fn handle_message_link(
    mut recv: iroh::endpoint::RecvStream,
    registry: Arc<ParticleRegistry>,
    peers: Arc<crate::transport::PeerLinks>,
    cluster: Arc<crate::cluster::Cluster>,
) -> anyhow::Result<()> {
    while let Some(msg) = read_frame(&mut recv).await? {
        handle_peer_message(msg, &registry, &peers, &cluster).await;
    }
    Ok(())
}

/// Act on one message from a peer.
///
/// Names are resolved here, on the receiving node — that is what lets a named
/// destination cost no round trip on the sending side.
async fn handle_peer_message(
    msg: PeerMessage,
    registry: &Arc<ParticleRegistry>,
    peers: &Arc<crate::transport::PeerLinks>,
    cluster: &Arc<crate::cluster::Cluster>,
) {
    async fn resolve(registry: &Arc<ParticleRegistry>, a: &Addressee) -> Option<Pid> {
        match a {
            Addressee::Pid(p) => Some(p.clone()),
            Addressee::Name(name) => registry.get_by_name(name).await,
        }
    }

    match msg {
        PeerMessage::Announce { nodes } => {
            // Announce onward only when we learned something, or announcements
            // would circulate forever between peers that already agree.
            let learned = cluster.learn(nodes).await;
            if !learned.is_empty() {
                tracing::info!(count = learned.len(), "Learned of new cluster members");
                announce_to(cluster, peers, &learned).await;
            }
        }
        PeerMessage::Deliver(envelope) => {
            let Some(pid) = resolve(registry, &envelope.target).await else {
                tracing::debug!(target = ?envelope.target, "no particle for destination; dropped");
                return;
            };
            // Fire-and-forget: a message for a particle that has died is
            // dropped, exactly as it would be locally.
            let delivered = match envelope.ref_id {
                Some(r) => registry.send_tagged_to_pid(&pid, r, envelope.payload).await,
                None => registry.send_to_pid(&pid, envelope.payload).await,
            };
            if delivered.is_err() {
                tracing::debug!(pid = %pid, "message for an unknown particle; dropped");
            }
        }
        PeerMessage::Link { from, to } => {
            match resolve(registry, &to).await {
                // Record our half. The sender recorded its own before sending.
                Some(target) => registry.link_remote(&target, from).await,
                // Nothing here to link to: tell the linker the way a death would.
                None => {
                    let _ = peers.send(
                        from.node,
                        &PeerMessage::Exit {
                            from: from.clone(),
                            to: from,
                            reason: ExitReason::NoProc,
                        },
                    );
                }
            }
        }
        PeerMessage::Unlink { from, to } => {
            if let Some(target) = resolve(registry, &to).await {
                registry.unlink_remote(&target, &from).await;
            }
        }
        PeerMessage::Monitor {
            watcher,
            target,
            ref_id,
        } => match resolve(registry, &target).await {
            Some(target_pid) => registry.monitor_remote(&watcher, target_pid, ref_id).await,
            None => {
                let _ = peers.send(
                    watcher.node,
                    &PeerMessage::Down {
                        from: watcher.clone(),
                        to: watcher,
                        ref_id,
                        reason: ExitReason::NoProc,
                    },
                );
            }
        },
        PeerMessage::Demonitor { watcher, ref_id } => {
            registry.demonitor(&watcher, ref_id).await;
        }
        PeerMessage::Exit { from, to, reason } => {
            registry.apply_exit_signal(&to, &from, reason).await;
        }
        PeerMessage::Down {
            from,
            to,
            ref_id,
            reason,
        } => {
            registry.deliver_down(&to, &from, ref_id, reason).await;
        }
    }
}

/// Tell some nodes about the whole cluster, and tell the cluster about them.
///
/// Sending to a node dials it if needed, so this both introduces us and pulls
/// the new members into the mesh.
pub(crate) async fn announce_to(
    cluster: &Arc<crate::cluster::Cluster>,
    peers: &Arc<crate::transport::PeerLinks>,
    to: &[iroh::EndpointId],
) {
    let roster = cluster.nodes().await;
    for node in to {
        // Do not name the recipient to itself; it is not its own peer.
        let nodes: Vec<_> = roster.iter().copied().filter(|n| n != node).collect();
        if let Err(e) = peers.send(*node, &PeerMessage::Announce { nodes }) {
            tracing::debug!(peer = %node.fmt_short(), error = %e, "Announce dropped");
        }
    }
}
