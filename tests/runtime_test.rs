use plasmoid::ActorRuntime;

#[tokio::test]
async fn test_runtime_creation() {
    let runtime = ActorRuntime::new().await.unwrap();
    assert!(!runtime.has_actor(b"test-alpn").await);
}

#[tokio::test]
async fn test_runtime_database_access() {
    let runtime = ActorRuntime::new().await.unwrap();
    let db = runtime.database();

    db.set("test-key", b"test-value".to_vec()).unwrap();
    assert_eq!(db.get("test-key"), Some(b"test-value".to_vec()));
}
