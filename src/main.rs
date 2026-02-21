use anyhow::{bail, Result};
use iroh::EndpointId;
use plasmoid::client::NodeClient;
use plasmoid::policy::PolicySet;
use plasmoid::Runtime;
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
        --load-path <dir>                    Load all .wasm files from directory
        --peer <node-id>                     Bootstrap peer for cluster sync
        --spawn <component> [--name <name>] [--init <hex>]
                                             Spawn a particle after loading

      Component name is derived from the file stem (e.g. echo.wasm -> echo).

  plasmoid spawn [--node <id>] <component> [--name <name>] [--init <hex>]
      Spawn a particle on a running node. Prints the PID.
      Uses PLASMOID_NODE env var if --node not specified.

  plasmoid send [<node-id>] <name-or-pid> <message>
      Send a message to a particle. Message is a UTF-8 string sent as bytes.
      If the first arg is not a valid node ID, uses PLASMOID_NODE env var.

Examples:
  plasmoid start --load-path target/debug
  plasmoid start --load-path target/debug --peer a3f7bc...
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
                 Use 'plasmoid spawn' to spawn processes on a running node.\n\n{}",
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
    init_msg: Vec<u8>,
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

/// Parse a hex string into bytes. Returns empty vec for empty/missing input.
fn parse_hex_init(hex_str: &str) -> Result<Vec<u8>> {
    if hex_str.is_empty() {
        return Ok(vec![]);
    }
    hex::decode(hex_str)
        .map_err(|e| anyhow::anyhow!("invalid hex for --init: {}", e))
}

async fn cmd_start(args: &[String]) -> Result<()> {
    let mut peers: Vec<EndpointId> = Vec::new();
    let mut wasm_files: Vec<String> = Vec::new();
    let mut spawn_specs: Vec<SpawnSpec> = Vec::new();
    let mut data_dir: Option<PathBuf> = None;
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
                let mut init_msg = vec![];

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
                            let hex = args
                                .get(i + 1)
                                .ok_or_else(|| anyhow::anyhow!("--init requires a hex value"))?;
                            init_msg = parse_hex_init(hex)?;
                            i += 2;
                        }
                        _ => break,
                    }
                }

                spawn_specs.push(SpawnSpec {
                    component,
                    name,
                    init_msg,
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

    // Join cluster with explicit peers (mDNS handles local discovery automatically)
    if !peers.is_empty() {
        runtime.join_cluster(peers).await?;
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
                    &spec.init_msg,
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
    let mut init_msg: Vec<u8> = vec![];

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
                let hex = args
                    .get(i + 1)
                    .ok_or_else(|| anyhow::anyhow!("--init requires a hex value"))?;
                init_msg = parse_hex_init(hex)?;
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
                anyhow::anyhow!(
                    "no --node specified and PLASMOID_NODE env var is not set"
                )
            })?;
            bootstrap
                .parse()
                .map_err(|e| anyhow::anyhow!("invalid PLASMOID_NODE '{}': {}", bootstrap, e))?
        }
    };

    let mdns = iroh::address_lookup::mdns::MdnsAddressLookup::builder();
    let endpoint = iroh::Endpoint::builder()
        .address_lookup(mdns)
        .bind()
        .await?;

    let client = NodeClient::new(endpoint, node_id);
    let result = client
        .spawn(&component, name.as_deref(), &init_msg)
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

    let mdns = iroh::address_lookup::mdns::MdnsAddressLookup::builder();
    let endpoint = iroh::Endpoint::builder()
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

const RUNTIME_WIT: &str = r#"package plasmoid:runtime@0.3.0;

interface process {
    resource pid {
        to-string: func() -> string;
    }

    self-pid: func() -> pid;
    self-name: func() -> option<string>;

    spawn: func(component: string, name: option<string>, init-msg: list<u8>) -> result<pid, spawn-error>;
    exit: func(reason: exit-reason);

    send: func(target: borrow<pid>, msg: list<u8>) -> result<_, send-error>;
    resolve: func(pid-string: string) -> option<pid>;

    register: func(name: string) -> result<_, registry-error>;
    unregister: func(name: string) -> result<_, registry-error>;
    lookup: func(name: string) -> option<pid>;

    link: func(target: borrow<pid>) -> result<_, link-error>;
    unlink: func(target: borrow<pid>);
    monitor: func(target: borrow<pid>) -> monitor-ref;
    demonitor: func(ref: monitor-ref);
    trap-exit: func(enabled: bool);

    log: func(level: log-level, message: string);

    type monitor-ref = u64;

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
        monitor-ref: monitor-ref,
        reason: exit-reason,
    }

    variant message {
        user(list<u8>),
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

world actor-process {
    import process;

    use process.{message};

    export init: func(msg: list<u8>) -> result<_, list<u8>>;
    export handle: func(msg: message);
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
    let cargo_toml = format!(
        r#"[workspace]
members = ["components/*"]
resolver = "2"
"#
    );
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
world = "actor-process"

[package.metadata.component.target.dependencies]
"plasmoid:runtime" = {{ path = "../../wit/components/{name}/deps/runtime" }}
"#
    );
    std::fs::write(component_dir.join("Cargo.toml"), cargo_toml)?;

    // components/<name>/src/lib.rs
    let lib_rs = format!(
        r#"#[allow(warnings)]
mod bindings;

use bindings::plasmoid::runtime::process;

struct {pascal_name};

impl bindings::Guest for {pascal_name} {{
    fn init(msg: Vec<u8>) -> Result<(), Vec<u8>> {{
        process::log(process::LogLevel::Info, &format!("{name_underscored} initialized with {{}} bytes", msg.len()));
        Ok(())
    }}

    fn handle(msg: bindings::plasmoid::runtime::process::Message) {{
        match msg {{
            bindings::plasmoid::runtime::process::Message::User(data) => {{
                process::log(process::LogLevel::Info, &format!("{name_underscored} received {{}} bytes", data.len()));
            }}
            bindings::plasmoid::runtime::process::Message::Exit(signal) => {{
                process::log(process::LogLevel::Warn, &format!("{name_underscored} received exit signal"));
            }}
            bindings::plasmoid::runtime::process::Message::Down(signal) => {{
                process::log(process::LogLevel::Warn, &format!("{name_underscored} received down signal"));
            }}
        }}
    }}
}}

bindings::export!({pascal_name} with_types_in bindings);
"#
    );
    std::fs::write(component_dir.join("src/lib.rs"), lib_rs)?;

    // wit/components/<name>/<name>.wit -- just re-export the actor-process world
    // The component uses the runtime's actor-process world directly
    let component_wit = format!(
        r#"package {namespace}:{name}@0.1.0;

world {name_underscored} {{
    include plasmoid:runtime/actor-process@0.3.0;
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
