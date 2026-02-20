# Async WASM Invocation Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Switch from sync WASM invocation (one OS thread per blocked particle) to async fiber-based invocation (thousands of suspended futures on a handful of threads).

**Architecture:** Enable wasmtime's `async_support`, convert all `func_wrap` to `func_wrap_async`, convert `invoke_component` and `dispatch_call` to async, drop all `spawn_blocking` / `block_on` bridges. Components unchanged.

**Tech Stack:** wasmtime 41 (async feature), tokio, wasmtime-wasi (async linker)

---

### Task 1: Enable async support in Cargo.toml and engine config

This is the foundation. Code won't compile until tasks 2-4 are also complete.

**Files:**
- Modify: `Cargo.toml:20`
- Modify: `src/runtime/engine.rs:80-82`

**Step 1: Add the `async` feature to wasmtime in Cargo.toml**

Change line 20 from:
```toml
wasmtime = { version = "41", features = ["component-model", "wave"] }
```
to:
```toml
wasmtime = { version = "41", features = ["component-model", "wave", "async"] }
```

**Step 2: Enable async support in engine config**

In `src/runtime/engine.rs`, change lines 80-82 from:
```rust
let mut config = Config::new();
config.wasm_component_model(true);
let engine = Engine::new(&config)?;
```
to:
```rust
let mut config = Config::new();
config.wasm_component_model(true);
config.async_support(true);
let engine = Engine::new(&config)?;
```

**Step 3: Commit (won't compile yet — that's expected)**

```bash
git add Cargo.toml src/runtime/engine.rs
git commit -m "feat: enable wasmtime async support in config"
```

---

### Task 2: Convert invoke_component and add_host_functions to async

The big task. Convert the entire `src/runtime/invoke.rs` file. Every `func_wrap` becomes `func_wrap_async`, every `rt.block_on()` becomes `.await`, the WASI linker switches to async.

**Files:**
- Modify: `src/runtime/invoke.rs` (entire file)

**Step 1: Convert `invoke_component` signature and body to async**

Change the function signature from:
```rust
pub fn invoke_component(
    engine: &Engine,
    component: &Component,
    capabilities: &PolicySet,
    particle_id: &str,
    pid: Option<Pid>,
    remote_node_id: Option<String>,
    function: &str,
    args: &[String],
    endpoint: Option<&Endpoint>,
    registry: Option<Arc<ParticleRegistry>>,
    doc_registry: Option<Arc<DocRegistry>>,
) -> Result<Vec<String>> {
```
to:
```rust
pub async fn invoke_component(
    engine: &Engine,
    component: &Component,
    capabilities: &PolicySet,
    particle_id: &str,
    pid: Option<Pid>,
    remote_node_id: Option<String>,
    function: &str,
    args: &[String],
    endpoint: Option<Endpoint>,
    registry: Option<Arc<ParticleRegistry>>,
    doc_registry: Option<Arc<DocRegistry>>,
) -> Result<Vec<String>> {
```

Note: `endpoint` changes from `Option<&Endpoint>` to `Option<Endpoint>` because async functions can't hold references across await points. `Endpoint` is `Clone`, so callers will `.clone()`.

Inside the function body, change these three lines:

```rust
// Before
let instance = linker.instantiate(&mut store, component)?;
// ...
func.call(&mut store, &params, &mut results)?;
func.post_return(&mut store)?;

// After
let instance = linker.instantiate_async(&mut store, component).await?;
// ...
func.call_async(&mut store, &params, &mut results).await?;
func.post_return_async(&mut store).await?;
```

Also change the endpoint setup from:
```rust
state.set_endpoint(endpoint.cloned());
```
to:
```rust
state.set_endpoint(endpoint.clone());
```

**Step 2: Switch WASI linker to async**

In `add_host_functions`, change line 190 from:
```rust
wasmtime_wasi::p2::add_to_linker_sync(linker)?;
```
to:
```rust
wasmtime_wasi::p2::add_to_linker_async(linker)?;
```

**Step 3: Convert logging host functions to func_wrap_async**

Change the logging `func_wrap` calls (lines 197-212) to `func_wrap_async`. The pattern for each:

```rust
// Before
logging.func_wrap(
    "log",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (level, message): (LogLevel, String)| {
        let state = caller.data();
        log_message(state, level, &message);
        Ok(())
    },
)?;

// After
logging.func_wrap_async(
    "log",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (level, message): (LogLevel, String)| {
        Box::new(async move {
            let state = caller.data();
            log_message(state, level, &message);
            Ok(())
        })
    },
)?;
```

Apply the same pattern to the disabled logging variant (the `else` branch).

**Step 4: Convert self-pid, self-name, caller-pid to func_wrap_async**

These are trivial — no I/O, just wrap in `Box::new(async move { ... })`:

```rust
// Before
context.func_wrap(
    "self-pid",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()|
     -> Result<(String,), _> {
        let id = match caller.data().pid() {
            Some(pid) => pid.to_string(),
            None => caller.data().particle_id().to_string(),
        };
        Ok((id,))
    },
)?;

// After
context.func_wrap_async(
    "self-pid",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()| {
        Box::new(async move {
            let id = match caller.data().pid() {
                Some(pid) => pid.to_string(),
                None => caller.data().particle_id().to_string(),
            };
            Ok((id,))
        })
    },
)?;
```

Same pattern for `self-name` and `caller-pid`.

**Step 5: Convert `spawn` host function — drop `rt.block_on()`**

```rust
// Before
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

// After
context.func_wrap_async(
    "spawn",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (component, name): (String, Option<String>)| {
        Box::new(async move {
            let registry = match caller.data().registry() {
                Some(r) => r.clone(),
                None => {
                    return Ok((Err("no registry available for spawn".to_string()),));
                }
            };

            let result = registry.spawn(&component, name.as_deref(), None).await;

            match result {
                Ok(pid) => Ok((Ok(pid.to_string()),)),
                Err(e) => Ok((Err(e.to_string()),)),
            }
        })
    },
)?;
```

**Step 6: Convert `call` host function — `dispatch_call` becomes awaited**

```rust
// Before
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
        let caller_id = caller.data().particle_id().to_string();

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

// After
context.func_wrap_async(
    "call",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (target, function, args): (String, String, Vec<String>)| {
        Box::new(async move {
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
            let caller_id = caller.data().particle_id().to_string();

            let result = dispatch_call(
                &engine,
                registry.as_ref(),
                doc_registry.as_ref(),
                endpoint.as_ref(),
                &caller_id,
                &target,
                &function,
                &args,
            )
            .await;

            match result {
                Ok(results) => Ok((Ok(results),)),
                Err(e) => Ok((Err(e.to_string()),)),
            }
        })
    },
)?;
```

**Step 7: Convert `notify` host function — simplify to `tokio::spawn`**

This was the most convoluted: `rt.spawn(async { spawn_blocking(dispatch_call) })`. Now it's just `tokio::spawn(dispatch_call().await)`:

```rust
// Before
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
                return Ok((Err(
                    "no engine available for actor-to-actor calls".to_string(),
                ),));
            }
        };

        let registry = caller.data().registry().cloned();
        let doc_registry = caller.data().doc_registry().cloned();
        let endpoint = caller.data().endpoint().cloned();
        let caller_id = caller.data().particle_id().to_string();

        let rt = tokio::runtime::Handle::current();
        rt.spawn(async move {
            let result = tokio::task::spawn_blocking(move || {
                dispatch_call(
                    &engine,
                    registry.as_ref(),
                    doc_registry.as_ref(),
                    endpoint.as_ref(),
                    &caller_id,
                    &target,
                    &function,
                    &args,
                )
            })
            .await;

            match result {
                Ok(Ok(_)) => {}
                Ok(Err(e)) => {
                    tracing::warn!(error = %e, "notify dispatch failed");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "notify task panicked");
                }
            }
        });

        Ok((Ok(()),))
    },
)?;

// After
context.func_wrap_async(
    "notify",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (target, function, args): (String, String, Vec<String>)| {
        Box::new(async move {
            if !caller.data().capabilities().allows("actor:notify") {
                return Ok((Err("unauthorized: actor:notify not permitted".to_string()),));
            }

            let engine = match caller.data().engine() {
                Some(e) => e.clone(),
                None => {
                    return Ok((Err(
                        "no engine available for actor-to-actor calls".to_string(),
                    ),));
                }
            };

            let registry = caller.data().registry().cloned();
            let doc_registry = caller.data().doc_registry().cloned();
            let endpoint = caller.data().endpoint().cloned();
            let caller_id = caller.data().particle_id().to_string();

            tokio::spawn(async move {
                let result = dispatch_call(
                    &engine,
                    registry.as_ref(),
                    doc_registry.as_ref(),
                    endpoint.as_ref(),
                    &caller_id,
                    &target,
                    &function,
                    &args,
                )
                .await;

                match result {
                    Ok(_) => {}
                    Err(e) => {
                        tracing::warn!(error = %e, "notify dispatch failed");
                    }
                }
            });

            Ok((Ok(()),))
        })
    },
)?;
```

**Step 8: Convert `send` host function — drop `rt.block_on()`**

```rust
// Before
context.func_wrap(
    "send",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (target, message): (String, Vec<String>)|
     -> Result<(Result<(), String>,), _> {
        if !caller.data().capabilities().allows("actor:send") {
            return Ok((Err("unauthorized: actor:send not permitted".to_string()),));
        }

        let registry = match caller.data().registry() {
            Some(r) => r.clone(),
            None => {
                return Ok((Err("no registry available for send".to_string()),));
            }
        };

        let rt = tokio::runtime::Handle::current();
        let result = rt.block_on(registry.send_message(&target, message));

        match result {
            Ok(()) => Ok((Ok(()),)),
            Err(e) => Ok((Err(e.to_string()),)),
        }
    },
)?;

// After
context.func_wrap_async(
    "send",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (target, message): (String, Vec<String>)| {
        Box::new(async move {
            if !caller.data().capabilities().allows("actor:send") {
                return Ok((Err("unauthorized: actor:send not permitted".to_string()),));
            }

            let registry = match caller.data().registry() {
                Some(r) => r.clone(),
                None => {
                    return Ok((Err("no registry available for send".to_string()),));
                }
            };

            let result = registry.send_message(&target, message).await;

            match result {
                Ok(()) => Ok((Ok(()),)),
                Err(e) => Ok((Err(e.to_string()),)),
            }
        })
    },
)?;
```

**Step 9: Convert `receive` host function — drop `rt.block_on()`**

```rust
// Before
context.func_wrap(
    "receive",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()|
     -> Result<(Vec<String>,), _> {
        let pid = match caller.data().pid() {
            Some(pid) => pid.clone(),
            None => {
                return Ok((vec!["error: no mailbox (particle not spawned)".to_string()],));
            }
        };

        let registry = match caller.data().registry() {
            Some(r) => r.clone(),
            None => {
                return Ok((vec!["error: no registry available".to_string()],));
            }
        };

        let rt = tokio::runtime::Handle::current();
        match rt.block_on(registry.receive_message(&pid)) {
            Ok(msg) => Ok((msg,)),
            Err(e) => Ok((vec![format!("error: {}", e)],)),
        }
    },
)?;

// After
context.func_wrap_async(
    "receive",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()| {
        Box::new(async move {
            let pid = match caller.data().pid() {
                Some(pid) => pid.clone(),
                None => {
                    return Ok((vec!["error: no mailbox (particle not spawned)".to_string()],));
                }
            };

            let registry = match caller.data().registry() {
                Some(r) => r.clone(),
                None => {
                    return Ok((vec!["error: no registry available".to_string()],));
                }
            };

            match registry.receive_message(&pid).await {
                Ok(msg) => Ok((msg,)),
                Err(e) => Ok((vec![format!("error: {}", e)],)),
            }
        })
    },
)?;
```

**Step 10: Commit (won't compile yet — tasks 3-4 still needed)**

```bash
git add src/runtime/invoke.rs
git commit -m "feat: convert invoke_component and host functions to async"
```

---

### Task 3: Convert dispatch_call and remote_call to async

Still in `src/runtime/invoke.rs`. These functions currently use `rt.block_on()` to bridge sync→async. Now they're async natively.

**Files:**
- Modify: `src/runtime/invoke.rs:435-562`

**Step 1: Convert `dispatch_call` to async**

```rust
// Before
fn dispatch_call(
    engine: &Engine,
    registry: Option<&Arc<ParticleRegistry>>,
    doc_registry: Option<&Arc<DocRegistry>>,
    endpoint: Option<&Endpoint>,
    caller_id: &str,
    target: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let rt = tokio::runtime::Handle::current();

    if let Some(registry) = registry {
        if let Some(pid) = rt.block_on(registry.resolve_target(target)) {
            if let Some(particle) = rt.block_on(registry.get_by_pid(&pid)) {
                return invoke_component(
                    engine,
                    &particle.component,
                    &particle.capabilities,
                    &particle.name.unwrap_or_else(|| particle.pid.to_string()),
                    Some(particle.pid),
                    Some(caller_id.to_string()),
                    function,
                    args,
                    endpoint,
                    Some(registry.clone()),
                    doc_registry.map(|r| r.clone()),
                );
            }
        }
    }

    if let Some(doc_registry) = doc_registry {
        if let Some(resolved) = rt.block_on(doc_registry.resolve_name(target)) {
            match resolved {
                ResolvedParticle::Local(pid) => {
                    if let Some(registry) = registry {
                        if let Some(particle) = rt.block_on(registry.get_by_pid(&pid)) {
                            return invoke_component(
                                engine,
                                &particle.component,
                                &particle.capabilities,
                                &particle.name.unwrap_or_else(|| particle.pid.to_string()),
                                Some(particle.pid),
                                Some(caller_id.to_string()),
                                function,
                                args,
                                endpoint,
                                Some(registry.clone()),
                                Some(doc_registry.clone()),
                            );
                        }
                    }
                }
                ResolvedParticle::Remote(remote) => {
                    let endpoint = endpoint
                        .ok_or_else(|| anyhow!("no endpoint available for remote call"))?;
                    return remote_call(endpoint, &remote, target, function, args);
                }
            }
        }
    }

    Err(anyhow!("no particle found with name '{}'", target))
}

// After
async fn dispatch_call(
    engine: &Engine,
    registry: Option<&Arc<ParticleRegistry>>,
    doc_registry: Option<&Arc<DocRegistry>>,
    endpoint: Option<&Endpoint>,
    caller_id: &str,
    target: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    if let Some(registry) = registry {
        if let Some(pid) = registry.resolve_target(target).await {
            if let Some(particle) = registry.get_by_pid(&pid).await {
                return invoke_component(
                    engine,
                    &particle.component,
                    &particle.capabilities,
                    &particle.name.unwrap_or_else(|| particle.pid.to_string()),
                    Some(particle.pid),
                    Some(caller_id.to_string()),
                    function,
                    args,
                    endpoint.cloned(),
                    Some(registry.clone()),
                    doc_registry.map(|r| r.clone()),
                )
                .await;
            }
        }
    }

    if let Some(doc_registry) = doc_registry {
        if let Some(resolved) = doc_registry.resolve_name(target).await {
            match resolved {
                ResolvedParticle::Local(pid) => {
                    if let Some(registry) = registry {
                        if let Some(particle) = registry.get_by_pid(&pid).await {
                            return invoke_component(
                                engine,
                                &particle.component,
                                &particle.capabilities,
                                &particle.name.unwrap_or_else(|| particle.pid.to_string()),
                                Some(particle.pid),
                                Some(caller_id.to_string()),
                                function,
                                args,
                                endpoint.cloned(),
                                Some(registry.clone()),
                                Some(doc_registry.clone()),
                            )
                            .await;
                        }
                    }
                }
                ResolvedParticle::Remote(remote) => {
                    let endpoint = endpoint
                        .ok_or_else(|| anyhow!("no endpoint available for remote call"))?;
                    return remote_call(endpoint, &remote, target, function, args).await;
                }
            }
        }
    }

    Err(anyhow!("no particle found with name '{}'", target))
}
```

**Step 2: Convert `remote_call` to async**

```rust
// Before
fn remote_call(
    endpoint: &Endpoint,
    remote: &crate::doc_registry::RemoteParticle,
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
        // ... rest of QUIC I/O ...
    })
}

// After
async fn remote_call(
    endpoint: &Endpoint,
    remote: &crate::doc_registry::RemoteParticle,
    target: &str,
    function: &str,
    args: &[String],
) -> Result<Vec<String>> {
    let conn = endpoint
        .connect(remote.addr.clone(), PLASMOID_ALPN)
        .await
        .map_err(|e| anyhow!("failed to connect to remote node: {}", e))?;

    let (mut send, mut recv) = conn
        .open_bi()
        .await
        .map_err(|e| anyhow!("failed to open stream: {}", e))?;

    let request = wire::CallRequest {
        id: 0,
        target: wire::Target::Name(target.to_string()),
        function: function.to_string(),
        args: args.to_vec(),
    };

    let command = wire::Command::Call(request);
    let request_bytes = wire::serialize(&command)
        .map_err(|e| anyhow!("failed to serialize command: {}", e))?;

    send.write_all(&request_bytes).await?;
    send.finish()?;

    let response_bytes = recv.read_to_end(1024 * 1024).await?;
    let response: wire::CommandResponse = wire::deserialize(&response_bytes)
        .map_err(|e| anyhow!("failed to deserialize response: {}", e))?;

    match response {
        wire::CommandResponse::Call(call_response) => call_response
            .result
            .map_err(|e| anyhow!("particle returned error: {}", e)),
        other => Err(anyhow!("unexpected response type: expected Call, got {:?}", other)),
    }
}
```

**Step 3: Commit**

```bash
git add src/runtime/invoke.rs
git commit -m "feat: convert dispatch_call and remote_call to async"
```

---

### Task 4: Update protocol handler — drop spawn_blocking

**Files:**
- Modify: `src/protocol.rs:166-196`

**Step 1: Replace `spawn_blocking` with direct await in `handle_call`**

Change lines 174-196 from:
```rust
let result = tokio::task::spawn_blocking(move || {
    invoke_component(
        &engine,
        &component,
        &capabilities,
        &particle_id,
        Some(pid),
        Some(remote),
        &function,
        &args,
        Some(&endpoint),
        Some(registry),
        doc_registry,
    )
})
.await;

// Flatten: JoinError (panic) or invocation error -> error response
let result = match result {
    Ok(Ok(wave_results)) => Ok(wave_results),
    Ok(Err(e)) => Err(e.to_string()),
    Err(join_err) => Err(format!("invocation panicked: {}", join_err)),
};
```

to:
```rust
let result = invoke_component(
    &engine,
    &component,
    &capabilities,
    &particle_id,
    Some(pid),
    Some(remote),
    &function,
    &args,
    Some(endpoint),
    Some(registry),
    doc_registry,
)
.await;

let result = match result {
    Ok(wave_results) => Ok(wave_results),
    Err(e) => Err(e.to_string()),
};
```

Note: `Some(&endpoint)` becomes `Some(endpoint)` (owned, not borrowed) to match the new async signature.

**Step 2: Commit**

```bash
git add src/protocol.rs
git commit -m "feat: drop spawn_blocking from protocol handler"
```

---

### Task 5: Verify compilation and tests

**Step 1: Run cargo check**

Run: `cargo check 2>&1`
Expected: no errors. If there are lifetime or Send bound issues, fix them.

**Common issues to watch for:**
- `HostState` must be `Send` (it already is)
- `func_wrap_async` closures must be `Send + Sync + 'static`
- References across `.await` points need to become owned values
- The `endpoint` parameter change from `&Endpoint` to `Endpoint` may need updates at call sites

**Step 2: Run cargo test**

Run: `cargo test 2>&1`
Expected: all 31 tests pass (same as before)

**Step 3: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve async compilation issues"
```

---

### Task 6: Validate with ring benchmark — 2000 particles

**Step 1: Build the ring component**

```bash
cd components/ring && cargo component build --release && cd ../..
```

**Step 2: Start the runtime and run the benchmark**

Terminal 1:
```bash
mise run ring:node
```

Terminal 2 — run multiple times with increasing particle counts:
```bash
mise run ring:run -- 100 100
mise run ring:run -- 500 50
mise run ring:run -- 2000 10
mise run ring:run -- 2000 10   # second run to verify no thread exhaustion
```

Expected: all four complete without hanging. The 2000-particle run proves we're not limited to 512 blocking threads anymore.

**Step 3: Commit validation results as a comment or log**

```bash
git add -A
git commit -m "feat: async WASM invocation — validated with 2000 particle ring"
```
