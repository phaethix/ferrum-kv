use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use crate::error::FerrumError;
use crate::persistence::AofWriter;

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
/// Mutating commands (`SET`, `DEL`, `FLUSHDB`) are optionally forwarded to an
/// [`AofWriter`] so changes survive a restart. The log is appended while the
/// write lock is still held, which preserves the ordering invariant described
/// in the whitepaper (§8.7): the in-memory state and the on-disk log always
/// agree on the relative order of successful writes.
///
/// Public methods return [`Result`] so lock poisoning can be reported instead
/// of causing a panic.
#[derive(Clone)]
pub struct KvEngine {
    store: Arc<RwLock<HashMap<String, String>>>,
    aof: Option<Arc<AofWriter>>,
}

impl Default for KvEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl KvEngine {
    /// Creates a new empty key-value engine without persistence.
    pub fn new() -> Self {
        Self {
            store: Arc::default(),
            aof: None,
        }
    }

    /// Attaches an AOF writer so subsequent mutating commands are persisted.
    ///
    /// The writer is shared via [`Arc`], allowing the same instance to be used
    /// across cloned engine handles.
    pub fn with_aof(mut self, writer: Arc<AofWriter>) -> Self {
        self.aof = Some(writer);
        self
    }

    /// Sets a key-value pair and returns the previous value, if any.
    ///
    /// Returns [`FerrumError::KeyTooLong`] or [`FerrumError::ValueTooLarge`] if
    /// the configured size limits are exceeded.
    pub fn set(&self, key: String, value: String) -> Result<Option<String>, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;

        let mut store = self.store.write()?;
        let previous = store.insert(key.clone(), value.clone());
        if let Some(aof) = &self.aof {
            log_aof_result("SET", aof.append_set(&key, &value));
        }
        Ok(previous)
    }

    /// Returns the value for `key`, or `None` if the key does not exist.
    pub fn get(&self, key: &str) -> Result<Option<String>, FerrumError> {
        let store = self.store.read()?;
        Ok(store.get(key).cloned())
    }

    /// Deletes `key` and returns `true` if it existed.
    pub fn del(&self, key: &str) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let existed = store.remove(key).is_some();
        if existed && let Some(aof) = &self.aof {
            log_aof_result("DEL", aof.append_del(key));
        }
        Ok(existed)
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
        if let Some(aof) = &self.aof {
            log_aof_result("FLUSHDB", aof.append_flushdb());
        }
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

/// Logs AOF append failures without failing the originating command.
///
/// The whitepaper (§7.2) specifies that persistence errors are best-effort:
/// in-memory state is authoritative during runtime and a failed AOF append is
/// reported but does not propagate to the client.
fn log_aof_result(cmd: &str, result: Result<(), FerrumError>) {
    if let Err(e) = result {
        eprintln!("[WARN] aof append for {cmd} failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persistence::AofWriter;
    use crate::persistence::config::{AofConfig, FsyncPolicy};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_aof_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ferrum-engine-{label}-{nanos}-{n}.aof"))
    }

    fn engine_with_aof(path: &PathBuf) -> (KvEngine, Arc<AofWriter>) {
        let cfg = AofConfig::new(path, FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = KvEngine::new().with_aof(Arc::clone(&writer));
        (engine, writer)
    }

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

    #[test]
    fn mutating_commands_are_appended_to_aof() {
        let path = tmp_aof_path("mutating");
        let (engine, writer) = engine_with_aof(&path);

        engine.set("a".into(), "1".into()).unwrap();
        engine.del("a").unwrap();
        engine.flushdb().unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
            "*1\r\n$7\r\nFLUSHDB\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_only_commands_do_not_touch_aof() {
        let path = tmp_aof_path("readonly");
        let (engine, writer) = engine_with_aof(&path);

        engine.get("missing").unwrap();
        engine.exists("missing").unwrap();
        engine.dbsize().unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.is_empty());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn del_of_missing_key_is_not_logged() {
        let path = tmp_aof_path("del-missing");
        let (engine, writer) = engine_with_aof(&path);

        assert!(!engine.del("missing").unwrap());
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.is_empty());
        let _ = fs::remove_file(&path);
    }
}
