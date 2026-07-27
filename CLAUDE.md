# CLAUDE.md

How to work in this repo. Architecture is **not** documented here — it lives in one place, linked below, so the two can't drift apart.

| Read this | For |
|---|---|
| [README.md](./README.md) | Architecture, quick start, SDK, CLI, WIT contract, project structure |
| [ROADMAP.md](./ROADMAP.md) | What's built, what isn't, and what's built-but-unwired |
| [CONTEXT.md](./CONTEXT.md) | Vocabulary — read before naming anything |

Check `ROADMAP.md` before claiming the system does something. Several subsystems exist in source but are unreachable at runtime, which makes the code look more capable than it is.

## Commands

`mise` is the primary workflow; it puts `./target/debug` on `PATH`, so `plasmoid` is directly runnable after a build.

```bash
mise run build:all          # runtime + both components
mise run run                # boot a node with all components in target/debug
mise run node:a             # boot a node with the echo particle spawned

cargo check --workspace --all-targets
cargo test --workspace      # 55 tests
```

Building a WASM component **must happen from inside its directory** — see Gotchas:

```bash
cd components/echo && cargo component build --release
# -> target/wasm32-wasip1/release/echo.wasm
```

CI (`.github/workflows/ci.yml`) enforces three gates on every push and PR. Run them before claiming work is done:

```bash
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace     # build the components first — see Gotchas
```

## Conventions

- **Vocabulary is fixed.** A running WASM instance is a **particle** — never *actor*, never *process*. `process` survives only as the name of the WIT interface in `wit/world.wit`; renaming it is tracked in [#9](https://github.com/scrogson/plasmoid/issues/9). Full glossary in `CONTEXT.md`.
- **`wit/world.wit` is the contract.** Both host (`wasmtime::component::bindgen!`) and guest (`wit-bindgen`) derive from it. Changing it is a breaking change for every component and needs a package version bump.
- **Verify before documenting.** Grep for the symbol, run the command. This repo accumulated ~1100 lines of documentation describing a system that had been replaced underneath it; the cleanup is tracked in [#1](https://github.com/scrogson/plasmoid/issues/1).

## Gotchas

- **`components/echo` and `components/ring` are excluded from the workspace** (`Cargo.toml:3`) and carry their own lockfiles. `cargo check --workspace` **does not cover them**, and `cargo component build -p echo` from the root fails with `package ID specification 'echo' did not match any packages`. Build from inside the component directory. (In an app scaffolded by `plasmoid new`, components *are* workspace members, so `-p <name>` works there.)
- **`cargo test` can report a hollow green.** The e2e tests look for `components/echo/target/wasm32-wasip1/release/echo.wasm` on disk and **silently skip** when it's missing — still printing `test result: ok`, with an unchanged test count. Build the components before trusting a passing run. CI builds them and fails if the skip message appears.
- **iroh rejects self-connection** — `ConnectWithOptsError::SelfConnect`. A test that connects needs two endpoints, not one.
- **Don't swap the `cargo install cargo-component` step in CI for an install action.** cargo-component isn't in `taiki-e/install-action`'s manifests and publishes no release binaries, so `cargo-binstall` can't fetch it either.
- **`plasmoid component new` scaffolds raw `wit-bindgen`**, not the SDK — no `plasmoid-sdk` dependency and no `#[main]`. A freshly scaffolded component looks nothing like the examples in `README.md`.
- **`mise run ring:run` is broken.** It invokes `plasmoid call`, which was replaced by `plasmoid send` and now exits with an error. `call` and `run` survive only as deprecation stubs.
- **`spawn` is not in the SDK prelude.** It resolves inside `#[plasmoid_sdk::main]` because the macro emits `use crate::bindings::plasmoid::runtime::process::*`.

## Agent skills

### Issue tracker

Issues live in GitHub Issues for `scrogson/plasmoid`, via the `gh` CLI. See `docs/agents/issue-tracker.md`.

### Triage labels

Default vocabulary — `needs-triage`, `needs-info`, `ready-for-agent`, `ready-for-human`, `wontfix`. See `docs/agents/triage-labels.md`.

### Domain docs

Single-context — `CONTEXT.md` + `docs/adr/` at the repo root. See `docs/agents/domain.md`.
