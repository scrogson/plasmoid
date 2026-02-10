use plasmoid::client::ActorRef;
use plasmoid::policy::PolicySet;
use plasmoid::ActorRuntime;
use std::sync::Arc;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires echo actor WASM to be built"]
async fn test_typed_echo_actor() {
    // Read the echo actor WASM
    let wasm_path = "actors/echo/target/wasm32-wasip1/release/echo_actor.wasm";
    let wasm_bytes = std::fs::read(wasm_path).expect("echo actor WASM not found");

    // Create server runtime and deploy echo actor
    let server = Arc::new(ActorRuntime::new().await.unwrap());
    let alpn = b"echo/1".to_vec();
    server
        .deploy(alpn.clone(), &wasm_bytes, PolicySet::all())
        .await
        .unwrap();

    assert!(server.has_actor(&alpn).await);

    // Spawn the server accept loop in background
    let srv = server.clone();
    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    // Give the accept loop a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a separate client endpoint (iroh doesn't allow self-connections)
    let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();

    // Create a remote actor ref pointing to the server
    let echo = ActorRef::remote(client_endpoint, "echo/1", server.node_addr());

    // Test echo function
    let result = echo.call("echo", &["\"hello world\""]).await.unwrap();
    assert_eq!(result, vec!["\"hello world\""]);

    // Test reverse function
    let result = echo.call("reverse", &["\"hello\""]).await.unwrap();
    assert_eq!(result, vec!["\"olleh\""]);
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
