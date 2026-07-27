# Roadmap

Plasmoid is a **distributed particle runtime for WebAssembly components**, built on iroh. Distribution is the goal; this file is the honest account of how far along it is.

The rest of the documentation describes only what works. Anything not yet real lives here.

See [`CONTEXT.md`](./CONTEXT.md) for vocabulary — a *particle* is a running WASM component instance.

---

## Works today

**The particle model, on a single node.**

| | |
|---|---|
| Particles | WASM components instantiated per-spawn, with linear memory persisting across messages for the instance's lifetime |
| Pids | `Pid { node, seq }` — a WIT resource handle, O(1) routing, invalidated on death so stale references are detectable |
| Mailboxes | Bounded queue (default capacity 1024), sequential delivery — a particle never handles two messages at once. Overflow returns `MailboxFull` to the sender |
| Messaging | `send` / `recv`, plus `send-ref` / `recv-ref` for tagged request-reply correlation. `recv` takes an optional timeout |
| Naming | `register` / `unregister` / `lookup` by name; `resolve` from a pid string |
| Links & monitors | `link` / `unlink` (bidirectional, exit-propagating), `monitor` / `demonitor` (unidirectional, non-propagating), `trap-exit` to receive exit signals as ordinary messages |
| Exit | `exit(reason)` with `normal` / `kill` / `shutdown` / `exception`; exit and down signals arrive as mailbox messages |
| Execution | Async invocation via wasmtime fibers; host functions are natively async |
| WASI | `wasm32-wasip1` components supported via `wasmtime-wasi` |

**Transport and operations.**

- Nodes form a peer-to-peer QUIC mesh over iroh, with mDNS discovery on the local network and n0 relay fallback. Node identity is an Ed25519 keypair, stable across restarts.
- All Plasmoid traffic uses a single ALPN, `plasmoid/1`.
- **External clients can spawn and message particles on a remote node** — `plasmoid spawn --node <id>` and `plasmoid send <node-id> <target> <msg>`. This is operator-level access, not particle-to-particle.
- CLI: `new`, `component new`, `start`, `spawn`, `send`.

**Authoring.** The `plasmoid-sdk` crate provides `#[main]` and `#[gen_server]` (which generates the receive loop, dispatch, and typed `call`/`cast` client methods), `send!` / `recv!` over postcard, logging macros, and init-argument helpers. Example components: `echo` and a `ring` benchmark.

---

## Not built

**Cross-node particle addressing** — the headline gap. A particle cannot spawn on, or send to, another node. `spawn` takes no node argument, and message routing resolves against a node-local table. Everything under *Works today* stops at the node boundary; distribution today is transport and tooling, not programming model.

**Supervision** — no supervisor exists, and there are no restart strategies or restart-intensity limits. The primitives a supervisor would be built from — `link`, `monitor`, `trap-exit`, exit-signal propagation — are all in place and working. Supervisors are intended to be ordinary particles rather than a runtime feature, as in OTP.

**Capability enforcement** — every host function is linked unconditionally. No policy is evaluated, so a particle's imports are not restricted.

**Component distribution** — `spawn` requires the component to be already loaded on the target node. There is no mechanism to ship a component to a node that lacks it.

**Node-failure signalling** — losing a peer does not produce exit or down signals. There is no `node-unreachable` exit reason.

**Signalling another particle's exit** — `exit` terminates the caller only; a particle cannot send `kill` or any other signal to a different particle.

---

## Built but unwired

Present in the source, compiling, partly tested — and unreachable. Listed because their presence otherwise makes the system look more complete than it is.

| Subsystem | State |
|---|---|
| `Database` (`src/host/database.rs`) | A working key-value store with unit tests, but no corresponding WIT interface. No particle can call it. |
| `PolicySet` (`src/policy.rs`) | Threaded through six modules; its `allows()` is never called. The `cedar-policy` dependency is used in no source file. |
| Distributed registry reads (`src/doc_registry.rs`) | Nodes announce spawns into a replicated iroh-docs document, but `resolve_name`, `resolve_pid`, and `announce_down` have no callers. The document is written, never read, and never pruned. |

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
