# Resource PID — Design

## Goal

Replace string-based PID passing with a WIT resource type. `send` becomes O(1) handle-to-mailbox instead of O(n) string scanning.

## Problem

Every `send` call does `resolve_target(pid_string)`, which linearly scans all mailbox keys comparing `pid.to_string() == target`. With 2000 particles, that's up to 2000 string allocations and comparisons per send. The ring benchmark drops from 479k msg/s (100 particles) to 43k msg/s (2000 particles).

## Approach: WIT resource

Define `resource pid` in the actor-context interface. The host owns the `Pid` values in a `ResourceTable`; components hold opaque handles. `send(target: pid, ...)` resolves the handle in O(1) via `ResourceTable::get`, then does a direct `HashMap::get` on the mailbox map.

### WIT interface

```wit
interface actor-context {
    resource pid {
        to-string: func() -> string;
    }

    resolve: func(pid-string: string) -> option<pid>;

    self-pid: func() -> pid;
    self-name: func() -> option<string>;
    caller-pid: func() -> option<pid>;

    spawn: func(component: string, name: option<string>) -> result<pid, string>;

    send: func(target: pid, message: list<string>) -> result<_, string>;
    receive: func() -> list<string>;

    call: func(target: string, function: string, args: list<string>) -> result<list<string>, string>;
    notify: func(target: string, function: string, args: list<string>) -> result<_, string>;
}
```

`call`/`notify` stay string-targeted — they address by name or PID string, go through wave encoding, not the performance-critical path.

### Host implementation

`HostState` already has a `ResourceTable` (for WASI). PID handles share it.

- Register `ResourceType::host::<Pid>()` in the linker
- `spawn`: push `Pid` into `ResourceTable`, return `Resource<Pid>`
- `self-pid`: push current particle's `Pid` into table, return handle
- `caller-pid`: same pattern, returns `Option<Resource<Pid>>`
- `send`: `ResourceTable::get(&target)` → `&Pid` → `registry.send_to_pid(&pid, message)`
- `resolve`: `resolve_target(string)` → push into table → return handle
- `pid.to-string`: `ResourceTable::get(&self)` → `pid.to_string()`

### ParticleRegistry

New method — the fast path:

```rust
pub async fn send_to_pid(&self, pid: &Pid, message: Vec<String>) -> Result<()> {
    let mailboxes = self.mailboxes.read().await;
    let handle = mailboxes.get(pid).ok_or_else(|| anyhow!("no mailbox for pid"))?;
    handle.tx.send(message).map_err(|_| anyhow!("mailbox closed"))
}
```

Existing `send_message(&str)` stays for `call`/`notify` paths.

### Ring component

Orchestrator uses typed PIDs from `spawn`/`self-pid`/`send`. Workers resolve the next-PID string once at startup via `resolve()`, then use the handle for all subsequent sends.

```rust
fn start(next_pid_str: String) {
    let next_pid = actor_context::resolve(&next_pid_str).unwrap();
    loop {
        let msg = actor_context::receive();
        if msg.is_empty() || msg[0] == "stop" { return; }
        let hops: u32 = msg[0].parse().unwrap();
        let master_str = &msg[1];
        if hops == 0 {
            let master = actor_context::resolve(master_str).unwrap();
            actor_context::send(&master, &["finished".to_string()]).unwrap();
            continue;
        }
        actor_context::send(&next_pid, &[(hops - 1).to_string(), master_str.to_string()]).unwrap();
    }
}
```

## What stays the same

- `call`/`notify` — string targets, wave args
- `receive` — returns `list<string>`
- `dispatch_call`/`remote_call` — string-based resolution
- Wire protocol — unchanged
- `resolve_target` — used by `call`/`notify`/`resolve`

## What's new

- `resource pid` in WIT (both copies)
- `send_to_pid(&Pid)` on `ParticleRegistry`
- `pid` resource type registration in linker
- `resolve` and `pid.to-string` host functions

## Files changed

- `wit/world.wit` — add resource pid, update signatures
- `wit/components/ring/deps/runtime/world.wit` — same
- `wit/components/echo/deps/runtime/world.wit` — same (if exists)
- `wit/components/caller/deps/runtime/world.wit` — same (if exists)
- `src/runtime/invoke.rs` — register resource, update host functions
- `src/registry.rs` — add `send_to_pid`
- `components/ring/src/lib.rs` — use typed PIDs

## Validation

Ring benchmark before/after with 2000 particles. Expect significant improvement at high particle counts.
