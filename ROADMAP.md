# Roadmap

Plasmoid is a **distributed particle runtime for WebAssembly components**, built on iroh. Distribution is the goal; this file is the honest account of how far along it is.

The rest of the documentation describes only what works. Anything not yet real lives here.

See [`CONTEXT.md`](./CONTEXT.md) for vocabulary — a *particle* is a running WASM component instance.

---

## Works today

**The particle model.**

| | |
|---|---|
| Particles | WASM components instantiated per-spawn, with linear memory persisting across messages for the instance's lifetime |
| Pids | `Pid { node, seq }` — a WIT resource handle, O(1) routing, invalidated on death so stale references are detectable |
| Mailboxes | **Unbounded**, sequential delivery — a particle never handles two messages at once. Following Erlang: `send` is fire-and-forget, so a bound could only drop silently; a slow particle instead grows until the node dies, which is loud and diagnosable |
| Messaging | `send` / `recv`, plus `send-ref` / `recv-ref` for tagged request-reply correlation. `send` takes a **destination** — a pid, a local name, or a name on another node — and **routes to any node** and is fire-and-forget: it never blocks and never reports a delivery failure, local or remote. Messages from one particle to another arrive in send order, across nodes. Liveness is discovered with `monitor` |
| Naming | `register` / `unregister` / `lookup` by name, node-scoped as in Erlang; `resolve` from a pid string. A name on another node is addressed as a `named(node, name)` **destination**, resolved on the receiving node — there is no remote lookup |
| Global naming | `global-register` claims a name **cluster-wide**, locking every member in `EndpointId` order; it **blocks**, as Erlang's `register_name` does. The table is replicated, so `global-lookup` is a local read — but it waits for any in-flight merge, bounded, so it never names a particle about to be killed. A simultaneous claim is refused with `taken`; a conflict found when partitions merge is resolved by **`min(pid)`**, computed independently on every node, and the loser is killed. Erlang picks its winner at random and must designate one side to decide; determinism is the divergence that removes the arbiter |
| Links & monitors | `link` / `unlink` (bidirectional, exit-propagating), `monitor` / `demonitor` (unidirectional, non-propagating), `trap-exit` to receive exit signals as ordinary messages. **They work across nodes**, taking a destination like `send`. Both are asynchronous: `link` is infallible and `monitor` always returns a ref, with a missing target arriving as a signal carrying `noproc` |
| Exit | `exit(reason)` terminates the caller unconditionally. `exit-signal(dest, reason)` signals **any particle on any node** — asynchronous and infallible like `send`, so the caller learns nothing and `monitor`s if it cares. `kill` **sent** this way is untrappable and the target dies as `killed`; `kill` or `killed` **inherited** through a link is an ordinary, trappable reason. Erlang's `exit/1` and `exit_signal/2` |
| Execution | Async invocation via wasmtime fibers; host functions are natively async |
| WASI | `wasm32-wasip1` components supported via `wasmtime-wasi` |

**Transport and operations.**

- Nodes form a peer-to-peer QUIC mesh over iroh, with mDNS discovery on the local network and n0 relay fallback. Node identity is an Ed25519 keypair, stable across restarts. **Discovery is a fallback, not a dependency**: `--peer` and `Announce` carry full addresses, so a cluster forms without any lookup service.
- All Plasmoid traffic uses a single ALPN, `plasmoid/1`.
- **Node loss fires every crossing relationship.** The QUIC idle timeout decides (default 60s, configurable), and each link and monitor fires individually with reason `noconnection` — so a lost node is indistinguishable in shape from the particles on it dying separately.
- **Nodes form a cluster.** A node introduced to one member (`--peer <node-id>`) learns the rest and they learn it — a transitive full mesh, as in Erlang. Membership *is* connectivity: connecting is joining, and a node leaves by the same signal that fires its links.
- **Partitions are kept fully connected.** A node that loses a link tells the cluster, and everyone disconnects from the node reported lost — so a partition never overlaps, and every member agrees who the members are. Erlang made this `global`'s default in OTP 25 because the alternative corrupts state in a way that *outlives the partition*. The cost is Erlang's: one broken link ejects both of its endpoints, firing every relationship crossing to either.
- **Particles spawn on other nodes** — `spawn-on` waits for the target to allocate the pid (a remote pid must come from the target, since `seq` is node-allocated); `spawn-request` returns immediately and delivers the outcome as a `spawn-reply` message. Blocking is acceptable for spawn because it is not a hot path.
- **Particles message across nodes.** A pid carries its home node, so `send` routes there with no registry lookup; each node pair shares one ordered link, drained by a writer task so sending never blocks on a handshake.
- External clients can also spawn and message particles on a remote node — `plasmoid spawn --node <id>` and `plasmoid send <node-id> <target> <msg>`.
- CLI: `new`, `component new`, `start`, `spawn`, `send`.

**Authoring.** The `plasmoid-sdk` crate provides `#[main]` and `#[gen_server]` (which generates the receive loop, dispatch, and typed `call`/`cast` client methods), `send!` / `recv!` over postcard, logging macros, and init-argument helpers. Example components: `echo` and a `ring` benchmark.

---

## Not built

**Supervision** — **works**. A supervisor is an ordinary particle: the policy is a pure decision core (`crates/plasmoid-sdk/src/supervisor.rs`, 17 host-side tests) and `run_supervisor!` drives it against the host, starting children with `spawn-link`, restarting them per strategy and restart type, honouring shutdown, and giving up when restart intensity is exceeded. `components/supervised` is a working example and is exercised end to end in CI. An application is declared by a **manifest** (`plasmoid start <wasm> --app plasmoid.toml`) naming a root component and a restart type; when a `permanent` root dies the **node exits**, non-zero unless the root exited `normal` — which is how a collapsed tree becomes visible to systemd or Kubernetes, since the runtime deliberately cannot see a supervision tree. **Dynamic children** work too: `run_dynamic_supervisor!` starts empty and takes `start-child` / `terminate-child` as tagged messages, since a supervisor is a particle rather than an object. Each dynamic child carries a whole spec, so restart is the same path as a static child's, and a pool of `temporary` workers can churn indefinitely without tripping restart intensity — because intensity counts restarts, not deaths.

**Capability enforcement** — every host function is linked unconditionally. No policy is evaluated, so a particle's imports are not restricted.

**Component distribution** — `spawn` requires the component to be already loaded on the target node. There is no mechanism to ship a component to a node that lacks it.

---

## Built but unwired

Present in the source, compiling, partly tested — and unreachable. Listed because their presence otherwise makes the system look more complete than it is.

| Subsystem | State |
|---|---|
| `Database` (`src/host/database.rs`) | A working key-value store with unit tests, but no corresponding WIT interface. No particle can call it. |
| `PolicySet` (`src/policy.rs`) | Threaded through six modules; its `allows()` is never called. The `cedar-policy` dependency is used in no source file. |

---

## Non-goals

- **A general-purpose WASM runtime.** Plasmoid is a concurrency runtime that happens to use wasmtime, not a wasmtime distribution.
- **HTTP or gRPC gateways.** Particles speak mailbox, not request-response.
- **Durable state in the runtime.** Linear memory does not survive a crash; a restarted particle starts fresh. Durability is an application concern, best handled by separating behaviour from a supervised state-holding particle.
- **Hot code reloading.** Restart-to-upgrade is the model.

---

## Open questions

Design decisions that remain genuinely open — mailbox overflow policy, cross-node supervision granularity, component distribution strategy, capability scoping, and observability surface — are tracked in [issue #10](https://github.com/scrogson/plasmoid/issues/10).

---

*The gap between what this project claimed and what it did was inventoried in [issue #2](https://github.com/scrogson/plasmoid/issues/2); this file exists so that gap stays visible.*
