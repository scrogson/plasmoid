//! WASM actor invocation module.
//!
//! This module handles instantiating WASM components and invoking their
//! exported functions with dynamic dispatch using wasm-wave typed values.

use crate::doc_registry::{DocRegistry, ResolvedProcess};
use crate::host::{log_message, HostState, LogLevel};
use crate::pid::Pid;
use crate::policy::PolicySet;
use crate::registry::ProcessRegistry;
use crate::runtime::PLASMOID_ALPN;
use crate::wire;
use anyhow::{anyhow, Result};
use iroh::Endpoint;
use std::sync::Arc;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker, Type, Val};
use wasmtime::Engine;

/// Invoke a function on a WASM actor component.
pub fn invoke_actor(
    engine: &Engine,
    component: &Component,
    capabilities: &PolicySet,
    actor_id: &str,
    pid: Option<Pid>,
    remote_node_id: Option<String>,
    function: &str,
    args: &[String],
    endpoint: Option<&Endpoint>,
    registry: Option<Arc<ProcessRegistry>>,
    doc_registry: Option<Arc<DocRegistry>>,
) -> Result<Vec<String>> {
    // Create host state for this invocation
    let mut state = HostState::new(actor_id.to_string(), capabilities.clone());
    state.set_actor_name(Some(actor_id.to_string()));
    state.set_pid(pid);
    state.set_remote_node_id(remote_node_id);
    state.set_endpoint(endpoint.cloned());
    state.set_engine(Some(engine.clone()));
    state.set_registry(registry);
    state.set_doc_registry(doc_registry);

    // Create a store for this invocation
    let mut store = wasmtime::Store::new(engine, state);

    // Create linker with host functions
    let mut linker = Linker::<HostState>::new(engine);
    add_host_functions(&mut linker, capabilities)?;

    // Instantiate the component
    let instance = linker.instantiate(&mut store, component)?;

    // Find the exported function.
    let func = if let Some(func) = instance.get_func(&mut store, function) {
        func
    } else {
        find_function_in_exports(engine, component, &instance, &mut store, function)?
    };

    tracing::debug!(function = %function, "Invoking actor function");

    // Get the function's type to determine parameter and result types
    let func_ty = func.ty(&store);
    let param_types: Vec<(String, Type)> = func_ty
        .params()
        .map(|(name, ty)| (name.to_string(), ty))
        .collect();
    let result_types: Vec<Type> = func_ty.results().collect();

    // Parse wasm-wave encoded arguments into typed Vals
    if args.len() != param_types.len() {
        return Err(anyhow!(
            "function '{}' expects {} arguments but got {}",
            function,
            param_types.len(),
            args.len()
        ));
    }

    let params: Vec<Val> = args
        .iter()
        .zip(param_types.iter())
        .map(|(wave_str, (_name, ty))| {
            wasm_wave::from_str::<Val>(ty, wave_str).map_err(|e| {
                anyhow!(
                    "failed to parse argument '{}' as {}: {}",
                    wave_str,
                    wasm_wave::wasm::DisplayType(ty),
                    e
                )
            })
        })
        .collect::<Result<Vec<_>>>()?;

    // Prepare result slots
    let mut results: Vec<Val> = result_types
        .iter()
        .map(|_| Val::Bool(false))
        .collect();

    // Call the function
    func.call(&mut store, &params, &mut results)?;

    // Post-return cleanup (required by component model)
    func.post_return(&mut store)?;

    // Encode results as wasm-wave strings
    let wave_results: Vec<String> = results
        .iter()
        .map(|val| {
            wasm_wave::to_string(val)
                .map_err(|e| anyhow!("failed to encode result: {}", e))
        })
        .collect::<Result<Vec<_>>>()?;

    Ok(wave_results)
}

/// Search for a function export by walking the component's export tree.
fn find_function_in_exports(
    engine: &Engine,
    component: &Component,
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<HostState>,
    function_name: &str,
) -> Result<wasmtime::component::Func> {
    let component_type = component.component_type();

    for (export_name, item) in component_type.exports(engine) {
        match item {
            ComponentItem::ComponentInstance(ci) => {
                for (func_name, func_item) in ci.exports(engine) {
                    if func_name == function_name {
                        if let ComponentItem::ComponentFunc(_) = func_item {
                            let iface_index = component
                                .get_export_index(None, export_name)
                                .ok_or_else(|| {
                                    anyhow!(
                                        "interface '{}' not found at runtime",
                                        export_name
                                    )
                                })?;

                            let func_index = component
                                .get_export_index(Some(&iface_index), function_name)
                                .ok_or_else(|| {
                                    anyhow!(
                                        "function '{}' not found in interface '{}'",
                                        function_name,
                                        export_name,
                                    )
                                })?;

                            let func = instance
                                .get_func(&mut *store, &func_index)
                                .ok_or_else(|| {
                                    anyhow!(
                                        "function '{}' in '{}' could not be resolved",
                                        function_name,
                                        export_name,
                                    )
                                })?;

                            return Ok(func);
                        }
                    }
                }
            }
            ComponentItem::ComponentFunc(_) => {
                if export_name == function_name {
                    if let Some(func) = instance.get_func(&mut *store, export_name) {
                        return Ok(func);
                    }
                }
            }
            _ => {}
        }
    }

    Err(anyhow!(
        "function '{}' not found in any exported interface",
        function_name,
    ))
}

/// Add host functions to the linker based on capabilities.
fn add_host_functions(linker: &mut Linker<HostState>, capabilities: &PolicySet) -> Result<()> {
    // Add WASI interfaces (required by wasm32-wasip1 components)
    wasmtime_wasi::p2::add_to_linker_sync(linker)?;

    // Add logging interface: plasmoid:runtime/logging@0.2.0
    {
        let mut logging = linker.instance("plasmoid:runtime/logging@0.2.0")?;

        if capabilities.allows("logging") {
            logging.func_wrap(
                "log",
                |caller: wasmtime::StoreContextMut<'_, HostState>,
                 (level, message): (LogLevel, String)| {
                    let state = caller.data();
                    log_message(state, level, &message);
                    Ok(())
                },
            )?;
        } else {
            logging.func_wrap(
                "log",
                |_caller: wasmtime::StoreContextMut<'_, HostState>,
                 (_level, _message): (LogLevel, String)| { Ok(()) },
            )?;
        }
    }

    // Add actor-context interface: plasmoid:runtime/actor-context@0.2.0
    {
        let mut context = linker.instance("plasmoid:runtime/actor-context@0.2.0")?;

        // self-pid: func() -> string
        context.func_wrap(
            "self-pid",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             _: ()|
             -> Result<(String,), _> {
                let id = match caller.data().pid() {
                    Some(pid) => pid.to_string(),
                    None => caller.data().actor_id().to_string(),
                };
                Ok((id,))
            },
        )?;

        // self-name: func() -> option<string>
        context.func_wrap(
            "self-name",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             _: ()|
             -> Result<(Option<String>,), _> {
                let name = caller.data().actor_name().map(|s| s.to_string());
                Ok((name,))
            },
        )?;

        // caller-pid: func() -> option<string>
        context.func_wrap(
            "caller-pid",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             _: ()|
             -> Result<(Option<String>,), _> {
                let id = caller.data().remote_node_id().cloned();
                Ok((id,))
            },
        )?;

        // spawn: func(component: string, name: option<string>) -> result<string, string>
        context.func_wrap(
            "spawn",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             (component, name): (String, Option<String>)|
             -> Result<(Result<String, String>,), _> {
                let registry = match caller.data().registry() {
                    Some(r) => r.clone(),
                    None => {
                        return Ok((Err("no registry available for spawn".to_string()),));
                    }
                };

                let rt = tokio::runtime::Handle::current();
                let result = rt.block_on(async {
                    registry.spawn(&component, name.as_deref(), None).await
                });

                match result {
                    Ok(pid) => Ok((Ok(pid.to_string()),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;

        // call: func(target: string, function: string, args: list<string>) -> result<list<string>, string>
        context.func_wrap(
            "call",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             (target, function, args): (String, String, Vec<String>)|
             -> Result<(Result<Vec<String>, String>,), _> {
                if !caller.data().capabilities().allows("actor:call") {
                    return Ok((Err("unauthorized: actor:call not permitted".to_string()),));
                }

                let engine = match caller.data().engine() {
                    Some(e) => e.clone(),
                    None => {
                        return Ok((Err("no engine available for actor-to-actor calls".to_string()),));
                    }
                };

                let registry = caller.data().registry().cloned();
                let doc_registry = caller.data().doc_registry().cloned();
                let endpoint = caller.data().endpoint().cloned();
                let caller_id = caller.data().actor_id().to_string();

                let result = dispatch_call(
                    &engine,
                    registry.as_ref(),
                    doc_registry.as_ref(),
                    endpoint.as_ref(),
                    &caller_id,
                    &target,
                    &function,
                    &args,
                );

                match result {
                    Ok(results) => Ok((Ok(results),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;

        // notify: func(target: string, function: string, args: list<string>) -> result<_, string>
        context.func_wrap(
            "notify",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             (target, function, args): (String, String, Vec<String>)|
             -> Result<(Result<(), String>,), _> {
                if !caller.data().capabilities().allows("actor:notify") {
                    return Ok((Err("unauthorized: actor:notify not permitted".to_string()),));
                }

                let engine = match caller.data().engine() {
                    Some(e) => e.clone(),
                    None => {
                        return Ok((Err("no engine available for actor-to-actor calls".to_string()),));
                    }
                };

                let registry = caller.data().registry().cloned();
                let doc_registry = caller.data().doc_registry().cloned();
                let endpoint = caller.data().endpoint().cloned();
                let caller_id = caller.data().actor_id().to_string();

                let result = dispatch_call(
                    &engine,
                    registry.as_ref(),
                    doc_registry.as_ref(),
                    endpoint.as_ref(),
                    &caller_id,
                    &target,
                    &function,
                    &args,
                );

                match result {
                    Ok(_) => Ok((Ok(()),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;
    }

    Ok(())
}

/// Dispatch a call: resolve the target locally or remotely.
///
/// 1. Check local registry by name
/// 2. If doc registry exists, check for remote processes
/// 3. Local -> direct WASM invocation
/// 4. Remote -> QUIC call to remote node
fn dispatch_call(
    engine: &Engine,
    registry: Option<&Arc<ProcessRegistry>>,
    doc_registry: Option<&Arc<DocRegistry>>,
    endpoint: Option<&Endpoint>,
    caller_id: &str,
    target: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let rt = tokio::runtime::Handle::current();

    // Try local resolution first
    if let Some(registry) = registry {
        if let Some(pid) = rt.block_on(registry.get_by_name(target)) {
            if let Some(process) = rt.block_on(registry.get_by_pid(&pid)) {
                return invoke_actor(
                    engine,
                    &process.component,
                    &process.capabilities,
                    &process.name.unwrap_or_else(|| process.pid.to_string()),
                    Some(process.pid),
                    Some(caller_id.to_string()),
                    function,
                    args,
                    endpoint,
                    None, // don't pass registry to avoid recursive borrow issues
                    None,
                );
            }
        }
    }

    // Try doc registry resolution
    if let Some(doc_registry) = doc_registry {
        if let Some(resolved) = rt.block_on(doc_registry.resolve_name(target)) {
            match resolved {
                ResolvedProcess::Local(pid) => {
                    // Shouldn't happen (we checked local first), but handle it
                    if let Some(registry) = registry {
                        if let Some(process) = rt.block_on(registry.get_by_pid(&pid)) {
                            return invoke_actor(
                                engine,
                                &process.component,
                                &process.capabilities,
                                &process.name.unwrap_or_else(|| process.pid.to_string()),
                                Some(process.pid),
                                Some(caller_id.to_string()),
                                function,
                                args,
                                endpoint,
                                None,
                                None,
                            );
                        }
                    }
                }
                ResolvedProcess::Remote(remote) => {
                    let endpoint = endpoint
                        .ok_or_else(|| anyhow!("no endpoint available for remote call"))?;
                    return remote_actor_call(
                        endpoint,
                        &remote,
                        target,
                        function,
                        args,
                    );
                }
            }
        }
    }

    Err(anyhow!("no process found with name '{}'", target))
}

/// Perform a remote actor call via QUIC.
fn remote_actor_call(
    endpoint: &Endpoint,
    remote: &crate::doc_registry::RemoteProcess,
    target: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let rt = tokio::runtime::Handle::current();

    rt.block_on(async {
        let conn = endpoint
            .connect(remote.addr.clone(), PLASMOID_ALPN)
            .await
            .map_err(|e| anyhow!("failed to connect to remote node: {}", e))?;

        let (mut send, mut recv) = conn
            .open_bi()
            .await
            .map_err(|e| anyhow!("failed to open stream: {}", e))?;

        let request = wire::Request {
            id: 0,
            target: wire::Target::Name(target.to_string()),
            function: function.to_string(),
            args: args.to_vec(),
        };

        let request_bytes = wire::serialize(&request)
            .map_err(|e| anyhow!("failed to serialize request: {}", e))?;

        send.write_all(&request_bytes).await?;
        send.finish()?;

        let response_bytes = recv.read_to_end(1024 * 1024).await?;
        let response: wire::Response = wire::deserialize(&response_bytes)
            .map_err(|e| anyhow!("failed to deserialize response: {}", e))?;

        response
            .result
            .map_err(|e| anyhow!("remote actor error: {}", e))
    })
}
