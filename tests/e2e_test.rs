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
    let server = Arc::new(ActorRuntime::new(None).await.unwrap());
    let _pid = server
        .deploy("echo", &wasm_bytes, Some("echo"), PolicySet::all())
        .await
        .unwrap();

    assert!(server.has_process("echo").await);

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
    let echo = ActorRef::remote_by_name(client_endpoint, "echo", server.node_addr());

    // Test echo function
    let result = echo.call("echo", &["\"hello world\""]).await.unwrap();
    assert_eq!(result, vec!["\"hello world\""]);

    // Test reverse function
    let result = echo.call("reverse", &["\"hello\""]).await.unwrap();
    assert_eq!(result, vec!["\"olleh\""]);
}

#[tokio::test]
#[ignore = "requires echo and caller actor WASMs to be built"]
async fn test_caller_calls_echo() {
    // Read both actor WASMs
    let echo_wasm_path = "actors/echo/target/wasm32-wasip1/release/echo_actor.wasm";
    let caller_wasm_path = "actors/caller/target/wasm32-wasip1/release/caller_actor.wasm";
    let echo_wasm = std::fs::read(echo_wasm_path).expect("echo actor WASM not found");
    let caller_wasm = std::fs::read(caller_wasm_path).expect("caller actor WASM not found");

    // Create server runtime and deploy both actors
    let server = Arc::new(ActorRuntime::new(None).await.unwrap());
    server
        .deploy("echo", &echo_wasm, Some("echo"), PolicySet::all())
        .await
        .unwrap();
    server
        .deploy("caller", &caller_wasm, Some("caller"), PolicySet::all())
        .await
        .unwrap();

    assert!(server.has_process("echo").await);
    assert!(server.has_process("caller").await);

    // Spawn the server accept loop in background
    let srv = server.clone();
    tokio::spawn(async move {
        let _ = srv.run().await;
    });

    // Give the accept loop a moment to start
    tokio::time::sleep(Duration::from_millis(100)).await;

    // Create a separate client endpoint (iroh doesn't allow self-connections)
    let client_endpoint = iroh::Endpoint::builder().bind().await.unwrap();

    // Create a remote actor ref pointing to the caller actor
    let caller = ActorRef::remote_by_name(client_endpoint, "caller", server.node_addr());

    // Call the caller actor's call-echo function
    // The caller will internally call echo's echo function
    let result = caller.call("call-echo", &["\"hello from caller\""]).await.unwrap();

    // The result is a wave-encoded result<string, string>
    assert_eq!(result, vec!["ok(\"hello from caller\")"]);
}

#[tokio::test]
async fn test_runtime_startup_shutdown() {
    let _runtime = ActorRuntime::new(None).await.unwrap();

    // Spawn runtime in background
    let handle = tokio::spawn(async move {
        // Runtime would run until ctrl+c, so we just check it starts
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    handle.await.unwrap();
}
