use crate::policy::PolicySet;
use crate::runtime::{accept, WasmActor};
use anyhow::Result;
use iroh::{Endpoint, EndpointAddr, EndpointId};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;
use wasmtime::{Config, Engine};

/// The actor runtime - hosts WASM actors on an iroh endpoint.
pub struct ActorRuntime {
    endpoint: Endpoint,
    engine: Engine,
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
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

        tracing::info!(endpoint_id = %endpoint.id(), "Actor runtime initialized");

        Ok(Self {
            endpoint,
            engine,
            actors: Arc::new(RwLock::new(HashMap::new())),
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

    /// Deploy a WASM actor with the given ALPN and capabilities.
    pub async fn deploy(
        &self,
        alpn: Vec<u8>,
        wasm_bytes: &[u8],
        capabilities: PolicySet,
    ) -> Result<()> {
        let actor = WasmActor::new(&self.engine, wasm_bytes, capabilities)?;
        self.actors.write().await.insert(alpn.clone(), actor);
        tracing::info!(alpn = ?String::from_utf8_lossy(&alpn), "Actor deployed");
        Ok(())
    }

    /// Check if an actor is deployed for the given ALPN.
    pub async fn has_actor(&self, alpn: &[u8]) -> bool {
        self.actors.read().await.contains_key(alpn)
    }

    /// Run the accept loop.
    pub async fn run(&self) -> Result<()> {
        tracing::info!(node_id = %self.node_id(), "Actor runtime accepting connections");

        loop {
            tokio::select! {
                Some(incoming) = self.endpoint.accept() => {
                    let actors = self.actors.clone();
                    let engine = self.engine.clone();
                    let endpoint = self.endpoint.clone();
                    tokio::spawn(async move {
                        if let Err(e) = accept::handle_incoming(incoming, actors, engine, endpoint).await {
                            tracing::error!(error = %e, "Failed to handle incoming connection");
                        }
                    });
                }
                _ = tokio::signal::ctrl_c() => {
                    tracing::info!("Shutting down");
                    break;
                }
            }
        }

        Ok(())
    }
}
