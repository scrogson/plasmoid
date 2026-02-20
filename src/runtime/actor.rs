use crate::policy::PolicySet;
use anyhow::Result;
use wasmtime::component::Component;
use wasmtime::Engine;

/// A deployed WASM component instance.
pub struct WasmActor {
    component: Component,
    capabilities: PolicySet,
}

impl WasmActor {
    /// Create a new WASM actor from component bytes.
    pub fn new(engine: &Engine, wasm_bytes: &[u8], capabilities: PolicySet) -> Result<Self> {
        let component = Component::from_binary(engine, wasm_bytes)?;
        Ok(Self {
            component,
            capabilities,
        })
    }

    /// Create a WASM actor from an already-compiled component.
    pub fn from_component(component: Component, capabilities: PolicySet) -> Self {
        Self {
            component,
            capabilities,
        }
    }

    /// Get the compiled component.
    pub fn component(&self) -> &Component {
        &self.component
    }

    /// Get the component's capabilities.
    pub fn capabilities(&self) -> &PolicySet {
        &self.capabilities
    }
}
