use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::protocol::PlasmoidProtocol;
use crate::registry::ParticleRegistry;
use crate::runtime::start_particle;
use anyhow::Result;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointAddr, EndpointId, SecretKey};
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::{Config, Engine};

/// The single ALPN used for all plasmoid traffic.
pub const PLASMOID_ALPN: &[u8] = b"plasmoid/1";

/// How long a peer may be silent before it is considered down.
///
/// Erlang's `net_ticktime` default, adopted for its reasoning rather than its
/// number: the cost of being wrong is asymmetric. A false positive kills live
/// particles, while being slow only delays supervision — and on a mesh with
/// relay fallback and hole punching, multi-second gaps are ordinary. See #17.
pub const DEFAULT_NODE_TIMEOUT: Duration = Duration::from_secs(60);

/// The runtime - hosts WASM component instances on an iroh endpoint.
pub struct Runtime {
    router: Router,
    endpoint: Endpoint,
    engine: Engine,
    registry: Arc<ParticleRegistry>,
    peers: Arc<crate::transport::PeerLinks>,
    cluster: Arc<crate::cluster::Cluster>,
    #[allow(dead_code)]
    partitions: Arc<crate::partitions::Partitions>,
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
        Self::with_node_timeout(data_dir, DEFAULT_NODE_TIMEOUT).await
    }

    /// Create a runtime with a custom node-failure detection window.
    ///
    /// Exists because the default is deliberately slow: a minute is right for
    /// production and useless in a test that needs to observe a node dying.
    pub async fn with_node_timeout(
        data_dir: Option<&Path>,
        node_timeout: Duration,
    ) -> Result<Self> {
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
        // The QUIC idle timeout is what declares a peer down (#17): keep-alives
        // hold a healthy link open, and exceeding the timeout closes it, which
        // is what `Connection::closed` observes. No tick protocol of our own.
        let transport = iroh::endpoint::QuicTransportConfig::builder()
            .keep_alive_interval(node_timeout / 4)
            .max_idle_timeout(Some(
                node_timeout
                    .try_into()
                    .map_err(|_| anyhow::anyhow!("node timeout out of range"))?,
            ));

        let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder();
        let endpoint = Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret_key)
            .address_lookup(mdns)
            .transport_config(transport.build())
            .bind()
            .await?;

        let pid_gen = PidGenerator::new(endpoint.id());
        let registry = Arc::new(ParticleRegistry::new(pid_gen, engine.clone()));

        let peers = Arc::new(crate::transport::PeerLinks::new(endpoint.clone()));
        let cluster = Arc::new(crate::cluster::Cluster::new(endpoint.id()));
        let partitions = Arc::new(crate::partitions::Partitions::new(endpoint.id()));

        let protocol = PlasmoidProtocol::new(
            registry.clone(),
            engine.clone(),
            endpoint.clone(),
            peers.clone(),
            cluster.clone(),
            partitions.clone(),
        );

        crate::cluster_reactor::spawn_membership_reactor(cluster.clone(), peers.clone());
        crate::partitions::spawn_loss_reporter(cluster.clone(), peers.clone(), partitions.clone());
        crate::signals::spawn_signal_forwarder(registry.clone(), peers.clone());
        crate::signals::spawn_node_loss_reactor(registry.clone(), peers.clone());

        let router = Router::builder(endpoint.clone())
            .accept(PLASMOID_ALPN, protocol)
            .spawn();

        tracing::info!(endpoint_id = %endpoint.id(), "Runtime initialized");

        Ok(Self {
            router,
            endpoint,
            engine,
            registry,
            peers,
            cluster,
            partitions,
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
        let envelope = crate::transport::PeerMessage::Deliver(crate::transport::Envelope {
            target: crate::transport::Addressee::Pid(target),
            ref_id,
            payload: msg,
        });
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

    /// The nodes this node is clustered with.
    pub async fn nodes(&self) -> Vec<EndpointId> {
        self.cluster.nodes().await
    }

    /// Introduce this node to a cluster via one of its members.
    ///
    /// One introduction is enough: the mesh is transitive, so this node learns
    /// the rest and they learn it (#26).
    pub async fn join(&self, peer: EndpointId) {
        self.cluster.learn([peer]).await;
        crate::protocol::announce_to(&self.cluster, &self.peers, &[peer]).await;
    }

    /// The peer links this node sends over.
    #[doc(hidden)]
    pub fn peers_for_test(&self) -> Arc<crate::transport::PeerLinks> {
        self.peers.clone()
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

        Ok(pid)
    }

    /// Check if a particle with the given name exists.
    pub async fn has_particle(&self, name: &str) -> bool {
        self.registry.get_by_name(name).await.is_some()
    }

    /// Shut this node down.
    ///
    /// Dropping a `Runtime` does **not** do this: the endpoint is cloned into
    /// the router, the peer links and their writer tasks, so it outlives the
    /// struct and the node keeps answering. Closing it explicitly also lets
    /// peers learn of the departure from the connection close immediately,
    /// rather than waiting out the idle timeout.
    pub async fn shutdown(&self) -> Result<()> {
        self.router.shutdown().await?;
        self.endpoint.close().await;
        Ok(())
    }

    /// Wait for shutdown (ctrl+c). The Router handles accept in the background.
    pub async fn run(&self) -> Result<()> {
        tracing::info!(node_id = %self.node_id(), "Runtime running");

        wait_for_shutdown_signal().await?;
        tracing::info!("Shutting down");

        self.shutdown().await
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
