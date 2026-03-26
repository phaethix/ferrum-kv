use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Thread-safe KV storage engine backed by HashMap
///
/// Uses `Arc<RwLock<HashMap>>` for concurrent read/write access.
/// Multiple readers can access simultaneously; writers get exclusive access.
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
        KvEngine {
            store: Arc::new(RwLock::new(HashMap::new())),
        }
    }

    /// Set a key-value pair, returns the previous value if the key existed
    pub fn set(&self, key: String, value: String) -> Option<String> {
        let mut store = self.store.write().unwrap();
        store.insert(key, value)
    }

    /// Get the value for a key, returns None if key does not exist
    pub fn get(&self, key: &str) -> Option<String> {
        let store = self.store.read().unwrap();
        store.get(key).cloned()
    }

    /// Delete a key, returns true if the key existed
    pub fn del(&self, key: &str) -> bool {
        let mut store = self.store.write().unwrap();
        store.remove(key).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get() {
        let engine = KvEngine::new();
        engine.set("name".to_string(), "ferrum".to_string());
        assert_eq!(engine.get("name"), Some("ferrum".to_string()));
    }

    #[test]
    fn test_get_nonexistent() {
        let engine = KvEngine::new();
        assert_eq!(engine.get("missing"), None);
    }

    #[test]
    fn test_set_overwrite() {
        let engine = KvEngine::new();
        engine.set("key".to_string(), "v1".to_string());
        let old = engine.set("key".to_string(), "v2".to_string());
        assert_eq!(old, Some("v1".to_string()));
        assert_eq!(engine.get("key"), Some("v2".to_string()));
    }

    #[test]
    fn test_del_existing() {
        let engine = KvEngine::new();
        engine.set("key".to_string(), "value".to_string());
        assert!(engine.del("key"));
        assert_eq!(engine.get("key"), None);
    }

    #[test]
    fn test_del_nonexistent() {
        let engine = KvEngine::new();
        assert!(!engine.del("missing"));
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
                engine.set(format!("key{}", i), format!("value{}", i));
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        // Verify all keys were written
        for i in 0..10 {
            assert_eq!(
                engine.get(&format!("key{}", i)),
                Some(format!("value{}", i))
            );
        }
    }
}
