use anyhow::{Result, bail};
use iroh::EndpointId;
use plasmoid::Runtime;
use plasmoid::client::NodeClient;
use plasmoid::policy::PolicySet;
use std::path::{Path, PathBuf};
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
Usage:
  plasmoid new <app-name>
      Create a new application workspace.

  plasmoid component new <name>
      Create a new component in the current application.

  plasmoid start [options] [<wasm-file> ...]
      Boot node, load WASM components. No auto-spawning unless --spawn is used.

      Options:
        --data-dir <dir>                     Data directory for persistent node identity
                                             (default: ~/.config/plasmoid)
        --peer <node-id>                     Join the cluster this node belongs to
        --load-path <dir>                    Load all .wasm files from directory
        --spawn <component> [--name <name>] [--init <wave-expr>]
                                             Spawn a particle after loading

      Component name is derived from the file stem (e.g. echo.wasm -> echo).

  plasmoid spawn [--node <id>] <component> [--name <name>] [--init <wave-expr>]
      Spawn a particle on a running node. Prints the PID.
      Uses PLASMOID_NODE env var if --node not specified.

  plasmoid send [<node-id>] <name-or-pid> <message>
      Send a message to a particle. Message is a UTF-8 string sent as bytes.
      If the first arg is not a valid node ID, uses PLASMOID_NODE env var.

Examples:
  plasmoid start --load-path target/debug
  plasmoid start echo.wasm --spawn echo --name echo
  plasmoid start --load-path target/debug --spawn echo --name echo
  plasmoid spawn --node a3f7bc... echo --name echo
  PLASMOID_NODE=a3f7bc... plasmoid spawn echo --name echo
  plasmoid send a3f7bc... echo \"hello world\"
  PLASMOID_NODE=a3f7bc... plasmoid send echo \"hello world\"
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let subcmd = args.get(1).map(|s| s.as_str());
    match subcmd {
        Some("new") => cmd_new(&args[2..]),
        Some("component") => match args.get(2).map(|s| s.as_str()) {
            Some("new") => cmd_component_new(&args[3..]),
            _ => bail!("usage: plasmoid component new <name>"),
        },
        Some("start") => cmd_start(&args[2..]).await,
        Some("spawn") => cmd_spawn(&args[2..]).await,
        Some("send") => cmd_send(&args[2..]).await,
        Some("call") => {
            bail!(
                "'plasmoid call' has been replaced by 'plasmoid send'.\n\
                 Use 'plasmoid send' to send messages to particles.\n\n{}",
                USAGE
            );
        }
        Some("run") => {
            bail!(
                "'plasmoid run' has been replaced by 'plasmoid start'.\n\
                 Use 'plasmoid start' to boot a node and load components.\n\
                 Use 'plasmoid spawn' to spawn particles on a running node.\n\n{}",
                USAGE
            );
        }
        _ => {
            eprint!("{USAGE}");
            bail!("expected subcommand: new, component, start, spawn, or send");
        }
    }
}

/// A parsed spawn spec from --spawn flags.
struct SpawnSpec {
    component: String,
    name: Option<String>,
    init_args: String,
}

fn default_data_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CONFIG_HOME") {
        PathBuf::from(xdg).join("plasmoid")
    } else if let Ok(home) = std::env::var("HOME") {
        PathBuf::from(home).join(".config/plasmoid")
    } else {
        PathBuf::from(".config/plasmoid")
    }
}

async fn cmd_start(args: &[String]) -> Result<()> {
    let mut wasm_files: Vec<String> = Vec::new();
    let mut spawn_specs: Vec<SpawnSpec> = Vec::new();
    let mut data_dir: Option<PathBuf> = None;
    let mut peers: Vec<EndpointId> = Vec::new();
    let mut i = 0;

    while i < args.len() {
        match args[i].as_str() {
            "--data-dir" => {
                let dir = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--data-dir requires a directory"))?;
                data_dir = Some(PathBuf::from(dir));
                i += 2;
            }
            "--peer" => {
                let id_str = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--peer requires a node ID"))?;
                let peer_id: EndpointId = id_str
                    .parse()
                    .map_err(|e| anyhow::anyhow!("invalid peer node ID '{}': {}", id_str, e))?;
                peers.push(peer_id);
                i += 2;
            }
            "--load-path" => {
                let dir = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--load-path requires a directory"))?;
                let path = std::path::Path::new(dir);
                if !path.is_dir() {
                    bail!("--load-path '{}' is not a directory", dir);
                }
                for entry in std::fs::read_dir(path)? {
                    let entry = entry?;
                    let file_path = entry.path();
                    if file_path.extension().is_some_and(|ext| ext == "wasm") {
                        wasm_files.push(file_path.to_string_lossy().to_string());
                    }
                }
                i += 2;
            }
            "--spawn" => {
                let component = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--spawn requires a component name"))?
                    .clone();
                i += 2;

                let mut name = None;
                let mut init_args = String::new();

                // Parse optional --name and --init after --spawn <component>
                while i < args.len() {
                    match args[i].as_str() {
                        "--name" => {
                            let n = args
                                .get(i + 1)
                                .ok_or_else(|| anyhow::anyhow!("--name requires a value"))?;
                            name = Some(n.clone());
                            i += 2;
                        }
                        "--init" => {
                            let wave = args.get(i + 1).ok_or_else(|| {
                                anyhow::anyhow!("--init requires a wasm-wave expression")
                            })?;
                            init_args = wave.clone();
                            i += 2;
                        }
                        _ => break,
                    }
                }

                spawn_specs.push(SpawnSpec {
                    component,
                    name,
                    init_args,
                });
            }
            arg if arg.ends_with(".wasm") => {
                wasm_files.push(arg.to_string());
                i += 1;
            }
            other => {
                bail!("unexpected argument: '{}'\n\n{}", other, USAGE);
            }
        }
    }

    if wasm_files.is_empty() {
        bail!(
            "no WASM modules found. Specify .wasm files or use --load-path <dir>\n\n{}",
            USAGE
        );
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,plasmoid=debug")),
        )
        .init();

    eprintln!("Plasmoid v{}", env!("CARGO_PKG_VERSION"));
    eprintln!();

    let data_dir = data_dir.unwrap_or_else(default_data_dir);
    let runtime = Runtime::new(Some(&data_dir)).await?;

    eprintln!("Node: {}", runtime.node_id());
    eprintln!();

    // One introduction is enough: the mesh is transitive, so this node learns
    // the rest of the cluster and they learn it.
    for peer in peers {
        runtime.join(peer).await;
        eprintln!("Joining via {}", peer);
    }

    // Load all WASM modules (without spawning)
    let mut loaded = Vec::new();
    for wasm_path in &wasm_files {
        let wasm_bytes = std::fs::read(wasm_path)?;

        // Derive component name from file stem
        let component = std::path::Path::new(wasm_path)
            .file_stem()
            .and_then(|s| s.to_str())
            .ok_or_else(|| anyhow::anyhow!("invalid wasm file path: {}", wasm_path))?;

        runtime
            .load(component, &wasm_bytes, PolicySet::all())
            .await?;
        loaded.push(component.to_string());
    }

    eprintln!("Components loaded:");
    for name in &loaded {
        eprintln!("  {name}");
    }
    eprintln!();

    // Spawn any inline --spawn specs
    if !spawn_specs.is_empty() {
        let mut pids = Vec::new();
        for spec in &spawn_specs {
            let pid = runtime
                .spawn(
                    &spec.component,
                    spec.name.as_deref(),
                    Some(PolicySet::all()),
                    &spec.init_args,
                )
                .await?;
            pids.push((pid, spec.component.clone(), spec.name.clone()));
        }

        eprintln!("Particles:");
        for (pid, component, name) in &pids {
            match name {
                Some(n) => eprintln!("  {pid}  {component}  (name: {n})"),
                None => eprintln!("  {pid}  {component}"),
            }
        }
        eprintln!();
    }

    runtime.run().await?;

    Ok(())
}

async fn cmd_spawn(args: &[String]) -> Result<()> {
    if args.is_empty() {
        bail!("usage: plasmoid spawn [--node <id>] <component> [--name <name>] [--init <hex>]");
    }

    let mut i = 0;
    let mut node_id: Option<EndpointId> = None;

    // Parse --node option
    if args.get(i).map(|s| s.as_str()) == Some("--node") {
        let id_str = args
            .get(i + 1)
            .ok_or_else(|| anyhow::anyhow!("--node requires a node ID"))?;
        node_id = Some(
            id_str
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid node ID '{}': {}", id_str, e))?,
        );
        i += 2;
    }

    let component = args
        .get(i)
        .ok_or_else(|| anyhow::anyhow!("missing component name"))?
        .clone();
    i += 1;

    let mut name = None;
    let mut init_args = String::new();

    while i < args.len() {
        match args[i].as_str() {
            "--name" => {
                let n = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--name requires a value"))?;
                name = Some(n.as_str().to_string());
                i += 2;
            }
            "--init" => {
                let wave = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--init requires a wasm-wave expression"))?;
                init_args = wave.clone();
                i += 2;
            }
            other => {
                bail!("unexpected argument: '{}'\n\n{}", other, USAGE);
            }
        }
    }

    // Resolve node ID from --node or PLASMOID_NODE env var
    let node_id = match node_id {
        Some(id) => id,
        None => {
            let bootstrap = std::env::var("PLASMOID_NODE").map_err(|_| {
                anyhow::anyhow!("no --node specified and PLASMOID_NODE env var is not set")
            })?;
            bootstrap
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid PLASMOID_NODE '{}': {}", bootstrap, e))?
        }
    };

    let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .address_lookup(mdns)
        .bind()
        .await?;

    let client = NodeClient::new(endpoint, node_id);
    let result = client
        .spawn(&component, name.as_deref(), &init_args)
        .await?;

    println!("{}", result.pid);

    Ok(())
}

async fn cmd_send(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: plasmoid send [<node-id>] <name-or-pid> <message>");
    }

    // Try parsing the first arg as an EndpointId.
    // If it parses, use explicit node addressing.
    // If not, use PLASMOID_NODE env var as the bootstrap node.
    let (node_id, target, message) = match args[0].parse::<EndpointId>() {
        Ok(id) => {
            if args.len() < 3 {
                bail!("usage: plasmoid send <node-id> <name-or-pid> <message>");
            }
            (id, &args[1], &args[2])
        }
        Err(_) => {
            let bootstrap = std::env::var("PLASMOID_NODE").map_err(|_| {
                anyhow::anyhow!(
                    "first argument is not a node ID and PLASMOID_NODE env var is not set"
                )
            })?;
            let id: EndpointId = bootstrap
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid PLASMOID_NODE '{}': {}", bootstrap, e))?;
            (id, &args[0], &args[1])
        }
    };

    let mdns = iroh_mdns_address_lookup::MdnsAddressLookup::builder();
    let endpoint = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
        .address_lookup(mdns)
        .bind()
        .await?;

    let client = NodeClient::new(endpoint, node_id);
    client.send(target, message.as_bytes()).await?;

    println!("sent");

    Ok(())
}

// ---------------------------------------------------------------------------
// Scaffolding commands
// ---------------------------------------------------------------------------

const RUNTIME_WIT: &str = r#"package plasmoid:runtime@0.11.0;

interface host {
    resource pid {
        to-string: func() -> string;
    }

    /// A node's identity, as full hex. Never the truncated form `pid.to-string`
    /// renders, which keeps only 4 of 32 bytes and cannot be parsed back.
    type node-id = string;

    /// Anything a message can be addressed to.
    ///
    /// One destination type rather than one function per address form, so a new
    /// form costs a case here instead of a parallel set of functions. A `named`
    /// destination is resolved on the *receiving* node — there is no lookup.
    variant destination {
        pid(borrow<pid>),
        local-named(string),
        named(tuple<node-id, string>),
    }

    self-pid: func() -> pid;
    self-name: func() -> option<string>;
    self-node: func() -> node-id;
    node-of: func(p: borrow<pid>) -> node-id;

    make-ref: func() -> u64;

    spawn: func(component: string, name: option<string>, init-args: string) -> result<pid, spawn-error>;

    /// Spawn and link **atomically**, as Erlang's `spawn_link` does.
    ///
    /// Not a convenience. Spawning and then linking loses the exit reason: the
    /// child runs immediately and may be gone before the link lands, and a link
    /// to a dead particle reports `noproc` rather than what actually happened.
    /// A supervisor cannot then tell "finished its job" from "crashed", which is
    /// exactly the distinction a `transient` child's restart policy turns on --
    /// so a child that exits normally too quickly would be restarted forever.
    spawn-link: func(component: string, name: option<string>, init-args: string) -> result<pid, spawn-error>;

    /// Spawn on another node, waiting for it to allocate the pid and reply.
    /// Blocking is acceptable here because spawn is not a hot path.
    spawn-on: func(node: node-id, component: string, name: option<string>, init-args: string) -> result<pid, spawn-error>;

    /// Spawn on another node without waiting. Returns a ref; the outcome
    /// arrives as a `spawn-reply` message, correlated with `recv-ref`.
    spawn-request: func(node: node-id, component: string, name: option<string>, init-args: string) -> u64;

    /// Terminate the calling particle, unconditionally.
    ///
    /// Distinct from `exit-signal` addressed at yourself, which goes through
    /// your own trap rules like anybody else's signal: `exit-signal` with
    /// `normal` while trapping delivers a message and you keep running, so
    /// collapsing the two would leave no way to say *terminate me now*.
    /// Erlang draws the same line between `exit/1` and `exit/2`.
    exit: func(reason: exit-reason);

    /// Send an exit signal to another particle, on any node.
    ///
    /// Asynchronous and infallible, like `send`: it reports nothing, and a
    /// dead target or an unreachable node drops it silently. A caller that
    /// needs to know the target died `monitor`s it first.
    ///
    /// `kill` sent this way is **untrappable** — the target dies with `killed`
    /// whether or not it traps exits. `killed` *inherited* through a link is an
    /// ordinary reason and can be trapped. That asymmetry is deliberate: it
    /// makes a kill a guarantee without making anything unkillable. Erlang's
    /// `exit_signal/2` behaves identically.
    exit-signal: func(dest: destination, reason: exit-reason);

    send: func(dest: destination, msg: list<u8>);
    send-ref: func(dest: destination, ref: u64, msg: list<u8>);

    recv: func(timeout-ms: option<u64>) -> option<message>;
    recv-ref: func(ref: u64, timeout-ms: option<u64>) -> option<message>;

    resolve: func(pid-string: string) -> option<pid>;

    register: func(name: string) -> result<_, registry-error>;
    unregister: func(name: string) -> result<_, registry-error>;
    lookup: func(name: string) -> option<pid>;

    /// Claim a name across the whole cluster, so exactly one particle holds it.
    ///
    /// **Blocks.** Claiming locks every member, which is a round trip by
    /// nature; Erlang's `global:register_name` is synchronous for the same
    /// reason. Not a hot path.
    ///
    /// Losing a *simultaneous* claim is an ordinary `taken` return and nothing
    /// dies. Losing a conflict discovered when two partitions merge is
    /// different: nobody was wrong when they claimed, so the lower pid keeps
    /// the name and the other particle is killed.
    global-register: func(name: string) -> result<_, claim-error>;
    global-unregister: func(name: string);

    /// **Blocks** while two name tables are merging, so it never hands back a
    /// particle that is about to lose a conflict and be killed. Erlang does not
    /// wait, and may. Bounded: a merge has no natural limit, and an unbounded
    /// wait would wedge every lookup the first time one stalled.
    global-lookup: func(name: string) -> result<option<pid>, lookup-error>;

    variant claim-error {
        /// Held by a live particle.
        taken(pid),
        /// The claimant already holds a global name. Erlang allows a process
        /// exactly one, and calls supporting several broken.
        already-named(string),
        /// The cluster would not settle: locks stayed busy, or membership kept
        /// changing under the claim.
        unsettled,
    }

    variant lookup-error {
        /// A merge did not settle in time. The name may or may not exist —
        /// which is why this is not `none`, a claim we cannot make.
        unsettled,
    }

    /// Infallible: if the target is already gone, that arrives as an exit
    /// signal carrying `noproc`, not as a return value.
    link: func(dest: destination);
    unlink: func(dest: destination);
    /// Always returns a valid ref. A target that does not exist produces an
    /// immediate down signal carrying `noproc`.
    monitor: func(dest: destination) -> u64;
    demonitor: func(ref: u64);
    trap-exit: func(enabled: bool);

    log: func(level: log-level, message: string);

    enum log-level { trace, debug, info, warn, error }

    variant exit-reason {
        normal,
        /// Asked for by name. Untrappable when sent with `exit-signal`;
        /// trappable when inherited through a link.
        kill,
        /// The result of being killed. Distinct from `shutdown` so a supervisor
        /// can tell "I stopped it" from "something killed it".
        killed,
        shutdown(string),
        exception(string),
        /// The target did not exist.
        noproc,
        /// The node holding the target was lost.
        noconnection,
    }

    /// A trapped exit signal, delivered as an ordinary message. Erlang's
    /// `{'EXIT', From, Reason}` — a signal becomes a message when trapped,
    /// which is why this is not called `exit-signal`.
    record exit-message {
        sender: pid,
        reason: exit-reason,
    }

    record down-message {
        sender: pid,
        ref: u64,
        reason: exit-reason,
    }

    record tagged-message {
        ref: u64,
        payload: list<u8>,
    }

    /// The outcome of a `spawn-request`, delivered as a message.
    record spawn-reply {
        ref: u64,
        outcome: result<pid, spawn-error>,
    }

    variant message {
        data(list<u8>),
        tagged(tagged-message),
        exit(exit-message),
        down(down-message),
        spawn-reply(spawn-reply),
    }

    enum spawn-error {
        component-not-found,
        init-failed,
        resource-limit,
        node-unreachable,
    }

    enum registry-error {
        already-registered,
        not-registered,
    }

}

world particle {
    import host;
}
"#;

/// Convert a kebab-case name to PascalCase.
/// "order-service" -> "OrderService", "echo" -> "Echo"
fn to_pascal_case(name: &str) -> String {
    name.split('-')
        .map(|part| {
            let mut chars = part.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().to_string() + chars.as_str(),
            }
        })
        .collect()
}

/// Validate a name for use as a crate/component name.
fn validate_name(name: &str) -> Result<()> {
    if name.is_empty() {
        bail!("name cannot be empty");
    }
    let first = name.chars().next().unwrap();
    if !first.is_ascii_lowercase() {
        bail!("name must start with a lowercase letter");
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
    {
        bail!("name must contain only lowercase letters, digits, and hyphens");
    }
    if name.ends_with('-') {
        bail!("name must not end with a hyphen");
    }
    Ok(())
}

/// Find the workspace root by walking up from the current directory.
fn find_workspace_root() -> Result<PathBuf> {
    let mut dir = std::env::current_dir()?;
    loop {
        let cargo_toml = dir.join("Cargo.toml");
        if cargo_toml.exists() {
            let content = std::fs::read_to_string(&cargo_toml)?;
            if content.contains("[workspace]") {
                return Ok(dir);
            }
        }
        if !dir.pop() {
            bail!("not inside a plasmoid application (no workspace Cargo.toml found)");
        }
    }
}

fn cmd_new(args: &[String]) -> Result<()> {
    let arg = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: plasmoid new <app-name>"))?;

    let root = Path::new(arg);

    // Derive the app name from the last path component
    let app_name = root
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot derive app name from '{}'", arg))?;

    validate_name(app_name)?;

    if root.exists() {
        bail!("directory '{}' already exists", root.display());
    }

    // Create directory structure
    std::fs::create_dir_all(root.join("wit/components/deps/runtime"))?;
    std::fs::create_dir_all(root.join("components"))?;

    // Cargo.toml
    let cargo_toml = r#"[workspace]
members = ["components/*"]
resolver = "2"
"#
    .to_string();
    std::fs::write(root.join("Cargo.toml"), cargo_toml)?;

    // wit/world.wit
    std::fs::write(root.join("wit/world.wit"), RUNTIME_WIT)?;

    // wit/components/deps/runtime/world.wit (copy)
    std::fs::write(
        root.join("wit/components/deps/runtime/world.wit"),
        RUNTIME_WIT,
    )?;

    // components/.gitkeep
    std::fs::write(root.join("components/.gitkeep"), "")?;

    let display_path = root.display();
    println!(r#"Created application "{app_name}""#);
    println!();
    println!("  {display_path}/Cargo.toml");
    println!("  {display_path}/wit/world.wit");
    println!("  {display_path}/wit/components/deps/runtime/world.wit");
    println!("  {display_path}/components/");
    println!();
    println!("Next: cd {display_path} && plasmoid component new <name>");

    Ok(())
}

fn cmd_component_new(args: &[String]) -> Result<()> {
    let name = args
        .first()
        .ok_or_else(|| anyhow::anyhow!("usage: plasmoid component new <name>"))?;

    validate_name(name)?;

    let workspace = find_workspace_root()?;

    // Derive namespace from workspace directory name
    let namespace = workspace
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| anyhow::anyhow!("cannot determine app namespace from workspace path"))?
        .to_string();

    // Verify this is a plasmoid app
    let runtime_wit = workspace.join("wit/components/deps/runtime/world.wit");
    if !runtime_wit.exists() {
        bail!("not a plasmoid application (missing wit/components/deps/runtime/world.wit)");
    }

    // Check component doesn't already exist
    let component_dir = workspace.join("components").join(name);
    if component_dir.exists() {
        bail!("component '{}' already exists", name);
    }

    let name_underscored = name.replace('-', "_");
    let pascal_name = to_pascal_case(name);

    // Create directories
    std::fs::create_dir_all(component_dir.join("src"))?;
    std::fs::create_dir_all(
        workspace
            .join("wit/components")
            .join(name)
            .join("deps/runtime"),
    )?;

    // components/<name>/Cargo.toml
    let cargo_toml = format!(
        r#"[package]
name = "{name}"
version = "0.1.0"
edition = "2021"

[lib]
crate-type = ["cdylib"]

[dependencies]
wit-bindgen-rt = "0.41"

[package.metadata.component]
package = "{namespace}:{name}"

[package.metadata.component.target]
path = "../../wit/components/{name}"
world = "{name_underscored}"

[package.metadata.component.target.dependencies]
"plasmoid:runtime" = {{ path = "../../wit/components/{name}/deps/runtime" }}
"#
    );
    std::fs::write(component_dir.join("Cargo.toml"), cargo_toml)?;

    // components/<name>/src/lib.rs
    let lib_rs = format!(
        r#"#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::host;

struct {pascal_name};

impl bindings::Guest for {pascal_name} {{
    fn start() -> Result<(), String> {{
        host::log(host::LogLevel::Info, "{name_underscored} started");
        loop {{
            match host::recv(None) {{
                Some(host::Message::Data(data)) => {{
                    if data == b"stop" {{
                        return Ok(());
                    }}
                    host::log(host::LogLevel::Info, &format!("{name_underscored} received {{}} bytes", data.len()));
                }}
                Some(host::Message::Exit(_)) | Some(host::Message::Down(_)) => {{}}
                Some(host::Message::Tagged(_)) => {{}}
                Some(host::Message::SpawnReply(_)) => {{}}
                None => return Ok(()),
            }}
        }}
    }}
}}

bindings::export!({pascal_name} with_types_in bindings);
"#
    );
    std::fs::write(component_dir.join("src/lib.rs"), lib_rs)?;

    // wit/components/<name>/<name>.wit
    let component_wit = format!(
        r#"package {namespace}:{name}@0.1.0;

world {name_underscored} {{
    include plasmoid:runtime/particle@0.11.0;
    export start: func() -> result<_, string>;
}}
"#
    );
    let wit_dir = workspace.join("wit/components").join(name);
    std::fs::write(wit_dir.join(format!("{name}.wit")), component_wit)?;

    // wit/components/<name>/deps/runtime/world.wit (copy from workspace)
    let runtime_content = std::fs::read_to_string(&runtime_wit)?;
    std::fs::write(wit_dir.join("deps/runtime/world.wit"), runtime_content)?;

    println!(r#"Created component "{name}" in app "{namespace}""#);
    println!();
    println!("  components/{name}/Cargo.toml");
    println!("  components/{name}/src/lib.rs");
    println!("  wit/components/{name}/{name}.wit");
    println!("  wit/components/{name}/deps/runtime/world.wit");
    println!();
    println!("Build: cargo component build -p {name} --release");

    Ok(())
}
