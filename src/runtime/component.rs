use crate::policy::PolicySet;
use anyhow::Result;
use wasmtime::Engine;
use wasmtime::component::Component;

/// A deployed WASM component instance.
pub struct LoadedComponent {
    component: Component,
    capabilities: PolicySet,
}

impl LoadedComponent {
    /// Create a new loaded component from component bytes.
    pub fn new(engine: &Engine, wasm_bytes: &[u8], capabilities: PolicySet) -> Result<Self> {
        let component = Component::from_binary(engine, wasm_bytes)?;
        Ok(Self {
            component,
            capabilities,
        })
    }

    /// Create a loaded component from an already-compiled component.
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
