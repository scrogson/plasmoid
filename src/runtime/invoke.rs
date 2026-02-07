//! WASM actor invocation module.
//!
//! This module handles instantiating WASM components and invoking their
//! handler functions with proper host function linking.

use crate::host::{log_message, Database, HostState, LogLevel};
use crate::policy::PolicySet;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use wasmtime::component::{Component, Linker, Val};
use wasmtime::{Engine, Store};

/// Trait for types that can be invoked as actors.
pub trait ActorLike {
    /// Get the WASM component.
    fn component(&self) -> &Component;
    /// Get the actor's capabilities.
    fn capabilities(&self) -> &PolicySet;
}

// Implement for WasmActor
impl ActorLike for crate::runtime::WasmActor {
    fn component(&self) -> &Component {
        crate::runtime::WasmActor::component(self)
    }

    fn capabilities(&self) -> &PolicySet {
        crate::runtime::WasmActor::capabilities(self)
    }
}

/// Context for invoking a WASM actor.
pub struct InvokeContext {
    engine: Engine,
    database: Arc<Database>,
}

impl InvokeContext {
    /// Create a new invoke context.
    pub fn new(engine: Engine, database: Arc<Database>) -> Self {
        Self { engine, database }
    }

    /// Invoke a WASM actor with a request payload.
    pub fn invoke<A: ActorLike>(
        &self,
        actor: &A,
        actor_id: &str,
        remote_node_id: Option<String>,
        request: Vec<u8>,
    ) -> Result<Vec<u8>> {
        // Create host state for this invocation
        let mut state = HostState::new(actor_id.to_string(), actor.capabilities().clone());
        state.set_remote_node_id(remote_node_id);

        // Create a store for this invocation
        let mut store = Store::new(&self.engine, InvokeState {
            host_state: state,
            database: self.database.clone(),
        });

        // Create linker with host functions
        let mut linker = Linker::<InvokeState>::new(&self.engine);
        add_host_functions(&mut linker, actor.capabilities())?;

        // Instantiate the component
        let instance = linker.instantiate(&mut store, actor.component())?;

        // Get the handler export and call it
        // The function is exported as "plasmoid:actor/handler@0.1.0#handle"
        let func = instance
            .get_func(&mut store, "plasmoid:actor/handler@0.1.0#handle")
            .ok_or_else(|| anyhow!("handler function not found"))?;

        // Call the function with the request bytes
        // The function signature is: handle(request: list<u8>) -> result<list<u8>, string>
        let request_val = Val::List(
            request
                .into_iter()
                .map(|b| Val::U8(b))
                .collect(),
        );

        let mut results = vec![Val::Bool(false)]; // placeholder, will be replaced
        func.call(&mut store, &[request_val], &mut results)?;

        // Parse the result
        // The result is a result<list<u8>, string> which comes back as a variant
        parse_result(&results[0])
    }
}

/// Internal state for an invocation.
struct InvokeState {
    host_state: HostState,
    database: Arc<Database>,
}

/// Parse a result<list<u8>, string> value.
fn parse_result(val: &Val) -> Result<Vec<u8>> {
    match val {
        Val::Result(result) => {
            match result.as_ref() {
                Ok(Some(inner)) => {
                    // Success case - extract the list<u8>
                    if let Val::List(list) = &**inner {
                        let bytes: Vec<u8> = list
                            .iter()
                            .map(|v| match v {
                                Val::U8(b) => Ok(*b),
                                _ => Err(anyhow!("expected u8 in list")),
                            })
                            .collect::<Result<Vec<u8>>>()?;
                        Ok(bytes)
                    } else {
                        Err(anyhow!("expected list in result"))
                    }
                }
                Ok(None) => {
                    // Success with no payload (unit)
                    Ok(vec![])
                }
                Err(Some(inner)) => {
                    // Error case - extract the string
                    if let Val::String(s) = &**inner {
                        Err(anyhow!("actor error: {}", s))
                    } else {
                        Err(anyhow!("actor returned error"))
                    }
                }
                Err(None) => {
                    Err(anyhow!("actor returned error"))
                }
            }
        }
        _ => Err(anyhow!("expected result type, got {:?}", val)),
    }
}

fn add_host_functions(linker: &mut Linker<InvokeState>, capabilities: &PolicySet) -> Result<()> {
    // Add logging interface
    // Always available if logging capability is granted
    {
        let mut logging = linker.instance("plasmoid:actor/logging@0.1.0")?;

        if capabilities.allows("logging") {
            logging.func_wrap(
                "log",
                |caller: wasmtime::StoreContextMut<'_, InvokeState>, (level, message): (u32, String)| {
                    let state = &caller.data().host_state;
                    log_message(state, LogLevel::from(level), &message);
                    Ok(())
                },
            )?;
        } else {
            // Provide a no-op implementation
            logging.func_wrap(
                "log",
                |_caller: wasmtime::StoreContextMut<'_, InvokeState>, (_level, _message): (u32, String)| {
                    Ok(())
                },
            )?;
        }
    }

    // Add actor-context interface
    {
        let mut context = linker.instance("plasmoid:actor/actor-context@0.1.0")?;

        context.func_wrap(
            "self-id",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, _: ()| -> Result<(String,), _> {
                let id = caller.data().host_state.actor_id().to_string();
                Ok((id,))
            },
        )?;

        context.func_wrap(
            "remote-id",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, _: ()| -> Result<(Option<String>,), _> {
                let id = caller.data().host_state.remote_node_id().cloned();
                Ok((id,))
            },
        )?;
    }

    // Add database interface
    {
        let mut database = linker.instance("plasmoid:actor/database@0.1.0")?;

        // get: func(key: string) -> option<list<u8>>
        database.func_wrap(
            "get",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, (key,): (String,)| -> Result<(Option<Vec<u8>>,), _> {
                if !caller.data().host_state.capabilities().allows("db:read") {
                    return Ok((None,));
                }
                let scoped_key = format!("{}:{}", caller.data().host_state.actor_id(), key);
                let result = caller.data().database.get(&scoped_key);
                Ok((result,))
            },
        )?;

        // set: func(key: string, value: list<u8>) -> result<_, string>
        database.func_wrap(
            "set",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, (key, value): (String, Vec<u8>)| -> Result<(Result<(), String>,), _> {
                if !caller.data().host_state.capabilities().allows("db:write") {
                    return Ok((Err("db:write capability not granted".to_string()),));
                }
                let scoped_key = format!("{}:{}", caller.data().host_state.actor_id(), key);
                match caller.data().database.set(&scoped_key, value) {
                    Ok(()) => Ok((Ok(()),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;

        // delete: func(key: string) -> result<bool, string>
        database.func_wrap(
            "delete",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, (key,): (String,)| -> Result<(Result<bool, String>,), _> {
                if !caller.data().host_state.capabilities().allows("db:write") {
                    return Ok((Err("db:write capability not granted".to_string()),));
                }
                let scoped_key = format!("{}:{}", caller.data().host_state.actor_id(), key);
                match caller.data().database.delete(&scoped_key) {
                    Ok(deleted) => Ok((Ok(deleted),)),
                    Err(e) => Ok((Err(e.to_string()),)),
                }
            },
        )?;

        // list-keys: func(prefix: string) -> list<string>
        database.func_wrap(
            "list-keys",
            |caller: wasmtime::StoreContextMut<'_, InvokeState>, (prefix,): (String,)| -> Result<(Vec<String>,), _> {
                if !caller.data().host_state.capabilities().allows("db:read") {
                    return Ok((vec![],));
                }
                let scoped_prefix = format!("{}:{}", caller.data().host_state.actor_id(), prefix);
                let keys = caller.data().database.list_keys(&scoped_prefix);
                // Strip the actor prefix from returned keys
                let actor_prefix = format!("{}:", caller.data().host_state.actor_id());
                let stripped: Vec<String> = keys
                    .into_iter()
                    .map(|k| k.strip_prefix(&actor_prefix).unwrap_or(&k).to_string())
                    .collect();
                Ok((stripped,))
            },
        )?;
    }

    Ok(())
}

/// Invoke a WASM actor (convenience function).
pub fn invoke_actor<A: ActorLike>(
    engine: &Engine,
    database: &Arc<Database>,
    actor: &A,
    actor_id: &str,
    remote_node_id: Option<String>,
    request: Vec<u8>,
) -> Result<Vec<u8>> {
    let ctx = InvokeContext::new(engine.clone(), database.clone());
    ctx.invoke(actor, actor_id, remote_node_id, request)
}
