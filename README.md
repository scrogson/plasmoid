# Plasmoid

A distributed particle runtime for WebAssembly components, built on [iroh](https://iroh.computer).

A **particle** is a running WASM component instance with its own mailbox, an unforgeable identity, and the ability to link to and monitor other particles — an Erlang-style concurrency model in a sandbox, with [WIT](https://component-model.bytecodealliance.org/design/wit.html) as the host/guest contract.

> Particles message, spawn, link and monitor across nodes, and a lost node fires every relationship crossing to it. See [ROADMAP.md](./ROADMAP.md) for what's built and what isn't.

See [CONTEXT.md](./CONTEXT.md) for the project's vocabulary.

## Quick Start

```bash
# Install the runtime
cargo install --path .

# Create a new application
plasmoid new my-app
cd my-app

# Create a component
plasmoid component new greeter

# Build it (components are workspace members)
cargo component build -p greeter --release

# Boot a node with the component loaded and one particle spawned
plasmoid start target/wasm32-wasip1/release/greeter.wasm --spawn greeter --name greeter
```

## Architecture

```
┌─────────────────────────────────────────────┐
│  Plasmoid Node                              │
│                                             │
│  ┌───────────────┐  ┌─────────────────────┐ │
│  │ iroh Endpoint │  │ Particle Registry   │ │
│  │ (QUIC mesh)   │  │ pid  -> mailbox     │ │
│  └───────────────┘  │ name -> pid         │ │
│                     └─────────────────────┘ │
│  ┌─────────┐  ┌─────────┐  ┌─────────┐      │
│  │ WASM    │  │ WASM    │  │ WASM    │      │
│  │ Particle│  │ Particle│  │ Particle│      │
│  └─────────┘  └─────────┘  └─────────┘      │
│                                             │
│  ┌─────────────────────────────────────┐    │
│  │ Host Functions                      │    │
│  │ spawn, send, recv, link, monitor,   │    │
│  │ exit-signal, trap-exit, register,   │    │
│  │ lookup, global-register, log        │    │
│  └─────────────────────────────────────┘    │
└─────────────────────────────────────────────┘
```

Each particle has an unbounded mailbox and receives messages sequentially — it never handles two at once, so its linear memory needs no locking. Particles can spawn other particles, and `link` / `monitor` / `trap-exit` / `exit-signal` provide the primitives OTP-style supervision is built from — across nodes as well as within one. A supervisor can stop a child gracefully or, when it will not go, kill it: a `kill` sent as a signal cannot be trapped.

Nodes form a peer-to-peer QUIC mesh with cryptographic identity, discovered over mDNS on the local network with relay fallback. Introducing a node to one cluster member is enough — the mesh is transitive, so it learns the rest and they learn it.

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

Runtime calls like `spawn` are not exported by the prelude — `#[plasmoid_sdk::main]` generates `mod bindings` and glob-imports the WIT interface, so every host function is in scope inside an annotated module.

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

Spawns N worker particles in a ring and passes M messages around it:

```bash
plasmoid start ring.wasm --spawn ring --init '{"orchestrator":[100,1000]}'
```

## CLI

```
plasmoid new <app-name>              Create a new application workspace
plasmoid component new <name>        Create a new component
plasmoid start [options] [<wasm>...] Boot a node and load components
    --peer <node-id>                 Join the cluster that node belongs to
    --app <manifest.toml>            Start an application: spawn its root and
                                     exit the node when that root dies
plasmoid spawn <component>           Spawn a particle on a running node
plasmoid send <target> <message>     Send a message to a particle
```

`spawn` and `send` accept a node id, so a running node can be driven from outside — `plasmoid send <node-id> <name> <message>`.

## WIT Interface

Particles import the WIT interface `host` — the runtime API surface. Abridged; see [`wit/world.wit`](./wit/world.wit) for the full contract:

```wit
interface host {
    resource pid { to-string: func() -> string; }

    // Identity
    self-pid:  func() -> pid;
    self-name: func() -> option<string>;
    make-ref:  func() -> u64;

    // Identity of nodes
    type node-id = string;                 // full hex
    self-node: func() -> node-id;
    node-of:   func(p: borrow<pid>) -> node-id;

    // Anything a message can be addressed to
    variant destination {
        pid(borrow<pid>),
        local-named(string),
        named(tuple<node-id, string>),     // resolved on the receiving node
    }

    // Lifecycle
    spawn:    func(component: string, name: option<string>, init-args: string)
        -> result<pid, spawn-error>;
    spawn-on: func(node: node-id, component: string, name: option<string>, init-args: string)
        -> result<pid, spawn-error>;       // waits for the target's reply
    spawn-request: func(node: node-id, component: string, name: option<string>, init-args: string)
        -> u64;                            // returns immediately; reply is a message

    // Terminate yourself, unconditionally.
    exit:  func(reason: exit-reason);
    // Signal another particle, anywhere. `kill` sent this way is untrappable;
    // `killed` inherited through a link is not. Reports nothing, like `send`.
    exit-signal: func(dest: destination, reason: exit-reason);

    // Messaging
    // fire-and-forget, routed to wherever the destination lives
    send:     func(dest: destination, msg: list<u8>);
    send-ref: func(dest: destination, ref: u64, msg: list<u8>);
    recv:     func(timeout-ms: option<u64>) -> option<message>;
    recv-ref: func(ref: u64, timeout-ms: option<u64>) -> option<message>;

    // Naming, node-scoped
    register:   func(name: string) -> result<_, registry-error>;
    unregister: func(name: string) -> result<_, registry-error>;
    lookup:     func(name: string) -> option<pid>;

    // Naming, cluster-wide: exactly one particle holds the name
    global-register:   func(name: string) -> result<_, claim-error>;   // blocks
    global-unregister: func(name: string);
    global-lookup:     func(name: string) -> result<option<pid>, lookup-error>;
    resolve:    func(pid-string: string) -> option<pid>;

    // Failure
    link:      func(dest: destination);          // infallible; noproc arrives as a signal
    unlink:    func(dest: destination);
    monitor:   func(dest: destination) -> u64;   // always a valid ref
    demonitor: func(ref: u64);
    trap-exit: func(enabled: bool);

    // Logging
    log: func(level: log-level, message: string);
}
```

## Supervision

A supervisor is an ordinary particle, as in OTP — the runtime does not know what a supervision tree is.

```rust
plasmoid_sdk::run_supervisor!(
    SupFlags { strategy: Strategy::OneForOne, intensity: 1, period_ms: 5_000 },
    vec![
        ChildSpec::new("worker", "worker").restart(Restart::Permanent),
        ChildSpec::new("job", "batch").restart(Restart::Transient),
    ]
);
```

`one_for_one` / `one_for_all` / `rest_for_one`; `permanent` / `transient` / `temporary`; `brutal-kill` / a timeout / `infinity` for shutdown. Children start left to right and stop in reverse. Exceed the restart intensity and the supervisor terminates its children and exits, rather than spinning.

An **application** is declared by a manifest:

```toml
name = "my-app"
root = "app"          # a loaded component
type = "permanent"    # permanent | transient | temporary
```

```bash
plasmoid start app.wasm --app plasmoid.toml
```

When a `permanent` root exits, so does the node — non-zero unless it exited `normal`. The runtime cannot see a supervision tree, so it cannot report that one collapsed; a dead process is something systemd, Kubernetes and Docker already understand. A node is therefore only as resilient as whatever restarts it.

See [`components/supervised`](./components/supervised) for a worked example.

## Project Structure

```
├── Cargo.toml                 # Workspace root
├── src/                       # Runtime
│   ├── main.rs                # CLI
│   ├── runtime/               # WASM engine, particle lifecycle, invocation
│   ├── host/                  # Host function implementations
│   ├── registry.rs            # Particle registry (pids, names, links, monitors)
│   ├── cluster.rs             # Cluster membership (the connected set)
│   ├── transport.rs           # Ordered peer links
│   ├── signals.rs             # Cross-node exit and down propagation
│   ├── mailbox.rs             # Per-particle message queue
│   ├── pid.rs                 # Particle identifiers
│   ├── protocol.rs            # ALPN handler for remote spawn/send
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
| Networking | `iroh` 1.0 | QUIC mesh with mDNS discovery (`iroh-mdns-address-lookup`) |
| WASM Runtime | `wasmtime` 41 | Component model execution |
| Serialization | `postcard` 1 | Binary message encoding |
| Async | `tokio` 1 | Async runtime |

## License

MIT — see [LICENSE](./LICENSE).
