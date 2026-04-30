use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::FerrumError;

/// Maximum allowed key size in bytes (64 KiB).
pub const KEY_MAX_BYTES: usize = 64 * 1024;

/// Maximum allowed value size in bytes (16 MiB).
pub const VALUE_MAX_BYTES: usize = 16 * 1024 * 1024;

/// A thread-safe key-value storage engine backed by a [`HashMap`].
///
/// The store uses `Arc<RwLock<HashMap<String, String>>>` for concurrent access.
/// Multiple readers may access the map at the same time, while writers acquire
/// exclusive access.
///
/// Public methods return [`Result`] so lock poisoning can be reported instead
/// of causing a panic.
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
    /// Creates a new empty key-value engine.
    pub fn new() -> Self {
        Self {
            store: Arc::default(),
        }
    }

    /// Sets a key-value pair and returns the previous value, if any.
    ///
    /// Returns [`FerrumError::KeyTooLong`] or [`FerrumError::ValueTooLarge`] if
    /// the configured size limits are exceeded.
    pub fn set(&self, key: String, value: String) -> Result<Option<String>, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;
        let mut store = self.store.write()?;
        Ok(store.insert(key, value))
    }

    /// Returns the value for `key`, or `None` if the key does not exist.
    pub fn get(&self, key: &str) -> Result<Option<String>, FerrumError> {
        let store = self.store.read()?;
        Ok(store.get(key).cloned())
    }

    /// Deletes `key` and returns `true` if it existed.
    pub fn del(&self, key: &str) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        Ok(store.remove(key).is_some())
    }

    /// Returns `true` if `key` exists.
    pub fn exists(&self, key: &str) -> Result<bool, FerrumError> {
        let store = self.store.read()?;
        Ok(store.contains_key(key))
    }

    /// Returns the number of keys currently stored.
    pub fn dbsize(&self) -> Result<usize, FerrumError> {
        let store = self.store.read()?;
        Ok(store.len())
    }

    /// Removes all keys from the store.
    pub fn flushdb(&self) -> Result<(), FerrumError> {
        let mut store = self.store.write()?;
        store.clear();
        Ok(())
    }
}

/// Validates the key length constraint.
fn validate_key(key: &str) -> Result<(), FerrumError> {
    let len = key.len();
    if len == 0 {
        return Err(FerrumError::ParseError("key must not be empty".into()));
    }
    if len > KEY_MAX_BYTES {
        return Err(FerrumError::KeyTooLong {
            len,
            max: KEY_MAX_BYTES,
        });
    }
    Ok(())
}

/// Validates the value length constraint.
fn validate_value(value: &str) -> Result<(), FerrumError> {
    let len = value.len();
    if len > VALUE_MAX_BYTES {
        return Err(FerrumError::ValueTooLarge {
            len,
            max: VALUE_MAX_BYTES,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_set_and_get() {
        let engine = KvEngine::new();
        engine
            .set("name".to_string(), "ferrum".to_string())
            .unwrap();
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
    fn test_exists() {
        let engine = KvEngine::new();
        assert!(!engine.exists("key").unwrap());
        engine.set("key".to_string(), "value".to_string()).unwrap();
        assert!(engine.exists("key").unwrap());
        engine.del("key").unwrap();
        assert!(!engine.exists("key").unwrap());
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
    fn test_set_rejects_empty_key() {
        let engine = KvEngine::new();
        let err = engine.set(String::new(), "v".into()).unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn test_set_rejects_oversized_key() {
        let engine = KvEngine::new();
        let big_key = "k".repeat(KEY_MAX_BYTES + 1);
        let err = engine.set(big_key, "v".into()).unwrap_err();
        assert!(matches!(
            err,
            FerrumError::KeyTooLong {
                len,
                max: KEY_MAX_BYTES,
            } if len == KEY_MAX_BYTES + 1
        ));
    }

    #[test]
    fn test_set_accepts_boundary_key_length() {
        let engine = KvEngine::new();
        let key = "k".repeat(KEY_MAX_BYTES);
        assert!(engine.set(key.clone(), "v".into()).is_ok());
        assert_eq!(engine.get(&key).unwrap(), Some("v".into()));
    }

    #[test]
    fn test_set_rejects_oversized_value() {
        let engine = KvEngine::new();
        let big_value = "v".repeat(VALUE_MAX_BYTES + 1);
        let err = engine.set("k".into(), big_value).unwrap_err();
        assert!(matches!(
            err,
            FerrumError::ValueTooLarge {
                len,
                max: VALUE_MAX_BYTES,
            } if len == VALUE_MAX_BYTES + 1
        ));
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
                engine.set(format!("key{i}"), format!("value{i}")).unwrap();
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
