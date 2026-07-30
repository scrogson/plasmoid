use crate::mailbox::Mailbox;
use crate::message::{ExitReason, SystemMessage};
use crate::pid::{Pid, PidGenerator};
use crate::policy::PolicySet;
use crate::runtime::LoadedComponent;
use anyhow::{Result, anyhow};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use tokio::sync::{RwLock, broadcast};
use wasmtime::Engine;
use wasmtime::component::Component;

/// A compiled component (WASM component) that can be spawned as particles.
pub struct ComponentTemplate {
    pub component: Component,
    pub default_capabilities: PolicySet,
}

/// A running particle instance.
pub struct ParticleEntry {
    pub pid: Pid,
    pub code: LoadedComponent,
    pub component_name: String,
    pub name: Option<String>,
}

/// Per-particle state: links, monitors, mailbox.
pub struct ParticleState {
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
    particle_states: RwLock<HashMap<Pid, ParticleState>>,
    next_ref: AtomicU64,
    deaths: broadcast::Sender<ParticleDeath>,
}

/// Broadcast when a particle dies, so observers can react after the fact.
///
/// The registered name rides along because `exit_particle` has already removed
/// it by the time a subscriber runs — there is no way to look it up afterwards.
#[derive(Debug, Clone, PartialEq)]
pub struct ParticleDeath {
    pub pid: Pid,
    pub name: Option<String>,
    pub reason: ExitReason,
    /// Particles on other nodes linked to the deceased. They cannot be told
    /// locally, so a forwarder sends them exit signals over the peer link.
    pub remote_links: Vec<Pid>,
    /// Watchers on other nodes, with the ref each is waiting on.
    pub remote_monitors: Vec<(Pid, u64)>,
}

/// How many deaths may queue for a slow subscriber before the oldest are dropped.
const DEATH_BROADCAST_CAPACITY: usize = 1024;

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
            particle_states: RwLock::new(HashMap::new()),
            next_ref: AtomicU64::new(1),
            deaths: broadcast::channel(DEATH_BROADCAST_CAPACITY).0,
        }
    }

    /// Subscribe to particle deaths.
    ///
    /// Every death is published exactly once, including deaths cascaded through
    /// links, because they all funnel through [`Self::exit_particle`].
    pub fn subscribe_deaths(&self) -> broadcast::Receiver<ParticleDeath> {
        self.deaths.subscribe()
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
        let code = LoadedComponent::from_component(template.component.clone(), caps);
        let pid = self.pid_gen.next();

        let entry = ParticleEntry {
            pid: pid.clone(),
            code,
            component_name: component.to_string(),
            name: name.map(|s| s.to_string()),
        };

        // Create unified mailbox
        let mailbox = Arc::new(Mailbox::new());

        let particle_state = ParticleState {
            links: HashSet::new(),
            monitors: HashMap::new(),
            monitored_by: Vec::new(),
            trap_exit: false,
            mailbox: mailbox.clone(),
        };

        // Insert into registries
        self.particles.write().await.insert(pid.clone(), entry);
        self.particle_states
            .write()
            .await
            .insert(pid.clone(), particle_state);

        // Atomically insert name (re-check under write lock to prevent TOCTOU race)
        if let Some(name) = name {
            let mut names = self.names.write().await;
            if names.contains_key(name) {
                // Another spawn raced us — roll back
                self.particle_states.write().await.remove(&pid);
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

    /// Register a mailbox under a fresh pid, with no component behind it.
    ///
    /// For tests that exercise delivery rather than execution — a real spawn
    /// needs a compiled WASM component, which says nothing about routing.
    #[doc(hidden)]
    pub async fn insert_test_particle(&self, mailbox: Arc<Mailbox>) -> Pid {
        let pid = self.pid_gen.next();
        self.particle_states.write().await.insert(
            pid.clone(),
            ParticleState {
                links: HashSet::new(),
                monitors: HashMap::new(),
                monitored_by: Vec::new(),
                trap_exit: false,
                mailbox,
            },
        );
        pid
    }

    /// Record our half of a link to a particle on another node.
    ///
    /// One-sided by necessity: the peer records its own half when the link
    /// control message arrives, because neither node can write the other's state.
    pub async fn link_remote(&self, local: &Pid, remote: Pid) {
        if let Some(state) = self.particle_states.write().await.get_mut(local) {
            state.links.insert(remote);
        }
    }

    pub async fn unlink_remote(&self, local: &Pid, remote: &Pid) {
        if let Some(state) = self.particle_states.write().await.get_mut(local) {
            state.links.remove(remote);
        }
    }

    /// Record that a watcher on another node is monitoring a local particle.
    pub async fn monitor_remote(&self, watcher: &Pid, target: Pid, ref_id: u64) {
        let mut states = self.particle_states.write().await;
        if let Some(state) = states.get_mut(watcher) {
            state.monitors.insert(ref_id, target.clone());
        }
        if let Some(state) = states.get_mut(&target) {
            state.monitored_by.push((watcher.clone(), ref_id));
        }
    }

    /// Monitor with a caller-supplied ref, so the ref exists before the target
    /// is known to and can be reported in an immediate `noproc`.
    pub async fn monitor_with_ref(&self, watcher: &Pid, target: &Pid, ref_id: u64) {
        let mut states = self.particle_states.write().await;
        if let Some(state) = states.get_mut(watcher) {
            state.monitors.insert(ref_id, target.clone());
        }
        if let Some(state) = states.get_mut(target) {
            state.monitored_by.push((watcher.clone(), ref_id));
        }
    }

    /// Deliver an exit signal straight to a local particle's mailbox.
    pub async fn deliver_exit(&self, to: &Pid, from: &Pid, reason: ExitReason) {
        let _ = self
            .send_system(
                to,
                SystemMessage::Exit {
                    from: from.clone(),
                    reason,
                },
            )
            .await;
    }

    /// Apply an exit signal *inherited* through a link, when the particle that
    /// died was on another node.
    ///
    /// Delivering it as a message unconditionally — which is what
    /// [`Self::deliver_exit`] does — is wrong for a non-trapping particle: it
    /// would survive a death that would have killed it had the peer been local.
    /// That makes a remote death distinguishable from a local one, which #21
    /// promised it would not be, and leaves supervision broken across nodes.
    ///
    /// Every reason is trappable here, `kill` included. A particle that calls
    /// `exit(kill)` on itself propagates `kill` to its links, and Erlang lets
    /// them trap it — untrappability belongs to the *sending*, not the reason.
    /// See [`Self::apply_directed_exit`].
    pub async fn apply_inherited_exit(&self, to: &Pid, from: &Pid, reason: ExitReason) {
        let trapping = match self.particle_states.read().await.get(to) {
            Some(state) => state.trap_exit,
            None => return, // already gone
        };

        if trapping {
            self.deliver_exit(to, from, reason).await;
        } else if reason.is_abnormal() {
            // Cascade, exactly as a local abnormal exit does.
            Box::pin(self.exit_particle(to, reason)).await;
        }
        // Normal and not trapping: no action, as locally.
    }

    /// Apply an exit signal sent *directly* at a particle, by `exit-signal`.
    ///
    /// Identical to [`Self::apply_inherited_exit`] but for `kill`, which is
    /// untrappable: the target dies with [`ExitReason::Killed`] whether or not
    /// it traps exits, and its links inherit `killed` — an ordinary reason they
    /// may trap. Erlang's `exit_signal/2` does exactly this, changing `kill` to
    /// `killed` "to hint to linked processes that the killed process got killed
    /// by a call to `exit(Dest, kill)`".
    ///
    /// The guarantee matters: #28 resolves a name conflict by killing the loser,
    /// which is only a resolution if the loser cannot decline. Nothing can make
    /// itself unkillable, and nothing else gains that power — every other reason
    /// still goes through `trap-exit`.
    pub async fn apply_directed_exit(&self, to: &Pid, from: &Pid, reason: ExitReason) {
        if !matches!(reason, ExitReason::Kill) {
            self.apply_inherited_exit(to, from, reason).await;
            return;
        }
        if !self.particle_states.read().await.contains_key(to) {
            return; // already gone
        }
        Box::pin(self.exit_particle(to, ExitReason::Killed)).await;
    }

    /// Deliver a down signal straight to a local particle's mailbox.
    pub async fn deliver_down(&self, to: &Pid, from: &Pid, ref_id: u64, reason: ExitReason) {
        let _ = self
            .send_system(
                to,
                SystemMessage::Down {
                    from: from.clone(),
                    monitor_ref: ref_id,
                    reason,
                },
            )
            .await;
    }

    /// Every local particle holding a relationship with a particle on `node`.
    ///
    /// Used when a node is lost: each such relationship must fire, individually,
    /// so a lost node is indistinguishable from separate deaths (#17).
    pub async fn relationships_with_node(
        &self,
        node: &iroh::EndpointId,
    ) -> (Vec<(Pid, Pid)>, Vec<(Pid, Pid, u64)>) {
        let states = self.particle_states.read().await;
        let mut links = Vec::new();
        let mut monitors = Vec::new();
        for (pid, state) in states.iter() {
            for linked in state.links.iter().filter(|p| p.is_local_to(node)) {
                links.push((pid.clone(), linked.clone()));
            }
            for (ref_id, target) in state.monitors.iter().filter(|(_, t)| t.is_local_to(node)) {
                monitors.push((pid.clone(), target.clone(), *ref_id));
            }
        }
        (links, monitors)
    }

    /// Drop every relationship with a node we can no longer reach.
    pub async fn forget_node(&self, node: &iroh::EndpointId) {
        let mut states = self.particle_states.write().await;
        for state in states.values_mut() {
            state.links.retain(|p| !p.is_local_to(node));
            state.monitors.retain(|_, t| !t.is_local_to(node));
            state.monitored_by.retain(|(w, _)| !w.is_local_to(node));
        }
    }

    /// Look up a particle by PID.
    pub async fn get_by_pid(&self, pid: &Pid) -> Option<ParticleRef> {
        let particles = self.particles.read().await;
        particles.get(pid).map(|entry| ParticleRef {
            pid: entry.pid.clone(),
            component: entry.code.component().clone(),
            capabilities: entry.code.capabilities().clone(),
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
        self.components.read().await.keys().cloned().collect()
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

        // Try matching PID display string against registered particles
        let states = self.particle_states.read().await;
        for pid in states.keys() {
            if pid.to_string() == target {
                return Some(pid.clone());
            }
        }

        None
    }

    /// Send a user message to a particle by PID.
    pub async fn send_to_pid(&self, pid: &Pid, msg: Vec<u8>) -> Result<(), SendError> {
        let states = self.particle_states.read().await;
        let state = states.get(pid).ok_or(SendError::NoParticle)?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_data(msg).await.map_err(|e| match e {
            crate::mailbox::SendError::NoParticle => SendError::NoParticle,
        })
    }

    /// Send a tagged message to a particle by PID.
    pub async fn send_tagged_to_pid(
        &self,
        pid: &Pid,
        ref_id: u64,
        msg: Vec<u8>,
    ) -> Result<(), SendError> {
        let states = self.particle_states.read().await;
        let state = states.get(pid).ok_or(SendError::NoParticle)?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_tagged(ref_id, msg).await.map_err(|e| match e {
            crate::mailbox::SendError::NoParticle => SendError::NoParticle,
        })
    }

    /// Send a system message to a particle.
    pub async fn send_system(&self, pid: &Pid, msg: SystemMessage) -> Result<()> {
        let states = self.particle_states.read().await;
        let state = states
            .get(pid)
            .ok_or_else(|| anyhow!("no particle for pid '{}'", pid))?;
        let mailbox = state.mailbox.clone();
        drop(states);
        mailbox.push_system(msg).await;
        Ok(())
    }

    /// Create a bidirectional link between two particles.
    pub async fn link(&self, pid_a: &Pid, pid_b: &Pid) -> Result<()> {
        let mut states = self.particle_states.write().await;

        // Both must exist
        if !states.contains_key(pid_a) {
            return Err(anyhow!("no particle for pid '{}'", pid_a));
        }
        if !states.contains_key(pid_b) {
            return Err(anyhow!("no particle for pid '{}'", pid_b));
        }

        states.get_mut(pid_a).unwrap().links.insert(pid_b.clone());
        states.get_mut(pid_b).unwrap().links.insert(pid_a.clone());

        Ok(())
    }

    /// Remove a bidirectional link between two particles.
    pub async fn unlink(&self, pid_a: &Pid, pid_b: &Pid) {
        let mut states = self.particle_states.write().await;
        if let Some(state) = states.get_mut(pid_a) {
            state.links.remove(pid_b);
        }
        if let Some(state) = states.get_mut(pid_b) {
            state.links.remove(pid_a);
        }
    }

    /// Monitor a target particle. Returns a monitor reference.
    pub async fn monitor(&self, watcher: &Pid, target: &Pid) -> Result<u64> {
        let monitor_ref = self.next_ref.fetch_add(1, Ordering::Relaxed);
        let mut states = self.particle_states.write().await;

        // If target doesn't exist, immediately deliver a Down signal
        if !states.contains_key(target) {
            if let Some(watcher_state) = states.get(watcher) {
                let mailbox = watcher_state.mailbox.clone();
                drop(states);
                mailbox
                    .push_system(SystemMessage::Down {
                        from: target.clone(),
                        monitor_ref,
                        reason: ExitReason::Normal,
                    })
                    .await;
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
            .ok_or_else(|| anyhow!("watcher particle not found"))?
            .monitors
            .insert(monitor_ref, target.clone());

        Ok(monitor_ref)
    }

    /// Remove a monitor.
    pub async fn demonitor(&self, watcher: &Pid, monitor_ref: u64) {
        let mut states = self.particle_states.write().await;

        // Remove from watcher's monitors map
        let target = if let Some(state) = states.get_mut(watcher) {
            state.monitors.remove(&monitor_ref)
        } else {
            None
        };

        // Remove from target's monitored_by list
        if let Some(target_pid) = target
            && let Some(state) = states.get_mut(&target_pid)
        {
            state
                .monitored_by
                .retain(|(w, r)| !(w == watcher && *r == monitor_ref));
        }
    }

    /// Set trap_exit flag on a particle.
    pub async fn set_trap_exit(&self, pid: &Pid, enabled: bool) {
        let mut states = self.particle_states.write().await;
        if let Some(state) = states.get_mut(pid) {
            state.trap_exit = enabled;
        }
    }

    /// Register a name for a particle.
    pub async fn register_name(&self, pid: &Pid, name: &str) -> Result<()> {
        let mut names = self.names.write().await;
        if names.contains_key(name) {
            return Err(anyhow!("name '{}' is already registered", name));
        }
        names.insert(name.to_string(), pid.clone());
        Ok(())
    }

    /// Unregister a name. Only the owning particle can unregister it.
    pub async fn unregister_name(&self, pid: &Pid, name: &str) -> Result<()> {
        let mut names = self.names.write().await;
        match names.get(name) {
            Some(registered_pid) if registered_pid == pid => {
                names.remove(name);
                Ok(())
            }
            Some(_) => Err(anyhow!(
                "name '{}' is registered to a different particle",
                name
            )),
            None => Err(anyhow!("name '{}' is not registered", name)),
        }
    }

    /// Look up a PID by name.
    pub async fn lookup_name(&self, name: &str) -> Option<Pid> {
        self.names.read().await.get(name).cloned()
    }

    /// Exit a particle with the given reason.
    ///
    /// This is the exit propagation algorithm:
    /// 1. Remove the particle state, particle entry, and name.
    /// 2. Remove self from all linked peers' link sets.
    /// 3. For each linked peer:
    ///    - If peer has trap_exit=true: deliver Exit system message.
    ///    - If peer has trap_exit=false and reason is abnormal: cascade kill.
    ///    - If Kill: propagated as Shutdown("killed"), and is untrappable on origin.
    ///    - If Normal: no action on non-trapping peers.
    /// 4. For each monitor watcher: deliver Down system message.
    /// 5. Log exit.
    pub async fn exit_particle(&self, pid: &Pid, reason: ExitReason) {
        // Step 1: Remove particle state
        let particle_state = {
            let mut states = self.particle_states.write().await;
            states.remove(pid)
        };

        let particle_state = match particle_state {
            Some(s) => s,
            None => return, // Already exited
        };

        // Close the mailbox to wake any blocked recv
        particle_state.mailbox.close().await;

        // Remove particle entry, keeping the name so it can ride along on the
        // death broadcast — observers cannot look it up once it is gone.
        let registered_name = {
            let entry = self.particles.write().await.remove(pid);
            let name = entry.and_then(|e| e.name);
            if let Some(ref name) = name {
                self.names.write().await.remove(name);
            }
            name
        };

        let links = particle_state.links;
        let monitored_by = particle_state.monitored_by;

        // Split the relationships that cannot be honoured locally. A particle on
        // another node cannot be reached through the registry, so the death is
        // published with them attached and a forwarder sends the signals.
        let me = self.pid_gen.node();
        let remote_links: Vec<Pid> = links
            .iter()
            .filter(|p| !p.is_local_to(&me))
            .cloned()
            .collect();
        let remote_monitors: Vec<(Pid, u64)> = monitored_by
            .iter()
            .filter(|(w, _)| !w.is_local_to(&me))
            .cloned()
            .collect();

        // Publish the death. Send fails only when nobody is listening, which is
        // the normal case for a node with no distributed registry attached.
        let _ = self.deaths.send(ParticleDeath {
            pid: pid.clone(),
            name: registered_name,
            reason: reason.clone(),
            remote_links,
            remote_monitors,
        });

        // The reason propagates to links unchanged, including `kill`.
        //
        // It used to be rewritten to `shutdown("killed")`, which reported a
        // graceful shutdown for a particle that had been killed -- exactly the
        // distinction a supervisor needs. Erlang draws it the other way round:
        // the *sender* of a directed kill changes `kill` to `killed`, and what
        // links inherit is whatever the dead particle actually died of.
        let propagated_reason = reason.clone();

        // Step 2 + 3: Process links
        // We need to collect the peers to cascade-kill outside the lock
        let mut cascade_kills: Vec<Pid> = Vec::new();

        // Collect mailboxes to deliver to outside the lock
        let mut exit_deliveries: Vec<Arc<Mailbox>> = Vec::new();
        let mut down_deliveries: Vec<(Arc<Mailbox>, u64)> = Vec::new();

        {
            let mut states = self.particle_states.write().await;

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
            mailbox
                .push_system(SystemMessage::Exit {
                    from: pid.clone(),
                    reason: propagated_reason.clone(),
                })
                .await;
        }

        // Deliver down signals outside the lock
        for (mailbox, monitor_ref) in down_deliveries {
            mailbox
                .push_system(SystemMessage::Down {
                    from: pid.clone(),
                    monitor_ref,
                    reason: propagated_reason.clone(),
                })
                .await;
        }

        // Step 3 (continued): Cascade kills outside the lock to avoid deadlock
        for linked_pid in cascade_kills {
            // Use Box::pin to allow recursive async calls
            Box::pin(self.exit_particle(&linked_pid, propagated_reason.clone())).await;
        }

        tracing::info!(pid = %pid, reason = ?reason, "Particle exited");
    }

    /// Check if a particle exists.
    pub async fn particle_exists(&self, pid: &Pid) -> bool {
        self.particle_states.read().await.contains_key(pid)
    }
}

/// Errors that can occur when sending a user message.
#[derive(Debug, Clone, PartialEq)]
pub enum SendError {
    NoParticle,
}

impl std::fmt::Display for SendError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SendError::NoParticle => write!(f, "no particle"),
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
        let key = SecretKey::generate();
        let node = key.public();
        let engine = make_engine();
        Arc::new(ParticleRegistry::new(PidGenerator::new(node), engine))
    }

    #[tokio::test]
    async fn test_exit_broadcasts_death() {
        let registry = make_registry();
        let mut deaths = registry.subscribe_deaths();
        let (pid, _mb) = spawn_test_particle(&registry).await;

        registry.exit_particle(&pid, ExitReason::Normal).await;

        let death = deaths.try_recv().expect("exit should broadcast a death");
        assert_eq!(death.pid, pid);
        assert_eq!(death.name, None);
    }

    #[tokio::test]
    async fn test_death_carries_registered_name() {
        let registry = make_registry();
        let mut deaths = registry.subscribe_deaths();
        let (pid, _mb) = spawn_test_particle(&registry).await;
        registry
            .particles
            .write()
            .await
            .insert(pid.clone(), test_entry(&pid, Some("counter")));
        registry
            .names
            .write()
            .await
            .insert("counter".to_string(), pid.clone());

        registry.exit_particle(&pid, ExitReason::Normal).await;

        // The name must ride along on the event: by the time a subscriber reacts,
        // exit_particle has already removed it from the registry, so it cannot be
        // looked up after the fact.
        let death = deaths.try_recv().expect("exit should broadcast a death");
        assert_eq!(death.name.as_deref(), Some("counter"));
    }

    #[tokio::test]
    async fn test_exiting_twice_broadcasts_once() {
        let registry = make_registry();
        let mut deaths = registry.subscribe_deaths();
        let (pid, _mb) = spawn_test_particle(&registry).await;

        registry.exit_particle(&pid, ExitReason::Normal).await;
        registry.exit_particle(&pid, ExitReason::Normal).await;

        assert!(deaths.try_recv().is_ok());
        assert!(
            deaths.try_recv().is_err(),
            "a second exit of the same pid must not broadcast again"
        );
    }

    #[tokio::test]
    async fn test_cascading_exit_broadcasts_each_death() {
        let registry = make_registry();
        let mut deaths = registry.subscribe_deaths();
        let (pid_a, _a) = spawn_test_particle(&registry).await;
        let (pid_b, _b) = spawn_test_particle(&registry).await;
        registry.link(&pid_a, &pid_b).await.unwrap();

        // b is not trapping exits, so an abnormal exit of a cascades into b.
        registry
            .exit_particle(&pid_a, ExitReason::Exception("crash".into()))
            .await;

        let mut dead: Vec<Pid> = Vec::new();
        while let Ok(d) = deaths.try_recv() {
            dead.push(d.pid);
        }
        assert!(dead.contains(&pid_a), "origin death should broadcast");
        assert!(
            dead.contains(&pid_b),
            "link-propagated death should broadcast too"
        );
    }

    #[tokio::test]
    async fn test_a_remote_exit_kills_a_non_trapping_particle() {
        // A remote death must do what a local one does. Delivering it as a
        // message instead would let a particle survive an abnormal exit that
        // would have killed it had the peer been local — making remote and
        // local distinguishable, which #21 says they must not be.
        let registry = make_registry();
        let (pid, _mb) = spawn_test_particle(&registry).await;
        let remote = Pid {
            node: SecretKey::generate().public(),
            seq: 1,
        };

        registry
            .apply_inherited_exit(&pid, &remote, ExitReason::Exception("boom".into()))
            .await;

        assert!(
            !registry.particle_exists(&pid).await,
            "a non-trapping particle must die on an abnormal remote exit"
        );
    }

    #[tokio::test]
    async fn test_a_trapping_particle_receives_a_remote_exit_instead() {
        let registry = make_registry();
        let (pid, mailbox) = spawn_test_particle(&registry).await;
        registry.set_trap_exit(&pid, true).await;
        let remote = Pid {
            node: SecretKey::generate().public(),
            seq: 1,
        };

        registry
            .apply_inherited_exit(&pid, &remote, ExitReason::Exception("boom".into()))
            .await;

        assert!(
            registry.particle_exists(&pid).await,
            "a trapping particle survives"
        );
        match mailbox.recv(Some(Duration::from_millis(100))).await {
            Some(crate::mailbox::MailboxMessage::Exit { from, .. }) => assert_eq!(from, remote),
            other => panic!("expected an exit message, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_a_normal_remote_exit_does_not_kill_a_non_trapping_particle() {
        let registry = make_registry();
        let (pid, _mb) = spawn_test_particle(&registry).await;
        let remote = Pid {
            node: SecretKey::generate().public(),
            seq: 1,
        };

        registry
            .apply_inherited_exit(&pid, &remote, ExitReason::Normal)
            .await;

        assert!(
            registry.particle_exists(&pid).await,
            "a normal exit is ignored by a non-trapping particle, as locally"
        );
    }

    /// Helper to build a ParticleEntry for tests that need a named particle.
    fn test_entry(pid: &Pid, name: Option<&str>) -> ParticleEntry {
        ParticleEntry {
            pid: pid.clone(),
            code: LoadedComponent::from_component(
                Component::from_binary(&make_engine(), &wat_noop()).unwrap(),
                PolicySet::all(),
            ),
            component_name: "test".to_string(),
            name: name.map(|s| s.to_string()),
        }
    }

    /// Smallest valid component binary, for entries that never get instantiated.
    fn wat_noop() -> Vec<u8> {
        wat::parse_str(r#"(component)"#).unwrap()
    }

    /// Helper to create a particle state directly for testing (no WASM needed).
    async fn spawn_test_particle(registry: &ParticleRegistry) -> (Pid, Arc<Mailbox>) {
        let pid = registry.pid_gen().next();

        let mailbox = Arc::new(Mailbox::new());

        let particle_state = ParticleState {
            links: HashSet::new(),
            monitors: HashMap::new(),
            monitored_by: Vec::new(),
            trap_exit: false,
            mailbox: mailbox.clone(),
        };

        registry
            .particle_states
            .write()
            .await
            .insert(pid.clone(), particle_state);

        (pid, mailbox)
    }

    #[tokio::test]
    async fn test_spawn_returns_mailbox() {
        let key = SecretKey::generate();
        let node = key.public();
        let engine = make_engine();
        let registry = ParticleRegistry::new(PidGenerator::new(node), engine);

        // We can't easily create a real WASM component in a unit test,
        // but we verify the particle_states bookkeeping via spawn_test_particle.
        let (pid, _mailbox) = spawn_test_particle(&registry).await;
        assert!(registry.particle_exists(&pid).await);
    }

    #[tokio::test]
    async fn test_send_and_receive() {
        let registry = make_registry();
        let (pid, mailbox) = spawn_test_particle(&registry).await;

        // Send a message
        registry.send_to_pid(&pid, b"hello".to_vec()).await.unwrap();

        // Receive it
        let msg = mailbox
            .recv(Some(Duration::from_millis(100)))
            .await
            .unwrap();
        match msg {
            crate::mailbox::MailboxMessage::Data(data) => assert_eq!(data, b"hello"),
            other => panic!("expected Data, got {:?}", other),
        }
    }

    #[tokio::test]
    async fn test_link_and_exit_propagation() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_particle(&registry).await;
        let (pid_b, _mailbox_b) = spawn_test_particle(&registry).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a abnormally
        registry
            .exit_particle(&pid_a, ExitReason::Exception("crash".into()))
            .await;

        // b should be killed (cascade) since it doesn't trap exits
        assert!(!registry.particle_exists(&pid_b).await);
    }

    #[tokio::test]
    async fn test_normal_exit_no_propagation() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_particle(&registry).await;
        let (pid_b, _mailbox_b) = spawn_test_particle(&registry).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a normally
        registry.exit_particle(&pid_a, ExitReason::Normal).await;

        // b should still be alive (normal exit doesn't kill non-trapping peers)
        assert!(registry.particle_exists(&pid_b).await);
    }

    #[tokio::test]
    async fn test_trap_exit() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_particle(&registry).await;
        let (pid_b, mailbox_b) = spawn_test_particle(&registry).await;

        // b traps exits
        registry.set_trap_exit(&pid_b, true).await;

        // Link a and b
        registry.link(&pid_a, &pid_b).await.unwrap();

        // Exit a abnormally
        registry
            .exit_particle(&pid_a, ExitReason::Exception("crash".into()))
            .await;

        // b should still be alive (trapping)
        assert!(registry.particle_exists(&pid_b).await);

        // b should have received an Exit system message
        let msg = mailbox_b
            .recv(Some(Duration::from_millis(100)))
            .await
            .unwrap();
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
        let (target_pid, _mailbox_target) = spawn_test_particle(&registry).await;
        let (watcher_pid, mailbox_watcher) = spawn_test_particle(&registry).await;

        // Watcher monitors target
        let monitor_ref = registry.monitor(&watcher_pid, &target_pid).await.unwrap();

        // Target exits
        registry
            .exit_particle(&target_pid, ExitReason::Shutdown("bye".into()))
            .await;

        // Watcher should have received a Down system message
        let msg = mailbox_watcher
            .recv(Some(Duration::from_millis(100)))
            .await
            .unwrap();
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
        let (pid, _mailbox) = spawn_test_particle(&registry).await;

        // Register
        registry.register_name(&pid, "test_proc").await.unwrap();
        assert_eq!(registry.lookup_name("test_proc").await, Some(pid.clone()));

        // Duplicate registration should fail
        let (pid2, _mailbox2) = spawn_test_particle(&registry).await;
        let result = registry.register_name(&pid2, "test_proc").await;
        assert!(result.is_err());

        // Unregister by wrong particle should fail
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
        let key = SecretKey::generate();
        let node = key.public();
        let engine = make_engine();
        let registry = ParticleRegistry::new(PidGenerator::new(node), engine);

        // Without a registered behavior, spawn should fail with "not registered"
        let result = registry.spawn("echo", Some("echo"), None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not registered"));
    }

    #[tokio::test]
    async fn test_send_no_particle() {
        let registry = make_registry();
        let fake_pid = registry.pid_gen().next();

        let result = registry.send_to_pid(&fake_pid, b"hello".to_vec()).await;
        assert_eq!(result, Err(SendError::NoParticle));
    }

    #[tokio::test]
    async fn test_monitor_dead_particle() {
        let registry = make_registry();
        let (watcher_pid, mailbox_watcher) = spawn_test_particle(&registry).await;
        let dead_pid = registry.pid_gen().next();

        // Monitor a particle that doesn't exist
        let monitor_ref = registry.monitor(&watcher_pid, &dead_pid).await.unwrap();

        // Should immediately receive Down
        let msg = mailbox_watcher
            .recv(Some(Duration::from_millis(100)))
            .await
            .unwrap();
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

    /// Read the reason off the next exit message, or fail.
    async fn next_exit_reason(mailbox: &Arc<Mailbox>) -> ExitReason {
        match mailbox.recv(Some(Duration::from_millis(100))).await {
            Some(crate::mailbox::MailboxMessage::Exit { reason, .. }) => reason,
            other => panic!("expected an Exit message, got {other:?}"),
        }
    }

    /// A `kill` *inherited* through a link is an ordinary, trappable reason.
    ///
    /// Erlang: a process that calls `exit(kill)` on itself "will terminate with
    /// exit reason kill and also emit exit signals with exit reason kill (not
    /// killed) to all linked processes. Such exit signals ... can be trapped."
    #[tokio::test]
    async fn test_an_inherited_kill_is_trappable() {
        let registry = make_registry();
        let (pid_a, _mailbox_a) = spawn_test_particle(&registry).await;
        let (pid_b, mailbox_b) = spawn_test_particle(&registry).await;

        registry.set_trap_exit(&pid_b, true).await;
        registry.link(&pid_a, &pid_b).await.unwrap();

        registry.exit_particle(&pid_a, ExitReason::Kill).await;

        assert!(
            registry.particle_exists(&pid_b).await,
            "trapping the signal is what keeps b alive"
        );
        assert_eq!(
            next_exit_reason(&mailbox_b).await,
            ExitReason::Kill,
            "the reason reaches links unchanged; it used to be rewritten to \
             shutdown(\"killed\"), reporting a graceful stop for a killed particle"
        );
    }

    /// A `kill` *sent* at a particle bypasses `trap_exit` entirely (#27).
    ///
    /// This is the guarantee #28 rests on: killing the loser of a name conflict
    /// only resolves the conflict if the loser cannot decline.
    #[tokio::test]
    async fn test_a_directed_kill_is_untrappable() {
        let registry = make_registry();
        let (killer, _) = spawn_test_particle(&registry).await;
        let (victim, _) = spawn_test_particle(&registry).await;

        registry.set_trap_exit(&victim, true).await;

        registry
            .apply_directed_exit(&victim, &killer, ExitReason::Kill)
            .await;

        assert!(
            !registry.particle_exists(&victim).await,
            "trapping exits must not survive a directed kill"
        );
    }

    /// The killed particle dies as `killed`, and that is what its links inherit.
    #[tokio::test]
    async fn test_a_directed_kill_propagates_killed() {
        let registry = make_registry();
        let (killer, _) = spawn_test_particle(&registry).await;
        let (victim, _) = spawn_test_particle(&registry).await;
        let (watcher, watcher_mailbox) = spawn_test_particle(&registry).await;

        registry.set_trap_exit(&watcher, true).await;
        registry.link(&victim, &watcher).await.unwrap();

        registry
            .apply_directed_exit(&victim, &killer, ExitReason::Kill)
            .await;

        assert_eq!(
            next_exit_reason(&watcher_mailbox).await,
            ExitReason::Killed,
            "kill becomes killed when sent, hinting to links how the particle died"
        );
        assert!(
            registry.particle_exists(&watcher).await,
            "an inherited killed is trappable like any other reason"
        );
    }

    /// Only `kill` is special. Everything else still respects `trap_exit`.
    #[tokio::test]
    async fn test_a_directed_shutdown_is_still_trappable() {
        let registry = make_registry();
        let (from, _) = spawn_test_particle(&registry).await;
        let (target, target_mailbox) = spawn_test_particle(&registry).await;

        registry.set_trap_exit(&target, true).await;

        registry
            .apply_directed_exit(&target, &from, ExitReason::Shutdown("stop".into()))
            .await;

        assert!(registry.particle_exists(&target).await);
        assert_eq!(
            next_exit_reason(&target_mailbox).await,
            ExitReason::Shutdown("stop".into())
        );
    }

    /// A directed `normal` signal is why `exit` and `exit-signal` stay separate.
    ///
    /// `exit(normal)` terminates you unconditionally; `exit-signal(self, normal)`
    /// goes through your own trap rules, so a trapping particle gets a message
    /// and keeps running. Erlang draws exactly this line between `exit/1` and
    /// `exit/2`, and collapsing the two would leave no way to say *stop now*.
    #[tokio::test]
    async fn test_a_directed_normal_does_not_kill_a_trapping_particle() {
        let registry = make_registry();
        let (target, target_mailbox) = spawn_test_particle(&registry).await;
        registry.set_trap_exit(&target, true).await;

        registry
            .apply_directed_exit(&target, &target, ExitReason::Normal)
            .await;

        assert!(
            registry.particle_exists(&target).await,
            "a trapping particle survives a normal signal aimed at itself"
        );
        assert_eq!(
            next_exit_reason(&target_mailbox).await,
            ExitReason::Normal,
            "and hears about it"
        );

        // Whereas exit() on the same particle is unconditional.
        registry.exit_particle(&target, ExitReason::Normal).await;
        assert!(!registry.particle_exists(&target).await);
    }

    /// Signalling something already dead is a no-op, and reports nothing.
    #[tokio::test]
    async fn test_killing_a_dead_particle_is_a_no_op() {
        let registry = make_registry();
        let (from, _) = spawn_test_particle(&registry).await;
        let (target, _) = spawn_test_particle(&registry).await;

        registry.exit_particle(&target, ExitReason::Normal).await;
        registry
            .apply_directed_exit(&target, &from, ExitReason::Kill)
            .await;

        assert!(!registry.particle_exists(&target).await);
    }
}
