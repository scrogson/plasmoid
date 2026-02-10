use anyhow::{bail, Result};
use iroh::EndpointAddr;
use plasmoid::client::ActorRef;
use plasmoid::policy::PolicySet;
use plasmoid::ActorRuntime;
use tracing_subscriber::EnvFilter;

const USAGE: &str = "\
Usage:
  plasmoid run <wasm-file> <alpn>
      Start the runtime with an actor deployed.

  plasmoid call <node-addr-json> <alpn> <function> [args...]
      Call a function on a remote actor.
      <node-addr-json> is the JSON printed by 'plasmoid run'.
      Arguments are wasm-wave encoded (strings need quotes: '\"hello\"').

Examples:
  plasmoid run actors/echo/target/wasm32-wasip1/release/echo_actor.wasm echo/1
  plasmoid call '{...}' echo/1 echo '\"hello world\"'
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
    if args.len() < 2 {
        bail!("usage: plasmoid run <wasm-file> <alpn>");
    }

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,plasmoid=debug")),
        )
        .init();

    let wasm_path = &args[0];
    let alpn = &args[1];

    tracing::info!("Plasmoid Actor Runtime v{}", env!("CARGO_PKG_VERSION"));

    let runtime = ActorRuntime::new().await?;

    tracing::info!(node_id = %runtime.node_id(), "Node identity");

    let wasm_bytes = std::fs::read(wasm_path)?;
    runtime
        .deploy(alpn.as_bytes().to_vec(), &wasm_bytes, PolicySet::all())
        .await?;

    // Print the node address as JSON so the client can connect
    let addr = runtime.node_addr();
    let addr_json = serde_json::to_string(&addr)?;
    eprintln!();
    eprintln!("  Node address (pass to 'plasmoid call'):");
    eprintln!("  {addr_json}");
    eprintln!();

    runtime.run().await?;

    Ok(())
}

async fn cmd_call(args: &[String]) -> Result<()> {
    if args.len() < 3 {
        bail!("usage: plasmoid call <node-addr-json> <alpn> <function> [args...]");
    }

    let addr: EndpointAddr = serde_json::from_str(&args[0])?;
    let alpn = &args[1];
    let function = &args[2];
    let call_args: Vec<&str> = args[3..].iter().map(|s| s.as_str()).collect();

    let endpoint = iroh::Endpoint::builder().bind().await?;
    let actor = ActorRef::remote(endpoint, alpn, addr);

    let results = actor.call(function, &call_args).await?;

    for result in &results {
        println!("{result}");
    }

    Ok(())
}
