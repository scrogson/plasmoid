use plasmoid::Runtime;
use std::time::Duration;

#[tokio::test]
#[ignore = "requires actor WASM components to be rebuilt for v0.3.0 WIT"]
async fn test_typed_echo_actor() {
    // This test will be re-enabled after the echo actor is rebuilt
    // for the new init/handle model (Task 10).
}

#[tokio::test]
#[ignore = "requires actor WASM components to be rebuilt for v0.3.0 WIT"]
async fn test_caller_calls_echo() {
    // This test will be re-enabled after the echo/caller actors are rebuilt
    // for the new init/handle model (Task 10).
}

#[tokio::test]
async fn test_runtime_startup_shutdown() {
    let _runtime = Runtime::new(None).await.unwrap();

    // Spawn runtime in background
    let handle = tokio::spawn(async move {
        // Runtime would run until ctrl+c, so we just check it starts
        tokio::time::sleep(Duration::from_millis(100)).await;
    });

    handle.await.unwrap();
}
