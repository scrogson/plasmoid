//! WASM component invocation module.
//!
//! This module handles instantiating WASM components, calling their `start`
//! export dynamically via `Func::call`, and providing `recv`/`recv-ref` host
//! functions that let the component own its control flow.

use crate::doc_registry::DocRegistry;
use crate::host::HostState;
use crate::mailbox::{Mailbox, MailboxMessage};
use crate::message::ExitReason;
pub use crate::pid::Pid;
use crate::policy::PolicySet;
use crate::registry::ParticleRegistry;
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
        let doc_registry = self.doc_registry().cloned();
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
                doc_registry,
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

    /// Deliver a message, routing by the target's home node.
    ///
    /// Fire-and-forget per #14: this never reports a delivery failure. A dead
    /// target, an unreachable node or a stale resource handle all drop the
    /// message silently, exactly as they do locally. Liveness is discovered
    /// with `monitor`, not with `send`.
    async fn send(&mut self, target: Resource<Pid>, msg: Vec<u8>) -> wasmtime::Result<()> {
        let Ok(pid) = self.resource_table().get(&target).cloned() else {
            tracing::debug!("send to a stale pid handle; dropped");
            return Ok(());
        };
        self.deliver(pid, None, msg).await;
        Ok(())
    }

    async fn send_ref(
        &mut self,
        target: Resource<Pid>,
        ref_id: u64,
        msg: Vec<u8>,
    ) -> wasmtime::Result<()> {
        let Ok(pid) = self.resource_table().get(&target).cloned() else {
            tracing::debug!("send-ref to a stale pid handle; dropped");
            return Ok(());
        };
        self.deliver(pid, Some(ref_id), msg).await;
        Ok(())
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

    async fn link(
        &mut self,
        target: Resource<Pid>,
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::host::LinkError>> {
        let target_pid = match self.resource_table().get(&target) {
            Ok(p) => p.clone(),
            Err(_) => return Ok(Err(plasmoid::runtime::host::LinkError::NoParticle)),
        };
        let my_pid = self.pid().clone();

        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::host::LinkError::NoParticle)),
        };

        let result = registry
            .link(&my_pid, &target_pid)
            .await
            .map_err(|_| plasmoid::runtime::host::LinkError::NoParticle);
        Ok(result)
    }

    async fn unlink(&mut self, target: Resource<Pid>) -> wasmtime::Result<()> {
        let target_pid = match self.resource_table().get(&target) {
            Ok(pid) => pid.clone(),
            Err(_) => return Ok(()),
        };
        let my_pid = self.pid().clone();

        if let Some(registry) = self.registry() {
            let registry = registry.clone();
            registry.unlink(&my_pid, &target_pid).await;
        }
        Ok(())
    }

    async fn monitor(&mut self, target: Resource<Pid>) -> wasmtime::Result<u64> {
        let target_pid = match self.resource_table().get(&target) {
            Ok(pid) => pid.clone(),
            Err(_) => return Ok(0),
        };
        let my_pid = self.pid().clone();

        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(0),
        };

        Ok(registry.monitor(&my_pid, &target_pid).await.unwrap_or(0))
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
        plasmoid::runtime::host::ExitReason::Shutdown(s) => ExitReason::Shutdown(s),
        plasmoid::runtime::host::ExitReason::Exception(s) => ExitReason::Exception(s),
    }
}

/// Convert internal ExitReason to WIT exit-reason.
fn internal_exit_reason_to_wit(reason: &ExitReason) -> plasmoid::runtime::host::ExitReason {
    match reason {
        ExitReason::Normal => plasmoid::runtime::host::ExitReason::Normal,
        ExitReason::Kill => plasmoid::runtime::host::ExitReason::Kill,
        ExitReason::Shutdown(s) => plasmoid::runtime::host::ExitReason::Shutdown(s.clone()),
        ExitReason::Exception(s) => plasmoid::runtime::host::ExitReason::Exception(s.clone()),
    }
}

/// Convert a MailboxMessage to the WIT Message variant.
fn mailbox_message_to_wit(
    msg: MailboxMessage,
    resource_table: &mut wasmtime::component::ResourceTable,
) -> Result<plasmoid::runtime::host::Message> {
    match msg {
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
                plasmoid::runtime::host::ExitSignal {
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
                plasmoid::runtime::host::DownSignal {
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
    pub doc_registry: Option<Arc<DocRegistry>>,
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
    state.set_doc_registry(ctx.doc_registry);
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

impl HostState {
    /// Route a message to its target, wherever that particle lives.
    ///
    /// A pid carries its home node, so this needs no registry lookup: local
    /// targets go straight to the mailbox, remote ones onto the ordered peer
    /// link. Failures are logged and swallowed — `send` is fire-and-forget, and
    /// a sending particle must not be able to tell the two cases apart.
    async fn deliver(&mut self, pid: Pid, ref_id: Option<u64>, msg: Vec<u8>) {
        let is_local = match self.endpoint() {
            Some(ep) => pid.is_local_to(&ep.id()),
            // No endpoint means a test harness with only a local registry.
            None => true,
        };

        if is_local {
            let Some(registry) = self.registry().cloned() else {
                tracing::debug!(pid = %pid, "no registry; message dropped");
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

        let Some(peers) = self.peers().cloned() else {
            tracing::debug!(pid = %pid, "no peer transport; remote message dropped");
            return;
        };
        let node = pid.node;
        let envelope = match ref_id {
            Some(r) => crate::transport::Envelope::Tagged {
                target: pid,
                ref_id: r,
                payload: msg,
            },
            None => crate::transport::Envelope::Data {
                target: pid,
                payload: msg,
            },
        };
        // Enqueues and returns; the peer's writer task owns the connection, so
        // a particle never waits on a handshake.
        if let Err(e) = peers.send(node, &envelope) {
            tracing::debug!(peer = %node.fmt_short(), error = %e, "remote message dropped");
        }
    }
}
