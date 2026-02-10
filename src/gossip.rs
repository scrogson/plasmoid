use crate::pid::Pid;
use crate::registry::ProcessRegistry;
use bytes::Bytes;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_gossip::api::Event;
use iroh_gossip::net::Gossip;
use iroh_gossip::proto::TopicId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Well-known topic for plasmoid registry gossip.
/// Derived from blake3 hash of "plasmoid-registry-v1".
fn registry_topic() -> TopicId {
    let hash = blake3::hash(b"plasmoid-registry-v1");
    TopicId::from(hash.as_bytes().to_owned())
}

/// Messages exchanged via gossip for distributed registry sync.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RegistryMessage {
    ProcessUp {
        pid: Pid,
        behavior: String,
        name: Option<String>,
        node: EndpointId,
        addr: EndpointAddr,
    },
    ProcessDown {
        pid: Pid,
        node: EndpointId,
    },
    ClaimName {
        name: String,
        pid: Pid,
        node: EndpointId,
        timestamp: u64,
    },
    ReleaseName {
        name: String,
        node: EndpointId,
    },
}

/// Information about a process on a remote node.
#[derive(Debug, Clone)]
pub struct RemoteProcess {
    pub pid: Pid,
    pub behavior: String,
    pub name: Option<String>,
    pub node: EndpointId,
    pub addr: EndpointAddr,
}

/// Result of resolving a name or PID.
#[derive(Debug, Clone)]
pub enum ResolvedProcess {
    Local(Pid),
    Remote(RemoteProcess),
}

/// Distributed registry that combines local process registry with
/// gossip-based discovery of remote processes.
pub struct DistributedRegistry {
    local: Arc<ProcessRegistry>,
    endpoint: Endpoint,
    gossip: Gossip,
    remote_names: Arc<RwLock<HashMap<String, RemoteProcess>>>,
    remote_pids: Arc<RwLock<HashMap<Pid, RemoteProcess>>>,
}

impl std::fmt::Debug for DistributedRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DistributedRegistry")
            .finish_non_exhaustive()
    }
}

impl DistributedRegistry {
    pub fn new(
        local: Arc<ProcessRegistry>,
        endpoint: Endpoint,
        gossip: Gossip,
    ) -> Arc<Self> {
        Arc::new(Self {
            local,
            endpoint,
            gossip,
            remote_names: Arc::new(RwLock::new(HashMap::new())),
            remote_pids: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// Join the gossip cluster with optional bootstrap peers.
    /// Spawns a background task to process incoming registry messages.
    pub async fn start(
        self: &Arc<Self>,
        bootstrap: &[EndpointId],
    ) -> anyhow::Result<()> {
        let topic = registry_topic();

        let topic_handle = if bootstrap.is_empty() {
            self.gossip
                .subscribe(topic, vec![])
                .await?
        } else {
            self.gossip
                .subscribe_and_join(topic, bootstrap.to_vec())
                .await?
        };

        let (_sender, mut receiver) = topic_handle.split();

        // Spawn receiver loop
        let this = self.clone();
        tokio::spawn(async move {
            use futures_lite::StreamExt;
            while let Some(event) = receiver.next().await {
                match event {
                    Ok(Event::Received(msg)) => {
                        if let Err(e) = this.handle_message(&msg.content).await {
                            tracing::warn!(error = %e, "Failed to handle gossip message");
                        }
                    }
                    Ok(Event::NeighborUp(id)) => {
                        tracing::info!(peer = %id.fmt_short(), "Gossip peer connected");
                    }
                    Ok(Event::NeighborDown(id)) => {
                        tracing::info!(peer = %id.fmt_short(), "Gossip peer disconnected");
                    }
                    Ok(Event::Lagged) => {
                        tracing::warn!("Gossip receiver lagged, some messages may be lost");
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Gossip receiver error");
                        break;
                    }
                }
            }
            tracing::debug!("Gossip receiver loop ended");
        });

        Ok(())
    }

    /// Handle an incoming gossip message.
    async fn handle_message(&self, content: &[u8]) -> anyhow::Result<()> {
        let msg: RegistryMessage = postcard::from_bytes(content)?;

        match msg {
            RegistryMessage::ProcessUp {
                pid,
                behavior,
                name,
                node,
                addr,
            } => {
                // Skip if this is about our own node
                if node == self.endpoint.id() {
                    return Ok(());
                }

                let remote = RemoteProcess {
                    pid: pid.clone(),
                    behavior,
                    name: name.clone(),
                    node,
                    addr,
                };

                self.remote_pids.write().await.insert(pid, remote.clone());
                if let Some(name) = name {
                    tracing::info!(
                        pid = %remote.pid,
                        name = %name,
                        node = %remote.node.fmt_short(),
                        "Remote process registered"
                    );
                    self.remote_names.write().await.insert(name, remote);
                }
            }
            RegistryMessage::ProcessDown { pid, node } => {
                if node == self.endpoint.id() {
                    return Ok(());
                }

                if let Some(remote) = self.remote_pids.write().await.remove(&pid) {
                    if let Some(name) = &remote.name {
                        self.remote_names.write().await.remove(name);
                    }
                    tracing::info!(
                        pid = %pid,
                        node = %node.fmt_short(),
                        "Remote process deregistered"
                    );
                }
            }
            RegistryMessage::ClaimName {
                name,
                pid,
                node,
                timestamp: _,
            } => {
                if node == self.endpoint.id() {
                    return Ok(());
                }

                // First-writer-wins: only insert if no local or remote claim exists
                if self.local.get_by_name(&name).await.is_none() {
                    if let Some(remote) = self.remote_pids.read().await.get(&pid).cloned() {
                        self.remote_names.write().await.insert(name, remote);
                    }
                }
            }
            RegistryMessage::ReleaseName { name, node } => {
                if node == self.endpoint.id() {
                    return Ok(());
                }

                let mut names = self.remote_names.write().await;
                if let Some(remote) = names.get(&name) {
                    if remote.node == node {
                        names.remove(&name);
                    }
                }
            }
        }

        Ok(())
    }

    /// Resolve a name: local first, then remote.
    pub async fn resolve_name(&self, name: &str) -> Option<ResolvedProcess> {
        // Check local registry first
        if let Some(pid) = self.local.get_by_name(name).await {
            return Some(ResolvedProcess::Local(pid));
        }

        // Check remote registry
        if let Some(remote) = self.remote_names.read().await.get(name) {
            return Some(ResolvedProcess::Remote(remote.clone()));
        }

        None
    }

    /// Resolve a PID: check if local, then remote.
    pub async fn resolve_pid(&self, pid: &Pid) -> Option<ResolvedProcess> {
        if pid.is_local_to(&self.endpoint.id()) {
            if self.local.get_by_pid(pid).await.is_some() {
                return Some(ResolvedProcess::Local(pid.clone()));
            }
        }

        if let Some(remote) = self.remote_pids.read().await.get(pid) {
            return Some(ResolvedProcess::Remote(remote.clone()));
        }

        None
    }

    /// Announce a newly spawned process to the gossip cluster.
    pub async fn announce_spawn(
        &self,
        pid: &Pid,
        behavior: &str,
        name: Option<&str>,
    ) -> anyhow::Result<()> {
        let msg = RegistryMessage::ProcessUp {
            pid: pid.clone(),
            behavior: behavior.to_string(),
            name: name.map(|s| s.to_string()),
            node: self.endpoint.id(),
            addr: self.endpoint.addr(),
        };

        let bytes = postcard::to_allocvec(&msg)?;
        let topic = registry_topic();
        let mut topic_handle = self.gossip.subscribe(topic, vec![]).await?;
        topic_handle.broadcast(Bytes::from(bytes)).await?;

        Ok(())
    }

    /// Announce that a process is down.
    pub async fn announce_down(&self, pid: &Pid) -> anyhow::Result<()> {
        let msg = RegistryMessage::ProcessDown {
            pid: pid.clone(),
            node: self.endpoint.id(),
        };

        let bytes = postcard::to_allocvec(&msg)?;
        let topic = registry_topic();
        let mut topic_handle = self.gossip.subscribe(topic, vec![]).await?;
        topic_handle.broadcast(Bytes::from(bytes)).await?;

        Ok(())
    }

    /// Get the gossip instance for use in Router.
    pub fn gossip(&self) -> &Gossip {
        &self.gossip
    }

    /// Get the local registry.
    pub fn local(&self) -> &Arc<ProcessRegistry> {
        &self.local
    }
}
