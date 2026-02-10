use crate::doc_registry::DocRegistry;
use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::protocol::PlasmoidProtocol;
use crate::registry::ProcessRegistry;
use anyhow::Result;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::BlobsProtocol;
use iroh_blobs::protocol::ALPN as BLOBS_ALPN;
use iroh_docs::net::ALPN as DOCS_ALPN;
use iroh_docs::protocol::Docs;
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use std::sync::Arc;
use wasmtime::{Config, Engine};

/// The single ALPN used for all plasmoid traffic.
pub const PLASMOID_ALPN: &[u8] = b"plasmoid/1";

/// The actor runtime - hosts WASM actors on an iroh endpoint.
pub struct ActorRuntime {
    router: Router,
    endpoint: Endpoint,
    engine: Engine,
    registry: Arc<ProcessRegistry>,
    doc_registry: Arc<DocRegistry>,
}

impl ActorRuntime {
    /// Create a new actor runtime.
    pub async fn new() -> Result<Self> {
        // Configure wasmtime
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;

        // Configure iroh endpoint with default settings
        let endpoint = Endpoint::builder().bind().await?;

        let pid_gen = PidGenerator::new(endpoint.id());
        let registry = Arc::new(ProcessRegistry::new(pid_gen, engine.clone()));

        // Create blob store (in-memory)
        let blobs = MemStore::new();

        // Create gossip instance (used by iroh-docs internally for sync)
        let gossip = Gossip::builder().spawn(endpoint.clone());

        // Create docs protocol backed by blobs and gossip
        let docs = Docs::memory()
            .spawn(endpoint.clone(), blobs.clone().into(), gossip.clone())
            .await?;

        // Create doc-backed distributed registry
        let doc_registry = DocRegistry::new(
            registry.clone(),
            endpoint.clone(),
            docs.clone(),
            blobs.clone(),
        )
        .await?;

        let protocol = PlasmoidProtocol::new(
            registry.clone(),
            engine.clone(),
            endpoint.clone(),
            Some(doc_registry.clone()),
        );

        let router = Router::builder(endpoint.clone())
            .accept(PLASMOID_ALPN, protocol)
            .accept(GOSSIP_ALPN, gossip)
            .accept(BLOBS_ALPN, BlobsProtocol::new(&blobs, None))
            .accept(DOCS_ALPN, docs)
            .spawn();

        tracing::info!(endpoint_id = %endpoint.id(), "Actor runtime initialized");

        Ok(Self {
            router,
            endpoint,
            engine,
            registry,
            doc_registry,
        })
    }

    /// Get the endpoint's unique identity.
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Get the endpoint's address information.
    pub fn node_addr(&self) -> EndpointAddr {
        self.endpoint.addr()
    }

    /// Get a reference to the iroh endpoint.
    pub fn endpoint(&self) -> &Endpoint {
        &self.endpoint
    }

    /// Get a reference to the WASM engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to the process registry.
    pub fn registry(&self) -> &Arc<ProcessRegistry> {
        &self.registry
    }

    /// Get a reference to the doc registry.
    pub fn doc_registry(&self) -> &Arc<DocRegistry> {
        &self.doc_registry
    }

    /// Join the cluster with bootstrap peers.
    pub async fn join_cluster(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.doc_registry.start(&peers).await?;
        tracing::info!(peers = peers.len(), "Joined cluster");
        Ok(())
    }

    /// Deploy a WASM actor: register it as a component and spawn one process.
    ///
    /// The `name` is used both as the component name and the process name.
    /// Returns the PID of the spawned process.
    pub async fn deploy(
        &self,
        name: &str,
        wasm_bytes: &[u8],
        capabilities: PolicySet,
    ) -> Result<Pid> {
        self.registry
            .register_component(name, wasm_bytes, capabilities.clone())
            .await?;

        let pid = self
            .registry
            .spawn(name, Some(name), Some(capabilities))
            .await?;

        // Announce to registry document (best-effort)
        if let Err(e) = self
            .doc_registry
            .announce_spawn(&pid, name, Some(name))
            .await
        {
            tracing::debug!(error = %e, "Failed to announce spawn (no peers yet?)");
        }

        Ok(pid)
    }

    /// Spawn a new process from a registered component.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
        capabilities: Option<PolicySet>,
    ) -> Result<Pid> {
        let pid = self.registry.spawn(component, name, capabilities).await?;

        // Announce to registry document (best-effort)
        if let Err(e) = self
            .doc_registry
            .announce_spawn(&pid, component, name)
            .await
        {
            tracing::debug!(error = %e, "Failed to announce spawn (no peers yet?)");
        }

        Ok(pid)
    }

    /// Check if a process with the given name exists.
    pub async fn has_process(&self, name: &str) -> bool {
        self.registry.get_by_name(name).await.is_some()
    }

    /// Wait for shutdown (ctrl+c). The Router handles accept in the background.
    pub async fn run(&self) -> Result<()> {
        tracing::info!(node_id = %self.node_id(), "Actor runtime running");

        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutting down");

        self.router.shutdown().await?;
        Ok(())
    }
}
