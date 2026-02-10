use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::runtime::WasmActor;
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use tokio::sync::RwLock;
use wasmtime::component::Component;
use wasmtime::Engine;

/// A compiled component (WASM component) that can be spawned as processes.
pub struct ComponentTemplate {
    pub component: Component,
    pub default_capabilities: PolicySet,
}

/// A running process instance.
pub struct ProcessEntry {
    pub pid: Pid,
    pub actor: WasmActor,
    pub component_name: String,
    pub name: Option<String>,
}

/// Local process registry — manages components and running process instances.
///
/// Thread-safe: all internal state is behind RwLocks.
pub struct ProcessRegistry {
    pid_gen: PidGenerator,
    engine: Engine,
    processes: RwLock<HashMap<Pid, ProcessEntry>>,
    names: RwLock<HashMap<String, Pid>>,
    components: RwLock<HashMap<String, ComponentTemplate>>,
}

impl std::fmt::Debug for ProcessRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRegistry").finish_non_exhaustive()
    }
}

impl ProcessRegistry {
    pub fn new(pid_gen: PidGenerator, engine: Engine) -> Self {
        Self {
            pid_gen,
            engine,
            processes: RwLock::new(HashMap::new()),
            names: RwLock::new(HashMap::new()),
            components: RwLock::new(HashMap::new()),
        }
    }

    /// Register a compiled component (WASM component) by name.
    pub async fn register_component(
        &self,
        name: &str,
        wasm_bytes: &[u8],
        capabilities: PolicySet,
    ) -> Result<()> {
        let component = Component::from_binary(&self.engine, wasm_bytes)?;
        let template = ComponentTemplate {
            component,
            default_capabilities: capabilities,
        };
        self.components
            .write()
            .await
            .insert(name.to_string(), template);
        tracing::info!(component = %name, "Component registered");
        Ok(())
    }

    /// Spawn a new process from a registered component, optionally with a name.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
        capabilities: Option<PolicySet>,
    ) -> Result<Pid> {
        // Check name uniqueness
        if let Some(name) = name {
            let names = self.names.read().await;
            if names.contains_key(name) {
                return Err(anyhow!("name '{}' is already registered", name));
            }
        }

        let components = self.components.read().await;
        let template = components
            .get(component)
            .ok_or_else(|| anyhow!("component '{}' not registered", component))?;

        let caps = capabilities.unwrap_or_else(|| template.default_capabilities.clone());
        let actor = WasmActor::from_component(template.component.clone(), caps);
        let pid = self.pid_gen.next();

        let entry = ProcessEntry {
            pid: pid.clone(),
            actor,
            component_name: component.to_string(),
            name: name.map(|s| s.to_string()),
        };

        // Insert into registries
        self.processes.write().await.insert(pid.clone(), entry);
        if let Some(name) = name {
            self.names
                .write()
                .await
                .insert(name.to_string(), pid.clone());
        }

        tracing::info!(
            pid = %pid,
            component = %component,
            name = ?name,
            "Process spawned"
        );

        Ok(pid)
    }

    /// Look up a process by PID.
    pub async fn get_by_pid(&self, pid: &Pid) -> Option<ProcessRef> {
        let processes = self.processes.read().await;
        processes.get(pid).map(|entry| ProcessRef {
            pid: entry.pid.clone(),
            component: entry.actor.component().clone(),
            capabilities: entry.actor.capabilities().clone(),
            component_name: entry.component_name.clone(),
            name: entry.name.clone(),
        })
    }

    /// Resolve a name to a PID.
    pub async fn get_by_name(&self, name: &str) -> Option<Pid> {
        self.names.read().await.get(name).cloned()
    }

    /// Remove a process by PID.
    pub async fn remove(&self, pid: &Pid) -> Option<ProcessEntry> {
        let entry = self.processes.write().await.remove(pid);
        if let Some(ref entry) = entry {
            if let Some(ref name) = entry.name {
                self.names.write().await.remove(name);
            }
            tracing::info!(pid = %pid, "Process removed");
        }
        entry
    }

    /// List all running processes.
    pub async fn list_processes(&self) -> Vec<(Pid, String, Option<String>)> {
        self.processes
            .read()
            .await
            .values()
            .map(|entry| {
                (
                    entry.pid.clone(),
                    entry.component_name.clone(),
                    entry.name.clone(),
                )
            })
            .collect()
    }

    /// Get the PidGenerator (for creating PIDs externally, e.g. in gossip).
    pub fn pid_gen(&self) -> &PidGenerator {
        &self.pid_gen
    }

    /// Get the engine reference.
    pub fn engine(&self) -> &Engine {
        &self.engine
    }
}

/// A lightweight reference to a process (avoids holding the RwLock).
#[derive(Clone)]
pub struct ProcessRef {
    pub pid: Pid,
    pub component: Component,
    pub capabilities: PolicySet,
    pub component_name: String,
    pub name: Option<String>,
}

impl std::fmt::Debug for ProcessRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProcessRef")
            .field("pid", &self.pid)
            .field("component_name", &self.component_name)
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pid::PidGenerator;
    use iroh::SecretKey;

    fn make_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).unwrap()
    }

    #[tokio::test]
    async fn test_spawn_and_lookup() {
        let key = SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let engine = make_engine();
        let registry = ProcessRegistry::new(PidGenerator::new(node), engine);

        // We can't easily create a real WASM component in a unit test,
        // so we test the name/pid bookkeeping with a real deploy flow
        // in integration tests. Here we verify the empty state.
        assert!(registry.list_processes().await.is_empty());
        assert!(registry.get_by_name("echo").await.is_none());
    }

    #[tokio::test]
    async fn test_name_uniqueness() {
        let key = SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let engine = make_engine();
        let registry = ProcessRegistry::new(PidGenerator::new(node), engine);

        // Without a registered behavior, spawn should fail with "not registered"
        let result = registry.spawn("echo", Some("echo"), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }
}
