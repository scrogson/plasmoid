# WASM Actor System on Iroh

A distributed actor runtime where actors are WebAssembly components running on iroh mesh nodes, with WIT as the interface contract and Cedar for capability-based authorization.

---

## Core Architecture

The system is built on four pillars:

- **Iroh** — provides the networking layer (QUIC-based mesh with peer discovery, NAT traversal, relay fallback, and connection migration)
- **WebAssembly Component Model** — provides sandboxed actor execution with the WIT IDL defining actor interfaces
- **Cedar** — provides capability-based authorization, governing what each actor is allowed to do
- **WIT** — serves as the single source of truth for actor interfaces, replacing protobuf in this context

### Why These Fit Together

The actor model requires: addressable entities, message passing, sequential processing, and the ability to spawn new actors. Mapping onto iroh + WASM:

| Actor Concept | Implementation |
|---|---|
| Actor address | `NodeId` (iroh public key) + ALPN (protocol identifier) |
| Message passing | QUIC bidirectional streams |
| Isolation | WASM sandbox — actors cannot corrupt each other or the runtime |
| Mailbox | QUIC incoming stream queue |
| Supervision | Hub nodes monitor and restart actor instances |
| Capabilities | Cedar policies + WASM host function linking |

---

## The Runtime ("The VM")

A single iroh endpoint hosts many actors, analogous to the BEAM VM hosting many Erlang processes. The accept loop routes incoming QUIC connections to the appropriate WASM actor based on ALPN.

```
┌───────────────────────────────────────────────────────┐
│  Iroh Node (ActorRuntime)                             │
│                                                       │
│  ┌──────────────┐  ┌──────────────────────────────┐   │
│  │ QUIC Accept   │  │  Actor Registry              │   │
│  │ Loop          │──│  ALPN → WASM Component       │   │
│  └──────────────┘  └──────────────────────────────┘   │
│                                                       │
│  ┌───────────┐  ┌───────────┐  ┌───────────┐         │
│  │ WASM      │  │ WASM      │  │ WASM      │         │
│  │ Actor A   │  │ Actor B   │  │ Actor C   │         │
│  │           │  │           │  │           │         │
│  │ Imports:  │  │ Imports:  │  │ Imports:  │         │
│  │ - log     │  │ - log     │  │ - log     │         │
│  │ - ask     │  │ - ask     │  │ - ask     │         │
│  │           │  │ - db      │  │ - db      │         │
│  └───────────┘  └───────────┘  └───────────┘         │
│                                                       │
│  ┌───────────────────────────────────────────────┐    │
│  │  Host Functions (Capabilities)                │    │
│  │  Gated per-actor by Cedar policies            │    │
│  │  - logging, actor refs, DB, HTTP, streams     │    │
│  └───────────────────────────────────────────────┘    │
└───────────────────────────────────────────────────────┘
```

### Runtime Core

```rust
use iroh::{Endpoint, SecretKey, NodeId};
use wasmtime::component::*;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::RwLock;

pub struct ActorRuntime {
    endpoint: Endpoint,
    engine: wasmtime::Engine,
    actors: Arc<RwLock<HashMap<Vec<u8>, WasmActor>>>,
}

struct WasmActor {
    component: Component,
    linker: Linker<HostState>,
    capabilities: CedarPolicySet,
}

struct HostState {
    node_id: NodeId,
    endpoint: Endpoint,
    capabilities: CedarPolicySet,
}

impl ActorRuntime {
    pub async fn new(secret_key: SecretKey) -> anyhow::Result<Self> {
        let endpoint = Endpoint::builder()
            .secret_key(secret_key)
            .discovery_n0()
            .bind()
            .await?;

        let mut config = wasmtime::Config::new();
        config.wasm_component_model(true);
        config.async_support(true);
        let engine = wasmtime::Engine::new(&config)?;

        Ok(Self {
            endpoint,
            engine,
            actors: Arc::new(RwLock::new(HashMap::new())),
        })
    }

    /// The main accept loop — routes QUIC connections by ALPN
    pub async fn run(&self) -> anyhow::Result<()> {
        while let Some(incoming) = self.endpoint.accept().await {
            let actors = self.actors.clone();
            let endpoint = self.endpoint.clone();

            tokio::spawn(async move {
                let alpn = incoming.alpn().await?;
                let conn = incoming.await?;
                let remote = conn.remote_node_id();

                let actors = actors.read().await;
                let actor = actors.get(&alpn)
                    .ok_or_else(|| anyhow::anyhow!("no actor for ALPN {:?}", alpn))?;

                // Each bidirectional stream is one request/response
                loop {
                    let (send, recv) = conn.accept_bi().await?;
                    let request_bytes = read_message(recv).await?;
                    let response_bytes = actor.handle(&endpoint, remote, &request_bytes).await?;
                    write_message(send, &response_bytes).await?;
                }
            });
        }
        Ok(())
    }
}
```

### Deploying Actors

```rust
impl ActorRuntime {
    pub async fn deploy(
        &self,
        alpn: Vec<u8>,
        wasm_bytes: &[u8],
        capabilities: CedarPolicySet,
    ) -> anyhow::Result<()> {
        let component = Component::from_binary(&self.engine, wasm_bytes)?;
        let mut linker = Linker::<HostState>::new(&self.engine);

        // Link only the host functions this actor is authorized to use
        if capabilities.allows("logging") {
            host_functions::add_logging(&mut linker)?;
        }
        if capabilities.allows("actor:ask") {
            host_functions::add_actor_ask(&mut linker)?;
        }
        if capabilities.allows("actor:tell") {
            host_functions::add_actor_tell(&mut linker)?;
        }
        if capabilities.allows("db:read") {
            host_functions::add_db_read(&mut linker)?;
        }
        if capabilities.allows("db:write") {
            host_functions::add_db_write(&mut linker)?;
        }

        self.actors.write().await.insert(alpn, WasmActor {
            component,
            linker,
            capabilities,
        });

        Ok(())
    }

    /// Hot-swap an actor with a new WASM module — no restart
    pub async fn hot_deploy(
        &self,
        alpn: Vec<u8>,
        new_wasm: &[u8],
        capabilities: CedarPolicySet,
    ) -> anyhow::Result<()> {
        // Compile and validate the new component
        let new_component = Component::from_binary(&self.engine, new_wasm)?;
        // TODO: validate it exports the expected interface

        // Atomic swap — new requests use new version,
        // in-flight requests on old version complete naturally
        self.deploy(alpn, new_wasm, capabilities).await
    }
}
```

---

## WIT as the Actor Interface

WIT (WebAssembly Interface Type) definitions serve as the single source of truth for actor contracts. WIT provides richer types than protobuf: proper result types, algebraic variants, option types, and resource handles.

### Example: Order Service Actor

```wit
package myapp:orders@1.0.0;

interface types {
    variant order-status {
        pending,
        confirmed(confirmation-details),
        shipped(tracking-info),
        delivered(delivery-proof),
        cancelled(cancellation-reason),
    }

    record confirmation-details {
        confirmed-at: u64,
        estimated-delivery: u64,
    }

    record tracking-info {
        carrier: string,
        tracking-number: string,
    }

    record delivery-proof {
        delivered-at: u64,
        signed-by: option<string>,
    }

    record cancellation-reason {
        reason: string,
        cancelled-at: u64,
        refund-issued: bool,
    }

    record line-item {
        sku: string,
        quantity: u32,
        unit-price: f64,
    }

    record order-request {
        customer-id: string,
        items: list<line-item>,
    }

    type order-id = string;

    record sku-shortage {
        sku: string,
        requested: u32,
        available: u32,
    }

    variant payment-error {
        declined(string),
        insufficient-funds,
        expired-card,
    }

    variant order-error {
        not-found(string),
        insufficient-stock(list<sku-shortage>),
        payment-failed(payment-error),
        unauthorized,
    }
}

interface orders {
    use types.{order-request, order-id, order-status, order-error};

    place-order: func(req: order-request) -> result<order-id, order-error>;
    get-status: func(id: order-id) -> result<order-status, order-error>;
    cancel-order: func(id: order-id, reason: string) -> result<_, order-error>;
}

/// The world defines the full contract:
/// what the actor exports + what the runtime provides
world order-actor {
    // Actor implements this
    export orders;

    // Runtime provides these (capability-gated)
    import myapp:runtime/logging;
    import myapp:runtime/actor-context;
    import myapp:runtime/database;
}
```

### Host-Provided Interfaces

```wit
package myapp:runtime@1.0.0;

interface logging {
    enum level { trace, debug, info, warn, error }
    log: func(level: level, message: string);
}

interface actor-context {
    /// Send a request to another actor and await response
    ask: func(alpn: string, node-id: option<string>, request: list<u8>) -> result<list<u8>, string>;

    /// Send a message to another actor with no response
    tell: func(alpn: string, node-id: option<string>, message: list<u8>) -> result<_, string>;

    /// Get this node's identity
    self-node-id: func() -> string;

    /// Get the caller's identity
    remote-node-id: func() -> string;
}

interface database {
    /// Simple key-value for actor state persistence
    get: func(key: string) -> option<list<u8>>;
    set: func(key: string, value: list<u8>) -> result<_, string>;
    delete: func(key: string) -> result<bool, string>;
    list-keys: func(prefix: string) -> list<string>;
}
```

---

## Implementing an Actor (Rust → WASM)

Actors are written in any language that compiles to WASM components. Here's a Rust example:

```rust
// Cargo.toml
// [lib]
// crate-type = ["cdylib"]
//
// [dependencies]
// wit-bindgen = "0.36"

wit_bindgen::generate!({
    world: "order-actor",
});

struct OrderActor;

impl exports::myapp::orders::orders::Guest for OrderActor {
    fn place_order(req: OrderRequest) -> Result<OrderId, OrderError> {
        // Use imported host functions
        logging::log(logging::Level::Info,
            &format!("Placing order for customer {}", req.customer_id));

        // Validate stock via another actor
        for item in &req.items {
            let stock_check = actor_context::ask(
                "/myapp.inventory/1",
                None, // local node
                &serialize_stock_check(item),
            ).map_err(|_| OrderError::Unauthorized)?;

            if !has_sufficient_stock(&stock_check, item) {
                return Err(OrderError::InsufficientStock(vec![
                    SkuShortage {
                        sku: item.sku.clone(),
                        requested: item.quantity,
                        available: parse_available(&stock_check),
                    }
                ]));
            }
        }

        // Persist via host-provided database
        let order_id = generate_order_id();
        let order_data = serialize_order(&req);
        database::set(&format!("order:{}", order_id), &order_data)
            .map_err(|_| OrderError::Unauthorized)?;

        Ok(order_id)
    }

    fn get_status(id: OrderId) -> Result<OrderStatus, OrderError> {
        let data = database::get(&format!("order:{}", id))
            .ok_or(OrderError::NotFound(id.clone()))?;
        Ok(deserialize_status(&data))
    }

    fn cancel_order(id: OrderId, reason: String) -> Result<(), OrderError> {
        let mut order = database::get(&format!("order:{}", id))
            .ok_or(OrderError::NotFound(id.clone()))?;

        // Notify downstream via another actor
        actor_context::tell(
            "/myapp.notifications/1",
            None,
            &serialize_cancellation_event(&id, &reason),
        ).ok(); // fire-and-forget

        database::set(&format!("order:{}", id), &serialize_cancelled(&reason))
            .map_err(|_| OrderError::Unauthorized)?;

        Ok(())
    }
}

export!(OrderActor);
```

Build with:

```bash
cargo component build --release
# produces target/wasm32-wasip2/release/order_actor.wasm
```

---

## Host Functions as Capabilities

The runtime links host functions into the WASM actor at instantiation time. Cedar policies determine which functions get linked — if an actor isn't authorized for database access, the `database` import simply isn't linked, and the component fails to instantiate if it requires it.

```rust
mod host_functions {
    use wasmtime::component::*;

    pub fn add_actor_ask(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
        linker.func_wrap(
            "myapp:runtime/actor-context", "ask",
            |caller: Caller<'_, HostState>,
             alpn: String,
             node_id: Option<String>,
             request: Vec<u8>| -> Result<Vec<u8>, String> {

                let state = caller.data();

                // Cedar authorization check
                if !state.capabilities.is_authorized(
                    &state.node_id.to_string(),
                    &format!("action::call::{}", alpn),
                ) {
                    return Err("unauthorized: cannot call this actor".into());
                }

                let target = node_id
                    .map(|id| id.parse::<NodeId>())
                    .transpose()
                    .map_err(|e| e.to_string())?
                    .unwrap_or(state.node_id);

                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    let conn = state.endpoint
                        .connect(target, alpn.as_bytes())
                        .await
                        .map_err(|e| e.to_string())?;
                    let (send, recv) = conn.open_bi().await
                        .map_err(|e| e.to_string())?;
                    write_message(send, &request).await.map_err(|e| e.to_string())?;
                    read_message(recv).await.map_err(|e| e.to_string())
                })
            }
        )?;
        Ok(())
    }

    pub fn add_logging(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
        linker.func_wrap(
            "myapp:runtime/logging", "log",
            |caller: Caller<'_, HostState>, level: u32, message: String| {
                let node = caller.data().node_id;
                match level {
                    0 => tracing::trace!(actor_node = %node, "{}", message),
                    1 => tracing::debug!(actor_node = %node, "{}", message),
                    2 => tracing::info!(actor_node = %node, "{}", message),
                    3 => tracing::warn!(actor_node = %node, "{}", message),
                    _ => tracing::error!(actor_node = %node, "{}", message),
                }
            }
        )?;
        Ok(())
    }

    pub fn add_db_read(linker: &mut Linker<HostState>) -> anyhow::Result<()> {
        linker.func_wrap(
            "myapp:runtime/database", "get",
            |caller: Caller<'_, HostState>, key: String| -> Option<Vec<u8>> {
                let rt = tokio::runtime::Handle::current();
                rt.block_on(async {
                    // Actor-scoped key prefix for isolation
                    let scoped_key = format!("{}:{}", caller.data().actor_id, key);
                    state.db.get(&scoped_key).await.ok().flatten()
                })
            }
        )?;
        Ok(())
    }
}
```

---

## Distributed Registry

When multiple iroh nodes run actor runtimes, they form a mesh. Each node announces which actors it hosts via iroh's gossip protocol. The registry maps ALPNs to NodeIds.

```
Runtime A (NodeId: abc123)          Runtime B (NodeId: def456)
├── /myapp.orders/1                 ├── /myapp.inventory/1
├── /myapp.notifications/1          ├── /myapp.payments/1
└── /myapp.audit/1                  └── /myapp.shipping/1

            ↕ iroh gossip / discovery ↕

        Distributed Registry:
        /myapp.orders/1         → [abc123]
        /myapp.notifications/1  → [abc123]
        /myapp.audit/1          → [abc123]
        /myapp.inventory/1      → [def456]
        /myapp.payments/1       → [def456]
        /myapp.shipping/1       → [def456]
```

If multiple nodes host the same actor type, the registry returns all NodeIds and the caller (or runtime) can load-balance.

### Registry Protocol

```wit
package myapp:registry@1.0.0;

interface registry {
    record actor-registration {
        alpn: string,
        node-id: string,
        version: string,
        metadata: list<tuple<string, string>>,
    }

    /// Announce actors hosted on this node
    register: func(actors: list<actor-registration>) -> result<_, string>;

    /// Remove actors (during shutdown/drain)
    deregister: func(alpns: list<string>) -> result<_, string>;

    /// Look up which nodes host a given actor
    resolve: func(alpn: string) -> list<actor-registration>;

    /// Subscribe to registry changes
    watch: func(alpn-prefix: string) -> result<_, string>;
}
```

---

## Supervision and the Hub

Hub nodes act as supervisors in the actor system. They monitor extension actors, restart them on failure, and manage deployments.

```
                    ┌──────────────────┐
                    │   Hub Node       │
                    │   (Supervisor)   │
                    │                  │
                    │ - Monitors health│
                    │ - Deploys WASM   │
                    │ - Manages Cedar  │
                    │   policies       │
                    └────────┬─────────┘
                             │
                ┌────────────┼────────────┐
                │            │            │
          ┌─────┴──────┐ ┌──┴───────┐ ┌──┴───────┐
          │ Runtime A   │ │Runtime B │ │Runtime C │
          │ orders      │ │inventory │ │payments  │
          │ audit       │ │shipping  │ │notifs    │
          └────────────┘ └──────────┘ └──────────┘
```

### Supervision Strategy

```wit
package myapp:supervisor@1.0.0;

interface supervisor {
    variant restart-strategy {
        /// Restart just the failed actor
        one-for-one,
        /// Restart all actors on the same runtime
        one-for-all,
        /// Restart the failed actor and all actors spawned after it
        rest-for-one,
    }

    record actor-spec {
        alpn: string,
        wasm-hash: string,          // content hash of the WASM module
        capabilities: list<string>, // Cedar policy references
        restart: restart-strategy,
        max-restarts: u32,
        max-restart-window-secs: u64,
    }

    /// Deploy an actor to a runtime node
    deploy: func(node-id: string, spec: actor-spec, wasm: list<u8>) -> result<_, string>;

    /// Drain an actor — stop accepting new requests, finish in-flight
    drain: func(node-id: string, alpn: string) -> result<_, string>;

    /// Get health status of all managed actors
    status: func() -> list<actor-health>;

    record actor-health {
        alpn: string,
        node-id: string,
        status: health-status,
        uptime-secs: u64,
        requests-handled: u64,
        last-error: option<string>,
    }

    variant health-status {
        healthy,
        degraded(string),
        unhealthy(string),
        restarting,
        stopped,
    }
}
```

---

## Cedar Authorization

Cedar policies govern which actors can call which, and which host capabilities each actor receives.

### Example Policies

```cedar
// Orders actor can call inventory and notifications
permit(
    principal == Actor::"order-actor",
    action == Action::"call",
    resource == Actor::"inventory-actor"
);

permit(
    principal == Actor::"order-actor",
    action == Action::"call",
    resource == Actor::"notification-actor"
);

// Orders actor can read and write to database
permit(
    principal == Actor::"order-actor",
    action in [Action::"db:read", Action::"db:write"],
    resource == Resource::"database"
);

// Inventory actor can only read from database
permit(
    principal == Actor::"inventory-actor",
    action == Action::"db:read",
    resource == Resource::"database"
);

forbid(
    principal == Actor::"inventory-actor",
    action == Action::"db:write",
    resource == Resource::"database"
);

// Only the hub supervisor can deploy actors
permit(
    principal == Actor::"hub-supervisor",
    action in [Action::"deploy", Action::"drain", Action::"hot-deploy"],
    resource in Namespace::"myapp"
);
```

---

## Wire Format: Network Serialization

WIT defines the interface contract. For messages crossing the QUIC transport between nodes, the runtime serializes using an efficient binary format. The actor never sees this — it works purely with WIT types.

```
Actor A (WASM)                              Actor B (WASM)
     │                                           ▲
     │ WIT types via canonical ABI               │ WIT types via canonical ABI
     ▼                                           │
┌──────────┐                               ┌──────────┐
│ Runtime A │─── QUIC stream ─────────────▶│ Runtime B │
│ serialize │   (msgpack / cbor / custom)  │deserialize│
└──────────┘                               └──────────┘
```

Recommended wire format options (in order of preference):

1. **MessagePack** — compact binary, schema-less, fast, good Rust support via `rmp-serde`
2. **CBOR** — similar to MessagePack, IETF standard (RFC 8949)
3. **Cap'n Proto** — zero-copy, excellent for high-throughput, but heavier dependency
4. **Protobuf** — if existing systems already use it for interop

The runtime handles the mapping: WIT canonical ABI ↔ wire format. This is an internal detail that can be changed without affecting actors.

---

## Code Generation Pipeline

WIT is the single source of truth. Code generation produces everything else:

```
                    ┌──────────────────┐
                    │  WIT             │
                    │  worlds +        │  ← single source of truth
                    │  interfaces      │
                    └────────┬─────────┘
                             │
            ┌────────────────┼────────────────┐
            │                │                │
       ┌────┴─────┐   ┌─────┴──────┐  ┌──────┴──────┐
       │ WASM     │   │ Non-WASM   │  │ Cedar       │
       │ Bindgen  │   │ Client Gen │  │ Schema Gen  │
       │          │   │            │  │             │
       │ - Rust   │   │ - Elixir   │  │ Actions     │
       │ - Go     │   │   gRPC     │  │ derived     │
       │ - TS     │   │   client   │  │ from WIT    │
       │ - Python │   │ - GraphQL  │  │ function    │
       │          │   │   schema   │  │ names       │
       └──────────┘   └────────────┘  └─────────────┘
```

For Elixir services that need to call into the actor system (but aren't WASM components), generate a thin client that speaks the wire format over QUIC.

---

## Hot Deployment Flow

WASM actors enable deployment without process restarts:

1. Build new WASM component: `cargo component build --release`
2. Submit to hub supervisor (over iroh, as a message to the supervisor actor)
3. Hub validates: signature check, WIT interface compatibility, Cedar policy compliance
4. Hub calls `hot_deploy` on the target runtime node
5. New requests route to the new version; in-flight requests complete on the old version
6. Old version is dropped after drain completes

No container builds. No Kubernetes rollouts. No connection migration. The runtime stays up, QUIC connections stay alive, only the WASM module swaps.

---

## Key Design Decisions

### Async Handling

The WASM component model's native async support is still in progress. Current workaround: host functions that perform async work (network calls, database access) bridge to the Tokio runtime via `tokio::runtime::Handle::current().block_on()`. This works but means an actor blocks its thread during async host calls. Mitigate by running actor invocations on a thread pool. As component model async stabilizes, this can be replaced with native async support.

### Actor State

Three tiers of state management:

1. **Stateless actors** — no state between requests. Simplest. Any instance handles any request. Scale horizontally by deploying the same WASM module on multiple nodes.

2. **Runtime-scoped state** — host-provided key-value store (the `database` interface). State persists across requests but is tied to the runtime node. Good for caches, session data.

3. **Event-sourced state** — actors emit events to a durable log. On restart/migration, replay the log to rebuild state. Most resilient. Pairs well with the registry — if an actor moves to a new node, it replays its event log and resumes.

### Local vs. Remote Transparency

Calling a local actor (same runtime) and a remote actor (different node) uses the identical `ask`/`tell` API. Both go through QUIC — even local calls are encrypted and framed. The latency tradeoff (hundreds of microseconds vs Erlang's single-digit microseconds for local calls) is accepted in exchange for zero behavioral differences between local and remote.

### Identity and Security

Every iroh node has a cryptographic identity (`NodeId` = public key). This means:

- Actor-to-actor calls are always authenticated (QUIC handshake verifies both sides)
- Cedar policies reference `NodeId` for authorization decisions
- No central authority needed for identity — it's inherent in the key pair
- WASM sandbox prevents actors from accessing the node's private key

---

## Dependencies

| Component | Crate / Tool | Purpose |
|---|---|---|
| Networking | `iroh` | QUIC mesh, discovery, relay |
| WASM Runtime | `wasmtime` | Component model execution |
| WIT Bindgen | `wit-bindgen` | Generate Rust bindings from WIT |
| Component Build | `cargo-component` | Build WASM components from Rust |
| Authorization | `cedar-policy` | Capability-based policy evaluation |
| Serialization | `rmp-serde` (MessagePack) | Wire format for cross-node messages |
| Async Runtime | `tokio` | Async I/O for the host runtime |
| Tracing | `tracing` | Structured logging |

---

## Project Structure

```
actor-system/
├── Cargo.toml                    # workspace root
├── wit/                          # WIT definitions (source of truth)
│   ├── runtime.wit               # host-provided interfaces
│   ├── registry.wit              # distributed registry protocol
│   ├── supervisor.wit            # supervision protocol
│   └── actors/
│       ├── orders.wit
│       ├── inventory.wit
│       └── notifications.wit
├── crates/
│   ├── runtime/                  # the actor runtime ("VM")
│   │   ├── src/
│   │   │   ├── lib.rs
│   │   │   ├── accept_loop.rs    # QUIC accept + ALPN routing
│   │   │   ├── host_functions.rs # WASM host function implementations
│   │   │   ├── registry.rs       # distributed actor registry
│   │   │   ├── supervisor.rs     # supervision and health monitoring
│   │   │   └── deploy.rs         # WASM deployment and hot-swap
│   │   └── Cargo.toml
│   ├── cedar-policies/           # authorization policy definitions
│   │   ├── policies/
│   │   └── schema.cedarschema
│   └── wire/                     # wire format serialization helpers
│       └── src/lib.rs
├── actors/                       # WASM actor implementations
│   ├── orders/
│   │   ├── src/lib.rs
│   │   └── Cargo.toml            # uses cargo-component
│   ├── inventory/
│   └── notifications/
└── tools/
    └── wit-codegen/              # code generation from WIT
        ├── elixir-client/        # generate Elixir client code
        └── cedar-schema/         # generate Cedar schema from WIT
```

---

## Getting Started

### 1. Define an actor interface in WIT

Create `wit/actors/greeter.wit` with the actor's world and interfaces.

### 2. Implement the actor in Rust

Use `wit-bindgen` and `cargo-component` to build a WASM component.

### 3. Set up the runtime

Initialize an `ActorRuntime` with an iroh endpoint and deploy the compiled WASM module with appropriate Cedar capabilities.

### 4. Mesh multiple runtimes

Start multiple runtime nodes. They discover each other via iroh's discovery system and synchronize their actor registries via gossip.

### 5. Call actors

Use `ActorRef` (typed reference with ALPN + optional NodeId) to send messages. The runtime resolves, routes, serializes, and deserializes transparently.
