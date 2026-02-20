use crate::doc_registry::DocRegistry;
use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::protocol::PlasmoidProtocol;
use crate::registry::ParticleRegistry;
use anyhow::Result;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_blobs::store::mem::MemStore;
use iroh_blobs::BlobsProtocol;
use iroh_blobs::protocol::ALPN as BLOBS_ALPN;
use iroh_docs::net::ALPN as DOCS_ALPN;
use iroh_docs::protocol::Docs;
use iroh_gossip::net::{Gossip, GOSSIP_ALPN};
use std::path::Path;
use std::sync::Arc;
use wasmtime::{Config, Engine};

/// The single ALPN used for all plasmoid traffic.
pub const PLASMOID_ALPN: &[u8] = b"plasmoid/1";

/// The runtime - hosts WASM component instances on an iroh endpoint.
pub struct Runtime {
    router: Router,
    endpoint: Endpoint,
    engine: Engine,
    registry: Arc<ParticleRegistry>,
    doc_registry: Arc<DocRegistry>,
}

/// Load or generate a secret key from a data directory.
///
/// Persists the secret key at `<data_dir>/secret_key` and writes the
/// public node ID to `<data_dir>/node_id` for easy scripting.
fn load_or_generate_secret_key(data_dir: &Path) -> Result<SecretKey> {
    let key_path = data_dir.join("secret_key");

    if key_path.exists() {
        let bytes = std::fs::read(&key_path)?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("invalid secret key file (expected 32 bytes)"))?;
        let key = SecretKey::from_bytes(&bytes);
        tracing::info!(path = %key_path.display(), "Loaded secret key");

        // Ensure node_id file is up to date
        let node_id_path = data_dir.join("node_id");
        let _ = std::fs::write(&node_id_path, key.public().to_string());

        Ok(key)
    } else {
        std::fs::create_dir_all(data_dir)?;
        let key = SecretKey::generate(&mut rand::rng());
        std::fs::write(&key_path, key.to_bytes())?;

        // Write public node ID for easy scripting
        let node_id_path = data_dir.join("node_id");
        std::fs::write(&node_id_path, key.public().to_string())?;

        // Best-effort: restrict secret key permissions to owner-only on unix
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key_path, std::fs::Permissions::from_mode(0o600))?;
        }

        tracing::info!(path = %key_path.display(), "Generated and saved new secret key");
        Ok(key)
    }
}

impl Runtime {
    /// Create a new runtime with an optional data directory for persistent identity.
    ///
    /// If `data_dir` is provided, the node's secret key is loaded from (or saved to)
    /// `<data_dir>/secret_key`, giving the node a stable identity across restarts.
    /// If `None`, a random key is generated each time.
    pub async fn new(data_dir: Option<&Path>) -> Result<Self> {
        // Configure wasmtime
        let mut config = Config::new();
        config.wasm_component_model(true);
        let engine = Engine::new(&config)?;

        // Load or generate secret key
        let secret_key = match data_dir {
            Some(dir) => load_or_generate_secret_key(dir)?,
            None => SecretKey::generate(&mut rand::rng()),
        };

        // Configure iroh endpoint with mDNS for local network discovery
        let mdns = iroh::address_lookup::mdns::MdnsAddressLookup::builder();
        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
            .address_lookup(mdns)
            .bind()
            .await?;

        let pid_gen = PidGenerator::new(endpoint.id());
        let registry = Arc::new(ParticleRegistry::new(pid_gen, engine.clone()));

        // Create blob store (in-memory)
        let blobs = MemStore::new();

        // Create gossip instance (used by iroh-docs internally for sync)
        let gossip = Gossip::builder().spawn(endpoint.clone());

        // Create docs protocol backed by blobs and gossip
        let docs = Docs::memory()
            .spawn(endpoint.clone(), blobs.clone().into(), gossip.clone())
            .await?;

        // Create doc-backed distributed registry and start event processing
        let doc_registry = DocRegistry::new(
            registry.clone(),
            endpoint.clone(),
            docs.clone(),
            blobs.clone(),
        )
        .await?;
        doc_registry.start(&[]).await?;

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

        tracing::info!(endpoint_id = %endpoint.id(), "Runtime initialized");

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

    /// Get a reference to the particle registry.
    pub fn registry(&self) -> &Arc<ParticleRegistry> {
        &self.registry
    }

    /// Get a reference to the doc registry.
    pub fn doc_registry(&self) -> &Arc<DocRegistry> {
        &self.doc_registry
    }

    /// Add bootstrap peers to the cluster for doc sync.
    pub async fn join_cluster(&self, peers: Vec<EndpointId>) -> Result<()> {
        self.doc_registry.add_peers(&peers).await?;
        tracing::info!(peers = peers.len(), "Joined cluster");
        Ok(())
    }

    /// Load a WASM component without spawning any particle.
    pub async fn load(
        &self,
        component: &str,
        wasm_bytes: &[u8],
        capabilities: PolicySet,
    ) -> Result<()> {
        self.registry
            .register_component(component, wasm_bytes, capabilities)
            .await
    }

    /// List all registered component names.
    pub async fn list_components(&self) -> Vec<String> {
        self.registry.list_components().await
    }

    /// Deploy a WASM component and spawn one particle from it.
    ///
    /// `component` is the module name (used to register the code).
    /// `name` is an optional registered name for the spawned particle.
    /// Returns the PID of the spawned particle.
    pub async fn deploy(
        &self,
        component: &str,
        wasm_bytes: &[u8],
        name: Option<&str>,
        capabilities: PolicySet,
    ) -> Result<Pid> {
        self.load(component, wasm_bytes, capabilities.clone())
            .await?;
        self.spawn(component, name, Some(capabilities)).await
    }

    /// Spawn a new particle from a registered component.
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

    /// Check if a particle with the given name exists.
    pub async fn has_particle(&self, name: &str) -> bool {
        self.registry.get_by_name(name).await.is_some()
    }

    /// Wait for shutdown (ctrl+c). The Router handles accept in the background.
    pub async fn run(&self) -> Result<()> {
        tracing::info!(node_id = %self.node_id(), "Runtime running");

        tokio::signal::ctrl_c().await?;
        tracing::info!("Shutting down");

        self.router.shutdown().await?;
        Ok(())
    }
}
