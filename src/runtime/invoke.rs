//! WASM actor invocation module.
//!
//! This module handles instantiating WASM components and invoking their
//! exported functions with dynamic dispatch using wasm-wave typed values.

use crate::host::{log_message, HostState, LogLevel};
use crate::policy::PolicySet;
use crate::wire::{deserialize, serialize, Request, Response};
use anyhow::{anyhow, Result};
use iroh::Endpoint;
use wasmtime::component::types::ComponentItem;
use wasmtime::component::{Component, Linker, Type, Val};
use wasmtime::Engine;

/// Invoke a function on a WASM actor component.
///
/// This performs dynamic dispatch: it searches the component's exports for a
/// function matching the given name, parses the wasm-wave encoded arguments
/// into typed `Val`s, calls the function, and returns wasm-wave encoded results.
///
/// # Arguments
///
/// * `engine` - The wasmtime engine
/// * `component` - The compiled WASM component
/// * `capabilities` - Cedar policy set for this actor
/// * `actor_id` - The actor's identifier (ALPN string)
/// * `remote_node_id` - The caller's node ID, if known
/// * `function` - The function name to call (matched against export suffixes)
/// * `args` - wasm-wave encoded argument strings
///
/// # Returns
///
/// A vector of wasm-wave encoded result strings.
pub fn invoke_actor(
    engine: &Engine,
    component: &Component,
    capabilities: &PolicySet,
    actor_id: &str,
    remote_node_id: Option<String>,
    function: &str,
    args: &[String],
    endpoint: Option<&Endpoint>,
) -> Result<Vec<String>> {
    // Create host state for this invocation
    let mut state = HostState::new(actor_id.to_string(), capabilities.clone());
    state.set_remote_node_id(remote_node_id);
    state.set_endpoint(endpoint.cloned());

    // Create a store for this invocation
    let mut store = wasmtime::Store::new(engine, state);

    // Create linker with host functions
    let mut linker = Linker::<HostState>::new(engine);
    add_host_functions(&mut linker, capabilities)?;

    // Instantiate the component
    let instance = linker.instantiate(&mut store, component)?;

    // Find the exported function.
    //
    // Component model exports are hierarchical:
    //   - Top-level can be functions directly, or interface instances
    //   - Interface instances contain functions
    //
    // We first try a direct lookup by name. If that fails, we walk
    // the component's export tree to find the function inside an
    // exported interface instance.
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
///
/// Component model exports are structured hierarchically:
///   top-level: interface instances like "namespace:package/interface@version"
///   nested: functions like "func-name" inside those instances
///
/// This function enumerates the component's top-level exports, and for each
/// interface instance, looks inside for the named function.
fn find_function_in_exports(
    engine: &Engine,
    component: &Component,
    instance: &wasmtime::component::Instance,
    store: &mut wasmtime::Store<HostState>,
    function_name: &str,
) -> Result<wasmtime::component::Func> {
    // Get the component's type-level exports to discover interface names
    let component_type = component.component_type();

    for (export_name, item) in component_type.exports(engine) {
        match item {
            ComponentItem::ComponentInstance(ci) => {
                // Check if this interface instance contains our function
                for (func_name, func_item) in ci.exports(engine) {
                    if func_name == function_name {
                        if let ComponentItem::ComponentFunc(_) = func_item {
                            // Found it! Now resolve through the runtime instance.
                            // First get the interface's export index, then the function's.
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
                // Top-level function export -- already checked by get_func above,
                // but try again with the exact export name in case of naming mismatch
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

    // Add logging interface: plasmoid:runtime/logging@0.1.0
    {
        let mut logging = linker.instance("plasmoid:runtime/logging@0.1.0")?;

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
            // Provide a no-op implementation
            logging.func_wrap(
                "log",
                |_caller: wasmtime::StoreContextMut<'_, HostState>,
                 (_level, _message): (LogLevel, String)| { Ok(()) },
            )?;
        }
    }

    // Add actor-context interface: plasmoid:runtime/actor-context@0.1.0
    {
        let mut context = linker.instance("plasmoid:runtime/actor-context@0.1.0")?;

        context.func_wrap(
            "self-id",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             _: ()|
             -> Result<(String,), _> {
                let id = caller.data().actor_id().to_string();
                Ok((id,))
            },
        )?;

        context.func_wrap(
            "remote-id",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             _: ()|
             -> Result<(Option<String>,), _> {
                let id = caller.data().remote_node_id().cloned();
                Ok((id,))
            },
        )?;

        // call: func(alpn: string, function: string, args: list<string>) -> result<list<string>, string>
        context.func_wrap(
            "call",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             (alpn, function, args): (String, String, Vec<String>)|
             -> Result<(Result<Vec<String>, String>,), _> {
                if !caller.data().capabilities().allows("actor:call") {
                    return Ok((Err("unauthorized: actor:call not permitted".to_string()),));
                }

                let endpoint = match caller.data().endpoint() {
                    Some(ep) => ep.clone(),
                    None => {
                        return Ok((Err(
                            "no endpoint available for actor-to-actor calls".to_string(),
                        ),));
                    }
                };

                let result = tokio::runtime::Handle::current().block_on(async {
                    actor_call(&endpoint, &alpn, &function, &args).await
                });

                match result {
                    Ok(results) => Ok((Ok(results),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;

        // notify: func(alpn: string, function: string, args: list<string>) -> result<_, string>
        context.func_wrap(
            "notify",
            |caller: wasmtime::StoreContextMut<'_, HostState>,
             (alpn, function, args): (String, String, Vec<String>)|
             -> Result<(Result<(), String>,), _> {
                if !caller.data().capabilities().allows("actor:notify") {
                    return Ok((Err("unauthorized: actor:notify not permitted".to_string()),));
                }

                let endpoint = match caller.data().endpoint() {
                    Some(ep) => ep.clone(),
                    None => {
                        return Ok((Err(
                            "no endpoint available for actor-to-actor calls".to_string(),
                        ),));
                    }
                };

                let result = tokio::runtime::Handle::current().block_on(async {
                    actor_call(&endpoint, &alpn, &function, &args).await
                });

                match result {
                    Ok(_) => Ok((Ok(()),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;
    }

    Ok(())
}

/// Perform an actor-to-actor call over QUIC.
///
/// Connects to the target actor (identified by ALPN) on the local endpoint,
/// sends a serialized Request, and reads back the Response.
async fn actor_call(
    endpoint: &Endpoint,
    alpn: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    // Connect to the target actor on the local node
    let addr = endpoint.addr();
    let conn = endpoint
        .connect(addr, alpn.as_bytes())
        .await
        .map_err(|e| anyhow!("failed to connect to actor '{}': {}", alpn, e))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow!("failed to open stream to actor '{}': {}", alpn, e))?;

    // Build and send the request
    let request = Request {
        id: 0,
        function: function.to_string(),
        args: args.to_vec(),
    };
    let request_bytes =
        serialize(&request).map_err(|e| anyhow!("failed to serialize request: {}", e))?;
    send.write_all(&request_bytes).await?;
    send.finish()?;

    // Read the response
    let response_bytes = recv.read_to_end(1024 * 1024).await?;
    let response: Response =
        deserialize(&response_bytes).map_err(|e| anyhow!("failed to deserialize response: {}", e))?;

    response
        .result
        .map_err(|e| anyhow!("actor '{}' returned error: {}", alpn, e))
}
