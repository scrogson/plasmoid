use std::collections::HashMap;
use std::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DatabaseError {
    #[error("lock poisoned")]
    LockPoisoned,
}

/// Simple in-memory key-value store for actor state.
#[derive(Debug, Default)]
pub struct Database {
    data: RwLock<HashMap<String, Vec<u8>>>,
}

impl Database {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, key: &str) -> Option<Vec<u8>> {
        self.data
            .read()
            .ok()
            .and_then(|guard| guard.get(key).cloned())
    }

    pub fn set(&self, key: &str, value: Vec<u8>) -> Result<(), DatabaseError> {
        self.data
            .write()
            .map_err(|_| DatabaseError::LockPoisoned)?
            .insert(key.to_string(), value);
        Ok(())
    }

    pub fn delete(&self, key: &str) -> Result<bool, DatabaseError> {
        let removed = self
            .data
            .write()
            .map_err(|_| DatabaseError::LockPoisoned)?
            .remove(key)
            .is_some();
        Ok(removed)
    }

    pub fn list_keys(&self, prefix: &str) -> Vec<String> {
        self.data
            .read()
            .ok()
            .map(|guard| {
                guard
                    .keys()
                    .filter(|k| k.starts_with(prefix))
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}
