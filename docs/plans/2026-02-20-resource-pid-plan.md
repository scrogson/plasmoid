# Resource PID Implementation Plan

> **For Claude:** REQUIRED SUB-SKILL: Use superpowers:executing-plans to implement this plan task-by-task.

**Goal:** Replace string-based PID routing with a WIT resource type so `send` is O(1) handle-to-mailbox instead of O(n) string scanning.

**Architecture:** Define `resource pid` in the actor-context WIT interface. Host stores `Pid` values in `ResourceTable`, components hold opaque handles. `send(target: pid, ...)` resolves the handle via `ResourceTable::get` (O(1)) then `HashMap::get` on the mailbox map (O(1)).

**Tech Stack:** wasmtime 41 (component model resources, `ResourceType::host`, `Resource<T>`), cargo-component 0.21.1 (bindgen)

---

### Task 1: Update WIT files — add resource pid, update signatures

Three copies of the WIT must be updated identically:
- `wit/world.wit`
- `wit/components/deps/runtime/world.wit`
- `wit/components/ring/deps/runtime/world.wit`

**Step 1: Replace the actor-context interface in all 3 files**

The new interface. The logging interface is unchanged — only actor-context changes:

```wit
interface actor-context {
    resource pid {
        /// Display representation, e.g. "<ec47b34e.1>"
        to-string: func() -> string;
    }

    /// Resolve a PID string back to a typed handle.
    /// Used when PIDs are received in messages or wave-encoded args.
    resolve: func(pid-string: string) -> option<pid>;

    /// Get this particle's PID.
    self-pid: func() -> pid;

    /// Get this particle's registered name, if any.
    self-name: func() -> option<string>;

    /// Get the caller's PID, if known.
    caller-pid: func() -> option<pid>;

    /// Call a function on another particle by name and await the response.
    /// target: the registered name of the target particle
    /// function: the function name to call
    /// args: wasm-wave encoded arguments
    /// Returns wasm-wave encoded results
    call: func(target: string, function: string, args: list<string>) -> result<list<string>, string>;

    /// Fire-and-forget function invocation on another particle by name.
    notify: func(target: string, function: string, args: list<string>) -> result<_, string>;

    /// Spawn a new particle from a registered component.
    /// Returns the PID of the new particle.
    spawn: func(component: string, name: option<string>) -> result<pid, string>;

    /// Deposit a message into a particle's mailbox. O(1) via resource handle.
    send: func(target: pid, message: list<string>) -> result<_, string>;

    /// Block until a message arrives in this particle's mailbox.
    receive: func() -> list<string>;
}
```

Also add `send` and `receive` to `wit/world.wit` — it's currently missing them (the component deps have them but the root doesn't).

**Step 2: Commit**

```bash
git add wit/world.wit wit/components/deps/runtime/world.wit wit/components/ring/deps/runtime/world.wit
git commit -m "feat: add resource pid to actor-context WIT interface"
```

---

### Task 2: Add ResourceTable accessors and send_to_pid

**Files:**
- Modify: `src/host/state.rs` — add `resource_table()` and `resource_table_mut()` accessors
- Modify: `src/registry.rs` — add `send_to_pid` method

**Step 1: Add ResourceTable accessors to HostState**

In `src/host/state.rs`, add these methods to the `impl HostState` block:

```rust
pub fn resource_table(&self) -> &ResourceTable {
    &self.resource_table
}

pub fn resource_table_mut(&mut self) -> &mut ResourceTable {
    &mut self.resource_table
}
```

**Step 2: Add `send_to_pid` to ParticleRegistry**

In `src/registry.rs`, add this method to the `impl ParticleRegistry` block, after the existing `send_message` method:

```rust
/// Send a message directly by Pid — no string resolution. O(1).
pub async fn send_to_pid(&self, pid: &Pid, message: Vec<String>) -> Result<()> {
    let mailboxes = self.mailboxes.read().await;
    let handle = mailboxes
        .get(pid)
        .ok_or_else(|| anyhow!("no mailbox for pid '{}'", pid))?;
    handle
        .tx
        .send(message)
        .map_err(|_| anyhow!("mailbox closed for pid '{}'", pid))
}
```

**Step 3: Commit**

```bash
git add src/host/state.rs src/registry.rs
git commit -m "feat: add ResourceTable accessors and send_to_pid"
```

---

### Task 3: Register pid resource and update host functions

This is the big task. All changes are in `src/runtime/invoke.rs`.

**Step 1: Add the Resource import**

Add to the imports at the top of `src/runtime/invoke.rs`:

```rust
use wasmtime::component::Resource;
```

**Step 2: Register the pid resource type in `add_host_functions`**

Inside the `actor-context` block (after the `let mut context = linker.instance(...)` line), add the resource registration before any function wraps:

```rust
// Register the pid resource type
context.resource(
    "pid",
    wasmtime::component::ResourceType::host::<Pid>(),
    |_ctx, _rep| Ok(()),
)?;
```

**Step 3: Convert `self-pid` to return `Resource<Pid>`**

Change from:
```rust
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

to:
```rust
context.func_wrap_async(
    "self-pid",
    |mut caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()| {
        Box::new(async move {
            let pid = caller.data().pid().cloned().unwrap_or_else(|| {
                // Fallback: create a synthetic PID from particle_id
                // This shouldn't happen for spawned particles
                panic!("self-pid called on particle without a PID")
            });
            let resource = caller.data_mut().resource_table_mut().push(pid)
                .map_err(|e| anyhow!("{}", e))?;
            Ok((resource,))
        })
    },
)?;
```

**Step 4: Convert `caller-pid` to return `Option<Resource<Pid>>`**

Change from returning `Option<String>` to `Option<Resource<Pid>>`:

```rust
context.func_wrap_async(
    "caller-pid",
    |mut caller: wasmtime::StoreContextMut<'_, HostState>,
     _: ()| {
        Box::new(async move {
            let resource = if let Some(pid) = caller.data().remote_pid().cloned() {
                Some(caller.data_mut().resource_table_mut().push(pid)
                    .map_err(|e| anyhow!("{}", e))?)
            } else {
                None
            };
            Ok((resource,))
        })
    },
)?;
```

Note: `caller-pid` should return the caller's `Pid` (from `remote_pid()`), not the remote_node_id string. This requires that the protocol handler passes the caller's Pid, which may need wiring. If `remote_pid` is `None`, return `None`. This is a pre-existing issue — `caller-pid` currently returns `remote_node_id` (a string), not a proper PID.

**Step 5: Convert `spawn` to return `Result<Resource<Pid>, String>`**

Change the spawn handler. The result type changes from `Result<String, String>` to `Result<Resource<Pid>, String>`:

```rust
context.func_wrap_async(
    "spawn",
    |mut caller: wasmtime::StoreContextMut<'_, HostState>,
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
                Ok(pid) => {
                    let resource = caller.data_mut().resource_table_mut().push(pid)
                        .map_err(|e| anyhow!("{}", e))?;
                    Ok((Ok(resource),))
                }
                Err(e) => Ok((Err(e.to_string()),)),
            }
        })
    },
)?;
```

**Step 6: Convert `send` to take `Resource<Pid>` target**

Change from string target to resource target:

```rust
context.func_wrap_async(
    "send",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (target, message): (Resource<Pid>, Vec<String>)| {
        Box::new(async move {
            if !caller.data().capabilities().allows("actor:send") {
                return Ok((Err("unauthorized: actor:send not permitted".to_string()),));
            }

            let pid = caller.data().resource_table().get(&target)
                .map_err(|e| anyhow!("{}", e))?
                .clone();

            let registry = match caller.data().registry() {
                Some(r) => r.clone(),
                None => {
                    return Ok((Err("no registry available for send".to_string()),));
                }
            };

            let result = registry.send_to_pid(&pid, message).await;

            match result {
                Ok(()) => Ok((Ok(()),)),
                Err(e) => Ok((Err(e.to_string()),)),
            }
        })
    },
)?;
```

**Step 7: Add `resolve` host function**

After the `receive` handler, add:

```rust
// resolve: func(pid-string: string) -> option<pid>
context.func_wrap_async(
    "resolve",
    |mut caller: wasmtime::StoreContextMut<'_, HostState>,
     (pid_string,): (String,)| {
        Box::new(async move {
            let registry = match caller.data().registry() {
                Some(r) => r.clone(),
                None => return Ok((None::<Resource<Pid>>,)),
            };

            match registry.resolve_target(&pid_string).await {
                Some(pid) => {
                    let resource = caller.data_mut().resource_table_mut().push(pid)
                        .map_err(|e| anyhow!("{}", e))?;
                    Ok((Some(resource),))
                }
                None => Ok((None,)),
            }
        })
    },
)?;
```

**Step 8: Add `[method]pid.to-string` host function**

After the `resolve` handler, add:

```rust
// [method]pid.to-string: func() -> string
context.func_wrap_async(
    "[method]pid.to-string",
    |caller: wasmtime::StoreContextMut<'_, HostState>,
     (self_,): (Resource<Pid>,)| {
        Box::new(async move {
            let pid = caller.data().resource_table().get(&self_)
                .map_err(|e| anyhow!("{}", e))?;
            Ok((pid.to_string(),))
        })
    },
)?;
```

**Step 9: Add `[resource-drop]pid` handler (may be needed explicitly)**

The destructor registered via `context.resource(...)` should handle this automatically. If not, add:

```rust
context.func_wrap_async(
    "[resource-drop]pid",
    |mut caller: wasmtime::StoreContextMut<'_, HostState>,
     (resource,): (Resource<Pid>,)| {
        Box::new(async move {
            caller.data_mut().resource_table_mut().delete(resource)
                .map_err(|e| anyhow!("{}", e))?;
            Ok(())
        })
    },
)?;
```

**Step 10: Commit (won't compile yet — ring component needs updating)**

```bash
git add src/runtime/invoke.rs
git commit -m "feat: register pid resource and update host functions"
```

---

### Task 4: Update ring component

**Files:**
- Modify: `components/ring/src/lib.rs`

The `cargo component build` will regenerate bindings from the updated WIT. The generated `actor_context::Pid` type will be an opaque resource handle with a `to_string()` method.

**Step 1: Rewrite the ring component to use typed PIDs**

```rust
#[allow(warnings)]
mod bindings;

use bindings::exports::plasmoid::ring::ring::Guest;
use bindings::plasmoid::runtime::{actor_context, logging};

struct Ring;

impl Guest for Ring {
    fn run(num_processes: u32, num_messages: u32) -> String {
        let self_pid = actor_context::self_pid();

        logging::log(
            logging::Level::Info,
            &format!(
                "Starting ring: {} processes, {} messages",
                num_processes, num_messages
            ),
        );

        // Spawn N unnamed particles, collect their PIDs (typed resources)
        let mut pids = Vec::new();
        for _ in 0..num_processes {
            match actor_context::spawn("ring", None) {
                Ok(pid) => pids.push(pid),
                Err(e) => return format!("Error spawning: {}", e),
            }
        }

        // Start each particle's receive loop, telling it who its next neighbor is.
        // notify args are wave-encoded; PID strings must be quoted.
        for i in 0..num_processes as usize {
            let next = pids[(i + 1) % pids.len()].to_string();
            let next_wave = format!("\"{}\"", next);
            // notify still takes string target
            if let Err(e) = actor_context::notify(&pids[i].to_string(), "start", &[next_wave]) {
                return format!("Error starting particle: {}", e);
            }
        }

        let start = std::time::Instant::now();

        // Send initial message to the last particle — typed PID, O(1)
        let last = &pids[pids.len() - 1];
        if let Err(e) = actor_context::send(last, &[num_messages.to_string(), self_pid.to_string()])
        {
            return format!("Error sending initial message: {}", e);
        }

        // Wait for completion
        let _msg = actor_context::receive();
        let elapsed = start.elapsed();

        // Shut down all ring particles — typed PID sends, O(1) each
        for pid in &pids {
            let _ = actor_context::send(pid, &["stop".to_string()]);
        }

        let total = num_processes as u64 * num_messages as u64;
        let rate = if elapsed.as_secs_f64() > 0.0 {
            total as f64 / elapsed.as_secs_f64()
        } else {
            0.0
        };

        format!(
            "Ring: {} processes, {} messages ({} total hops) in {:.3}s ({:.0} msg/s)",
            num_processes,
            num_messages,
            total,
            elapsed.as_secs_f64(),
            rate,
        )
    }

    fn start(next_pid_str: String) {
        // Resolve the next PID once at startup — O(n) once, then O(1) for all sends
        let next_pid = match actor_context::resolve(&next_pid_str) {
            Some(pid) => pid,
            None => {
                logging::log(
                    logging::Level::Error,
                    &format!("Failed to resolve next PID: {}", next_pid_str),
                );
                return;
            }
        };

        loop {
            let msg = actor_context::receive();

            if msg.is_empty() || msg[0] == "stop" {
                return;
            }

            let hops: u32 = match msg[0].parse() {
                Ok(h) => h,
                Err(_) => {
                    logging::log(
                        logging::Level::Error,
                        &format!("bad hop count: {}", msg[0]),
                    );
                    return;
                }
            };

            let master_str = if msg.len() > 1 { &msg[1] } else { return };

            if hops == 0 {
                // Resolve master PID and signal completion
                if let Some(master) = actor_context::resolve(master_str) {
                    let _ = actor_context::send(&master, &["finished".to_string()]);
                }
                // Wait for "stop" from orchestrator
                continue;
            }

            // Forward to next — O(1) typed PID send
            let _ = actor_context::send(
                &next_pid,
                &[(hops - 1).to_string(), master_str.to_string()],
            );
        }
    }
}

bindings::export!(Ring with_types_in bindings);
```

Key changes:
- `spawn` returns a typed `Pid` resource handle (not a string)
- `self_pid` returns a typed `Pid` resource handle
- `send` takes `&Pid` (resource handle) — O(1) routing
- `start` calls `resolve()` once to get a typed handle from the string arg
- `notify` target uses `pid.to_string()` (still string-based, not hot path)

**Step 2: Commit**

```bash
git add components/ring/src/lib.rs
git commit -m "feat: ring component uses typed PID resources for send"
```

---

### Task 5: Verify compilation and tests

**Step 1: Build the host runtime**

Run: `cargo check 2>&1`
Expected: no errors.

**Common issues to watch for:**
- `Resource<Pid>` needs `Pid: Send + 'static` (it already is)
- The `resource_table().get()` returns `Result<&T, ResourceTableError>` — may need `.map_err()` conversion
- Method name `[method]pid.to-string` must match exactly what wasmtime expects
- The `[resource-drop]pid` handler may conflict with the destructor from `context.resource()` — try without it first

**Step 2: Build the ring component**

Run: `cd components/ring && cargo component build --release 2>&1`
Expected: builds successfully. The bindings will auto-regenerate with the typed `Pid` resource.

**Step 3: Run tests**

Run: `cargo test 2>&1`
Expected: all tests pass.

**Step 4: Commit any fixes**

```bash
git add -A
git commit -m "fix: resolve resource pid compilation issues"
```

---

### Task 6: Benchmark — compare before and after

**Step 1: Copy ring WASM to target**

```bash
cp components/ring/target/wasm32-wasip1/release/ring.wasm target/debug/
```

**Step 2: Start runtime and benchmark**

Terminal 1:
```bash
cargo build && RUST_LOG=warn plasmoid start target/debug/ring.wasm --spawn ring --name ring-bench
```

Terminal 2:
```bash
NODE_ID=$(cat ~/.config/plasmoid/node_id)
PLASMOID_NODE="$NODE_ID" plasmoid call ring-bench run 100 100
PLASMOID_NODE="$NODE_ID" plasmoid call ring-bench run 500 50
PLASMOID_NODE="$NODE_ID" plasmoid call ring-bench run 2000 10
PLASMOID_NODE="$NODE_ID" plasmoid call ring-bench run 2000 10  # second run
```

Before (string PIDs):
- 100 particles: 479k msg/s
- 500 particles: 331k msg/s
- 2000 particles: 43k msg/s

Expected: significant improvement at 2000 particles since `send` is now O(1).

**Step 3: Commit**

```bash
git commit --allow-empty -m "feat: resource PID — validated with ring benchmark"
```
