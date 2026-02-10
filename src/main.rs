use anyhow::{bail, Result};
use iroh::EndpointId;
use plasmoid::client::ActorRef;
use plasmoid::policy::PolicySet;
use plasmoid::ActorRuntime;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
Usage:
  plasmoid run [--peer <node-id>] <wasm-file> <name> [<wasm-file> <name>...]
      Start the runtime with one or more actors deployed.
      --peer connects to an existing node for cluster sync.

  plasmoid call <node-id> <name> <function> [args...]
      Call a function on a remote actor (explicit node).

  plasmoid call <name> <function> [args...]
      Call a function via a bootstrap node (set PLASMOID_NODE env var).
      The bootstrap node resolves the name and forwards if needed.

      Arguments are wasm-wave encoded (strings need quotes: '\"hello\"').

Examples:
  plasmoid run echo_actor.wasm echo
  plasmoid run echo_actor.wasm echo caller_actor.wasm caller
  plasmoid run --peer a3f7bc1234567890... echo_actor.wasm echo
  plasmoid call a3f7bc1234567890... echo echo '\"hello world\"'
  PLASMOID_NODE=a3f7bc1234567890... plasmoid call echo echo '\"hello world\"'
";

#[tokio::main]
async fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    let subcmd = args.get(1).map(|s| s.as_str());
    match subcmd {
        Some("run") => cmd_run(&args[2..]).await,
        Some("call") => cmd_call(&args[2..]).await,
        _ => {
            eprint!("{USAGE}");
            bail!("expected subcommand: run or call");
        }
    }
}

async fn cmd_run(args: &[String]) -> Result<()> {
    // Parse --peer flag
    let mut peers: Vec<EndpointId> = Vec::new();
    let mut remaining = args;

    while remaining.len() >= 2 && remaining[0] == "--peer" {
        let peer_id: EndpointId = remaining[1]
            .parse()
            .map_err(|e| anyhow::anyhow!("invalid peer node ID '{}': {}", remaining[1], e))?;
        peers.push(peer_id);
        remaining = &remaining[2..];
    }

    if remaining.len() < 2 || remaining.len() % 2 != 0 {
        bail!("usage: plasmoid run [--peer <node-id>] <wasm-file> <name> [<wasm-file> <name>...]");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,plasmoid=debug")),
        )
        .init();

    eprintln!("Plasmoid v{}", env!("CARGO_PKG_VERSION"));
    eprintln!();

    let runtime = ActorRuntime::new().await?;

    eprintln!("Node: {}", runtime.node_id());
    eprintln!();

    // Join gossip cluster if peers specified
    if !peers.is_empty() {
        runtime.join_cluster(peers).await?;
    }

    // Deploy actors in pairs: <wasm-file> <name>
    let mut pids = Vec::new();
    for pair in remaining.chunks(2) {
        let wasm_path = &pair[0];
        let name = &pair[1];

        let wasm_bytes = std::fs::read(wasm_path)?;
        let pid = runtime.deploy(name, &wasm_bytes, PolicySet::all()).await?;
        pids.push((pid, name.clone()));
    }

    // Print process table
    eprintln!("Processes:");
    for (pid, name) in &pids {
        eprintln!("  {pid}  {name}");
    }
    eprintln!();

    runtime.run().await?;

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

    let endpoint = iroh::Endpoint::builder().bind().await?;
    let actor = ActorRef::remote_by_name(endpoint, name, node_id);

    let results = actor.call(function, &call_args).await?;

    for result in &results {
        println!("{result}");
    }

    Ok(())
}
