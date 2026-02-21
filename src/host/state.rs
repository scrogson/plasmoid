use crate::doc_registry::DocRegistry;
use crate::pid::Pid;
use crate::policy::PolicySet;
use crate::registry::ParticleRegistry;
use iroh::Endpoint;
use std::sync::Arc;
use wasmtime::Engine;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// State available to host functions during WASM execution.
pub struct HostState {
    particle_id: String,
    particle_name: Option<String>,
    capabilities: PolicySet,
    pid: Option<Pid>,
    remote_pid: Option<Pid>,
    remote_node_id: Option<String>,
    endpoint: Option<Endpoint>,
    engine: Option<Engine>,
    registry: Option<Arc<ParticleRegistry>>,
    doc_registry: Option<Arc<DocRegistry>>,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("particle_id", &self.particle_id)
            .field("capabilities", &self.capabilities)
            .field("pid", &self.pid)
            .field("remote_node_id", &self.remote_node_id)
            .finish_non_exhaustive()
    }
}

impl WasiView for HostState {
    fn ctx(&mut self) -> WasiCtxView<'_> {
        WasiCtxView {
            ctx: &mut self.wasi_ctx,
            table: &mut self.resource_table,
        }
    }
}

impl HostState {
    pub fn new(particle_id: String, capabilities: PolicySet) -> Self {
        Self {
            particle_id,
            particle_name: None,
            capabilities,
            pid: None,
            remote_pid: None,
            remote_node_id: None,
            endpoint: None,
            engine: None,
            registry: None,
            doc_registry: None,
            wasi_ctx: WasiCtxBuilder::new().build(),
            resource_table: ResourceTable::new(),
        }
    }

    pub fn particle_id(&self) -> &str {
        &self.particle_id
    }

    pub fn particle_name(&self) -> Option<&str> {
        self.particle_name.as_deref()
    }

    pub fn set_particle_name(&mut self, name: Option<String>) {
        self.particle_name = name;
    }

    pub fn capabilities(&self) -> &PolicySet {
        &self.capabilities
    }

    pub fn pid(&self) -> Option<&Pid> {
        self.pid.as_ref()
    }

    pub fn set_pid(&mut self, pid: Option<Pid>) {
        self.pid = pid;
    }

    pub fn remote_pid(&self) -> Option<&Pid> {
        self.remote_pid.as_ref()
    }

    pub fn set_remote_pid(&mut self, pid: Option<Pid>) {
        self.remote_pid = pid;
    }

    pub fn remote_node_id(&self) -> Option<&String> {
        self.remote_node_id.as_ref()
    }

    pub fn set_remote_node_id(&mut self, node_id: Option<String>) {
        self.remote_node_id = node_id;
    }

    pub fn endpoint(&self) -> Option<&Endpoint> {
        self.endpoint.as_ref()
    }

    pub fn set_endpoint(&mut self, endpoint: Option<Endpoint>) {
        self.endpoint = endpoint;
    }

    pub fn engine(&self) -> Option<&Engine> {
        self.engine.as_ref()
    }

    pub fn set_engine(&mut self, engine: Option<Engine>) {
        self.engine = engine;
    }

    pub fn registry(&self) -> Option<&Arc<ParticleRegistry>> {
        self.registry.as_ref()
    }

    pub fn set_registry(&mut self, registry: Option<Arc<ParticleRegistry>>) {
        self.registry = registry;
    }

    pub fn doc_registry(&self) -> Option<&Arc<DocRegistry>> {
        self.doc_registry.as_ref()
    }

    pub fn set_doc_registry(&mut self, doc_registry: Option<Arc<DocRegistry>>) {
        self.doc_registry = doc_registry;
    }

    pub fn resource_table(&self) -> &ResourceTable {
        &self.resource_table
    }

    pub fn resource_table_mut(&mut self) -> &mut ResourceTable {
        &mut self.resource_table
    }
}
