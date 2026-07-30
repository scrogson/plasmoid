//! WASM component invocation module.
//!
//! This module handles instantiating WASM components, calling their `start`
//! export dynamically via `Func::call`, and providing `recv`/`recv-ref` host
//! functions that let the component own its control flow.

use crate::host::HostState;
use crate::mailbox::{Mailbox, MailboxMessage, SpawnFailure};
use crate::message::ExitReason;
pub use crate::pid::Pid;
use crate::policy::PolicySet;
use crate::registry::ParticleRegistry;
use crate::transport::Addressee;
use anyhow::{Result, anyhow};
use iroh::Endpoint;
use std::sync::Arc;
use std::time::Duration;
use wasmtime::Engine;
use wasmtime::component::{Resource, Val, types::ComponentItem};

// Generate typed bindings from the WIT world "particle" (imports only).
//
// The particle world has no exports, so bindgen generates:
// - `plasmoid::runtime::host::Host` trait for import functions
// - `plasmoid::runtime::host::HostPid` trait for pid resource methods
// - `Particle::add_to_linker` to wire up imports
wasmtime::component::bindgen!({
    path: "wit",
    world: "particle",
    imports: {
        default: async | trappable,
    },
    with: {
        "plasmoid:runtime/host.pid": Pid,
    },
});

impl plasmoid::runtime::host::HostPid for HostState {
    async fn to_string(&mut self, self_: Resource<Pid>) -> wasmtime::Result<String> {
        let pid = self.resource_table().get(&self_)?;
        Ok(pid.to_string())
    }

    async fn drop(&mut self, rep: Resource<Pid>) -> wasmtime::Result<()> {
        self.resource_table_mut().delete(rep)?;
        Ok(())
    }
}

impl plasmoid::runtime::host::Host for HostState {
    async fn self_pid(&mut self) -> wasmtime::Result<Resource<Pid>> {
        let pid = self.pid().clone();
        let resource = self.resource_table_mut().push(pid)?;
        Ok(resource)
    }

    async fn self_name(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.name().map(|s| s.to_string()))
    }

    async fn make_ref(&mut self) -> wasmtime::Result<u64> {
        Ok(self.next_ref())
    }

    async fn spawn(
        &mut self,
        component: String,
        name: Option<String>,
        init_args: String,
    ) -> wasmtime::Result<Result<Resource<Pid>, plasmoid::runtime::host::SpawnError>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::host::SpawnError::InitFailed)),
        };
        let engine = match self.engine() {
            Some(e) => e.clone(),
            None => return Ok(Err(plasmoid::runtime::host::SpawnError::InitFailed)),
        };
        let endpoint = self.endpoint().cloned();
        let peers = self.peers().cloned();

        // Look up the component template
        let (comp, caps) = match registry.get_component(&component).await {
            Some((c, default_caps)) => (c, default_caps),
            None => {
                return Ok(Err(plasmoid::runtime::host::SpawnError::ComponentNotFound));
            }
        };

        // Spawn the particle in the registry
        let (pid, mailbox) = match registry
            .spawn(&component, name.as_deref(), Some(caps.clone()))
            .await
        {
            Ok(result) => result,
            Err(_) => return Ok(Err(plasmoid::runtime::host::SpawnError::InitFailed)),
        };

        // Start the particle
        let pid_clone = pid.clone();
        let registry_clone = registry.clone();
        if let Err(e) = start_particle(
            &engine,
            &comp,
            &caps,
            pid_clone,
            name,
            &init_args,
            ParticleContext {
                mailbox,
                registry: registry_clone,
                endpoint,
                peers,
            },
        )
        .await
        {
            tracing::error!(error = %e, "Failed to start spawned particle");
            return Ok(Err(plasmoid::runtime::host::SpawnError::InitFailed));
        }

        let resource = self
            .resource_table_mut()
            .push(pid)
            .map_err(|_| anyhow!("resource table full"))?;
        Ok(Ok(resource))
    }

    async fn exit(&mut self, reason: plasmoid::runtime::host::ExitReason) -> wasmtime::Result<()> {
        let exit_reason = wit_exit_reason_to_internal(reason);
        let pid = self.pid().clone();
        if let Some(registry) = self.registry() {
            let registry = registry.clone();
            registry.exit_particle(&pid, exit_reason).await;
        }
        Ok(())
    }

    /// Send an exit signal to another particle, wherever it lives (#27).
    ///
    /// Asynchronous and infallible, exactly like `send`: a dead target, an
    /// unreachable node or a stale handle all drop it silently, and the caller
    /// learns nothing either way. Erlang's `exit/2` likewise returns `true`
    /// unconditionally; a caller that needs to know monitors the target first.
    ///
    /// Addressed at yourself, this is *not* [`Self::exit`] — it goes through
    /// your own trap rules like anybody else's signal.
    async fn exit_signal(
        &mut self,
        dest: plasmoid::runtime::host::Destination,
        reason: plasmoid::runtime::host::ExitReason,
    ) -> wasmtime::Result<()> {
        let me = self.pid().clone();
        let reason = wit_exit_reason_to_internal(reason);

        match self.resolve_local(&dest) {
            LocalTarget::Here(t) => {
                let Some(registry) = self.registry().cloned() else {
                    return Ok(());
                };
                if let Some(target) = self.resolve_named(t).await {
                    registry.apply_directed_exit(&target, &me, reason).await;
                }
            }
            LocalTarget::Elsewhere(node, addressee) => {
                self.send_control(
                    node,
                    crate::transport::PeerMessage::ExitSignal {
                        from: me,
                        to: addressee,
                        reason,
                    },
                );
            }
            LocalTarget::Unknown => {}
        }
        Ok(())
    }

    /// Deliver a message, routing by the target's home node.
    ///
    /// Fire-and-forget per #14: this never reports a delivery failure. A dead
    /// target, an unreachable node or a stale resource handle all drop the
    /// message silently, exactly as they do locally. Liveness is discovered
    /// with `monitor`, not with `send`.
    async fn send(
        &mut self,
        dest: plasmoid::runtime::host::Destination,
        msg: Vec<u8>,
    ) -> wasmtime::Result<()> {
        self.deliver_to(dest, None, msg).await;
        Ok(())
    }

    async fn send_ref(
        &mut self,
        dest: plasmoid::runtime::host::Destination,
        ref_id: u64,
        msg: Vec<u8>,
    ) -> wasmtime::Result<()> {
        self.deliver_to(dest, Some(ref_id), msg).await;
        Ok(())
    }

    async fn self_node(&mut self) -> wasmtime::Result<String> {
        Ok(match self.endpoint() {
            Some(ep) => hex::encode(ep.id().as_bytes()),
            None => String::new(),
        })
    }

    /// The node a pid lives on. Full hex, never the truncated display form.
    async fn node_of(&mut self, p: Resource<Pid>) -> wasmtime::Result<String> {
        Ok(match self.resource_table().get(&p) {
            Ok(pid) => hex::encode(pid.node.as_bytes()),
            Err(_) => String::new(),
        })
    }

    /// Spawn on another node, waiting for it to allocate the pid and reply.
    async fn spawn_on(
        &mut self,
        node: String,
        component: String,
        name: Option<String>,
        init_args: String,
    ) -> wasmtime::Result<Result<Resource<Pid>, plasmoid::runtime::host::SpawnError>> {
        let outcome = self.do_remote_spawn(node, component, name, init_args).await;
        Ok(match outcome {
            Ok(pid) => Ok(self.resource_table_mut().push(pid)?),
            Err(e) => Err(spawn_failure_to_wit(e)),
        })
    }

    /// Spawn on another node without waiting; the outcome arrives as a message.
    async fn spawn_request(
        &mut self,
        node: String,
        component: String,
        name: Option<String>,
        init_args: String,
    ) -> wasmtime::Result<u64> {
        let ref_id = self.next_ref();
        let Some(mailbox) = self.mailbox().cloned() else {
            return Ok(ref_id);
        };
        let ctx = self.particle_context();

        // Spawned, not awaited: spawn-request returns before the target replies.
        tokio::spawn(async move {
            let outcome = remote_spawn(ctx, node, component, name, init_args).await;
            mailbox.push_spawn_reply(ref_id, outcome).await;
        });

        Ok(ref_id)
    }

    async fn recv(
        &mut self,
        timeout_ms: Option<u64>,
    ) -> wasmtime::Result<Option<plasmoid::runtime::host::Message>> {
        let mailbox = match self.mailbox() {
            Some(m) => m.clone(),
            None => return Ok(None),
        };

        let timeout = timeout_ms.map(Duration::from_millis);
        let msg = mailbox.recv(timeout).await;

        match msg {
            Some(mailbox_msg) => Ok(Some(mailbox_message_to_wit(
                mailbox_msg,
                self.resource_table_mut(),
            )?)),
            None => Ok(None),
        }
    }

    async fn recv_ref(
        &mut self,
        ref_id: u64,
        timeout_ms: Option<u64>,
    ) -> wasmtime::Result<Option<plasmoid::runtime::host::Message>> {
        let mailbox = match self.mailbox() {
            Some(m) => m.clone(),
            None => return Ok(None),
        };

        let timeout = timeout_ms.map(Duration::from_millis);
        let msg = mailbox.recv_ref(ref_id, timeout).await;

        match msg {
            Some(mailbox_msg) => Ok(Some(mailbox_message_to_wit(
                mailbox_msg,
                self.resource_table_mut(),
            )?)),
            None => Ok(None),
        }
    }

    async fn resolve(&mut self, pid_string: String) -> wasmtime::Result<Option<Resource<Pid>>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(None),
        };
        let pid = match registry.resolve_target(&pid_string).await {
            Some(p) => p,
            None => return Ok(None),
        };
        let resource = self.resource_table_mut().push(pid)?;
        Ok(Some(resource))
    }

    async fn register(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::host::RegistryError>> {
        let pid = self.pid().clone();
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => {
                return Ok(Err(plasmoid::runtime::host::RegistryError::NotRegistered));
            }
        };
        let result = registry
            .register_name(&pid, &name)
            .await
            .map_err(|_| plasmoid::runtime::host::RegistryError::AlreadyRegistered);
        Ok(result)
    }

    async fn unregister(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::host::RegistryError>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => {
                return Ok(Err(plasmoid::runtime::host::RegistryError::NotRegistered));
            }
        };
        let my_pid = self.pid().clone();
        let result = registry
            .unregister_name(&my_pid, &name)
            .await
            .map_err(|_| plasmoid::runtime::host::RegistryError::NotRegistered);
        Ok(result)
    }

    async fn lookup(&mut self, name: String) -> wasmtime::Result<Option<Resource<Pid>>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(None),
        };
        let pid = match registry.lookup_name(&name).await {
            Some(p) => p,
            None => return Ok(None),
        };
        let resource = self.resource_table_mut().push(pid)?;
        Ok(Some(resource))
    }

    /// Link to a destination, wherever it lives.
    ///
    /// Infallible per #21: a target that is already gone produces an exit
    /// signal carrying `noproc`, delivered through the same channel its later
    /// death would use. The caller therefore has one code path, not two.
    async fn link(&mut self, dest: plasmoid::runtime::host::Destination) -> wasmtime::Result<()> {
        let me = self.pid().clone();
        match self.resolve_local(&dest) {
            LocalTarget::Here(t) => {
                let Some(registry) = self.registry().cloned() else {
                    return Ok(());
                };
                match self.resolve_named(t).await {
                    Some(target) if registry.link(&me, &target).await.is_ok() => {}
                    // Nothing to link to: say so the way a death would.
                    _ => {
                        registry.deliver_exit(&me, &me, ExitReason::NoProc).await;
                    }
                }
            }
            LocalTarget::Elsewhere(node, addressee) => {
                // Record our half now; the peer records its own on arrival.
                if let Some(registry) = self.registry().cloned()
                    && let Addressee::Pid(ref target) = addressee
                {
                    registry.link_remote(&me, target.clone()).await;
                }
                self.send_control(
                    node,
                    crate::transport::PeerMessage::Link {
                        from: me,
                        to: addressee,
                    },
                );
            }
            LocalTarget::Unknown => {}
        }
        Ok(())
    }

    async fn unlink(&mut self, dest: plasmoid::runtime::host::Destination) -> wasmtime::Result<()> {
        let me = self.pid().clone();
        match self.resolve_local(&dest) {
            LocalTarget::Here(t) => {
                if let Some(registry) = self.registry().cloned()
                    && let Some(target) = self.resolve_named(t).await
                {
                    registry.unlink(&me, &target).await;
                }
            }
            LocalTarget::Elsewhere(node, addressee) => {
                if let Some(registry) = self.registry().cloned()
                    && let Addressee::Pid(ref target) = addressee
                {
                    registry.unlink_remote(&me, target).await;
                }
                self.send_control(
                    node,
                    crate::transport::PeerMessage::Unlink {
                        from: me,
                        to: addressee,
                    },
                );
            }
            LocalTarget::Unknown => {}
        }
        Ok(())
    }

    /// Monitor a destination. Always returns a valid ref (#21).
    async fn monitor(
        &mut self,
        dest: plasmoid::runtime::host::Destination,
    ) -> wasmtime::Result<u64> {
        let me = self.pid().clone();
        let ref_id = self.next_ref();

        match self.resolve_local(&dest) {
            LocalTarget::Here(t) => {
                let Some(registry) = self.registry().cloned() else {
                    return Ok(ref_id);
                };
                match self.resolve_named(t).await {
                    Some(target) => registry.monitor_with_ref(&me, &target, ref_id).await,
                    None => {
                        registry
                            .deliver_down(&me, &me, ref_id, ExitReason::NoProc)
                            .await
                    }
                }
            }
            LocalTarget::Elsewhere(node, addressee) => {
                if let Some(registry) = self.registry().cloned()
                    && let Addressee::Pid(ref target) = addressee
                {
                    registry.monitor_remote(&me, target.clone(), ref_id).await;
                }
                self.send_control(
                    node,
                    crate::transport::PeerMessage::Monitor {
                        watcher: me,
                        target: addressee,
                        ref_id,
                    },
                );
            }
            LocalTarget::Unknown => {
                if let Some(registry) = self.registry().cloned() {
                    registry
                        .deliver_down(&me, &me, ref_id, ExitReason::NoProc)
                        .await;
                }
            }
        }
        Ok(ref_id)
    }

    async fn demonitor(&mut self, monitor_ref: u64) -> wasmtime::Result<()> {
        let my_pid = self.pid().clone();
        if let Some(registry) = self.registry() {
            let registry = registry.clone();
            registry.demonitor(&my_pid, monitor_ref).await;
        }
        Ok(())
    }

    async fn trap_exit(&mut self, enabled: bool) -> wasmtime::Result<()> {
        let my_pid = self.pid().clone();
        if let Some(registry) = self.registry() {
            let registry = registry.clone();
            registry.set_trap_exit(&my_pid, enabled).await;
        }
        Ok(())
    }

    async fn log(
        &mut self,
        level: plasmoid::runtime::host::LogLevel,
        message: String,
    ) -> wasmtime::Result<()> {
        let pid = self.pid();
        let name_str = self.name().unwrap_or("?");
        match level {
            plasmoid::runtime::host::LogLevel::Trace => {
                tracing::trace!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::host::LogLevel::Debug => {
                tracing::debug!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::host::LogLevel::Info => {
                tracing::info!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::host::LogLevel::Warn => {
                tracing::warn!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::host::LogLevel::Error => {
                tracing::error!(pid = %pid, name = %name_str, "{}", message)
            }
        }
        Ok(())
    }
}

/// Convert WIT exit-reason to internal ExitReason.
fn wit_exit_reason_to_internal(reason: plasmoid::runtime::host::ExitReason) -> ExitReason {
    match reason {
        plasmoid::runtime::host::ExitReason::Normal => ExitReason::Normal,
        plasmoid::runtime::host::ExitReason::Kill => ExitReason::Kill,
        plasmoid::runtime::host::ExitReason::Killed => ExitReason::Killed,
        plasmoid::runtime::host::ExitReason::Shutdown(s) => ExitReason::Shutdown(s),
        plasmoid::runtime::host::ExitReason::Exception(s) => ExitReason::Exception(s),
        plasmoid::runtime::host::ExitReason::Noproc => ExitReason::NoProc,
        plasmoid::runtime::host::ExitReason::Noconnection => ExitReason::NoConnection,
    }
}

/// Convert internal ExitReason to WIT exit-reason.
fn internal_exit_reason_to_wit(reason: &ExitReason) -> plasmoid::runtime::host::ExitReason {
    match reason {
        ExitReason::Normal => plasmoid::runtime::host::ExitReason::Normal,
        ExitReason::Kill => plasmoid::runtime::host::ExitReason::Kill,
        ExitReason::Killed => plasmoid::runtime::host::ExitReason::Killed,
        ExitReason::Shutdown(s) => plasmoid::runtime::host::ExitReason::Shutdown(s.clone()),
        ExitReason::Exception(s) => plasmoid::runtime::host::ExitReason::Exception(s.clone()),
        ExitReason::NoProc => plasmoid::runtime::host::ExitReason::Noproc,
        ExitReason::NoConnection => plasmoid::runtime::host::ExitReason::Noconnection,
    }
}

/// Convert a MailboxMessage to the WIT Message variant.
fn mailbox_message_to_wit(
    msg: MailboxMessage,
    resource_table: &mut wasmtime::component::ResourceTable,
) -> Result<plasmoid::runtime::host::Message> {
    match msg {
        MailboxMessage::SpawnReply { ref_id, outcome } => {
            let outcome = match outcome {
                Ok(pid) => Ok(resource_table.push(pid)?),
                Err(e) => Err(spawn_failure_to_wit(e)),
            };
            Ok(plasmoid::runtime::host::Message::SpawnReply(
                plasmoid::runtime::host::SpawnReply {
                    ref_: ref_id,
                    outcome,
                },
            ))
        }
        MailboxMessage::Data(data) => Ok(plasmoid::runtime::host::Message::Data(data)),
        MailboxMessage::Tagged { ref_id, payload } => Ok(plasmoid::runtime::host::Message::Tagged(
            plasmoid::runtime::host::TaggedMessage {
                ref_: ref_id,
                payload,
            },
        )),
        MailboxMessage::Exit { from, reason } => {
            let sender_resource = resource_table.push(from)?;
            Ok(plasmoid::runtime::host::Message::Exit(
                plasmoid::runtime::host::ExitMessage {
                    sender: sender_resource,
                    reason: internal_exit_reason_to_wit(&reason),
                },
            ))
        }
        MailboxMessage::Down {
            from,
            ref_id,
            reason,
        } => {
            let sender_resource = resource_table.push(from)?;
            Ok(plasmoid::runtime::host::Message::Down(
                plasmoid::runtime::host::DownMessage {
                    sender: sender_resource,
                    ref_: ref_id,
                    reason: internal_exit_reason_to_wit(&reason),
                },
            ))
        }
    }
}

/// Parse wasm-wave init args against a component's start function parameter types.
fn parse_wave_args(
    init_args: &str,
    param_types: &[wasmtime::component::types::Type],
) -> Result<Vec<Val>> {
    if param_types.is_empty() {
        return Ok(vec![]);
    }

    if init_args.is_empty() && param_types.is_empty() {
        return Ok(vec![]);
    }

    // For single-param functions, parse the whole string as that type
    if param_types.len() == 1 {
        if init_args.is_empty() {
            return Err(anyhow!(
                "start function expects 1 argument but none provided"
            ));
        }
        // String params pass through raw — components handle their own parsing
        // (e.g., JSON via plasmoid_sdk::from_init_args)
        if matches!(param_types[0], wasmtime::component::types::Type::String) {
            return Ok(vec![Val::String(init_args.into())]);
        }
        let val = wasm_wave::from_str::<Val>(&param_types[0], init_args)
            .map_err(|e| anyhow!("failed to parse init args as wasm-wave: {}", e))?;
        return Ok(vec![val]);
    }

    // For multi-param, parse as a tuple
    // wasm-wave tuple format: (val1, val2, ...)
    // We need to split and parse each individually
    // For now, support comma-separated values
    let parts: Vec<&str> = init_args.splitn(param_types.len(), ',').collect();
    if parts.len() != param_types.len() {
        return Err(anyhow!(
            "start function expects {} arguments, got {}",
            param_types.len(),
            parts.len()
        ));
    }

    let mut vals = Vec::with_capacity(param_types.len());
    for (part, ty) in parts.iter().zip(param_types.iter()) {
        let val = wasm_wave::from_str::<Val>(ty, part.trim())
            .map_err(|e| anyhow!("failed to parse arg '{}': {}", part.trim(), e))?;
        vals.push(val);
    }

    Ok(vals)
}

/// The runtime context every particle needs, threaded into its host state.
///
/// These travelled as loose arguments until routing added a sixth; they are one
/// clump and are passed as one thing.
#[derive(Clone)]
pub struct ParticleContext {
    pub mailbox: Arc<Mailbox>,
    pub registry: Arc<ParticleRegistry>,
    pub endpoint: Option<Endpoint>,
    pub peers: Option<Arc<crate::transport::PeerLinks>>,
}

impl std::fmt::Debug for ParticleContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ParticleContext").finish_non_exhaustive()
    }
}

/// Start a particle: instantiate component, find `start` export, call it.
pub async fn start_particle(
    engine: &Engine,
    component: &wasmtime::component::Component,
    capabilities: &PolicySet,
    pid: Pid,
    name: Option<String>,
    init_args: &str,
    ctx: ParticleContext,
) -> Result<()> {
    // Create host state
    let mut state = HostState::new(pid.clone(), name, capabilities.clone());
    state.set_endpoint(ctx.endpoint);
    state.set_engine(Some(engine.clone()));
    state.set_registry(Some(ctx.registry.clone()));
    state.set_peers(ctx.peers);
    state.set_mailbox(Some(ctx.mailbox));

    // Create store and linker
    let mut store = wasmtime::Store::new(engine, state);
    let mut linker = wasmtime::component::Linker::<HostState>::new(engine);

    // Add WASI support
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Add the host interface imports (generated by bindgen!)
    Particle::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(
        &mut linker,
        |state: &mut HostState| state,
    )?;

    // Instantiate the component
    let instance = linker.instantiate_async(&mut store, component).await?;

    // Find the `start` export function
    // Components may nest exports in different ways, so we search for it
    let start_func = find_start_export(&instance, &mut store, engine, component)?;

    // Parse init_args against the start function's parameter types
    let func_ty = start_func.ty(&store);
    let param_types: Vec<_> = func_ty.params().map(|(_, ty)| ty).collect();
    let args = if param_types.is_empty() && init_args.is_empty() {
        vec![]
    } else {
        parse_wave_args(init_args, &param_types)?
    };

    // Determine result count for the call
    let result_count = func_ty.results().count();
    let mut results = vec![Val::Bool(false); result_count];

    // Spawn the start function as a background task
    let pid_for_task = pid.clone();
    let registry_for_task = ctx.registry.clone();
    tokio::spawn(async move {
        tracing::debug!(pid = %pid_for_task, "Calling start function");

        match start_func.call_async(&mut store, &args, &mut results).await {
            Ok(()) => {
                // post_return is required by the component model
                if let Err(e) = start_func.post_return_async(&mut store).await {
                    tracing::error!(pid = %pid_for_task, error = %e, "post_return failed");
                }

                let exit_reason = interpret_start_result(&results);
                registry_for_task
                    .exit_particle(&pid_for_task, exit_reason)
                    .await;
            }
            Err(e) => {
                tracing::error!(pid = %pid_for_task, error = %e, "start function trapped");
                registry_for_task
                    .exit_particle(
                        &pid_for_task,
                        ExitReason::Exception(format!("start trap: {}", e)),
                    )
                    .await;
            }
        }
    });

    Ok(())
}

/// Find the `start` export in the component instance.
/// It may be a top-level function or nested in a component instance export.
fn find_start_export(
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<HostState>,
    engine: &Engine,
    component: &wasmtime::component::Component,
) -> Result<wasmtime::component::Func> {
    // Try direct top-level export first
    if let Some(func) = instance.get_func(&mut *store, "start") {
        return Ok(func);
    }

    // Walk the component type's exports looking for a function named "start"
    let component_type = component.component_type();
    for (name, item) in component_type.exports(engine) {
        match item {
            ComponentItem::ComponentInstance(inst_type) => {
                // Try to find "start" inside this nested instance
                for (func_name, _) in inst_type.exports(engine) {
                    if func_name == "start" {
                        // Access via the instance export path: instance[name]["start"]
                        if let Some(func) = instance.get_func(&mut *store, format!("{name}/start"))
                        {
                            return Ok(func);
                        }
                    }
                }
            }
            ComponentItem::CoreFunc(_) => {
                if name == "start"
                    && let Some(func) = instance.get_func(&mut *store, "start")
                {
                    return Ok(func);
                }
            }
            _ => {}
        }
    }

    Err(anyhow!("component does not export a 'start' function"))
}

/// Interpret the result of a start function call.
/// If the result is `result<_, string>` and is Err, return Exception.
/// Otherwise return Normal.
fn interpret_start_result(results: &[Val]) -> ExitReason {
    if results.is_empty() {
        return ExitReason::Normal;
    }

    match &results[0] {
        Val::Result(result_val) => match result_val {
            Ok(_) => ExitReason::Normal,
            Err(Some(val)) => {
                let err_msg = match val.as_ref() {
                    Val::String(s) => s.to_string(),
                    other => format!("{:?}", other),
                };
                ExitReason::Exception(format!("start failed: {}", err_msg))
            }
            Err(None) => ExitReason::Exception("start failed (no error details)".to_string()),
        },
        _ => ExitReason::Normal,
    }
}

/// Where a destination points, before any name has been looked up.
enum LocalTarget {
    /// On this node — a pid, or a name to resolve in the local registry.
    Here(Addressed),
    /// On another node; the name (if any) is resolved there, not here.
    Elsewhere(iroh::EndpointId, Addressee),
    /// A stale handle: nothing to address.
    Unknown,
}

/// A local destination, still possibly a name.
enum Addressed {
    Pid(Pid),
    Name(String),
}

impl HostState {
    /// Decide which node a destination belongs to, without resolving names.
    fn resolve_local(&mut self, dest: &plasmoid::runtime::host::Destination) -> LocalTarget {
        use self::plasmoid::runtime::host::Destination as D;
        let me = self.endpoint().map(|e| e.id());

        match dest {
            D::Pid(handle) => match self.resource_table().get(handle) {
                Ok(pid) => {
                    let pid = pid.clone();
                    match me {
                        // No endpoint means a test harness with only a registry.
                        None => LocalTarget::Here(Addressed::Pid(pid)),
                        Some(me) if pid.is_local_to(&me) => LocalTarget::Here(Addressed::Pid(pid)),
                        Some(_) => LocalTarget::Elsewhere(pid.node, Addressee::Pid(pid)),
                    }
                }
                Err(_) => LocalTarget::Unknown,
            },
            D::LocalNamed(name) => LocalTarget::Here(Addressed::Name(name.clone())),
            D::Named((node, name)) => match Self::parse_node(node) {
                Some(n) if Some(n) == me => LocalTarget::Here(Addressed::Name(name.clone())),
                Some(n) => LocalTarget::Elsewhere(n, Addressee::Name(name.clone())),
                None => LocalTarget::Unknown,
            },
        }
    }

    /// Resolve a local destination to a concrete pid.
    async fn resolve_named(&mut self, target: Addressed) -> Option<Pid> {
        match target {
            Addressed::Pid(pid) => Some(pid),
            Addressed::Name(name) => self.registry()?.get_by_name(&name).await,
        }
    }

    fn parse_node(node: &str) -> Option<iroh::EndpointId> {
        let bytes = hex::decode(node).ok()?;
        let bytes: [u8; 32] = bytes.try_into().ok()?;
        iroh::EndpointId::from_bytes(&bytes).ok()
    }

    /// Queue a control message to a peer. Never blocks, like `send`.
    fn send_control(&mut self, node: iroh::EndpointId, msg: crate::transport::PeerMessage) {
        let Some(peers) = self.peers().cloned() else {
            tracing::debug!("no peer transport; control message dropped");
            return;
        };
        if let Err(e) = peers.send(node, &msg) {
            tracing::debug!(peer = %node.fmt_short(), error = %e, "control message dropped");
        }
    }

    /// Route a message to wherever its destination lives.
    ///
    /// A pid carries its home node and a `named` destination names one, so this
    /// needs no registry lookup for the remote case — a remote name travels
    /// unresolved and is looked up on arrival. Failures are logged and
    /// swallowed: `send` is fire-and-forget, and a sending particle must not be
    /// able to tell the cases apart.
    async fn deliver_to(
        &mut self,
        dest: plasmoid::runtime::host::Destination,
        ref_id: Option<u64>,
        msg: Vec<u8>,
    ) {
        let (node, addressee) = match self.resolve_local(&dest) {
            LocalTarget::Unknown => {
                tracing::debug!("send to an unusable destination; dropped");
                return;
            }
            LocalTarget::Here(target) => {
                let Some(pid) = self.resolve_named(target).await else {
                    tracing::debug!("no local particle for destination; dropped");
                    return;
                };
                let Some(registry) = self.registry().cloned() else {
                    return;
                };
                let result = match ref_id {
                    Some(r) => registry.send_tagged_to_pid(&pid, r, msg).await,
                    None => registry.send_to_pid(&pid, msg).await,
                };
                if result.is_err() {
                    tracing::debug!(pid = %pid, "local target is gone; message dropped");
                }
                return;
            }
            LocalTarget::Elsewhere(node, addressee) => (node, addressee),
        };

        let Some(peers) = self.peers().cloned() else {
            tracing::debug!("no peer transport; remote message dropped");
            return;
        };
        let envelope = crate::transport::PeerMessage::Deliver(crate::transport::Envelope {
            target: addressee,
            ref_id,
            payload: msg,
        });
        // Enqueues and returns; the peer's writer task owns the connection, so
        // a particle never waits on a handshake.
        if let Err(e) = peers.send(node, &envelope) {
            tracing::debug!(peer = %node.fmt_short(), error = %e, "remote message dropped");
        }
    }
}

fn spawn_failure_to_wit(e: SpawnFailure) -> plasmoid::runtime::host::SpawnError {
    match e {
        SpawnFailure::ComponentNotFound => plasmoid::runtime::host::SpawnError::ComponentNotFound,
        SpawnFailure::InitFailed => plasmoid::runtime::host::SpawnError::InitFailed,
        SpawnFailure::ResourceLimit => plasmoid::runtime::host::SpawnError::ResourceLimit,
        SpawnFailure::NodeUnreachable => plasmoid::runtime::host::SpawnError::NodeUnreachable,
    }
}

impl HostState {
    async fn do_remote_spawn(
        &mut self,
        node: String,
        component: String,
        name: Option<String>,
        init_args: String,
    ) -> Result<Pid, SpawnFailure> {
        remote_spawn(self.particle_context(), node, component, name, init_args).await
    }

    /// Rebuild the context this particle was started with, for spawning.
    fn particle_context(&self) -> Option<crate::runtime::ParticleContext> {
        Some(crate::runtime::ParticleContext {
            mailbox: self.mailbox().cloned()?,
            registry: self.registry().cloned()?,
            endpoint: self.endpoint().cloned(),
            peers: self.peers().cloned(),
        })
    }
}

/// Ask another node to spawn a particle, and wait for the pid it allocates.
///
/// A remote pid must come from the target: `seq` is allocated by that node's
/// generator, so the caller cannot mint one. This is why `spawn-on` costs a
/// round trip while `send` does not — and why it is affordable, since spawn is
/// not a hot path.
pub(crate) async fn remote_spawn(
    ctx: Option<crate::runtime::ParticleContext>,
    node: String,
    component: String,
    name: Option<String>,
    init_args: String,
) -> Result<Pid, SpawnFailure> {
    let Some(ctx) = ctx else {
        return Err(SpawnFailure::NodeUnreachable);
    };
    let Some(endpoint) = ctx.endpoint.clone() else {
        return Err(SpawnFailure::NodeUnreachable);
    };
    let Some(node_id) = HostState::parse_node(&node) else {
        return Err(SpawnFailure::NodeUnreachable);
    };

    // Erlang's spawn/4 accepts your own node, and iroh refuses to connect to
    // itself — so the local case must be handled here rather than dialled.
    if node_id == endpoint.id() {
        return local_spawn(ctx, &component, name.as_deref(), &init_args).await;
    }

    let client = crate::client::NodeClient::new(endpoint, node_id);
    match client
        .try_spawn(&component, name.as_deref(), &init_args)
        .await
    {
        // The target answered, and refused.
        Ok(Err(e)) => Err(match e {
            crate::wire::SpawnFailureWire::ComponentNotFound => SpawnFailure::ComponentNotFound,
            crate::wire::SpawnFailureWire::InitFailed => SpawnFailure::InitFailed,
            crate::wire::SpawnFailureWire::ResourceLimit => SpawnFailure::ResourceLimit,
        }),
        Ok(Ok(result)) => Ok(result.pid),
        // We never reached the target at all.
        Err(e) => {
            tracing::debug!(peer = %node_id.fmt_short(), error = %e, "Remote spawn could not reach the node");
            Err(SpawnFailure::NodeUnreachable)
        }
    }
}

/// Spawn on this node, used when `spawn-on` names our own node.
async fn local_spawn(
    ctx: crate::runtime::ParticleContext,
    component: &str,
    name: Option<&str>,
    init_args: &str,
) -> Result<Pid, SpawnFailure> {
    let Some((comp, caps)) = ctx.registry.get_component(component).await else {
        return Err(SpawnFailure::ComponentNotFound);
    };
    let Ok((pid, mailbox)) = ctx
        .registry
        .spawn(component, name, Some(caps.clone()))
        .await
    else {
        return Err(SpawnFailure::ResourceLimit);
    };

    let engine = ctx.registry.engine().clone();
    let started = start_particle(
        &engine,
        &comp,
        &caps,
        pid.clone(),
        name.map(|s| s.to_string()),
        init_args,
        crate::runtime::ParticleContext { mailbox, ..ctx },
    )
    .await;

    match started {
        Ok(()) => Ok(pid),
        Err(_) => Err(SpawnFailure::InitFailed),
    }
}
