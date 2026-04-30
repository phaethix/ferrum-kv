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
/// Keys and values are stored as `Vec<u8>`, making the engine fully
/// binary-safe: NUL bytes, bytes above 0x7F, and embedded CRLF are all
/// preserved verbatim. This matches the contract of the RESP2 bulk string,
/// which is already byte-oriented on the wire.
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
    store: Arc<RwLock<HashMap<Vec<u8>, Vec<u8>>>>,
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
    pub fn set(&self, key: Vec<u8>, value: Vec<u8>) -> Result<Option<Vec<u8>>, FerrumError> {
        validate_key(&key)?;
        validate_value(&value)?;

        let mut store = self.store.write()?;
        if let Some(aof) = &self.aof {
            log_aof_result("SET", aof.append_set(&key, &value));
        }
        let previous = store.insert(key, value);
        Ok(previous)
    }

    /// Returns the value for `key`, or `None` if the key does not exist.
    pub fn get(&self, key: &[u8]) -> Result<Option<Vec<u8>>, FerrumError> {
        let store = self.store.read()?;
        Ok(store.get(key).cloned())
    }

    /// Deletes `key` and returns `true` if it existed.
    pub fn del(&self, key: &[u8]) -> Result<bool, FerrumError> {
        let mut store = self.store.write()?;
        let existed = store.remove(key).is_some();
        if existed && let Some(aof) = &self.aof {
            log_aof_result("DEL", aof.append_del(key));
        }
        Ok(existed)
    }

    /// Deletes every key in `keys` and returns the count of keys that
    /// actually existed.
    ///
    /// The write lock is held for the entire batch so the operation is
    /// atomic from an observer's point of view: concurrent readers see
    /// either all deletions or none of them. Persisted log records are
    /// appended only for keys that were actually removed, mirroring
    /// Redis' behaviour.
    pub fn del_many(&self, keys: &[Vec<u8>]) -> Result<usize, FerrumError> {
        if keys.is_empty() {
            return Ok(0);
        }
        let mut store = self.store.write()?;
        let mut removed: Vec<&[u8]> = Vec::with_capacity(keys.len());
        for key in keys {
            if store.remove(key.as_slice()).is_some() {
                removed.push(key.as_slice());
            }
        }
        if let Some(aof) = &self.aof {
            for key in &removed {
                log_aof_result("DEL", aof.append_del(key));
            }
        }
        Ok(removed.len())
    }

    /// Returns `true` if `key` exists.
    pub fn exists(&self, key: &[u8]) -> Result<bool, FerrumError> {
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

fn validate_key(key: &[u8]) -> Result<(), FerrumError> {
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

fn validate_value(value: &[u8]) -> Result<(), FerrumError> {
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
        engine.set(b"name".to_vec(), b"ferrum".to_vec()).unwrap();
        assert_eq!(engine.get(b"name").unwrap(), Some(b"ferrum".to_vec()));
    }

    #[test]
    fn test_get_nonexistent() {
        let engine = KvEngine::new();
        assert_eq!(engine.get(b"missing").unwrap(), None);
    }

    #[test]
    fn test_set_overwrite() {
        let engine = KvEngine::new();
        engine.set(b"key".to_vec(), b"v1".to_vec()).unwrap();
        let old = engine.set(b"key".to_vec(), b"v2".to_vec()).unwrap();
        assert_eq!(old, Some(b"v1".to_vec()));
        assert_eq!(engine.get(b"key").unwrap(), Some(b"v2".to_vec()));
    }

    #[test]
    fn test_del_existing() {
        let engine = KvEngine::new();
        engine.set(b"key".to_vec(), b"value".to_vec()).unwrap();
        assert!(engine.del(b"key").unwrap());
        assert_eq!(engine.get(b"key").unwrap(), None);
    }

    #[test]
    fn test_del_nonexistent() {
        let engine = KvEngine::new();
        assert!(!engine.del(b"missing").unwrap());
    }

    #[test]
    fn del_many_counts_existing_keys_only() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();

        let removed = engine
            .del_many(&[b"a".to_vec(), b"missing".to_vec(), b"b".to_vec()])
            .unwrap();
        assert_eq!(removed, 2);
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn del_many_with_empty_input_returns_zero() {
        let engine = KvEngine::new();
        assert_eq!(engine.del_many(&[]).unwrap(), 0);
    }

    #[test]
    fn del_many_logs_only_existing_keys_to_aof() {
        let path = tmp_aof_path("del-many");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine
            .del_many(&[b"a".to_vec(), b"missing".to_vec()])
            .unwrap();
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn test_exists() {
        let engine = KvEngine::new();
        assert!(!engine.exists(b"key").unwrap());
        engine.set(b"key".to_vec(), b"value".to_vec()).unwrap();
        assert!(engine.exists(b"key").unwrap());
        engine.del(b"key").unwrap();
        assert!(!engine.exists(b"key").unwrap());
    }

    #[test]
    fn test_dbsize_empty() {
        let engine = KvEngine::new();
        assert_eq!(engine.dbsize().unwrap(), 0);
    }

    #[test]
    fn test_dbsize_after_operations() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        assert_eq!(engine.dbsize().unwrap(), 2);
        engine.del(b"a").unwrap();
        assert_eq!(engine.dbsize().unwrap(), 1);
    }

    #[test]
    fn test_flushdb() {
        let engine = KvEngine::new();
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        engine.flushdb().unwrap();
        assert_eq!(engine.dbsize().unwrap(), 0);
        assert_eq!(engine.get(b"a").unwrap(), None);
    }

    #[test]
    fn test_set_rejects_empty_key() {
        let engine = KvEngine::new();
        let err = engine.set(Vec::new(), b"v".to_vec()).unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn test_set_rejects_oversized_key() {
        let engine = KvEngine::new();
        let big_key = vec![b'k'; KEY_MAX_BYTES + 1];
        let err = engine.set(big_key, b"v".to_vec()).unwrap_err();
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
        let key = vec![b'k'; KEY_MAX_BYTES];
        assert!(engine.set(key.clone(), b"v".to_vec()).is_ok());
        assert_eq!(engine.get(&key).unwrap(), Some(b"v".to_vec()));
    }

    #[test]
    fn test_set_rejects_oversized_value() {
        let engine = KvEngine::new();
        let big_value = vec![b'v'; VALUE_MAX_BYTES + 1];
        let err = engine.set(b"k".to_vec(), big_value).unwrap_err();
        assert!(matches!(
            err,
            FerrumError::ValueTooLarge {
                len,
                max: VALUE_MAX_BYTES,
            } if len == VALUE_MAX_BYTES + 1
        ));
    }

    #[test]
    fn binary_safe_key_and_value_round_trip() {
        let engine = KvEngine::new();
        let key: Vec<u8> = vec![0x00, 0x01, 0xff, 0xfe];
        let value: Vec<u8> = vec![0x80, 0x00, b'\r', b'\n', 0xc3, 0x28];
        engine.set(key.clone(), value.clone()).unwrap();
        assert_eq!(engine.get(&key).unwrap(), Some(value));
        assert!(engine.exists(&key).unwrap());
        assert!(engine.del(&key).unwrap());
        assert_eq!(engine.get(&key).unwrap(), None);
    }

    #[test]
    fn test_concurrent_access() {
        use std::thread;

        let engine = KvEngine::new();
        let mut handles = vec![];

        for i in 0..10 {
            let engine = engine.clone();
            handles.push(thread::spawn(move || {
                engine
                    .set(
                        format!("key{i}").into_bytes(),
                        format!("value{i}").into_bytes(),
                    )
                    .unwrap();
            }));
        }

        for handle in handles {
            handle.join().unwrap();
        }

        for i in 0..10 {
            assert_eq!(
                engine.get(format!("key{i}").as_bytes()).unwrap(),
                Some(format!("value{i}").into_bytes())
            );
        }
    }

    #[test]
    fn mutating_commands_are_appended_to_aof() {
        let path = tmp_aof_path("mutating");
        let (engine, writer) = engine_with_aof(&path);

        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.del(b"a").unwrap();
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

        engine.get(b"missing").unwrap();
        engine.exists(b"missing").unwrap();
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

        assert!(!engine.del(b"missing").unwrap());
        drop(engine);
        drop(writer);

        let bytes = fs::read(&path).unwrap();
        assert!(bytes.is_empty());
        let _ = fs::remove_file(&path);
    }
}
