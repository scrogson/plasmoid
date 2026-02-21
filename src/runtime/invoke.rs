//! WASM component invocation module.
//!
//! This module handles instantiating WASM components, calling their `init`
//! export, and running the message loop that delivers user and system messages
//! to the `handle` export.

use crate::doc_registry::DocRegistry;
use crate::host::HostState;
use crate::message::{ExitReason, SystemMessage};
pub use crate::pid::Pid;
use crate::policy::PolicySet;
use crate::registry::{MailboxReceivers, ParticleRegistry, SendError};
use anyhow::{anyhow, Result};
use iroh::Endpoint;
use std::sync::Arc;
use wasmtime::component::Resource;
use wasmtime::Engine;

// Generate typed bindings from the WIT world "actor-process".
//
// This generates:
// - `ActorProcess` struct for instantiation and calling exports
// - `plasmoid::runtime::process::Host` trait for import functions
// - `plasmoid::runtime::process::HostPid` trait for pid resource methods
// - Type aliases matching WIT types (Message, ExitReason, etc.)
//
// With `trappable` imports, all Host trait methods return `wasmtime::Result<T>`
// instead of bare `T`, allowing host functions to trap on error.
wasmtime::component::bindgen!({
    path: "wit",
    world: "actor-process",
    imports: {
        default: async | trappable,
    },
    exports: {
        default: async,
    },
    with: {
        "plasmoid:runtime/process.pid": Pid,
    },
});

impl plasmoid::runtime::process::HostPid for HostState {
    async fn to_string(&mut self, self_: Resource<Pid>) -> wasmtime::Result<String> {
        let pid = self.resource_table().get(&self_)?;
        Ok(pid.to_string())
    }

    async fn drop(&mut self, rep: Resource<Pid>) -> wasmtime::Result<()> {
        self.resource_table_mut().delete(rep)?;
        Ok(())
    }
}

impl plasmoid::runtime::process::Host for HostState {
    async fn self_pid(&mut self) -> wasmtime::Result<Resource<Pid>> {
        let pid = self.pid().clone();
        let resource = self.resource_table_mut().push(pid)?;
        Ok(resource)
    }

    async fn self_name(&mut self) -> wasmtime::Result<Option<String>> {
        Ok(self.name().map(|s| s.to_string()))
    }

    async fn spawn(
        &mut self,
        component: String,
        name: Option<String>,
        init_msg: Vec<u8>,
    ) -> wasmtime::Result<Result<Resource<Pid>, plasmoid::runtime::process::SpawnError>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::process::SpawnError::InitFailed)),
        };
        let engine = match self.engine() {
            Some(e) => e.clone(),
            None => return Ok(Err(plasmoid::runtime::process::SpawnError::InitFailed)),
        };
        let endpoint = self.endpoint().cloned();
        let doc_registry = self.doc_registry().cloned();

        // Look up the component template
        let (comp, caps) = match registry.get_component(&component).await {
            Some((c, default_caps)) => (c, default_caps),
            None => return Ok(Err(plasmoid::runtime::process::SpawnError::ComponentNotFound)),
        };

        // Spawn the process in the registry
        let (pid, receivers) = match registry
            .spawn(&component, name.as_deref(), Some(caps.clone()))
            .await
        {
            Ok(result) => result,
            Err(_) => return Ok(Err(plasmoid::runtime::process::SpawnError::InitFailed)),
        };

        // Start the process (init + message loop)
        let pid_clone = pid.clone();
        let registry_clone = registry.clone();
        if let Err(e) = start_process(
            &engine,
            &comp,
            &caps,
            pid_clone,
            name,
            &init_msg,
            receivers,
            endpoint,
            registry_clone,
            doc_registry,
        )
        .await
        {
            tracing::error!(error = %e, "Failed to start spawned process");
            return Ok(Err(plasmoid::runtime::process::SpawnError::InitFailed));
        }

        let resource = self
            .resource_table_mut()
            .push(pid)
            .map_err(|_| anyhow!("resource table full"))?;
        Ok(Ok(resource))
    }

    async fn exit(&mut self, reason: plasmoid::runtime::process::ExitReason) -> wasmtime::Result<()> {
        let exit_reason = wit_exit_reason_to_internal(reason);
        let pid = self.pid().clone();
        if let Some(registry) = self.registry() {
            let registry = registry.clone();
            registry.exit_process(&pid, exit_reason).await;
        }
        Ok(())
    }

    async fn send(
        &mut self,
        target: Resource<Pid>,
        msg: Vec<u8>,
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::process::SendError>> {
        let pid = match self.resource_table().get(&target) {
            Ok(p) => p.clone(),
            Err(_) => return Ok(Err(plasmoid::runtime::process::SendError::NoProcess)),
        };

        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::process::SendError::NoProcess)),
        };

        let result = registry.send_to_pid(&pid, msg).await.map_err(|e| match e {
            SendError::NoProcess => plasmoid::runtime::process::SendError::NoProcess,
            SendError::MailboxFull => plasmoid::runtime::process::SendError::MailboxFull,
        });
        Ok(result)
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
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::process::RegistryError>> {
        let pid = self.pid().clone();
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::process::RegistryError::NotRegistered)),
        };
        let result = registry
            .register_name(&pid, &name)
            .await
            .map_err(|_| plasmoid::runtime::process::RegistryError::AlreadyRegistered);
        Ok(result)
    }

    async fn unregister(
        &mut self,
        name: String,
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::process::RegistryError>> {
        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::process::RegistryError::NotRegistered)),
        };
        let result = registry
            .unregister_name(&name)
            .await
            .map_err(|_| plasmoid::runtime::process::RegistryError::NotRegistered);
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
    ) -> wasmtime::Result<Result<(), plasmoid::runtime::process::LinkError>> {
        let target_pid = match self.resource_table().get(&target) {
            Ok(p) => p.clone(),
            Err(_) => return Ok(Err(plasmoid::runtime::process::LinkError::NoProcess)),
        };
        let my_pid = self.pid().clone();

        let registry = match self.registry() {
            Some(r) => r.clone(),
            None => return Ok(Err(plasmoid::runtime::process::LinkError::NoProcess)),
        };

        let result = registry
            .link(&my_pid, &target_pid)
            .await
            .map_err(|_| plasmoid::runtime::process::LinkError::NoProcess);
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

        Ok(registry
            .monitor(&my_pid, &target_pid)
            .await
            .unwrap_or(0))
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

    async fn log(&mut self, level: plasmoid::runtime::process::LogLevel, message: String) -> wasmtime::Result<()> {
        let pid = self.pid();
        let name_str = self.name().unwrap_or("?");
        match level {
            plasmoid::runtime::process::LogLevel::Trace => {
                tracing::trace!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::process::LogLevel::Debug => {
                tracing::debug!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::process::LogLevel::Info => {
                tracing::info!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::process::LogLevel::Warn => {
                tracing::warn!(pid = %pid, name = %name_str, "{}", message)
            }
            plasmoid::runtime::process::LogLevel::Error => {
                tracing::error!(pid = %pid, name = %name_str, "{}", message)
            }
        }
        Ok(())
    }
}

/// Convert WIT exit-reason to internal ExitReason.
fn wit_exit_reason_to_internal(reason: plasmoid::runtime::process::ExitReason) -> ExitReason {
    match reason {
        plasmoid::runtime::process::ExitReason::Normal => ExitReason::Normal,
        plasmoid::runtime::process::ExitReason::Kill => ExitReason::Kill,
        plasmoid::runtime::process::ExitReason::Shutdown(s) => ExitReason::Shutdown(s),
        plasmoid::runtime::process::ExitReason::Exception(s) => ExitReason::Exception(s),
    }
}

/// Convert internal ExitReason to WIT exit-reason.
fn internal_exit_reason_to_wit(reason: &ExitReason) -> plasmoid::runtime::process::ExitReason {
    match reason {
        ExitReason::Normal => plasmoid::runtime::process::ExitReason::Normal,
        ExitReason::Kill => plasmoid::runtime::process::ExitReason::Kill,
        ExitReason::Shutdown(s) => plasmoid::runtime::process::ExitReason::Shutdown(s.clone()),
        ExitReason::Exception(s) => plasmoid::runtime::process::ExitReason::Exception(s.clone()),
    }
}

/// Convert a SystemMessage to the WIT Message variant for calling handle.
fn system_message_to_wit(
    msg: SystemMessage,
    resource_table: &mut wasmtime::component::ResourceTable,
) -> Result<plasmoid::runtime::process::Message> {
    match msg {
        SystemMessage::Exit { from, reason } => {
            let sender_resource = resource_table.push(from)?;
            Ok(plasmoid::runtime::process::Message::Exit(
                plasmoid::runtime::process::ExitSignal {
                    sender: sender_resource,
                    reason: internal_exit_reason_to_wit(&reason),
                },
            ))
        }
        SystemMessage::Down {
            from,
            monitor_ref,
            reason,
        } => {
            let sender_resource = resource_table.push(from)?;
            Ok(plasmoid::runtime::process::Message::Down(
                plasmoid::runtime::process::DownSignal {
                    sender: sender_resource,
                    monitor_ref,
                    reason: internal_exit_reason_to_wit(&reason),
                },
            ))
        }
    }
}

/// Start a process: instantiate component, call init, run message loop.
pub async fn start_process(
    engine: &Engine,
    component: &wasmtime::component::Component,
    capabilities: &PolicySet,
    pid: Pid,
    name: Option<String>,
    init_msg: &[u8],
    receivers: MailboxReceivers,
    endpoint: Option<Endpoint>,
    registry: Arc<ParticleRegistry>,
    doc_registry: Option<Arc<DocRegistry>>,
) -> Result<()> {
    // Create host state
    let mut state = HostState::new(pid.clone(), name, capabilities.clone());
    state.set_endpoint(endpoint);
    state.set_engine(Some(engine.clone()));
    state.set_registry(Some(registry.clone()));
    state.set_doc_registry(doc_registry);

    // Create store and linker
    let mut store = wasmtime::Store::new(engine, state);
    let mut linker = wasmtime::component::Linker::<HostState>::new(engine);

    // Add WASI support
    wasmtime_wasi::p2::add_to_linker_async(&mut linker)?;

    // Add the process interface imports (generated by bindgen!)
    ActorProcess::add_to_linker::<HostState, wasmtime::component::HasSelf<HostState>>(&mut linker, |state: &mut HostState| state)?;

    // Instantiate the component
    let actor_process =
        ActorProcess::instantiate_async(&mut store, component, &linker).await?;

    // Call init
    let init_result = actor_process.call_init(&mut store, init_msg).await?;
    if let Err(ref err_bytes) = init_result {
        let err_msg = String::from_utf8_lossy(err_bytes).to_string();
        registry
            .exit_process(
                &pid,
                ExitReason::Exception(format!("init failed: {}", err_msg)),
            )
            .await;
        return Err(anyhow!("init failed: {}", err_msg));
    }

    // Spawn the message loop as a background task
    let pid_for_loop = pid.clone();
    let registry_for_loop = registry.clone();
    tokio::spawn(async move {
        message_loop(
            actor_process,
            store,
            receivers,
            pid_for_loop,
            registry_for_loop,
        )
        .await;
    });

    Ok(())
}

/// The message loop: receives user and system messages and calls handle.
async fn message_loop(
    actor_process: ActorProcess,
    mut store: wasmtime::Store<HostState>,
    receivers: MailboxReceivers,
    pid: Pid,
    registry: Arc<ParticleRegistry>,
) {
    let MailboxReceivers {
        mut user_rx,
        mut system_rx,
    } = receivers;

    loop {
        tokio::select! {
            biased;

            Some(sys_msg) = system_rx.recv() => {
                // Convert SystemMessage to WIT message variant
                let wit_msg = match system_message_to_wit(
                    sys_msg,
                    store.data_mut().resource_table_mut(),
                ) {
                    Ok(msg) => msg,
                    Err(e) => {
                        tracing::error!(pid = %pid, error = %e, "Failed to convert system message");
                        continue;
                    }
                };

                // Call handle export
                if let Err(e) = actor_process.call_handle(&mut store, &wit_msg).await {
                    tracing::error!(pid = %pid, error = %e, "handle trapped on system message");
                    registry
                        .exit_process(&pid, ExitReason::Exception(format!("handle trap: {}", e)))
                        .await;
                    return;
                }
            }

            Some(user_bytes) = user_rx.recv() => {
                // Wrap as Message::User(bytes)
                let wit_msg = plasmoid::runtime::process::Message::User(user_bytes);

                // Call handle export
                if let Err(e) = actor_process.call_handle(&mut store, &wit_msg).await {
                    tracing::error!(pid = %pid, error = %e, "handle trapped on user message");
                    registry
                        .exit_process(&pid, ExitReason::Exception(format!("handle trap: {}", e)))
                        .await;
                    return;
                }
            }

            else => break,
        }
    }

    // Loop exited (channels closed) -- process cleanup
    registry.exit_process(&pid, ExitReason::Normal).await;
}
