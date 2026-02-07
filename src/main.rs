use anyhow::Result;
use plasmoid::policy::PolicySet;
use plasmoid::ActorRuntime;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info,plasmoid=debug")),
        )
        .init();

    tracing::info!("Plasmoid Actor Runtime v{}", env!("CARGO_PKG_VERSION"));

    let runtime = ActorRuntime::new().await?;

    tracing::info!(node_id = %runtime.node_id(), "Node identity");

    // If there's a WASM file argument, deploy it
    let args: Vec<String> = std::env::args().collect();
    if args.len() > 1 {
        let wasm_path = &args[1];
        let alpn = args.get(2).cloned().unwrap_or_else(|| "default/1".to_string());

        tracing::info!(path = %wasm_path, alpn = %alpn, "Deploying actor");

        let wasm_bytes = std::fs::read(wasm_path)?;
        runtime
            .deploy(alpn.as_bytes().to_vec(), &wasm_bytes, PolicySet::all())
            .await?;
    }

    runtime.run().await?;

    Ok(())
}
