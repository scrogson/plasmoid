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
| Links & monitors | `link` / `unlink` (bidirectional, exit-propagating), `monitor` / `demonitor` (unidirectional, non-propagating), `trap-exit` to receive exit signals as ordinary messages. **They work across nodes**, taking a destination like `send`. Both are asynchronous: `link` is infallible and `monitor` always returns a ref, with a missing target arriving as a signal carrying `noproc` |
| Exit | `exit(reason)` with `normal` / `kill` / `shutdown` / `exception`; exit and down signals arrive as mailbox messages |
| Execution | Async invocation via wasmtime fibers; host functions are natively async |
| WASI | `wasm32-wasip1` components supported via `wasmtime-wasi` |

**Transport and operations.**

- Nodes form a peer-to-peer QUIC mesh over iroh, with mDNS discovery on the local network and n0 relay fallback. Node identity is an Ed25519 keypair, stable across restarts.
- All Plasmoid traffic uses a single ALPN, `plasmoid/1`.
- **Node loss fires every crossing relationship.** The QUIC idle timeout decides (default 60s, configurable), and each link and monitor fires individually with reason `noconnection` — so a lost node is indistinguishable in shape from the particles on it dying separately.
- **Nodes form a cluster.** A node introduced to one member (`--peer <node-id>`) learns the rest and they learn it — a transitive full mesh, as in Erlang. Membership *is* connectivity: connecting is joining, and a node leaves by the same signal that fires its links.
- **Particles spawn on other nodes** — `spawn-on` waits for the target to allocate the pid (a remote pid must come from the target, since `seq` is node-allocated); `spawn-request` returns immediately and delivers the outcome as a `spawn-reply` message. Blocking is acceptable for spawn because it is not a hot path.
- **Particles message across nodes.** A pid carries its home node, so `send` routes there with no registry lookup; each node pair shares one ordered link, drained by a writer task so sending never blocks on a handshake.
- External clients can also spawn and message particles on a remote node — `plasmoid spawn --node <id>` and `plasmoid send <node-id> <target> <msg>`.
- CLI: `new`, `component new`, `start`, `spawn`, `send`.

**Authoring.** The `plasmoid-sdk` crate provides `#[main]` and `#[gen_server]` (which generates the receive loop, dispatch, and typed `call`/`cast` client methods), `send!` / `recv!` over postcard, logging macros, and init-argument helpers. Example components: `echo` and a `ring` benchmark.

---

## Not built

**Supervision** — no supervisor exists, and there are no restart strategies or restart-intensity limits. The primitives a supervisor would be built from — `link`, `monitor`, `trap-exit`, exit-signal propagation — are all in place and working. Supervisors are intended to be ordinary particles rather than a runtime feature, as in OTP.

**Capability enforcement** — every host function is linked unconditionally. No policy is evaluated, so a particle's imports are not restricted.

**Component distribution** — `spawn` requires the component to be already loaded on the target node. There is no mechanism to ship a component to a node that lacks it.

**Signalling another particle's exit** — `exit` terminates the caller only; a particle cannot send `kill` or any other signal to a different particle.

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

Design decisions that remain genuinely open — mailbox overflow policy, cross-node supervision granularity, component distribution strategy, capability scoping, observability surface, and how a kill capability would be granted — are tracked in [issue #10](https://github.com/scrogson/plasmoid/issues/10).

---

*The gap between what this project claimed and what it did was inventoried in [issue #2](https://github.com/scrogson/plasmoid/issues/2); this file exists so that gap stays visible.*
