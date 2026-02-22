# Plasmoid

A distributed WASM actor runtime on [iroh](https://iroh.computer) mesh networking with [WIT](https://component-model.bytecodealliance.org/design/wit.html) interfaces and [Cedar](https://www.cedarpolicy.com/) authorization.

Actors (called **particles**) are WebAssembly components that communicate via message passing, run in sandboxed isolation, and can be deployed across a mesh of interconnected nodes.

## Quick Start

```bash
# Build the runtime
cargo build --release

# Create a new application
plasmoid new my-app
cd my-app

# Create a component
plasmoid component new greeter

# Build the component
cargo component build -p greeter --release

# Start a node with the component
plasmoid start greeter.wasm --spawn greeter --name greeter
```

## Architecture

```
┌─────────────────────────────────────────────┐
│  Plasmoid Node                              │
│                                             │
│  ┌──────────────┐  ┌─────────────────────┐  │
│  │ iroh Endpoint │  │ Particle Registry   │  │
│  │ (QUIC mesh)   │  │ Component -> WASM   │  │
│  └──────────────┘  └─────────────────────┘  │
│                                             │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐     │
│  │ WASM    │  │ WASM    │  │ WASM    │     │
│  │ Particle│  │ Particle│  │ Particle│     │
│  └─────────┘  └─────────┘  └─────────┘     │
│                                             │
│  ┌─────────────────────────────────────┐    │
│  │ Host Functions (Cedar-gated)        │    │
│  │ spawn, send, recv, log, link, ...   │    │
│  └─────────────────────────────────────┘    │
└─────────────────────────────────────────────┘
```

Particles communicate through typed messages, can spawn child processes, and support OTP-style links and monitors for fault tolerance.

## Plasmoid SDK

The `plasmoid-sdk` crate provides a high-level API for writing particles with minimal boilerplate.

### Function-based particles

```rust
use plasmoid_sdk::prelude::*;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
enum MyInit {
    Leader(u32),
    Worker,
}

#[plasmoid_sdk::main]
fn start(init: MyInit) -> Result<(), String> {
    match init {
        MyInit::Leader(n) => {
            info!("Leading {} workers", n);
            for _ in 0..n {
                let args = to_init_args(&MyInit::Worker);
                spawn("my-component", None, &args)?;
            }
            Ok(())
        }
        MyInit::Worker => {
            info!("Worker started");
            while let Some(msg) = recv!(MyMsg, None) {
                // handle messages
            }
            Ok(())
        }
    }
}
```

### GenServer-style particles

```rust
use plasmoid_sdk::prelude::*;
use serde::{Serialize, Deserialize};

#[derive(Serialize, Deserialize)]
enum Request { Get, Increment }

#[derive(Default)]
struct Counter { value: u64 }

#[plasmoid_sdk::gen_server]
impl Counter {
    fn handle_call(&mut self, req: Request) -> u64 {
        match req {
            Request::Get => self.value,
            Request::Increment => {
                self.value += 1;
                self.value
            }
        }
    }
}
```

The `#[gen_server]` macro generates the receive loop, message dispatch, and typed client methods (`Counter::call`, `Counter::cast`) automatically.

### SDK features

| Feature | Description |
|---|---|
| `#[plasmoid_sdk::main]` | Entry point macro with optional typed init args (JSON auto-deserialization) |
| `#[plasmoid_sdk::gen_server]` | GenServer macro with `handle_call`, `handle_cast`, `handle_info` |
| `send!` / `recv!` | Typed messaging macros using postcard serialization |
| `info!`, `debug!`, etc. | Structured logging macros |
| `encode` / `decode` | Postcard serialization helpers |
| `from_init_args` / `to_init_args` | JSON serialization for init arguments |

## Examples

### Echo (GenServer)

A minimal echo server in 16 lines:

```rust
#[derive(Default)]
struct Echo;

#[plasmoid_sdk::gen_server]
impl Echo {
    fn handle_call(&mut self, req: Vec<u8>) -> Vec<u8> {
        req
    }

    fn handle_info(&mut self, data: Vec<u8>) -> plasmoid_sdk::CastResult {
        if data == b"stop" {
            return plasmoid_sdk::CastResult::Stop;
        }
        plasmoid_sdk::CastResult::Continue
    }
}
```

### Ring Benchmark

Spawns N worker processes in a ring, passes M messages around:

```bash
plasmoid start ring.wasm --spawn ring --init '{"orchestrator":[100,1000]}'
```

## CLI

```
plasmoid new <app-name>              Create a new application workspace
plasmoid component new <name>        Create a new component
plasmoid start [options] [<wasm>...] Boot a node and load components
plasmoid spawn <component>           Spawn a particle on a running node
plasmoid send <target> <message>     Send a message to a particle
```

## WIT Interface

Particles interact with the runtime through a WIT-defined process interface:

```wit
interface process {
    // Identity
    self-pid: func() -> pid;
    spawn: func(component: string, name: option<string>, init-args: string) -> result<pid, spawn-error>;

    // Messaging
    send: func(target: borrow<pid>, msg: list<u8>) -> result<_, send-error>;
    recv: func(timeout-ms: option<u64>) -> option<message>;

    // Fault tolerance
    link: func(target: borrow<pid>) -> result<_, link-error>;
    monitor: func(target: borrow<pid>) -> u64;
    trap-exit: func(enabled: bool);

    // Logging
    log: func(level: log-level, message: string);
}
```

## Project Structure

```
├── Cargo.toml                 # Workspace root
├── src/                       # Runtime
│   ├── main.rs                # CLI
│   ├── runtime/               # WASM engine, actor lifecycle
│   ├── host/                  # Host functions (logging, database)
│   ├── registry.rs            # Process registry
│   ├── mailbox.rs             # Per-process message queue
│   ├── pid.rs                 # Process identifiers
│   └── client.rs              # Remote node client
├── crates/
│   ├── plasmoid-sdk/          # Component authoring SDK
│   └── plasmoid-macros/       # Proc macros (#[main], #[gen_server])
├── components/
│   ├── echo/                  # Echo example (GenServer)
│   └── ring/                  # Ring benchmark
└── wit/                       # WIT interface definitions
```

## Dependencies

| Component | Crate | Purpose |
|---|---|---|
| Networking | `iroh` 0.96 | QUIC mesh with mDNS discovery |
| WASM Runtime | `wasmtime` 41 | Component model execution |
| Authorization | `cedar-policy` 4 | Capability-based access control |
| Serialization | `postcard` 1 | Binary message encoding |
| Async | `tokio` 1 | Async runtime |

## License

MIT
