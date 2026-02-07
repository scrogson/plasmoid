use plasmoid::host::Database;
use std::sync::Arc;

#[test]
fn test_database_get_set() {
    let db = Database::new();

    db.set("key1", b"value1".to_vec()).unwrap();
    let value = db.get("key1");

    assert_eq!(value, Some(b"value1".to_vec()));
}

#[test]
fn test_database_get_missing() {
    let db = Database::new();
    assert_eq!(db.get("nonexistent"), None);
}

#[test]
fn test_database_delete() {
    let db = Database::new();

    db.set("key1", b"value1".to_vec()).unwrap();
    assert!(db.delete("key1").unwrap());
    assert_eq!(db.get("key1"), None);
}

#[test]
fn test_database_delete_missing() {
    let db = Database::new();
    assert!(!db.delete("nonexistent").unwrap());
}

#[test]
fn test_database_list_keys() {
    let db = Database::new();

    db.set("user:1", b"alice".to_vec()).unwrap();
    db.set("user:2", b"bob".to_vec()).unwrap();
    db.set("order:1", b"order-data".to_vec()).unwrap();

    let mut keys = db.list_keys("user:");
    keys.sort();

    assert_eq!(keys, vec!["user:1", "user:2"]);
}

#[test]
fn test_database_scoped_access() {
    let db = Arc::new(Database::new());

    db.set("actor1:data", b"actor1-value".to_vec()).unwrap();
    db.set("actor2:data", b"actor2-value".to_vec()).unwrap();

    assert_eq!(db.get("actor1:data"), Some(b"actor1-value".to_vec()));
    assert_eq!(db.get("actor2:data"), Some(b"actor2-value".to_vec()));
}
