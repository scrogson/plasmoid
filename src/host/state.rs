use crate::policy::PolicySet;
use iroh::Endpoint;
use wasmtime::component::ResourceTable;
use wasmtime_wasi::{WasiCtx, WasiCtxBuilder, WasiCtxView, WasiView};

/// State available to host functions during WASM execution.
pub struct HostState {
    actor_id: String,
    capabilities: PolicySet,
    remote_node_id: Option<String>,
    endpoint: Option<Endpoint>,
    wasi_ctx: WasiCtx,
    resource_table: ResourceTable,
}

impl std::fmt::Debug for HostState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HostState")
            .field("actor_id", &self.actor_id)
            .field("capabilities", &self.capabilities)
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
    pub fn new(actor_id: String, capabilities: PolicySet) -> Self {
        Self {
            actor_id,
            capabilities,
            remote_node_id: None,
            endpoint: None,
            wasi_ctx: WasiCtxBuilder::new().build(),
            resource_table: ResourceTable::new(),
        }
    }

    pub fn actor_id(&self) -> &str {
        &self.actor_id
    }

    pub fn capabilities(&self) -> &PolicySet {
        &self.capabilities
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
}
