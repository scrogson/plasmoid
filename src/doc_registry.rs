use crate::pid::Pid;
use crate::registry::ProcessRegistry;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::engine::LiveEvent;
use iroh_docs::protocol::Docs;
use iroh_docs::sync::Capability;
use iroh_docs::NamespaceSecret;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

/// Entry stored in the iroh-docs registry document.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegistryEntry {
    pub pid: Pid,
    pub name: Option<String>,
    pub component: String,
    pub node: EndpointId,
    pub addr: EndpointAddr,
}

/// Result of resolving a name or PID.
#[derive(Debug, Clone)]
pub enum ResolvedProcess {
    Local(Pid),
    Remote(RemoteProcess),
}

/// Information about a process on a remote node.
#[derive(Debug, Clone)]
pub struct RemoteProcess {
    pub pid: Pid,
    pub component: String,
    pub name: Option<String>,
    pub node: EndpointId,
    pub addr: EndpointAddr,
}

/// Distributed registry backed by iroh-docs CRDT.
///
/// Replaces the gossip-based registry with a replicated document that
/// provides automatic sync, persistence, and catch-up for late joiners.
pub struct DocRegistry {
    local: Arc<ProcessRegistry>,
    endpoint: Endpoint,
    docs: Docs,
    blobs: MemStore,
    doc: iroh_docs::api::Doc,
    author: iroh_docs::AuthorId,
    remote_names: Arc<RwLock<HashMap<String, RemoteProcess>>>,
    remote_pids: Arc<RwLock<HashMap<Pid, RemoteProcess>>>,
}

impl std::fmt::Debug for DocRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DocRegistry").finish_non_exhaustive()
    }
}

/// Well-known namespace for the plasmoid registry.
/// All nodes sharing this secret can read/write to the same document.
fn registry_namespace_secret() -> NamespaceSecret {
    let hash = blake3::hash(b"plasmoid-registry-v1");
    NamespaceSecret::from_bytes(hash.as_bytes())
}

impl DocRegistry {
    /// Create a new DocRegistry backed by a well-known shared document.
    ///
    /// All nodes derive the same namespace from "plasmoid-registry-v1",
    /// so they can sync the same document without ticket exchange.
    pub async fn new(
        local: Arc<ProcessRegistry>,
        endpoint: Endpoint,
        docs: Docs,
        blobs: MemStore,
    ) -> anyhow::Result<Arc<Self>> {
        let secret = registry_namespace_secret();
        let namespace_id = secret.id();

        // Try to open existing document, or import the well-known namespace.
        // Note: docs.open() errors (not returns None) when namespace doesn't exist,
        // and import_namespace() already calls open() internally.
        let doc = match docs.open(namespace_id).await {
            Ok(Some(doc)) => doc,
            _ => docs.import_namespace(Capability::Write(secret)).await?,
        };
        let author = docs.author_default().await?;

        Ok(Arc::new(Self {
            local,
            endpoint,
            docs,
            blobs,
            doc,
            author,
            remote_names: Arc::new(RwLock::new(HashMap::new())),
            remote_pids: Arc::new(RwLock::new(HashMap::new())),
        }))
    }

    /// Start syncing and processing live events.
    ///
    /// Must be called once during startup. Use `add_peers` to add
    /// bootstrap peers later.
    pub async fn start(
        self: &Arc<Self>,
        peers: &[EndpointId],
    ) -> anyhow::Result<()> {
        // Start sync (empty peers = accept incoming only)
        let peer_addrs: Vec<EndpointAddr> = peers.iter().map(|id| (*id).into()).collect();
        self.doc.start_sync(peer_addrs).await?;

        // Subscribe to live events and process them in background
        let mut events = self.doc.subscribe().await?;
        let this = self.clone();

        tokio::spawn(async move {
            use futures_lite::StreamExt;
            while let Some(event) = events.next().await {
                match event {
                    Ok(event) => {
                        if let Err(e) = this.handle_live_event(event).await {
                            tracing::warn!(error = %e, "Failed to handle doc live event");
                        }
                    }
                    Err(e) => {
                        tracing::error!(error = %e, "Doc event stream error");
                        break;
                    }
                }
            }
            tracing::debug!("Doc event stream ended");
        });

        Ok(())
    }

    /// Add bootstrap peers for doc sync.
    pub async fn add_peers(&self, peers: &[EndpointId]) -> anyhow::Result<()> {
        let peer_addrs: Vec<EndpointAddr> = peers.iter().map(|id| (*id).into()).collect();
        self.doc.start_sync(peer_addrs).await?;
        Ok(())
    }

    /// Handle a live event from the document subscription.
    async fn handle_live_event(&self, event: LiveEvent) -> anyhow::Result<()> {
        match event {
            LiveEvent::InsertRemote { entry, .. } => {
                let key = entry.key();
                let key_str = std::str::from_utf8(key)?;

                // Read content from blob store
                let content_hash = entry.content_hash();
                let content = self.blobs.get_bytes(content_hash).await?;
                let registry_entry: RegistryEntry = postcard::from_bytes(&content)?;

                // Skip our own entries
                if registry_entry.node == self.endpoint.id() {
                    return Ok(());
                }

                let remote = RemoteProcess {
                    pid: registry_entry.pid.clone(),
                    component: registry_entry.component,
                    name: registry_entry.name.clone(),
                    node: registry_entry.node,
                    addr: registry_entry.addr,
                };

                if key_str.starts_with("name/") {
                    if let Some(name) = &registry_entry.name {
                        tracing::info!(
                            pid = %remote.pid,
                            name = %name,
                            node = %remote.node.fmt_short(),
                            "Remote process registered (via doc)"
                        );
                        self.remote_names.write().await.insert(name.clone(), remote.clone());
                    }
                }

                if key_str.starts_with("pid/") {
                    self.remote_pids.write().await.insert(registry_entry.pid, remote);
                }
            }
            LiveEvent::ContentReady { hash } => {
                // Content became available — try to parse any deferred entries.
                // This handles the case where InsertRemote fires before content is downloaded.
                if let Ok(content) = self.blobs.get_bytes(hash).await {
                    if let Ok(entry) = postcard::from_bytes::<RegistryEntry>(&content) {
                        if entry.node == self.endpoint.id() {
                            return Ok(());
                        }

                        let remote = RemoteProcess {
                            pid: entry.pid.clone(),
                            component: entry.component,
                            name: entry.name.clone(),
                            node: entry.node,
                            addr: entry.addr,
                        };

                        self.remote_pids.write().await.insert(entry.pid, remote.clone());
                        if let Some(name) = entry.name {
                            self.remote_names.write().await.insert(name, remote);
                        }
                    }
                }
            }
            LiveEvent::NeighborUp(peer) => {
                tracing::info!(peer = %peer.fmt_short(), "Doc sync peer connected");
            }
            LiveEvent::NeighborDown(peer) => {
                tracing::info!(peer = %peer.fmt_short(), "Doc sync peer disconnected");
            }
            _ => {}
        }
        Ok(())
    }

    /// Announce a newly spawned process to the registry document.
    pub async fn announce_spawn(
        &self,
        pid: &Pid,
        component: &str,
        name: Option<&str>,
    ) -> anyhow::Result<()> {
        let entry = RegistryEntry {
            pid: pid.clone(),
            name: name.map(|s| s.to_string()),
            component: component.to_string(),
            node: self.endpoint.id(),
            addr: self.endpoint.addr(),
        };

        let bytes = postcard::to_allocvec(&entry)?;

        // Write pid entry
        let pid_key = format!("pid/{}", pid);
        self.doc.set_bytes(self.author, pid_key, bytes.clone()).await?;

        // Write name entry if present
        if let Some(name) = name {
            let name_key = format!("name/{}", name);
            self.doc.set_bytes(self.author, name_key, bytes).await?;
        }

        Ok(())
    }

    /// Announce that a process is down (delete entries).
    pub async fn announce_down(&self, pid: &Pid) -> anyhow::Result<()> {
        let pid_key = format!("pid/{}", pid);
        self.doc.del(self.author, pid_key).await?;

        // Also remove by name if we have it cached locally
        // (The local registry handles name cleanup, but we clean the doc too)
        if let Some(entry) = self.local.get_by_pid(pid).await {
            if let Some(name) = &entry.name {
                let name_key = format!("name/{}", name);
                self.doc.del(self.author, name_key).await?;
            }
        }

        Ok(())
    }

    /// Resolve a name: local first, then remote cache.
    pub async fn resolve_name(&self, name: &str) -> Option<ResolvedProcess> {
        if let Some(pid) = self.local.get_by_name(name).await {
            return Some(ResolvedProcess::Local(pid));
        }

        if let Some(remote) = self.remote_names.read().await.get(name) {
            return Some(ResolvedProcess::Remote(remote.clone()));
        }

        None
    }

    /// Resolve a PID: check if local, then remote cache.
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

    /// Get the Docs protocol handler for Router registration.
    pub fn docs(&self) -> &Docs {
        &self.docs
    }

    /// Get the local registry.
    pub fn local(&self) -> &Arc<ProcessRegistry> {
        &self.local
    }

    /// Get the gossip instance (accessible through docs internals).
    /// Note: gossip is managed by iroh-docs internally.
    pub fn blobs(&self) -> &MemStore {
        &self.blobs
    }
}
