use plasmoid::ActorRuntime;

#[tokio::test]
async fn test_runtime_creation() {
    let runtime = ActorRuntime::new(None).await.unwrap();
    assert!(!runtime.has_process("test").await);
}
