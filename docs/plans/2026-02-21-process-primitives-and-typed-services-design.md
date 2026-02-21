# Process Primitives and Typed Service Layer Design

## Goal

Rearchitect the component model from first principles, drawing on Erlang/OTP semantics:

1. Replace `init`/`handle` with `start`/`recv` — components own their control flow
2. Unify references (monitor refs, call correlation) into a single `make-ref` primitive
3. Add `recv-ref` for selective receive and `send-ref` for tagged messages
4. Use wasm-wave as the universal "terms" format (typed, self-describing, human-readable)
5. Build a gen-server-style typed service layer via codegen, not runtime awareness

The runtime stays dumb: load components, spawn processes, deliver messages, match refs. All typed dispatch lives in generated code.

## Motivation

### The current pain

Today every component manually packs and unpacks `list<u8>`. The echo component does length-prefixed PID strings. The ring benchmark uses role bytes, hop counters, and setup message parsing. Components manage state through `RefCell`/`thread_local!` hacks because `init` and `handle` are separate exports with no shared scope.

### The Erlang insight

In Erlang, a process is just a function that starts. It calls `receive` when it wants messages. State is local variables. `gen_server` is a convenience built on top — it's a process that implements a specific message protocol internally. The process doesn't know it's a gen_server. The VM doesn't know it's a gen_server. It's just a pattern.

Mapping to our system:

| Erlang | Plasmoid |
|--------|----------|
| Terms (atoms, tuples, lists) | wasm-wave values |
| Modules (namespaced code) | WASM components + WIT interfaces |
| Processes (spawned, have mailbox) | Particles (spawned from components) |
| Behaviours (gen_server, supervisor) | WIT worlds as contracts + codegen |
| `make_ref()` | `make-ref()` — unified ref type |
| `receive ... after T -> ...` | `recv(timeout)` / `recv-ref(ref, timeout)` |
| `Pid ! Msg` | `send(target, msg)` / `send-ref(target, ref, msg)` |

## Design

### Part 1: New base world — `particle`

Replace `actor-process` (init/handle) with `particle` (start/recv). The component owns its control flow.

```wit
package plasmoid:runtime@0.4.0;

interface process {
    resource pid {
        to-string: func() -> string;
    }

    // Identity
    self-pid: func() -> pid;
    self-name: func() -> option<string>;

    // References (unified: monitors, call correlation, general purpose)
    make-ref: func() -> u64;

    // Spawning & lifecycle
    spawn: func(component: string, name: option<string>, init-args: string) -> result<pid, spawn-error>;
    exit: func(reason: exit-reason);

    // Sending
    send: func(target: borrow<pid>, msg: list<u8>) -> result<_, send-error>;
    send-ref: func(target: borrow<pid>, ref: u64, msg: list<u8>) -> result<_, send-error>;

    // Receiving
    recv: func(timeout-ms: option<u64>) -> option<message>;
    recv-ref: func(ref: u64, timeout-ms: option<u64>) -> option<message>;

    // Resolution & registry
    resolve: func(pid-string: string) -> option<pid>;
    register: func(name: string) -> result<_, registry-error>;
    unregister: func(name: string) -> result<_, registry-error>;
    lookup: func(name: string) -> option<pid>;

    // Links & monitors
    link: func(target: borrow<pid>) -> result<_, link-error>;
    unlink: func(target: borrow<pid>);
    monitor: func(target: borrow<pid>) -> u64;
    demonitor: func(ref: u64);
    trap-exit: func(enabled: bool);

    // Logging
    log: func(level: log-level, message: string);

    // --- Types ---

    enum log-level { trace, debug, info, warn, error }

    variant exit-reason {
        normal,
        kill,
        shutdown(string),
        exception(string),
    }

    record exit-signal {
        sender: pid,
        reason: exit-reason,
    }

    record down-signal {
        sender: pid,
        ref: u64,
        reason: exit-reason,
    }

    record tagged-message {
        ref: u64,
        payload: list<u8>,
    }

    variant message {
        data(list<u8>),
        tagged(tagged-message),
        exit(exit-signal),
        down(down-signal),
    }

    enum spawn-error {
        component-not-found,
        init-failed,
        resource-limit,
    }

    enum send-error {
        no-process,
        mailbox-full,
    }

    enum registry-error {
        already-registered,
        not-registered,
    }

    enum link-error {
        no-process,
    }
}

world particle {
    import process;

    // Components define their own start signature.
    // The runtime calls start dynamically via Func::call + wasm-wave.
    //
    // Examples:
    //   export start: func(config: my-config) -> result<_, string>;
    //   export start: func(name: string) -> result<_, string>;
    //   export start: func() -> result<_, string>;
}
```

Key changes from v0.3.0:

- **`start` replaces `init` + `handle`**: The component exports a single `start` function with a component-defined signature. The runtime calls it dynamically using `Func::call()` with wasm-wave parsed values. The component calls `recv` when it wants messages.
- **`recv` / `recv-ref`**: Blocking receive with optional timeout. `recv-ref` does selective receive — scans the mailbox for messages matching a ref, leaving others in the queue.
- **`send-ref`**: Tags a message with a ref (mailbox metadata). The receiver sees it as `message::tagged { ref, payload }`.
- **`make-ref`**: Creates a unique reference. Same type used for monitor refs, call correlation, and general purpose.
- **`spawn` takes `string` (wasm-wave)**: The runtime parses the wasm-wave string against the target component's `start` parameter types. Validated before calling `start`.
- **`message` variant updated**: `data(list<u8>)` for untagged messages, `tagged(tagged-message)` for ref-tagged messages, plus `exit` and `down` as before. `down-signal` uses unified `ref: u64` instead of `monitor-ref`.

### Part 2: What components look like

**One-shot task** (no receive loop):

```rust
// A hasher component — compute and return, no message loop
fn start(data: Vec<u8>) -> Result<Vec<u8>, String> {
    Ok(sha256(&data))
}
```

**Long-running process** (explicit receive loop, state as local variables):

```rust
fn start(config: Config) -> Result<(), String> {
    let mut db = connect(&config.db_url).map_err(|e| e.to_string())?;
    let mut request_count = 0u64;

    loop {
        match process::recv(None).unwrap() {
            Message::Data(data) => {
                request_count += 1;
                handle_request(&mut db, &data);
            }
            Message::Tagged(msg) => {
                request_count += 1;
                let result = handle_call(&mut db, &msg.payload);
                // Reply using the same ref
                let caller_pid = extract_caller(&msg.payload);
                let _ = process::send_ref(&caller_pid, msg.ref, &result);
            }
            Message::Exit(_) => break,
            Message::Down(sig) => {
                process::log(LogLevel::Warn, &format!("peer {} died", sig.sender.to_string()));
            }
        }
    }
    Ok(())
}
```

**Two-phase protocol** (ring worker — sequential code, no state machine):

```rust
fn start(_init: Vec<u8>) -> Result<(), String> {
    // Phase 1: Wait for setup message
    let next_pid = loop {
        match process::recv(None).unwrap() {
            Message::Data(data) if data.starts_with(b"setup:") => {
                let pid_str = std::str::from_utf8(&data[6..]).unwrap();
                break process::resolve(pid_str).unwrap();
            }
            Message::Data(data) if data == b"stop" => return Ok(()),
            _ => continue,
        }
    };

    // Phase 2: Forward hops
    loop {
        match process::recv(None).unwrap() {
            Message::Data(data) if data == b"stop" => return Ok(()),
            Message::Data(data) => {
                let hops = u32::from_le_bytes(data[0..4].try_into().unwrap());
                if hops == 0 {
                    let master = process::resolve(std::str::from_utf8(&data[4..]).unwrap()).unwrap();
                    let _ = process::send(&master, b"finished");
                } else {
                    let mut fwd = (hops - 1).to_le_bytes().to_vec();
                    fwd.extend_from_slice(&data[4..]);
                    let _ = process::send(&next_pid, &fwd);
                }
            }
            _ => {}
        }
    }
}
```

No `RefCell`. No `thread_local!`. No `PendingWorker` state machine enum. The two-phase protocol is just sequential code.

### Part 3: Runtime changes

The runtime changes are mostly subtractive:

**Remove:**
- Host-side message loop that calls `handle` — the component owns this now via `recv`
- `wasmtime::component::bindgen!` for `actor-process` — no longer needed since `start` is called dynamically

**Add:**
- `recv` host function: async, awaits on the mailbox channel. Returns `None` on timeout or channel close.
- `recv-ref` host function: scans the mailbox queue for messages with matching ref. Non-matching messages stay in the queue.
- `send-ref` host function: sends a message with a ref attached as mailbox metadata.
- `make-ref` host function: returns a monotonically increasing u64 (per-process counter is sufficient, or global atomic).
- Dynamic `start` dispatch: use `Func::call()` with wasm-wave parsed `Val` arrays instead of bindgen-generated typed calls.

**Change:**
- Mailbox from mpsc channel to scannable queue (`VecDeque` or similar) to support `recv-ref` scanning.
- `spawn` parses the `init-args` string as wasm-wave against the target component's `start` parameter types before calling `start`.

The runtime does NOT know about: typed service interfaces, function name dispatch, gen-server protocol, wasm-wave encoding of domain types. It only knows: load components, spawn processes, deliver messages, match refs on u64.

### Part 4: Gen-server typed service layer (codegen)

The gen-server layer is built entirely through WIT interface definitions and generated code. The runtime is unaware of it.

#### Component author's workflow

1. Define a WIT interface with domain types and functions:

```wit
package myapp:orders@1.0.0;

interface orders {
    record order-request {
        customer-id: string,
        items: list<line-item>,
    }
    record line-item {
        sku: string,
        quantity: u32,
    }
    type order-id = string;
    enum order-error { not-found, unauthorized }

    place-order: func(req: order-request) -> result<order-id, order-error>;
    get-status: func(id: order-id) -> result<string, order-error>;
    cancel-order: func(id: order-id, reason: string) -> result<_, order-error>;
}
```

2. Run the codegen tool (e.g., `plasmoid gen-server orders`).

3. Implement the generated trait:

```rust
struct MyOrders {
    db: Database,
}

impl OrdersServer for MyOrders {
    fn init(config: OrdersConfig) -> Result<Self, String> {
        Ok(Self { db: Database::connect(&config.db_url)? })
    }

    fn place_order(&mut self, req: OrderRequest) -> Result<OrderId, OrderError> {
        let id = self.db.insert_order(&req)?;
        Ok(id)
    }

    fn get_status(&mut self, id: OrderId) -> Result<String, OrderError> {
        self.db.get_status(&id)?.ok_or(OrderError::NotFound)
    }

    fn cancel_order(&mut self, id: OrderId, reason: String) -> Result<(), OrderError> {
        self.db.cancel(&id, &reason)
    }
}
```

State is `&mut self` on a normal struct. No RefCell. No thread_local.

#### What the codegen produces

**Server shim** — a `start` export that:
1. Calls the user's `init` to create state
2. Loops on `recv`
3. For `tagged` messages: decodes the call envelope, dispatches to the matching function, encodes the result, sends the reply via `send-ref` with the same ref
4. For `data` messages: decodes the cast envelope, dispatches to the matching function, no reply
5. For `exit`/`down`: calls user-defined signal handlers if present

```rust
// Generated start function (simplified)
fn start(config: OrdersConfig) -> Result<(), String> {
    let mut server = MyOrders::init(config)?;

    loop {
        match process::recv(None) {
            Some(Message::Tagged(msg)) => {
                let envelope = CallEnvelope::decode(&msg.payload);
                let result = match envelope.function.as_str() {
                    "place-order" => {
                        let req: OrderRequest = wave::decode(&envelope.args);
                        wave::encode(&server.place_order(req))
                    }
                    "get-status" => {
                        let id: OrderId = wave::decode(&envelope.args);
                        wave::encode(&server.get_status(id))
                    }
                    "cancel-order" => {
                        let (id, reason): (OrderId, String) = wave::decode(&envelope.args);
                        wave::encode(&server.cancel_order(id, reason))
                    }
                    _ => wave::encode(&Err::<(), String>("unknown function".into())),
                };
                let caller = process::resolve(&envelope.caller_pid).unwrap();
                let _ = process::send_ref(&caller, msg.ref, &ReplyEnvelope::encode(&result));
            }
            Some(Message::Data(data)) => {
                let envelope = CastEnvelope::decode(&data);
                match envelope.function.as_str() {
                    // dispatch cast functions (fire-and-forget)...
                    _ => {}
                }
            }
            Some(Message::Exit(_)) => break,
            Some(Message::Down(sig)) => {
                // call user's handle_down if defined
            }
            None => break, // channel closed
        }
    }
    Ok(())
}
```

**Client stubs** — typed functions for calling the service from another component:

```rust
// Generated client module
pub mod orders_client {
    use super::*;

    /// Synchronous call — sends request, waits for typed reply.
    pub fn place_order(
        target: &Pid,
        req: &OrderRequest,
        timeout_ms: Option<u64>,
    ) -> Result<OrderId, OrderError> {
        let ref_id = process::make_ref();
        let envelope = CallEnvelope {
            function: "place-order".into(),
            caller_pid: process::self_pid().to_string(),
            args: wave::encode(req),
        };
        process::send_ref(target, ref_id, &envelope.encode()).unwrap();

        match process::recv_ref(ref_id, timeout_ms) {
            Some(Message::Tagged(reply)) => {
                wave::decode(&ReplyEnvelope::decode(&reply.payload).result)
            }
            _ => panic!("call timeout or unexpected message"),
        }
    }

    /// Fire-and-forget cast — sends request, no reply.
    pub fn cancel_order_cast(target: &Pid, id: &OrderId, reason: &str) {
        let envelope = CastEnvelope {
            function: "cancel-order".into(),
            args: wave::encode(&(id, reason)),
        };
        let _ = process::send(target, &envelope.encode());
    }
}
```

Usage from another component:

```rust
fn start(_args: Vec<u8>) -> Result<(), String> {
    let server = process::lookup("orders").unwrap();

    let order_id = orders_client::place_order(
        &server,
        &OrderRequest {
            customer_id: "cust-123".into(),
            items: vec![LineItem { sku: "WIDGET-1".into(), quantity: 2 }],
        },
        Some(5000), // 5s timeout
    )?;

    process::log(LogLevel::Info, &format!("placed order: {}", order_id));
    Ok(())
}
```

**Typed spawn stubs** — for spawning a gen-server with typed config:

```rust
pub mod orders_server {
    pub fn spawn(
        name: Option<&str>,
        config: &OrdersConfig,
    ) -> Result<Pid, SpawnError> {
        process::spawn("orders", name, &wave::encode(config))
    }
}
```

### Part 5: Message envelope format

The gen-server protocol uses a simple envelope encoded in `list<u8>`. This is internal to the generated code — the runtime never inspects it.

**Call envelope** (sent via `send-ref`):
```
[1 byte:  type = 0x01 (call)]
[4 bytes: caller PID string length (u32 LE)]
[N bytes: caller PID string (UTF-8)]
[4 bytes: function name length (u32 LE)]
[M bytes: function name (UTF-8)]
[remaining: wasm-wave encoded arguments (UTF-8 string bytes)]
```

**Cast envelope** (sent via `send`):
```
[1 byte:  type = 0x02 (cast)]
[4 bytes: function name length (u32 LE)]
[M bytes: function name (UTF-8)]
[remaining: wasm-wave encoded arguments (UTF-8 string bytes)]
```

**Reply envelope** (sent via `send-ref` with the same ref):
```
[1 byte:  type = 0x03 (reply)]
[remaining: wasm-wave encoded result (UTF-8 string bytes)]
```

The ref is NOT in the envelope — it travels as mailbox metadata via `send-ref`/`recv-ref`.

### Part 6: wasm-wave as the terms layer

wasm-wave serves as the universal interchange format:

- **CLI → runtime**: `plasmoid spawn orders --init '{ db-url: "postgres://...", max-connections: 32 }'`
- **spawn between components**: `process::spawn("orders", name, &wave::encode(&config))`
- **gen-server call args**: function arguments encoded as wasm-wave strings in the envelope
- **gen-server replies**: return values encoded as wasm-wave strings

wasm-wave is typed (validates against WIT types), human-readable (good for debugging and CLI), and self-describing. It plays the same role as Erlang terms in the BEAM.

For the raw process layer (`send`/`recv` with `list<u8>`), components can use any encoding they want — raw bytes, msgpack, wasm-wave, whatever suits their performance needs. The gen-server codegen standardizes on wasm-wave for its protocol.

### Part 7: Migration path

1. **Add `recv`, `recv-ref`, `send-ref`, `make-ref` to the process interface** — new imports, no breaking changes.
2. **Add `particle` world alongside `actor-process`** — both worlds coexist. The runtime detects which world a component implements (inspect exports for `start` vs `init`+`handle`).
3. **Port echo and ring to `particle` world** — validate the new model works.
4. **Build gen-server codegen** — generate server shims + client stubs from WIT interfaces.
5. **Port echo to gen-server** — validate the typed layer.
6. **Deprecate `actor-process` world** — once `particle` + gen-server cover all use cases.

### Part 8: What the runtime does NOT know

The runtime is deliberately unaware of:

- Typed service interfaces (WIT domain types)
- Function names or dispatch tables
- Gen-server protocol (call/cast/reply envelopes)
- wasm-wave encoding of domain values
- Which "behaviour" a component implements (gen-server, supervisor, raw process)

The runtime only knows:
- How to load WASM components
- How to spawn a process and call its `start` export (dynamically via `Func::call`)
- How to deliver `list<u8>` messages to mailboxes
- How to match `u64` refs for tagged messages and selective receive
- Links, monitors, exit propagation (existing fault tolerance primitives)
- Cedar policy evaluation (existing authorization)
