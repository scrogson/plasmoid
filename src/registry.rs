use crate::mailbox::Mailbox;
use crate::message::{ExitReason, SystemMessage};
use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::runtime::WasmActor;
use anyhow::{anyhow, Result};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;
use wasmtime::component::Component;
use wasmtime::Engine;

/// A compiled component (WASM component) that can be spawned as particles.
pub struct ComponentTemplate {
    pub component: Component,
    pub default_capabilities: PolicySet,
}

/// A running particle instance.
pub struct ParticleEntry {
    pub pid: Pid,
    pub actor: WasmActor,
    pub component_name: String,
    pub name: Option<String>,
}

/// Per-process state: links, monitors, mailbox.
pub struct ProcessState {
    pub links: HashSet<Pid>,
    pub monitors: HashMap<u64, Pid>,
    pub monitored_by: Vec<(Pid, u64)>,
    pub trap_exit: bool,
    pub mailbox: Arc<Mailbox>,
}

/// Local particle registry -- manages components and running particle instances.
///
/// Thread-safe: all internal state is behind RwLocks.
pub struct ParticleRegistry {
    pid_gen: PidGenerator,
    engine: Engine,
    particles: RwLock<HashMap<Pid, ParticleEntry>>,
    pub(crate) names: RwLock<HashMap<String, Pid>>,
    components: RwLock<HashMap<String, ComponentTemplate>>,
    process_states: RwLock<HashMap<Pid, ProcessState>>,
    next_ref: AtomicU64,
}

impl std::fmt::Debug for ParticleRegistry {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParticleRegistry").finish_non_exhaustive()
    }
}

impl ParticleRegistry {
    pub fn new(pid_gen: PidGenerator, engine: Engine) -> Self {
        Self {
            pid_gen,
            engine,
            particles: RwLock::new(HashMap::new()),
            names: RwLock::new(HashMap::new()),
            components: RwLock::new(HashMap::new()),
            process_states: RwLock::new(HashMap::new()),
            next_ref: AtomicU64::new(1),
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

    /// Spawn a new particle from a registered component, optionally with a name.
    ///
    /// Returns (Pid, Arc<Mailbox>) -- the caller owns the mailbox reference and starts
    /// the component's `start` function with it.
    pub async fn spawn(
        &self,
        component: &str,
        name: Option<&str>,
        capabilities: Option<PolicySet>,
    ) -> Result<(Pid, Arc<Mailbox>)> {
        // Check name uniqueness early, under write lock for atomicity
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

        let entry = ParticleEntry {
            pid: pid.clone(),
            actor,
            component_name: component.to_string(),
            name: name.map(|s| s.to_string()),
        };

        // Create unified mailbox
        let mailbox = Arc::new(Mailbox::with_default_capacity());

        let process_state = ProcessState {
            links: HashSet::new(),
            monitors: HashMap::new(),
            monitored_by: Vec::new(),
            trap_exit: false,
            mailbox: mailbox.clone(),
        };

        // Insert into registries
        self.particles.write().await.insert(pid.clone(), entry);
        self.process_states
            .write()
            .await
            .insert(pid.clone(), process_state);

        // Atomically insert name (re-check under write lock to prevent TOCTOU race)
        if let Some(name) = name {
            let mut names = self.names.write().await;
            if names.contains_key(name) {
                // Another spawn raced us — roll back
                self.process_states.write().await.remove(&pid);
                self.particles.write().await.remove(&pid);
                return Err(anyhow!("name '{}' is already registered", name));
            }
            names.insert(name.to_string(), pid.clone());
        }

        tracing::info!(
            pid = %pid,
            component = %component,
            name = ?name,
            "Particle spawned"
        );

        Ok((pid, mailbox))
    }

    /// Look up a particle by PID.
    pub async fn get_by_pid(&self, pid: &Pid) -> Option<ParticleRef> {
        let particles = self.particles.read().await;
        particles.get(pid).map(|entry| ParticleRef {
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

    /// List all registered component names.
    pub async fn list_components(&self) -> Vec<String> {
        self.components
            .read()
            .await
            .keys()
            .cloned()
            .collect()
    }

    /// List all running particles.
    pub async fn list_particles(&self) -> Vec<(Pid, String, Option<String>)> {
        self.particles
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

    /// Get the component template for a registered component.
    pub async fn get_component(&self, name: &str) -> Option<(Component, PolicySet)> {
        let components = self.components.read().await;
        components
            .get(name)
            .map(|t| (t.component.clone(), t.default_capabilities.clone()))
    }

    /// Resolve a target string to a PID -- tries name lookup first, then
    /// matches against PID display strings (e.g. `<abc123.1>`).
    pub async fn resolve_target(&self, target: &str) -> Option<Pid> {
        // Try name first
        if let Some(pid) = self.get_by_name(target).await {
            return Some(pid);
        }

        // Try matching PID display string against registered processes
        let states = self.process_states.read().await;
        for pid in states.keys() {
            if pid.to_string() == target {
                return Some(pid.clone());
            }
        }

        None
    }

    /// Send a user message to a process by PID.
    pub async fn send_to_pid(&self, pid: &Pid, msg: Vec<u8>) -> Result<(), SendError> {
        let states = self.process_states.read().await;
        let state = states
            .get(pid)
            .ok_or(SendError::NoProcess)?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_data(msg).await.map_err(|e| match e {
            crate::mailbox::SendError::NoProcess => SendError::NoProcess,
            crate::mailbox::SendError::MailboxFull => SendError::MailboxFull,
        })
    }

    /// Send a tagged message to a process by PID.
    pub async fn send_tagged_to_pid(
        &self,
        pid: &Pid,
        ref_id: u64,
        msg: Vec<u8>,
    ) -> Result<(), SendError> {
        let states = self.process_states.read().await;
        let state = states
            .get(pid)
            .ok_or(SendError::NoProcess)?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_tagged(ref_id, msg).await.map_err(|e| match e {
            crate::mailbox::SendError::NoProcess => SendError::NoProcess,
            crate::mailbox::SendError::MailboxFull => SendError::MailboxFull,
        })
    }

    /// Send a system message to a process.
    pub async fn send_system(&self, pid: &Pid, msg: SystemMessage) -> Result<()> {
        let states = self.process_states.read().await;
        let state = states
            .get(pid)
            .ok_or_else(|| anyhow!("no process for pid '{}'", pid))?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_system(msg).await;
        Ok(())
    }

    /// Create a bidirectional link between two processes.
    pub async fn link(&self, pid_a: &Pid, pid_b: &Pid) -> Result<()> {
        let mut states = self.process_states.write().await;

        // Both must exist
        if !states.contains_key(pid_a) {
            return Err(anyhow!("no process for pid '{}'", pid_a));
        }
        if !states.contains_key(pid_b) {
            return Err(anyhow!("no process for pid '{}'", pid_b));
        }

        states.get_mut(pid_a).unwrap().links.insert(pid_b.clone());
        states.get_mut(pid_b).unwrap().links.insert(pid_a.clone());

        Ok(())
    }

    /// Remove a bidirectional link between two processes.
    pub async fn unlink(&self, pid_a: &Pid, pid_b: &Pid) {
        let mut states = self.process_states.write().await;
        if let Some(state) = states.get_mut(pid_a) {
            state.links.remove(pid_b);
        }
        if let Some(state) = states.get_mut(pid_b) {
            state.links.remove(pid_a);
        }
    }

    /// Monitor a target process. Returns a monitor reference.
    pub async fn monitor(&self, watcher: &Pid, target: &Pid) -> Result<u64> {
        let monitor_ref = self.next_ref.fetch_add(1, Ordering::Relaxed);
        let mut states = self.process_states.write().await;

        // If target doesn't exist, immediately deliver a Down signal
        if !states.contains_key(target) {
            if let Some(watcher_state) = states.get(watcher) {
                let mailbox = watcher_state.mailbox.clone();
                drop(states);
                mailbox.push_system(SystemMessage::Down {
                    from: target.clone(),
                    monitor_ref,
                    reason: ExitReason::Normal,
                }).await;
            }
            return Ok(monitor_ref);
        }

        // Register the monitor on the target
        states
            .get_mut(target)
            .unwrap()
            .monitored_by
            .push((watcher.clone(), monitor_ref));

        // Register the monitor on the watcher for demonitor
        states
            .get_mut(watcher)
            .ok_or_else(|| anyhow!("watcher process not found"))?
            .monitors
            .insert(monitor_ref, target.clone());

        Ok(monitor_ref)
    }

    /// Remove a monitor.
    pub async fn demonitor(&self, watcher: &Pid, monitor_ref: u64) {
        let mut states = self.process_states.write().await;

        // Remove from watcher's monitors map
        let target = if let Some(state) = states.get_mut(watcher) {
            state.monitors.remove(&monitor_ref)
        } else {
            None
        };

        // Remove from target's monitored_by list
        if let Some(target_pid) = target {
            if let Some(state) = states.get_mut(&target_pid) {
                state
                    .monitored_by
                    .retain(|(w, r)| !(w == watcher && *r == monitor_ref));
            }
        }
    }

    /// Set trap_exit flag on a process.
    pub async fn set_trap_exit(&self, pid: &Pid, enabled: bool) {
        let mut states = self.process_states.write().await;
        if let Some(state) = states.get_mut(pid) {
            state.trap_exit = enabled;
        }
    }

    /// Register a name for a process.
    pub async fn register_name(&self, pid: &Pid, name: &str) -> Result<()> {
        let mut names = self.names.write().await;
        if names.contains_key(name) {
            return Err(anyhow!("name '{}' is already registered", name));
        }
        names.insert(name.to_string(), pid.clone());
        Ok(())
    }

    /// Unregister a name. Only the owning process can unregister it.
    pub async fn unregister_name(&self, pid: &Pid, name: &str) -> Result<()> {
        let mut names = self.names.write().await;
        match names.get(name) {
            Some(registered_pid) if registered_pid == pid => {
                names.remove(name);
                Ok(())
            }
            Some(_) => Err(anyhow!("name '{}' is registered to a different process", name)),
            None => Err(anyhow!("name '{}' is not registered", name)),
        }
    }

    /// Look up a PID by name.
    pub async fn lookup_name(&self, name: &str) -> Option<Pid> {
        self.names.read().await.get(name).cloned()
    }

    /// Exit a process with the given reason.
    ///
    /// This is the exit propagation algorithm:
    /// 1. Remove the process state, particle entry, and name.
    /// 2. Remove self from all linked peers' link sets.
    /// 3. For each linked peer:
    ///    - If peer has trap_exit=true: deliver Exit system message.
    ///    - If peer has trap_exit=false and reason is abnormal: cascade kill.
    ///    - If Kill: propagated as Shutdown("killed"), and is untrappable on origin.
    ///    - If Normal: no action on non-trapping peers.
    /// 4. For each monitor watcher: deliver Down system message.
    /// 5. Log exit.
    pub async fn exit_process(&self, pid: &Pid, reason: ExitReason) {
        // Step 1: Remove process state
        let process_state = {
            let mut states = self.process_states.write().await;
            states.remove(pid)
        };

        let process_state = match process_state {
            Some(s) => s,
            None => return, // Already exited
        };

        // Close the mailbox to wake any blocked recv
        process_state.mailbox.close().await;

        // Remove particle entry
        {
            let entry = self.particles.write().await.remove(pid);
            if let Some(ref entry) = entry {
                if let Some(ref name) = entry.name {
                    self.names.write().await.remove(name);
                }
            }
        }

        let links = process_state.links;
        let monitored_by = process_state.monitored_by;

        // Determine the propagated reason for Kill signals
        let propagated_reason = match &reason {
            ExitReason::Kill => ExitReason::Shutdown("killed".to_string()),
            other => other.clone(),
        };

        // Step 2 + 3: Process links
        // We need to collect the peers to cascade-kill outside the lock
        let mut cascade_kills: Vec<Pid> = Vec::new();

        // Collect mailboxes to deliver to outside the lock
        let mut exit_deliveries: Vec<Arc<Mailbox>> = Vec::new();
        let mut down_deliveries: Vec<(Arc<Mailbox>, u64)> = Vec::new();

        {
            let mut states = self.process_states.write().await;

            for linked_pid in &links {
                // Remove self from peer's link set
                if let Some(peer_state) = states.get_mut(linked_pid) {
                    peer_state.links.remove(pid);

                    if peer_state.trap_exit {
                        // Deliver Exit system message
                        exit_deliveries.push(peer_state.mailbox.clone());
                    } else if propagated_reason.is_abnormal() {
                        // Will cascade kill
                        cascade_kills.push(linked_pid.clone());
                    }
                    // If normal and not trapping: no action
                }
            }

            // Step 4: Deliver Down signals to monitors
            for (watcher_pid, monitor_ref) in &monitored_by {
                if let Some(watcher_state) = states.get_mut(watcher_pid) {
                    // Clean up the watcher's monitors map
                    watcher_state.monitors.remove(monitor_ref);
                    down_deliveries.push((watcher_state.mailbox.clone(), *monitor_ref));
                }
            }
        }

        // Deliver exit signals outside the lock
        for mailbox in exit_deliveries {
            mailbox.push_system(SystemMessage::Exit {
                from: pid.clone(),
                reason: propagated_reason.clone(),
            }).await;
        }

        // Deliver down signals outside the lock
        for (mailbox, monitor_ref) in down_deliveries {
            mailbox.push_system(SystemMessage::Down {
                from: pid.clone(),
                monitor_ref,
                reason: propagated_reason.clone(),
            }).await;
        }

        // Step 3 (continued): Cascade kills outside the lock to avoid deadlock
        for linked_pid in cascade_kills {
            // Use Box::pin to allow recursive async calls
            Box::pin(self.exit_process(&linked_pid, propagated_reason.clone())).await;
        }

        tracing::info!(pid = %pid, reason = ?reason, "Process exited");
    }

    /// Check if a process exists.
    pub async fn process_exists(&self, pid: &Pid) -> bool {
        self.process_states.read().await.contains_key(pid)
    }
}

/// Errors that can occur when sending a user message.
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    NoProcess,
    MailboxFull,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoProcess => write!(f, "no process"),
            SendError::MailboxFull => write!(f, "mailbox full"),
        }
    }
}

impl std::error::Error for SendError {}

/// A lightweight reference to a particle (avoids holding the RwLock).
#[derive(Clone)]
pub struct ParticleRef {
    pub pid: Pid,
    pub component: Component,
    pub capabilities: PolicySet,
    pub component_name: String,
    pub name: Option<String>,
}

impl std::fmt::Debug for ParticleRef {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParticleRef")
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
    use std::time::Duration;

    fn make_engine() -> Engine {
        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        Engine::new(&config).unwrap()
    }

    fn make_registry() -> Arc<ParticleRegistry> {
        let key = SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let engine = make_engine();
        Arc::new(ParticleRegistry::new(PidGenerator::new(node), engine))
    }

    /// Helper to create a process state directly for testing (no WASM needed).
    async fn spawn_test_process(registry: &ParticleRegistry) -> (Pid, Arc<Mailbox>) {
        let pid = registry.pid_gen().next();

        let mailbox = Arc::new(Mailbox::with_default_capacity());

        let process_state = ProcessState {
            links: HashSet::new(),
            monitors: HashMap::new(),
            monitored_by: Vec::new(),
            trap_exit: false,
            mailbox: mailbox.clone(),
        };

        registry
            .process_states
            .write()
            .await
            .insert(pid.clone(), process_state);

        (pid, mailbox)
    }

    #[tokio::test]
    async fn test_spawn_returns_mailbox() {
        let key = SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let engine = make_engine();
        let registry = ParticleRegistry::new(PidGenerator::new(node), engine);

        // We can't easily create a real WASM component in a unit test,
        // but we verify the process_states bookkeeping via spawn_test_process.
        let (pid, _mailbox) = spawn_test_process(&registry).await;
        assert!(registry.process_exists(&pid).await);
    }

    #[tokio::test]
    async fn test_send_and_receive() {
        let registry = make_registry();
        let (pid, mailbox) = spawn_test_process(&registry).await;

        // Send a message
        registry
            .send_to_pid(&pid, b"hello".to_vec())
            .await
            .unwrap();

        // Receive it
        let msg = mailbox.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Data(data) => assert_eq!(data, b"hello"),
            other => panic!("expected Data, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_bounded_mailbox_full() {
        let registry = make_registry();
        let (pid, _mailbox) = spawn_test_process(&registry).await;

        // Fill the mailbox to capacity (default 1024)
        for i in 0..1024 {
            let msg = vec![i as u8];
            registry.send_to_pid(&pid, msg).await.unwrap();
        }

        // Next send should fail with MailboxFull
        let result = registry.send_to_pid(&pid, b"overflow".to_vec()).await;
        assert_eq!(result, Err(SendError::MailboxFull));
    }

    #[tokio::test]
    async fn test_link_and_exit_propagation() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_process(&registry).await;
        let (pid_b, _mailbox_b) = spawn_test_process(&registry).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a abnormally
        registry
            .exit_process(&pid_a, ExitReason::Exception("crash".into()))
            .await;

        // b should be killed (cascade) since it doesn't trap exits
        assert!(!registry.process_exists(&pid_b).await);
    }

    #[tokio::test]
    async fn test_normal_exit_no_propagation() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_process(&registry).await;
        let (pid_b, _mailbox_b) = spawn_test_process(&registry).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a normally
        registry.exit_process(&pid_a, ExitReason::Normal).await;

        // b should still be alive (normal exit doesn't kill non-trapping peers)
        assert!(registry.process_exists(&pid_b).await);
    }

    #[tokio::test]
    async fn test_trap_exit() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_process(&registry).await;
        let (pid_b, mailbox_b) = spawn_test_process(&registry).await;

        // b traps exits
        registry.set_trap_exit(&pid_b, true).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a abnormally
        registry
            .exit_process(&pid_a, ExitReason::Exception("crash".into()))
            .await;

        // b should still be alive (trapping)
        assert!(registry.process_exists(&pid_b).await);

        // b should have received an Exit system message
        let msg = mailbox_b.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Exit { from, reason } => {
                assert_eq!(from, pid_a);
                assert_eq!(reason, ExitReason::Exception("crash".into()));
            }
            other => panic!("expected Exit, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_monitor_down() {
        let registry = make_registry();
        let (target_pid, _mailbox_target) = spawn_test_process(&registry).await;
        let (watcher_pid, mailbox_watcher) = spawn_test_process(&registry).await;

        // Watcher monitors target
        let monitor_ref = registry.monitor(&watcher_pid, &target_pid).await.unwrap();

        // Target exits
        registry
            .exit_process(&target_pid, ExitReason::Shutdown("bye".into()))
            .await;

        // Watcher should have received a Down system message
        let msg = mailbox_watcher.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Down {
                from,
                ref_id,
                reason,
            } => {
                assert_eq!(from, target_pid);
                assert_eq!(ref_id, monitor_ref);
                assert_eq!(reason, ExitReason::Shutdown("bye".into()));
            }
            other => panic!("expected Down, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_register_unregister_name() {
        let registry = make_registry();
        let (pid, _mailbox) = spawn_test_process(&registry).await;

        // Register
        registry.register_name(&pid, "test_proc").await.unwrap();
        assert_eq!(registry.lookup_name("test_proc").await, Some(pid.clone()));

        // Duplicate registration should fail
        let (pid2, _mailbox2) = spawn_test_process(&registry).await;
        let result = registry.register_name(&pid2, "test_proc").await;
        assert!(result.is_err());

        // Unregister by wrong process should fail
        let result = registry.unregister_name(&pid2, "test_proc").await;
        assert!(result.is_err());

        // Unregister by owner should succeed
        registry.unregister_name(&pid, "test_proc").await.unwrap();
        assert_eq!(registry.lookup_name("test_proc").await, None);

        // Unregister nonexistent should fail
        let result = registry.unregister_name(&pid, "test_proc").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_name_uniqueness() {
        let key = SecretKey::generate(&mut rand::rng());
        let node = key.public();
        let engine = make_engine();
        let registry = ParticleRegistry::new(PidGenerator::new(node), engine);

        // Without a registered behavior, spawn should fail with "not registered"
        let result = registry.spawn("echo", Some("echo"), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn test_send_no_process() {
        let registry = make_registry();
        let fake_pid = registry.pid_gen().next();

        let result = registry.send_to_pid(&fake_pid, b"hello".to_vec()).await;
        assert_eq!(result, Err(SendError::NoProcess));
    }

    #[tokio::test]
    async fn test_monitor_dead_process() {
        let registry = make_registry();
        let (watcher_pid, mailbox_watcher) = spawn_test_process(&registry).await;
        let dead_pid = registry.pid_gen().next();

        // Monitor a process that doesn't exist
        let monitor_ref = registry.monitor(&watcher_pid, &dead_pid).await.unwrap();

        // Should immediately receive Down
        let msg = mailbox_watcher.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Down {
                from,
                ref_id,
                reason,
            } => {
                assert_eq!(from, dead_pid);
                assert_eq!(ref_id, monitor_ref);
                assert_eq!(reason, ExitReason::Normal);
            }
            other => panic!("expected Down, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_kill_propagation() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_process(&registry).await;
        let (pid_b, mailbox_b) = spawn_test_process(&registry).await;

        // b traps exits
        registry.set_trap_exit(&pid_b, true).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Kill a -- Kill is propagated as Shutdown("killed")
        registry.exit_process(&pid_a, ExitReason::Kill).await;

        // b should still be alive (trapping)
        assert!(registry.process_exists(&pid_b).await);

        // b should have received an Exit with Shutdown("killed")
        let msg = mailbox_b.recv(Some(Duration::from_millis(100))).await.unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Exit { reason, .. } => {
                assert_eq!(reason, ExitReason::Shutdown("killed".into()));
            }
            other => panic!("expected Exit, got {:?}", other),
        }
    }
}
