use crate::host::Database;
use crate::policy::PolicySet;
use crate::runtime::WasmActor;
use anyhow::Result;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use wasmtime::{Config, Engine};

/// The actor runtime - hosts WASM actors on an iroh endpoint.
pub struct ActorRuntime {
    engine: Engine,
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
    database: Arc<Database>,
}

impl ActorRuntime {
    /// Create a new actor runtime.
    pub async fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = Engine::new(&config)?;

        Ok(Self {
            engine,
            actors: Arc::new(RwLock::new(HashMap::new())),
            database: Arc::new(Database::new()),
        })
    }

    /// Get a reference to the WASM engine.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Get a reference to the shared database.
    pub fn database(&self) -> &Arc<Database> {
        &self.database
    }

    /// Deploy a WASM actor with the given ALPN and capabilities.
    pub async fn deploy(
        &self,
        alpn: Vec<u8>,
        wasm_bytes: &[u8],
        capabilities: PolicySet,
    ) -> Result<()> {
        let actor = WasmActor::new(&self.engine, wasm_bytes, capabilities)?;
        self.actors.write().await.insert(alpn, actor);
        Ok(())
    }

    /// Check if an actor is deployed for the given ALPN.
    pub async fn has_actor(&self, alpn: &[u8]) -> bool {
        self.actors.read().await.contains_key(alpn)
    }

    /// Run the runtime (placeholder - will add iroh accept loop later).
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Actor runtime started");
        // TODO: Add iroh endpoint and accept loop
        tokio::signal::ctrl_c().await?;
        tracing::info!("Actor runtime shutting down");
        Ok(())
    }
}
