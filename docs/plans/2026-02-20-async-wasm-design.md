# Async WASM Invocation — Design

## Goal

Switch the runtime from sync WASM invocation (one OS thread per blocked particle) to async fiber-based invocation (thousands of suspended futures on a handful of threads).

## Problem

Every particle that calls `receive()` blocks an OS thread via `spawn_blocking` + `rt.block_on()`. Tokio's default `max_blocking_threads` is 512, capping particle count. The ring benchmark can't run twice with 500 particles because the first run's threads aren't released.

## Approach: Fiber-based async (wasmtime 41)

Enable `Config::async_support(true)`. Wasmtime uses fibers internally — when a host function awaits, the fiber suspends and the tokio task yields. No thread blocked. This is wasmtime's stable async path.

### Alternatives considered

- **Component-model-async**: True concurrent async ABI via `func_wrap_concurrent()`. Wasmtime docs say "support is currently incomplete." Revisit when it matures.
- **Increase thread pool**: Zero code changes but doesn't solve the fundamental problem. Postpones the inevitable.

## Changes

### 1. Engine config

```rust
let mut config = Config::new();
config.wasm_component_model(true);
config.async_support(true);  // NEW
```

All-or-nothing: async config means all stores, all invocations, all host functions must use async variants.

### 2. `invoke_component` becomes async

```rust
pub async fn invoke_component(...) -> Result<Vec<String>> {
    let instance = linker.instantiate_async(&mut store, component).await?;
    func.call_async(&mut store, &params, &mut results).await?;
    func.post_return_async(&mut store).await?;
}
```

Same signature, same inputs/outputs — just a future now.

### 3. Host functions: `func_wrap` → `func_wrap_async`

Every host function that bridges sync→async via `rt.block_on()` becomes a natural async closure:

```rust
// Before: blocks an OS thread
context.func_wrap("receive",
    |caller, _: ()| {
        let rt = tokio::runtime::Handle::current();
        match rt.block_on(registry.receive_message(&pid)) { ... }
    },
)?;

// After: suspends a fiber, no thread blocked
context.func_wrap_async("receive",
    |caller, _: ()| {
        Box::new(async move {
            match registry.receive_message(&pid).await { ... }
        })
    },
)?;
```

All 6 host functions follow this pattern:

| Host function | Before | After |
|---|---|---|
| `spawn` | `rt.block_on(registry.spawn(...))` | `registry.spawn(...).await` |
| `call` | sync `dispatch_call()` | `dispatch_call(...).await` |
| `notify` | `rt.spawn(spawn_blocking(dispatch_call))` | `tokio::spawn(dispatch_call(...))` |
| `send` | `rt.block_on(registry.send_message(...))` | `registry.send_message(...).await` |
| `receive` | `rt.block_on(registry.receive_message(...))` | `registry.receive_message(...).await` |
| `self-pid`, `self-name`, `caller-pid` | sync (no I/O) | `func_wrap_async` (trivially, for API consistency) |

WASI linking: `add_to_linker_sync` → `add_to_linker_async`.

### 4. `dispatch_call` becomes async

```rust
async fn dispatch_call(...) -> Result<Vec<String>> {
    if let Some(pid) = registry.resolve_target(target).await { ... }
    invoke_component(...).await
}
```

`remote_call` drops its `rt.block_on()` — the QUIC I/O just `.await`s directly.

### 5. Protocol handler drops `spawn_blocking`

```rust
// Before
let result = tokio::task::spawn_blocking(move || invoke_component(...)).await;

// After
let result = invoke_component(...).await;
```

The async task suspends when a host function awaits. No thread pool involved.

## What stays the same

- **WIT interfaces** — unchanged. Components don't know the host went async.
- **WASM components** — no recompilation. The ring component works as-is.
- **`HostState`** — same struct. `WasiView` implementation unchanged.
- **`ParticleRegistry`** — already fully async.
- **Wire protocol** — same serialization, same QUIC streams.
- **Tests** — unit tests unchanged (already `#[tokio::test]`).

## What gets deleted

- 8 `rt.block_on()` calls
- 2 `spawn_blocking` wrappers
- All `tokio::runtime::Handle::current()` calls (only needed for sync→async bridge)

## Files changed

- `Cargo.toml` — add wasmtime `async` feature flag
- `src/runtime/engine.rs` — `config.async_support(true)`
- `src/runtime/invoke.rs` — async `invoke_component`, async host functions, async `dispatch_call`
- `src/protocol.rs` — drop `spawn_blocking`, await directly

## Validation

Run the ring benchmark with 1000+ particles to prove no thread exhaustion:

```bash
mise run ring:run -- 2000 10
```

Should complete without hanging, using only a few OS threads.
