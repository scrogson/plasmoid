use plasmoid::ActorRuntime;

#[tokio::test]
async fn test_runtime_creation() {
    let runtime = ActorRuntime::new().await.unwrap();
    assert!(!runtime.has_actor(b"test-alpn").await);
}
