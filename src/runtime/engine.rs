use crate::doc_registry::DocRegistry;
use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::protocol::PlasmoidProtocol;
use crate::registry::ParticleRegistry;
use crate::runtime::start_particle;
use anyhow::Result;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use iroh_blobs::BlobsProtocol;
use iroh_blobs::protocol::ALPN as BLOBS_ALPN;
use iroh_blobs::store::mem::MemStore;
use iroh_docs::net::ALPN as DOCS_ALPN;
use iroh_docs::protocol::Docs;
use iroh_gossip::net::{GOSSIP_ALPN, Gossip};
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
    peers: Arc<crate::transport::PeerLinks>,
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
        let key = SecretKey::generate();
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
        config.async_support(true);
        let engine = Engine::new(&config)?;

        // Load or generate secret key
        let secret_key = match data_dir {
            Some(dir) => load_or_generate_secret_key(dir)?,
            None => SecretKey::generate(),
        };

        // Configure iroh endpoint with mDNS for local network discovery
        let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder();
        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
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

        let peers = Arc::new(crate::transport::PeerLinks::new(endpoint.clone()));

        let protocol = PlasmoidProtocol::new(
            registry.clone(),
            engine.clone(),
            endpoint.clone(),
            Some(doc_registry.clone()),
            peers.clone(),
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
            peers,
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

    /// The runtime context handed to every particle this node starts.
    pub fn particle_context(
        &self,
        mailbox: Arc<crate::mailbox::Mailbox>,
    ) -> crate::runtime::ParticleContext {
        crate::runtime::ParticleContext {
            mailbox,
            registry: self.registry.clone(),
            endpoint: Some(self.endpoint.clone()),
            doc_registry: Some(self.doc_registry.clone()),
            peers: Some(self.peers.clone()),
        }
    }

    /// Route a message exactly as a particle's `send` would.
    ///
    /// Exists so the routing decision can be tested without a WASM component,
    /// which would prove nothing extra about where a message goes.
    #[doc(hidden)]
    pub async fn deliver_for_test(&self, target: Pid, ref_id: Option<u64>, msg: Vec<u8>) {
        if target.is_local_to(&self.node_id()) {
            let _ = match ref_id {
                Some(r) => self.registry.send_tagged_to_pid(&target, r, msg).await,
                None => self.registry.send_to_pid(&target, msg).await,
            };
            return;
        }
        let node = target.node;
        let envelope = crate::transport::Envelope {
            target: crate::transport::Addressee::Pid(target),
            ref_id,
            payload: msg,
        };
        let _ = self.peers.send(node, &envelope);
    }

    /// Spawn on another node exactly as a particle's `spawn-on` would.
    ///
    /// Exists so remote spawn can be tested without a WASM component driving it.
    /// Spawn on a node exactly as a particle's `spawn-on` would.
    ///
    /// Goes through the same `remote_spawn` the host function calls, so the
    /// error classification and the self-node path are genuinely exercised —
    /// an earlier version dialled `NodeClient` directly and tested none of it.
    #[doc(hidden)]
    pub async fn spawn_on_for_test(
        &self,
        node: iroh::EndpointId,
        component: &str,
        name: Option<&str>,
        init_args: &str,
    ) -> Result<Pid, crate::mailbox::SpawnFailure> {
        crate::runtime::remote_spawn(
            Some(self.particle_context(Arc::new(crate::mailbox::Mailbox::new()))),
            hex::encode(node.as_bytes()),
            component.to_string(),
            name.map(|s| s.to_string()),
            init_args.to_string(),
        )
        .await
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
    /// `init_args` is the wasm-wave string passed to the component's `start` export.
    /// Returns the PID of the spawned particle.
    pub async fn deploy(
        &self,
        component: &str,
        wasm_bytes: &[u8],
        name: Option<&str>,
        capabilities: PolicySet,
        init_args: &str,
    ) -> Result<Pid> {
        self.load(component, wasm_bytes, capabilities.clone())
            .await?;
        self.spawn(component, name, Some(capabilities), init_args)
            .await
    }

    /// Spawn a new particle from a registered component.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
        capabilities: Option<PolicySet>,
        init_args: &str,
    ) -> Result<Pid> {
        // Look up the component template
        let (comp, default_caps) = self
            .registry
            .get_component(component)
            .await
            .ok_or_else(|| anyhow::anyhow!("component '{}' not registered", component))?;

        let caps = capabilities.unwrap_or(default_caps);

        // Spawn in the registry (creates mailbox)
        let (pid, mailbox) = self
            .registry
            .spawn(component, name, Some(caps.clone()))
            .await?;

        // Start the particle (calls component's start function)
        start_particle(
            &self.engine,
            &comp,
            &caps,
            pid.clone(),
            name.map(|s| s.to_string()),
            init_args,
            self.particle_context(mailbox),
        )
        .await?;

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

        wait_for_shutdown_signal().await?;
        tracing::info!("Shutting down");

        // Deregister before tearing down the router, so peers stop resolving
        // this node's particles immediately rather than waiting to notice the
        // connection is gone.
        self.doc_registry.announce_all_down().await;

        self.router.shutdown().await?;
        Ok(())
    }
}

/// Wait for an orderly shutdown signal.
///
/// SIGTERM matters as much as ctrl-c here: container runtimes send it, and a
/// node that misses it dies without deregistering, leaving peers advertising
/// particles that are gone.
async fn wait_for_shutdown_signal() -> Result<()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate())?;
        tokio::select! {
            r = tokio::signal::ctrl_c() => r?,
            _ = term.recv() => {}
        }
        Ok(())
    }
    #[cfg(not(unix))]
    {
        tokio::signal::ctrl_c().await?;
        Ok(())
    }
}
