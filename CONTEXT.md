# Plasmoid

A distributed particle runtime for WebAssembly components, built on iroh. Plasmoid gives WASM components an Erlang-style concurrency model — isolated units of execution with mailboxes, addressable identities, and failure that propagates along declared relationships.

## Language

### Core

**Particle**:
A running instance of a WebAssembly component. The fundamental unit of execution and fault isolation.
_Avoid_: Actor, process, service, task

**Node**:
A running instance of the Plasmoid runtime, holding many particles under one cryptographic identity.
_Avoid_: Server, host, peer, VM

**Component**:
A compiled WebAssembly artifact that a particle is an instance of. One component, many particles — the class to the particle's object.
_Avoid_: Module, binary, image

**Pid**:
The unforgeable identity of a particle, encoding its node and a node-local token. Invalidated when the particle dies, so stale references are detectable.
_Avoid_: Address, handle, reference, id

**Mailbox**:
The ordered queue of messages awaiting a particle. Delivery is sequential — a particle never handles two messages at once.
_Avoid_: Queue, inbox, channel

### Relationships and failure

**Link**:
A bidirectional relationship between two particles along which exit signals propagate. When one dies abnormally, the other dies too — unless it traps exits.
_Avoid_: Connection, binding, association

**Monitor**:
A unidirectional, non-propagating watch on a particle. The watcher receives a message when the target dies; it does not die itself.
_Avoid_: Observer, watcher, subscription

**Exit signal**:
The notification carrying a termination reason. Either **inherited** — sent along links when a particle dies — or **directed**, sent at a particle on purpose with `exit-signal`. Propagates death by default. The distinction matters only for `kill`, which is untrappable when directed and trappable when inherited.
_Avoid_: Error, exception, crash event

**Trap exits**:
The setting by which a particle receives incoming exit signals as ordinary mailbox messages rather than dying from them. The mechanism supervision is built from. Does not save a particle from a directed `kill` — nothing does, which is what makes a kill a guarantee.
_Avoid_: Catch, handle errors, intercept

**Kill / killed**:
`kill` is the reason you *ask* for; `killed` is the reason the particle *dies of*, and what its links inherit. Kept distinct from `shutdown` so a supervisor can tell "I stopped it" from "something killed it".
_Avoid_: Terminate, destroy, force-quit

### Naming notes

**Actor** — deliberately not used. Plasmoid implements the concurrency model the literature calls the actor model, but names its unit a *particle*, the way Erlang names its unit a *process*. Never a synonym for particle.

**Process** — deliberately not used, and no longer present anywhere in the codebase. It was once the name of the WIT interface and of numerous Rust identifiers; both were renamed. The interface a particle imports is now `host`, since it is the runtime's API surface rather than a particle itself. The word survives only as an ordinary English verb ("process the events"). Never a synonym for particle.
