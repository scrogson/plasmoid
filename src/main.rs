use anyhow::Result;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    tracing::info!("Starting Plasmoid actor runtime");

    // TODO: Initialize ActorRuntime once implemented
    // let runtime = ActorRuntime::new().await?;
    // runtime.run().await?;

    Ok(())
}
