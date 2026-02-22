# Plasmoid SDK Design

## Problem

Component authoring is too verbose and low-level. Echo is 97 lines of manual byte wrangling. Ring is 220 lines. Components deal with raw `list<u8>` messages, manual PID encoding, `process::log(LogLevel::Info, &format!(...))` for logging, and boilerplate for `mod bindings`, `impl Guest`, `export!`.

## Goal

A Rust SDK crate (`plasmoid-sdk`) that makes component authoring concise and idiomatic. Echo should be ~10 lines. Ring should be ~55 lines.

## Architecture

Two layers, each building on the last:

```
Layer 2:  GenServer — handle_call/handle_cast/handle_info, automatic recv loop
Layer 1:  Ergonomics — logging macros, #[plasmoid::main], typed send/recv, serde helpers
```

Key design principle: **call/cast are GenServer-specific concepts**. Raw processes just send and receive messages. The call/cast wire protocol lives entirely inside the GenServer machinery.

## Crate Structure

```
crates/
  plasmoid-macros/          # proc-macro crate
    Cargo.toml
    src/lib.rs              # #[plasmoid::main], #[gen_server]
  plasmoid-sdk/             # main SDK crate
    Cargo.toml
    src/
      lib.rs                # re-exports, prelude module
      log.rs                # trace!, debug!, info!, warn!, error!
      messaging.rs          # send_msg, recv_msg, encode, decode
      gen_server.rs         # GenServer receive loop, call/cast wire protocol
```

### Dependencies

**plasmoid-sdk:**
```toml
[dependencies]
plasmoid-macros = { path = "../plasmoid-macros" }
postcard = { version = "1", features = ["alloc"] }
serde = "1"
wit-bindgen-rt = "0.41"
```

**plasmoid-macros:**
```toml
[lib]
proc-macro = true

[dependencies]
syn = { version = "2", features = ["full"] }
quote = "1"
proc-macro2 = "1"
```

**Component Cargo.toml:**
```toml
[dependencies]
plasmoid-sdk = { path = "../../crates/plasmoid-sdk" }
serde = { version = "1", features = ["derive"] }  # only if using typed messages
```

## Layer 1: Ergonomics

### Logging Macros

`macro_rules!` macros that expand to `process::log()` calls. Since all particle components have `mod bindings;` at crate root, bare `crate::` in macro expansion resolves at the call site.

```rust
#[macro_export]
macro_rules! info {
    ($($arg:tt)*) => {{
        crate::bindings::plasmoid::runtime::process::log(
            crate::bindings::plasmoid::runtime::process::LogLevel::Info,
            &::std::format!($($arg)*),
        )
    }};
}
// Same pattern for trace!, debug!, warn!, error!
```

No structured key=value fields in v1. The WIT `log` function takes a flat string, so structured fields would just be formatted into the string anyway.

### `#[plasmoid::main]` Attribute Macro

Eliminates boilerplate. Detects function signature and generates the right code.

**No args:**
```rust
#[plasmoid::main]
fn start() -> Result<(), String> { ... }
```
Generates:
```rust
#[allow(warnings)]
mod bindings;
use crate::bindings::plasmoid::runtime::process::*;

struct __PlasmoidComponent;
impl bindings::Guest for __PlasmoidComponent {
    fn start() -> Result<(), String> { /* user body */ }
}
bindings::export!(__PlasmoidComponent with_types_in bindings);
```

**With init args:**
```rust
#[plasmoid::main]
fn start(init_args: String) -> Result<(), String> { ... }
```
Generates the same but with `fn start(init_args: String)` in the Guest impl.

**With struct (GenServer):**
```rust
#[plasmoid::main]
#[derive(Default)]
struct Echo;
```
Generates `mod bindings`, the `use` import, and `export!`. The `#[gen_server]` macro on the impl block generates the `Guest` trait impl.

### Prelude

`#[plasmoid::main]` brings everything into scope via `use crate::bindings::plasmoid::runtime::process::*`. The prelude re-exports SDK utilities:

```rust
pub mod prelude {
    pub use crate::{trace, debug, info, warn, error};
    pub use crate::{send_msg, recv_msg, encode, decode};
    pub use plasmoid_macros::{main as main, gen_server};
}
```

Components write `use plasmoid_sdk::prelude::*;` and get logging macros, messaging helpers, and proc macros.

### Serde Helpers

Using `postcard` (compact binary, no_std friendly, already a project dependency):

```rust
pub fn encode<T: Serialize>(val: &T) -> Vec<u8> {
    postcard::to_allocvec(val).expect("serialization failed")
}

pub fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<T, String> {
    postcard::from_bytes(bytes).map_err(|e| format!("decode error: {e}"))
}
```

### Typed Messaging

Thin wrappers around the raw `process::send` and `process::recv` that handle serde:

```rust
pub fn send_msg<T: Serialize>(target: &Pid, msg: &T) -> Result<(), SendError> {
    process::send(target, &encode(msg))
}

pub fn recv_msg<T: DeserializeOwned>(timeout_ms: Option<u64>) -> Option<T> {
    loop {
        match process::recv(timeout_ms) {
            Some(Message::Data(data)) => match decode(&data) {
                Ok(msg) => return Some(msg),
                Err(_) => continue,  // skip malformed messages
            },
            Some(_) => continue,     // skip system messages
            None => return None,     // closed or timeout
        }
    }
}
```

`recv_msg` filters to data messages, auto-decodes, and skips system messages. Returns `None` on mailbox close or timeout.

## Layer 2: GenServer

### The `#[gen_server]` Proc Macro

Reads an impl block, infers types from method signatures, and generates:
1. The receive loop (recv + decode + dispatch + encode reply)
2. Static client methods: `MyServer::call()`, `MyServer::cast()`
3. The `Guest` trait impl with `start` function
4. The call/cast wire protocol (invisible to users)

### Method Signatures

```rust
#[gen_server]
impl MyServer {
    // Optional — if absent, uses Default::default()
    fn init(args: String) -> Result<Self, String> { ... }

    // Optional — synchronous request/reply
    fn handle_call(&mut self, req: RequestType) -> ResponseType { ... }

    // Optional — async fire-and-forget
    fn handle_cast(&mut self, msg: CastType) -> CastResult { ... }

    // Optional — raw messages from non-GenServer senders
    fn handle_info(&mut self, data: Vec<u8>) -> CastResult { ... }
}

pub enum CastResult { Continue, Stop }
```

Only define what you need. The macro adapts based on which methods are present.

### Generated Client API

For each server, the macro generates static methods:

```rust
// If handle_call is defined:
impl MyServer {
    pub fn call(target: &Pid, req: &RequestType, timeout_ms: Option<u64>) -> Result<ResponseType, CallError>;
}

// If handle_cast is defined:
impl MyServer {
    pub fn cast(target: &Pid, msg: &CastType) -> Result<(), SendError>;
}
```

### Call/Cast Wire Protocol (internal)

Invisible to users. Exists only between `MyServer::call()` and the generated receive loop.

**Call** uses tagged messages (`send_ref`/`recv_ref`):
- Client: `make_ref()` -> encode `[pid_str_len: u32 LE][pid_str][request_bytes]` -> `send_ref(target, ref, payload)` -> `recv_ref(ref, timeout)`
- Server receive loop: decode tagged message -> extract sender PID + request -> call `handle_call` -> `send_ref(sender, ref, response_bytes)`

**Cast** uses data messages (`send`):
- Client: encode `[CAST_TAG: u8][message_bytes]` -> `send(target, payload)`
- Server receive loop: decode data message -> if starts with CAST_TAG, call `handle_cast`

**Raw messages** (no tag prefix): dispatched to `handle_info`.

### Generated Receive Loop (pseudocode)

```rust
fn start() -> Result<(), String> {
    let mut state = Self::init()?;  // or Default::default()
    loop {
        match process::recv(None) {
            Some(Message::Tagged(tagged)) => {
                // Decode call: extract sender, ref, request
                let (from, request) = decode_call_payload(&tagged.payload);
                let response = state.handle_call(request);
                process::send_ref(&from, tagged.ref_id, &encode(&response));
            }
            Some(Message::Data(data)) => {
                if data[0] == CAST_TAG {
                    let msg = decode(&data[1..]);
                    match state.handle_cast(msg) {
                        CastResult::Stop => return Ok(()),
                        CastResult::Continue => {}
                    }
                } else {
                    match state.handle_info(data) {
                        CastResult::Stop => return Ok(()),
                        CastResult::Continue => {}
                    }
                }
            }
            Some(Message::Exit(_)) | Some(Message::Down(_)) => {
                // Future: handle_system for supervised processes
            }
            None => return Ok(()),
        }
    }
}
```

## WIT Changes

None. The WIT interface stays at v0.4.0. All SDK abstractions are built on top of existing primitives (`send`, `send_ref`, `recv`, `recv_ref`, `make_ref`).

## Examples

### Echo (GenServer) — before: 97 lines, after: ~10 lines

```rust
use plasmoid_sdk::prelude::*;

#[plasmoid::main]
#[derive(Default)]
struct Echo;

#[gen_server]
impl Echo {
    fn handle_call(&mut self, req: Vec<u8>) -> Vec<u8> {
        req
    }
}
```

### Ring (raw process) — before: 220 lines, after: ~55 lines

```rust
use plasmoid_sdk::prelude::*;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
enum RingMsg { Setup(String), Hop { remaining: u32, master: String }, Finished, Stop }

#[plasmoid::main]
fn start(init_args: String) -> Result<(), String> {
    let trimmed = init_args.trim();
    if trimmed.starts_with("orchestrator(") {
        run_orchestrator(trimmed)
    } else if trimmed == "worker" {
        run_worker()
    } else {
        Err(format!("unknown role: {trimmed}"))
    }
}

fn run_orchestrator(args: &str) -> Result<(), String> {
    let (n, m) = parse_orchestrator_args(args)?;
    info!("Ring: spawning {n} processes, {m} messages");

    let self_str = self_pid().to_string();
    let workers: Vec<Pid> = (0..n)
        .map(|_| spawn("ring", None, "\"worker\""))
        .collect::<Result<_, _>>()?;

    for i in 0..n as usize {
        let next = workers[(i + 1) % workers.len()].to_string();
        send_msg(&workers[i], &RingMsg::Setup(next))?;
    }

    let t = std::time::Instant::now();
    send_msg(&workers[n as usize - 1], &RingMsg::Hop { remaining: m, master: self_str })?;

    while let Some(msg) = recv_msg::<RingMsg>(None) {
        if matches!(msg, RingMsg::Finished) {
            let e = t.elapsed();
            let total = n as u64 * m as u64;
            info!("Ring: {n}x{m} ({total} hops) in {:.3}s ({:.0} msg/s)",
                e.as_secs_f64(), total as f64 / e.as_secs_f64());
            for p in &workers { let _ = send_msg(p, &RingMsg::Stop); }
            return Ok(());
        }
    }
    Ok(())
}

fn run_worker() -> Result<(), String> {
    let next = loop {
        match recv_msg::<RingMsg>(None).ok_or("closed")? {
            RingMsg::Setup(p) => break resolve(&p).ok_or("bad pid")?,
            RingMsg::Stop => return Ok(()),
            _ => {}
        }
    };

    while let Some(msg) = recv_msg::<RingMsg>(None) {
        match msg {
            RingMsg::Stop => return Ok(()),
            RingMsg::Hop { remaining: 0, master } =>
                { send_msg(&resolve(&master).ok_or("bad master")?, &RingMsg::Finished)?; }
            RingMsg::Hop { remaining, master } =>
                { send_msg(&next, &RingMsg::Hop { remaining: remaining - 1, master })?; }
            _ => {}
        }
    }
    Ok(())
}
```

### Client calling a GenServer from another process

```rust
// Calling echo from the orchestrator:
let response: Vec<u8> = Echo::call(&echo_pid, b"hello".to_vec(), None)?;

// Casting to a worker:
Worker::cast(&worker_pid, &RingMsg::Stop)?;
```

## Implementation Order

1. **Crate scaffolding** — create `crates/plasmoid-macros` and `crates/plasmoid-sdk` with Cargo.toml
2. **Logging macros** — `trace!`, `debug!`, `info!`, `warn!`, `error!` in `plasmoid-sdk/src/log.rs`
3. **Serde helpers** — `encode`, `decode`, `send_msg`, `recv_msg` in `plasmoid-sdk/src/messaging.rs`
4. **`#[plasmoid::main]`** — proc macro that generates `mod bindings`, `Guest` impl, `export!`
5. **Prelude** — re-exports of macros and helpers
6. **Port echo and ring** — use Layer 1 (macros + typed messaging)
7. **`#[gen_server]`** — proc macro with receive loop generation, call/cast wire protocol
8. **Port echo to GenServer** — use Layer 2
9. **Generated client methods** — `Echo::call()`, `Worker::cast()`
