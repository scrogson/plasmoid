use crate::pid::Pid;
use crate::registry::ParticleRegistry;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_blobs::store::mem::MemStore;
use iroh_docs::NamespaceSecret;
use iroh_docs::engine::LiveEvent;
use iroh_docs::protocol::Docs;
use iroh_docs::sync::Capability;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{RwLock, broadcast};

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
pub enum ResolvedParticle {
    Local(Pid),
    Remote(RemoteParticle),
}

/// Information about a particle on a remote node.
#[derive(Debug, Clone)]
pub struct RemoteParticle {
    pub pid: Pid,
    pub component: String,
    pub name: Option<String>,
    pub node: EndpointId,
    pub addr: EndpointAddr,
}

impl From<RegistryEntry> for RemoteParticle {
    fn from(e: RegistryEntry) -> Self {
        Self {
            pid: e.pid,
            component: e.component,
            name: e.name,
            node: e.node,
            addr: e.addr,
        }
    }
}

/// A key in the registry document.
///
/// Deletions arrive as empty tombstones, so the key is the only thing that
/// identifies what was removed — the payload is gone.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum RegistryKey<'a> {
    Pid(&'a str),
    Name(&'a str),
}

impl<'a> RegistryKey<'a> {
    pub(crate) fn parse(key: &'a str) -> Option<Self> {
        if let Some(rest) = key.strip_prefix("pid/") {
            Some(RegistryKey::Pid(rest))
        } else {
            key.strip_prefix("name/").map(RegistryKey::Name)
        }
    }

    /// The document key. Writers must use this so the format cannot drift from
    /// [`Self::parse`].
    pub(crate) fn render(&self) -> String {
        match self {
            RegistryKey::Pid(p) => format!("pid/{p}"),
            RegistryKey::Name(n) => format!("name/{n}"),
        }
    }
}

/// Locally cached view of particles living on other nodes.
///
/// This is a cache of a CRDT, not the CRDT itself: entries may be evicted
/// locally (for example when a peer is lost) without deleting anything from the
/// replicated document, which is impossible for another node's entries anyway.
#[derive(Debug, Default)]
pub(crate) struct RemoteCache {
    names: HashMap<String, RemoteParticle>,
    pids: HashMap<Pid, RemoteParticle>,
}

impl RemoteCache {
    fn insert(&mut self, key: RegistryKey<'_>, remote: RemoteParticle) {
        match key {
            RegistryKey::Name(name) => {
                self.names.insert(name.to_string(), remote);
            }
            RegistryKey::Pid(pid_str) => {
                // Trust the key, not the payload: remove() parses the key, so an
                // entry inserted under a different pid would be unevictable.
                if let Ok(pid) = Pid::from_key(pid_str) {
                    self.pids.insert(pid, remote);
                }
            }
        }
    }

    /// Evict whatever the given key referred to.
    fn remove(&mut self, key: RegistryKey<'_>) {
        match key {
            RegistryKey::Name(name) => {
                self.names.remove(name);
            }
            RegistryKey::Pid(pid_str) => {
                if let Ok(pid) = Pid::from_key(pid_str) {
                    self.pids.remove(&pid);
                }
            }
        }
    }

    /// Evict everything belonging to a node. Returns how many entries went.
    fn remove_node(&mut self, node: &EndpointId) -> usize {
        let before = self.names.len() + self.pids.len();
        self.names.retain(|_, r| r.node != *node);
        self.pids.retain(|_, r| r.node != *node);
        before - (self.names.len() + self.pids.len())
    }

    fn len(&self) -> usize {
        self.names.len() + self.pids.len()
    }

    fn get_name(&self, name: &str) -> Option<&RemoteParticle> {
        self.names.get(name)
    }

    fn get_pid(&self, pid: &Pid) -> Option<&RemoteParticle> {
        self.pids.get(pid)
    }
}

/// Distributed registry backed by iroh-docs CRDT.
///
/// Replaces the gossip-based registry with a replicated document that
/// provides automatic sync, persistence, and catch-up for late joiners.
pub struct DocRegistry {
    local: Arc<ParticleRegistry>,
    endpoint: Endpoint,
    docs: Docs,
    blobs: MemStore,
    doc: iroh_docs::api::Doc,
    author: iroh_docs::AuthorId,
    remote: Arc<RwLock<RemoteCache>>,
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
        local: Arc<ParticleRegistry>,
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
            remote: Arc::new(RwLock::new(RemoteCache::default())),
        }))
    }

    /// Start syncing and processing live events.
    ///
    /// Must be called once during startup. Use `add_peers` to add
    /// bootstrap peers later.
    pub async fn start(self: &Arc<Self>, peers: &[EndpointId]) -> anyhow::Result<()> {
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

        self.spawn_death_announcer();

        Ok(())
    }

    /// Deregister particles from the document as they die.
    ///
    /// Every death funnels through `ParticleRegistry::exit_particle`, including
    /// deaths cascaded along links, so subscribing here catches all of them —
    /// which hooking individual call sites would not.
    fn spawn_death_announcer(self: &Arc<Self>) {
        let mut deaths = self.local.subscribe_deaths();
        let this = self.clone();

        tokio::spawn(async move {
            loop {
                match deaths.recv().await {
                    Ok(death) => {
                        if let Err(e) = this.announce_down(&death.pid, death.name.as_deref()).await
                        {
                            tracing::warn!(
                                pid = %death.pid,
                                error = %e,
                                "Failed to deregister dead particle from the registry"
                            );
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(missed)) => {
                        // The document now advertises particles that are gone.
                        // Loud, because it means the registry is lying.
                        tracing::error!(
                            missed,
                            "Death announcer fell behind; some dead particles remain advertised"
                        );
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            tracing::debug!("Death announcer stopped");
        });
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
                let key_str = std::str::from_utf8(entry.key())?;
                let Some(key) = RegistryKey::parse(key_str) else {
                    return Ok(()); // not ours
                };

                // A deletion arrives as an empty entry. There is no payload to
                // read, so the key alone says what went away.
                if entry.content_len() == 0 {
                    tracing::info!(key = %key_str, "Remote particle deregistered (via doc)");
                    self.remote.write().await.remove(key);
                    return Ok(());
                }

                let content = self.blobs.get_bytes(entry.content_hash()).await?;
                let registry_entry: RegistryEntry = postcard::from_bytes(&content)?;

                // Skip our own entries
                if registry_entry.node == self.endpoint.id() {
                    return Ok(());
                }

                tracing::info!(
                    pid = %registry_entry.pid,
                    name = ?registry_entry.name,
                    node = %registry_entry.node.fmt_short(),
                    "Remote particle registered (via doc)"
                );
                self.remote.write().await.insert(key, registry_entry.into());
            }
            LiveEvent::ContentReady { hash } => {
                // Content became available — try to parse any deferred entries.
                // This handles the case where InsertRemote fires before content is downloaded.
                if let Ok(content) = self.blobs.get_bytes(hash).await
                    && let Ok(entry) = postcard::from_bytes::<RegistryEntry>(&content)
                {
                    if entry.node == self.endpoint.id() {
                        return Ok(());
                    }

                    let name = entry.name.clone();
                    let pid_key = entry.pid.to_key();
                    let remote: RemoteParticle = entry.into();

                    let mut cache = self.remote.write().await;
                    cache.insert(RegistryKey::Pid(&pid_key), remote.clone());
                    if let Some(name) = name {
                        cache.insert(RegistryKey::Name(&name), remote);
                    }
                }
            }
            LiveEvent::NeighborUp(peer) => {
                // Sync will not redeliver entries this replica already holds, so
                // a peer that returns after being evicted would stay invisible.
                // Rebuild from the document instead of waiting for events.
                match self.reload_from_doc().await {
                    Ok(n) => tracing::info!(
                        peer = %peer.fmt_short(),
                        cached = n,
                        "Doc sync peer connected; reloaded registry"
                    ),
                    Err(e) => tracing::warn!(
                        peer = %peer.fmt_short(),
                        error = %e,
                        "Doc sync peer connected but registry reload failed"
                    ),
                }
            }
            LiveEvent::NeighborDown(peer) => {
                // A peer we can no longer reach may have died without announcing
                // its particles down, and no node can delete another node's
                // entries from the document. Evicting locally is the only way to
                // stop routing at a node we cannot talk to. This is recoverable:
                // NeighborUp reloads from the document.
                let evicted = self.evict_node(&peer).await;
                tracing::info!(
                    peer = %peer.fmt_short(),
                    evicted,
                    "Doc sync peer disconnected; evicted its particles"
                );
            }
            _ => {}
        }
        Ok(())
    }

    /// Announce a newly spawned particle to the registry document.
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

        // Keys use the lossless pid encoding, not Display, which truncates the
        // node id to four bytes and so cannot be parsed back or kept unique.
        self.doc
            .set_bytes(
                self.author,
                RegistryKey::Pid(&pid.to_key()).render(),
                bytes.clone(),
            )
            .await?;

        if let Some(name) = name {
            self.doc
                .set_bytes(self.author, RegistryKey::Name(name).render(), bytes)
                .await?;
        }

        Ok(())
    }

    /// Announce that a particle is down, deleting its entries.
    ///
    /// The name must be supplied by the caller: by the time a death is observed
    /// the local registry has already forgotten it, so it cannot be looked up.
    pub async fn announce_down(&self, pid: &Pid, name: Option<&str>) -> anyhow::Result<()> {
        self.doc
            .del(self.author, RegistryKey::Pid(&pid.to_key()).render())
            .await?;

        if let Some(name) = name {
            self.doc
                .del(self.author, RegistryKey::Name(name).render())
                .await?;
        }

        Ok(())
    }

    /// Announce every local particle as down. Best effort, for shutdown.
    ///
    /// Without this a node that stops cleanly still leaves its particles
    /// advertised, and peers would keep resolving them until they notice the
    /// connection is gone.
    pub async fn announce_all_down(&self) {
        for (pid, _component, name) in self.local.list_particles().await {
            if let Err(e) = self.announce_down(&pid, name.as_deref()).await {
                tracing::warn!(pid = %pid, error = %e, "Failed to deregister particle on shutdown");
            }
        }
    }

    /// Evict a peer's particles from the local cache.
    ///
    /// This does not touch the document — a node can only delete entries it
    /// authored — so it is purely a local statement of "I cannot reach these".
    /// [`Self::reload_from_doc`] undoes it when the peer returns.
    pub async fn evict_node(&self, node: &EndpointId) -> usize {
        self.remote.write().await.remove_node(node)
    }

    /// Rebuild the remote cache from the replicated document.
    ///
    /// The cache is otherwise only fed by live events, and sync does not
    /// redeliver entries this replica already holds. Without a rebuild, anything
    /// evicted would stay invisible even after the peer came back.
    ///
    /// Returns the number of cached entries.
    pub async fn reload_from_doc(&self) -> anyhow::Result<usize> {
        use futures_lite::StreamExt;

        let entries = self.doc.get_many(iroh_docs::store::Query::all()).await?;
        let mut entries = std::pin::pin!(entries);
        let mut rebuilt = RemoteCache::default();
        let me = self.endpoint.id();

        while let Some(entry) = entries.next().await {
            let entry = entry?;

            // Tombstones carry no payload; there is nothing to cache.
            if entry.content_len() == 0 {
                continue;
            }

            let Ok(key_str) = std::str::from_utf8(entry.key()) else {
                continue;
            };
            let Some(key) = RegistryKey::parse(key_str) else {
                continue;
            };

            let Ok(content) = self.blobs.get_bytes(entry.content_hash()).await else {
                continue; // content not downloaded yet; a live event will follow
            };
            let Ok(registry_entry) = postcard::from_bytes::<RegistryEntry>(&content) else {
                continue;
            };
            if registry_entry.node == me {
                continue;
            }

            rebuilt.insert(key, registry_entry.into());
        }

        let count = rebuilt.len();
        *self.remote.write().await = rebuilt;
        Ok(count)
    }

    /// Resolve a name: local first, then remote cache.
    pub async fn resolve_name(&self, name: &str) -> Option<ResolvedParticle> {
        if let Some(pid) = self.local.get_by_name(name).await {
            return Some(ResolvedParticle::Local(pid));
        }

        self.remote
            .read()
            .await
            .get_name(name)
            .cloned()
            .map(ResolvedParticle::Remote)
    }

    /// Resolve a PID: check if local, then remote cache.
    pub async fn resolve_pid(&self, pid: &Pid) -> Option<ResolvedParticle> {
        if pid.is_local_to(&self.endpoint.id()) && self.local.get_by_pid(pid).await.is_some() {
            return Some(ResolvedParticle::Local(pid.clone()));
        }

        self.remote
            .read()
            .await
            .get_pid(pid)
            .cloned()
            .map(ResolvedParticle::Remote)
    }

    /// Get the Docs protocol handler for Router registration.
    pub fn docs(&self) -> &Docs {
        &self.docs
    }

    /// Get the local registry.
    pub fn local(&self) -> &Arc<ParticleRegistry> {
        &self.local
    }

    /// Get the gossip instance (accessible through docs internals).
    /// Note: gossip is managed by iroh-docs internally.
    pub fn blobs(&self) -> &MemStore {
        &self.blobs
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::PidGenerator;
    use iroh::SecretKey;

    fn make_remote(node: EndpointId, name: Option<&str>) -> (Pid, RemoteParticle) {
        let pid = PidGenerator::new(node).next();
        let remote = RemoteParticle {
            pid: pid.clone(),
            component: "test".to_string(),
            name: name.map(|s| s.to_string()),
            node,
            addr: node.into(),
        };
        (pid, remote)
    }

    fn a_node() -> EndpointId {
        SecretKey::generate().public()
    }

    #[test]
    fn test_parse_registry_key() {
        assert_eq!(
            RegistryKey::parse("pid/abc.1"),
            Some(RegistryKey::Pid("abc.1"))
        );
        assert_eq!(
            RegistryKey::parse("name/counter"),
            Some(RegistryKey::Name("counter"))
        );
        assert_eq!(RegistryKey::parse("other/thing"), None);
        // A name containing a slash must not be truncated.
        assert_eq!(
            RegistryKey::parse("name/a/b"),
            Some(RegistryKey::Name("a/b"))
        );
    }

    #[test]
    fn test_tombstone_evicts_by_pid_key() {
        let node = a_node();
        let (pid, remote) = make_remote(node, None);
        let mut cache = RemoteCache::default();
        cache.insert(RegistryKey::Pid(&pid.to_key()), remote);
        assert!(cache.get_pid(&pid).is_some());

        // A deletion arrives as an empty entry: only the key identifies it.
        cache.remove(RegistryKey::Pid(&pid.to_key()));

        assert!(
            cache.get_pid(&pid).is_none(),
            "a pid tombstone must evict the cached particle"
        );
    }

    #[test]
    fn test_tombstone_evicts_by_name_key() {
        let node = a_node();
        let (_pid, remote) = make_remote(node, Some("counter"));
        let mut cache = RemoteCache::default();
        cache.insert(RegistryKey::Name("counter"), remote);
        assert!(cache.get_name("counter").is_some());

        cache.remove(RegistryKey::Name("counter"));

        assert!(cache.get_name("counter").is_none());
    }

    #[test]
    fn test_losing_a_peer_evicts_only_its_entries() {
        let gone = a_node();
        let kept = a_node();
        let mut cache = RemoteCache::default();

        let (gone_pid, gone_remote) = make_remote(gone, Some("doomed"));
        cache.insert(RegistryKey::Pid(&gone_pid.to_key()), gone_remote.clone());
        cache.insert(RegistryKey::Name("doomed"), gone_remote);

        let (kept_pid, kept_remote) = make_remote(kept, Some("survivor"));
        cache.insert(RegistryKey::Pid(&kept_pid.to_key()), kept_remote.clone());
        cache.insert(RegistryKey::Name("survivor"), kept_remote);

        let evicted = cache.remove_node(&gone);

        assert_eq!(evicted, 2, "both the pid and name entry should go");
        assert!(cache.get_pid(&gone_pid).is_none());
        assert!(cache.get_name("doomed").is_none());
        assert!(
            cache.get_pid(&kept_pid).is_some(),
            "another node's entries must survive"
        );
        assert!(cache.get_name("survivor").is_some());
    }

    #[test]
    fn test_unparseable_pid_tombstone_is_ignored() {
        let mut cache = RemoteCache::default();
        // Must not panic on a malformed key from the wire.
        cache.remove(RegistryKey::Pid("not-a-pid"));
    }
}
