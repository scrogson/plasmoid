use plasmoid::Runtime;

#[tokio::test]
async fn test_runtime_creation() {
    let runtime = Runtime::new(None).await.unwrap();
    assert!(!runtime.has_particle("test").await);
}
