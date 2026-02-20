use anyhow::{bail, Result};
use iroh::EndpointId;
use plasmoid::client::{ParticleRef, NodeClient};
use plasmoid::policy::PolicySet;
use plasmoid::Runtime;
use std::path::PathBuf;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
Usage:
  plasmoid start [options] [<wasm-file> ...]
      Boot node, load WASM components. No auto-spawning unless --spawn is used.

      Options:
        --data-dir <dir>                     Data directory for persistent node identity
                                             (default: ~/.config/plasmoid)
        --load-path <dir>                    Load all .wasm files from directory
        --peer <node-id>                     Bootstrap peer for cluster sync
        --spawn <component> [--name <name>]  Spawn a particle after loading

      Component name is derived from the file stem (e.g. echo_actor.wasm -> echo_actor).

  plasmoid spawn [--node <id>] <component> [--name <name>]
      Spawn a particle on a running node. Prints the PID.
      Uses PLASMOID_NODE env var if --node not specified.

  plasmoid call [<node-id>] <name> <function> [args...]
      Call a function on a particle. If the first arg is not a valid node ID,
      uses PLASMOID_NODE env var as the bootstrap node.
      Arguments are wasm-wave encoded (strings need quotes: '\"hello\"').

Examples:
  plasmoid start --load-path target/debug
  plasmoid start --load-path target/debug --peer a3f7bc...
  plasmoid start echo_actor.wasm --spawn echo_actor --name echo
  plasmoid start --load-path target/debug --spawn echo_actor --name echo
  plasmoid spawn --node a3f7bc... echo_actor --name echo
  PLASMOID_NODE=a3f7bc... plasmoid spawn echo_actor --name echo
  plasmoid call a3f7bc... echo echo '\"hello world\"'
  PLASMOID_NODE=a3f7bc... plasmoid call echo echo '\"hello world\"'
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let subcmd = args.get(1).map(|s| s.as_str());
    match subcmd {
        Some("start") => cmd_start(&args[2..]).await,
        Some("spawn") => cmd_spawn(&args[2..]).await,
        Some("call") => cmd_call(&args[2..]).await,
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
            bail!("expected subcommand: start, spawn, or call");
        }
    }
}

/// A parsed spawn spec from --spawn flags.
struct SpawnSpec {
    component: String,
    name: Option<String>,
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
                let name = if args.get(i + 2).map(|s| s.as_str()) == Some("--name") {
                    let n = args
                        .get(i + 3)
                        .ok_or_else(|| anyhow::anyhow!("--name requires a value"))?;
                    i += 4;
                    Some(n.clone())
                } else {
                    i += 2;
                    None
                };
                spawn_specs.push(SpawnSpec { component, name });
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
                .spawn(&spec.component, spec.name.as_deref(), Some(PolicySet::all()))
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
        bail!("usage: plasmoid spawn [--node <id>] <component> [--name <name>]");
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

    let name = if args.get(i).map(|s| s.as_str()) == Some("--name") {
        let n = args
            .get(i + 1)
            .ok_or_else(|| anyhow::anyhow!("--name requires a value"))?;
        Some(n.as_str())
    } else {
        None
    };

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
    let result = client.spawn(&component, name).await?;

    println!("{}", result.pid);

    Ok(())
}

async fn cmd_call(args: &[String]) -> Result<()> {
    if args.len() < 2 {
        bail!("usage: plasmoid call [<node-id>] <name> <function> [args...]");
    }

    // Try parsing the first arg as an EndpointId.
    // If it parses, use explicit node addressing.
    // If not, use PLASMOID_NODE env var as the bootstrap node.
    let (node_id, name, function, call_args) = match args[0].parse::<EndpointId>() {
        Ok(id) => {
            if args.len() < 3 {
                bail!("usage: plasmoid call <node-id> <name> <function> [args...]");
            }
            let call_args: Vec<&str> = args[3..].iter().map(|s| s.as_str()).collect();
            (id, &args[1], &args[2], call_args)
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
            let call_args: Vec<&str> = args[2..].iter().map(|s| s.as_str()).collect();
            (id, &args[0], &args[1], call_args)
        }
    };

    let mdns = iroh::address_lookup::mdns::MdnsAddressLookup::builder();
    let endpoint = iroh::Endpoint::builder()
        .address_lookup(mdns)
        .bind()
        .await?;
    let particle = ParticleRef::remote_by_name(endpoint, name, node_id);

    let results = particle.call(function, &call_args).await?;

    for result in &results {
        println!("{result}");
    }

    Ok(())
}
