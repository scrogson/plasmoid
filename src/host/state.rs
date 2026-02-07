use crate::policy::PolicySet;

/// State available to host functions during WASM execution.
#[derive(Debug)]
pub struct HostState {
    actor_id: String,
    capabilities: PolicySet,
    remote_node_id: Option<String>,
}

impl HostState {
    pub fn new(actor_id: String, capabilities: PolicySet) -> Self {
        Self {
            actor_id,
            capabilities,
            remote_node_id: None,
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
}
