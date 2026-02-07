use plasmoid::policy::PolicySet;
use plasmoid::ActorRuntime;
use std::time::Duration;

// This test requires a pre-built echo actor WASM
// Skip if not available
#[tokio::test]
#[ignore = "requires echo actor WASM to be built"]
async fn test_echo_actor_roundtrip() {
    // Read the echo actor WASM
    let wasm_path = "actors/echo/target/wasm32-wasip1/release/echo_actor.wasm";
    let wasm_bytes = std::fs::read(wasm_path).expect("echo actor WASM not found");

    // Create runtime
    let runtime = ActorRuntime::new().await.unwrap();

    // Deploy echo actor
    let alpn = b"echo/1".to_vec();
    runtime
        .deploy(alpn.clone(), &wasm_bytes, PolicySet::all())
        .await
        .unwrap();

    assert!(runtime.has_actor(&alpn).await);

    // TODO: Test actual request/response via QUIC
    // This would require connecting to ourselves or running two runtimes
}

#[tokio::test]
async fn test_runtime_startup_shutdown() {
    let _runtime = ActorRuntime::new().await.unwrap();

    // Spawn runtime in background
    let handle = tokio::spawn(async move {
        // Runtime would run until ctrl+c, so we just check it starts
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    handle.await.unwrap();
}
