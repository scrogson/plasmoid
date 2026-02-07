use plasmoid::ActorRuntime;

#[tokio::test]
async fn test_runtime_has_endpoint() {
    let runtime = ActorRuntime::new().await.unwrap();
    let endpoint_id = runtime.node_id();

    // Endpoint ID should be a valid public key (32 bytes, base32 encoded)
    assert!(!endpoint_id.to_string().is_empty());
}

#[tokio::test]
async fn test_runtime_node_addr() {
    let runtime = ActorRuntime::new().await.unwrap();
    let addr = runtime.node_addr();

    // Should have at least the endpoint ID
    assert_eq!(addr.id, runtime.node_id());
}
