use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::FerrumError;

/// Thread-safe KV storage engine backed by HashMap
///
/// Uses `Arc<RwLock<HashMap>>` for concurrent read/write access.
/// Multiple readers can access simultaneously; writers get exclusive access.
///
/// All public methods return `Result` to propagate lock poisoning errors
/// instead of panicking.
#[derive(Clone)]
pub struct KvEngine {
    store: Arc<RwLock<HashMap<String, String>>>,
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl KvEngine {
    /// Create a new empty KV engine
    pub fn new() -> Self {
        Self {
            store: Arc::default(),
        }
    }

    /// Set a key-value pair, returns the previous value if the key existed
    pub fn set(&self, key: String, value: String) -> Result<Option<String>, FerrumError> {
        let mut store = self.store.write()?;
        Ok(store.insert(key, value))
    }

    /// Get the value for a key, returns None if key does not exist
    pub fn get(&self, key: &str) -> Result<Option<String>, FerrumError> {
        let store = self.store.read()?;
        Ok(store.get(key).cloned())
    }

    /// Delete a key, returns true if the key existed
    pub fn del(&self, key: &str) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        Ok(store.remove(key).is_some())
    }

    /// Return the number of keys in the store
    pub fn dbsize(&self) -> Result<usize, FerrumError> {
        let store = self.store.read()?;
        Ok(store.len())
    }

    /// Remove all keys from the store
    pub fn flushdb(&self) -> Result<(), FerrumError> {
        let mut store = self.store.write()?;
        store.clear();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get() {
        let engine = KvEngine::new();
        engine.set("name".to_string(), "ferrum".to_string()).unwrap();
        assert_eq!(engine.get("name").unwrap(), Some("ferrum".to_string()));
    }

    #[test]
    fn test_get_nonexistent() {
        let engine = KvEngine::new();
        assert_eq!(engine.get("missing").unwrap(), None);
    }

    #[test]
    fn test_set_overwrite() {
        let engine = KvEngine::new();
        engine.set("key".to_string(), "v1".to_string()).unwrap();
        let old = engine.set("key".to_string(), "v2".to_string()).unwrap();
        assert_eq!(old, Some("v1".to_string()));
        assert_eq!(engine.get("key").unwrap(), Some("v2".to_string()));
    }

    #[test]
    fn test_del_existing() {
        let engine = KvEngine::new();
        engine.set("key".to_string(), "value".to_string()).unwrap();
        assert!(engine.del("key").unwrap());
        assert_eq!(engine.get("key").unwrap(), None);
    }

    #[test]
    fn test_del_nonexistent() {
        let engine = KvEngine::new();
        assert!(!engine.del("missing").unwrap());
    }

    #[test]
    fn test_dbsize_empty() {
        let engine = KvEngine::new();
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn test_dbsize_after_operations() {
        let engine = KvEngine::new();
        engine.set("a".to_string(), "1".to_string()).unwrap();
        engine.set("b".to_string(), "2".to_string()).unwrap();
        assert_eq!(engine.dbsize().unwrap(), 2);
        engine.del("a").unwrap();
        assert_eq!(engine.dbsize().unwrap(), 1);
    }

    #[test]
    fn test_flushdb() {
        let engine = KvEngine::new();
        engine.set("a".to_string(), "1".to_string()).unwrap();
        engine.set("b".to_string(), "2".to_string()).unwrap();
        engine.flushdb().unwrap();
        assert_eq!(engine.dbsize().unwrap(), 0);
        assert_eq!(engine.get("a").unwrap(), None);
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let engine = KvEngine::new();
        let mut handles = vec![];

        // Spawn 10 writer threads
        for i in 0..10 {
            let engine = engine.clone();
            handles.push(thread::spawn(move || {
                engine
                    .set(format!("key{i}"), format!("value{i}"))
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all keys were written
        for i in 0..10 {
            assert_eq!(
                engine.get(&format!("key{i}")).unwrap(),
                Some(format!("value{i}"))
            );
        }
    }
}
