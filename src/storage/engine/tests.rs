//! Unit tests for the storage engine, extracted from the main engine file so
//! the command API stays readable.
//!
//! These tests exercise the public `KvEngine` API plus a handful of
//! white-box assertions that reach into the store field directly. For that
//! reason `KvEngine`'s fields are `pub(super)` — visible to this test module
//! and the rest of the `engine` submodule, but not to the wider crate.

use super::*;
use crate::error::FerrumError;
use crate::persistence::AofWriter;
use crate::persistence::config::{AofConfig, FsyncPolicy};
use crate::storage::engine::types::TtlStatus;
use crate::storage::engine::util::current_epoch_ms;
use crate::storage::eviction::{EvictionConfig, EvictionPolicy};
use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
fn append_to_missing_key_creates_it() {
    let engine = KvEngine::new();
    let len = engine.append(b"k".to_vec(), b"hello".to_vec()).unwrap();
    assert_eq!(len, 5);
    assert_eq!(engine.get(b"k").unwrap(), Some(b"hello".to_vec()));
}

#[test]
fn append_extends_existing_value() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"hello ".to_vec()).unwrap();
    let len = engine.append(b"k".to_vec(), b"world".to_vec()).unwrap();
    assert_eq!(len, 11);
    assert_eq!(engine.get(b"k").unwrap(), Some(b"hello world".to_vec()));
}

#[test]
fn append_respects_value_size_limit() {
    let engine = KvEngine::new();
    let big = vec![b'x'; VALUE_MAX_BYTES];
    engine.set(b"k".to_vec(), big).unwrap();
    let err = engine.append(b"k".to_vec(), vec![b'y']).unwrap_err();
    assert!(matches!(err, FerrumError::ValueTooLarge { .. }));
}

#[test]
fn append_persists_final_state_to_aof() {
    let path = tmp_aof_path("append");
    let (engine, writer) = engine_with_aof(&path);

    engine.append(b"k".to_vec(), b"hello ".to_vec()).unwrap();
    engine.append(b"k".to_vec(), b"world".to_vec()).unwrap();
    drop(engine);
    drop(writer);

    // Each APPEND is logged as a SET carrying the new full value so
    // replay converges regardless of the order records are applied in.
    let bytes = fs::read(&path).unwrap();
    let expected = concat!(
        "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$6\r\nhello \r\n",
        "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$11\r\nhello world\r\n",
    );
    assert_eq!(bytes, expected.as_bytes());
    let _ = fs::remove_file(&path);
}

#[test]
fn strlen_returns_zero_for_missing_key() {
    let engine = KvEngine::new();
    assert_eq!(engine.strlen(b"missing").unwrap(), 0);
}

#[test]
fn strlen_counts_raw_bytes() {
    let engine = KvEngine::new();
    engine
        .set(b"k".to_vec(), vec![0x00, 0xff, b'a', b'b'])
        .unwrap();
    assert_eq!(engine.strlen(b"k").unwrap(), 4);
}

#[test]
fn set_nx_inserts_when_key_is_absent() {
    let engine = KvEngine::new();
    assert!(engine.set_nx(b"k".to_vec(), b"v1".to_vec()).unwrap());
    assert_eq!(engine.get(b"k").unwrap(), Some(b"v1".to_vec()));
}

#[test]
fn set_nx_is_noop_when_key_exists() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"original".to_vec()).unwrap();
    assert!(!engine.set_nx(b"k".to_vec(), b"other".to_vec()).unwrap());
    assert_eq!(engine.get(b"k").unwrap(), Some(b"original".to_vec()));
}

#[test]
fn set_nx_only_logs_successful_inserts_to_aof() {
    let path = tmp_aof_path("setnx");
    let (engine, writer) = engine_with_aof(&path);

    assert!(engine.set_nx(b"k".to_vec(), b"v".to_vec()).unwrap());
    assert!(!engine.set_nx(b"k".to_vec(), b"other".to_vec()).unwrap());
    drop(engine);
    drop(writer);

    let bytes = fs::read(&path).unwrap();
    assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
    let _ = fs::remove_file(&path);
}

#[test]
fn mset_inserts_every_pair() {
    let engine = KvEngine::new();
    engine
        .mset(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ])
        .unwrap();
    assert_eq!(engine.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
}

#[test]
fn mset_rejects_batch_without_applying_any_pair() {
    let engine = KvEngine::new();
    engine.set(b"existing".to_vec(), b"keep".to_vec()).unwrap();

    let too_big = vec![b'x'; VALUE_MAX_BYTES + 1];
    let err = engine
        .mset(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), too_big),
        ])
        .unwrap_err();
    assert!(matches!(err, FerrumError::ValueTooLarge { .. }));
    // Neither pair made it into the store because validation happens
    // before any mutation.
    assert_eq!(engine.get(b"a").unwrap(), None);
    assert_eq!(engine.get(b"b").unwrap(), None);
    assert_eq!(engine.get(b"existing").unwrap(), Some(b"keep".to_vec()));
}

#[test]
fn mset_writes_batch_atomically_to_aof() {
    let path = tmp_aof_path("mset");
    let (engine, writer) = engine_with_aof(&path);

    engine
        .mset(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"b".to_vec(), b"2".to_vec()),
        ])
        .unwrap();
    drop(engine);
    drop(writer);

    let bytes = fs::read(&path).unwrap();
    let expected = concat!(
        "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
        "*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n",
    );
    assert_eq!(bytes, expected.as_bytes());
    let _ = fs::remove_file(&path);
}

#[test]
fn mget_returns_values_in_request_order() {
    let engine = KvEngine::new();
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();

    let values = engine
        .mget(&[b"a".to_vec(), b"missing".to_vec(), b"c".to_vec()])
        .unwrap();
    assert_eq!(values, vec![Some(b"1".to_vec()), None, Some(b"3".to_vec())]);
}

#[test]
fn incr_by_initialises_missing_key_from_zero() {
    let engine = KvEngine::new();
    assert_eq!(engine.incr_by(b"counter".to_vec(), 1).unwrap(), 1);
    assert_eq!(engine.incr_by(b"counter".to_vec(), 4).unwrap(), 5);
    assert_eq!(engine.get(b"counter").unwrap(), Some(b"5".to_vec()));
}

#[test]
fn incr_by_supports_negative_delta() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"10".to_vec()).unwrap();
    assert_eq!(engine.incr_by(b"k".to_vec(), -3).unwrap(), 7);
}

#[test]
fn incr_by_rejects_non_integer_value() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"not a number".to_vec()).unwrap();
    let err = engine.incr_by(b"k".to_vec(), 1).unwrap_err();
    assert!(matches!(err, FerrumError::ParseError(ref m) if m.contains("integer")));
}

#[test]
fn incr_by_reports_overflow_as_parse_error() {
    let engine = KvEngine::new();
    engine
        .set(b"k".to_vec(), i64::MAX.to_string().into_bytes())
        .unwrap();
    let err = engine.incr_by(b"k".to_vec(), 1).unwrap_err();
    assert!(matches!(err, FerrumError::ParseError(ref m) if m.contains("integer")));
}

#[test]
fn incr_by_persists_new_integer_to_aof() {
    let path = tmp_aof_path("incrby");
    let (engine, writer) = engine_with_aof(&path);

    engine.incr_by(b"counter".to_vec(), 5).unwrap();
    engine.incr_by(b"counter".to_vec(), -2).unwrap();
    drop(engine);
    drop(writer);

    let bytes = fs::read(&path).unwrap();
    let expected = concat!(
        "*3\r\n$3\r\nSET\r\n$7\r\ncounter\r\n$1\r\n5\r\n",
        "*3\r\n$3\r\nSET\r\n$7\r\ncounter\r\n$1\r\n3\r\n",
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
fn exists_many_counts_live_keys_and_deduplicates() {
    let engine = KvEngine::new();
    engine.set(b"k1".to_vec(), b"v".to_vec()).unwrap();
    engine.set(b"k3".to_vec(), b"v".to_vec()).unwrap();
    // k1 and k3 exist; k2 missing; k1 duplicated — Redis counts
    // duplicates, so the total is 3.
    let keys: Vec<Vec<u8>> = vec![
        b"k1".to_vec(),
        b"k2".to_vec(),
        b"k3".to_vec(),
        b"k1".to_vec(),
    ];
    assert_eq!(engine.exists_many(&keys).unwrap(), 3);
}

#[test]
fn exists_many_lazily_expires_keys() {
    let engine = KvEngine::new();
    engine.set(b"k1".to_vec(), b"v".to_vec()).unwrap();
    // Expire k1 in the past — it should be lazily dropped and not counted.
    engine.expire_at_ms(b"k1", 1).unwrap();
    assert_eq!(
        engine
            .exists_many(&[b"k1".to_vec(), b"k2".to_vec()])
            .unwrap(),
        0
    );
}

#[test]
fn exists_many_empty_input_returns_zero() {
    let engine = KvEngine::new();
    assert_eq!(engine.exists_many(&[]).unwrap(), 0);
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

#[test]
fn expire_at_ms_sets_and_ttl_reports_remaining() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();

    let now = current_epoch_ms();
    assert!(engine.expire_at_ms(b"k", now + 60_000).unwrap());

    match engine.ttl_ms(b"k").unwrap() {
        TtlStatus::Millis(ms) => assert!(ms > 0 && ms <= 60_000),
        other => panic!("expected Millis(..), got {other:?}"),
    }
}

#[test]
fn expire_at_ms_returns_false_for_missing_key() {
    let engine = KvEngine::new();
    let now = current_epoch_ms();
    assert!(!engine.expire_at_ms(b"missing", now + 1000).unwrap());
}

#[test]
fn expire_in_the_past_deletes_key_immediately() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();

    let now = current_epoch_ms();
    assert!(engine.expire_at_ms(b"k", now - 1).unwrap());
    assert_eq!(engine.get(b"k").unwrap(), None);
    assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::Missing));
}

#[test]
fn persist_strips_ttl_only_when_present() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    assert!(!engine.persist(b"k").unwrap());

    let now = current_epoch_ms();
    engine.expire_at_ms(b"k", now + 10_000).unwrap();
    assert!(engine.persist(b"k").unwrap());
    assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
    assert!(!engine.persist(b"k").unwrap());
}

#[test]
fn ttl_status_for_missing_and_persistent_keys() {
    let engine = KvEngine::new();
    assert!(matches!(
        engine.ttl_ms(b"missing").unwrap(),
        TtlStatus::Missing
    ));
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
}

#[test]
fn lazy_expiration_drops_key_on_read() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_millis(20);
    {
        let mut store = engine.store.write().unwrap();
        if let Some(entry) = store.get_mut(b"k".as_slice()) {
            entry.expire_at = Some(deadline);
        }
    }
    std::thread::sleep(Duration::from_millis(40));
    assert_eq!(engine.get(b"k").unwrap(), None);
    assert!(!engine.exists(b"k").unwrap());
    assert_eq!(engine.dbsize().unwrap(), 0);
}

#[test]
fn set_overwrite_clears_existing_ttl() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let now = current_epoch_ms();
    engine.expire_at_ms(b"k", now + 60_000).unwrap();
    engine.set(b"k".to_vec(), b"v2".to_vec()).unwrap();
    assert!(matches!(engine.ttl_ms(b"k").unwrap(), TtlStatus::NoExpire));
}

#[test]
fn incr_preserves_ttl() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"1".to_vec()).unwrap();
    let now = current_epoch_ms();
    engine.expire_at_ms(b"k", now + 60_000).unwrap();
    engine.incr_by(b"k".to_vec(), 5).unwrap();
    match engine.ttl_ms(b"k").unwrap() {
        TtlStatus::Millis(ms) => assert!(ms > 0),
        other => panic!("expected Millis(..), got {other:?}"),
    }
}

#[test]
fn append_preserves_ttl() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"hi".to_vec()).unwrap();
    let now = current_epoch_ms();
    engine.expire_at_ms(b"k", now + 60_000).unwrap();
    engine.append(b"k".to_vec(), b"!".to_vec()).unwrap();
    match engine.ttl_ms(b"k").unwrap() {
        TtlStatus::Millis(ms) => assert!(ms > 0),
        other => panic!("expected Millis(..), got {other:?}"),
    }
}

#[test]
fn expire_logs_pexpireat_record_to_aof() {
    let path = tmp_aof_path("expire");
    let (engine, writer) = engine_with_aof(&path);

    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let abs_ms = current_epoch_ms() + 60_000;
    engine.expire_at_ms(b"k", abs_ms).unwrap();
    drop(engine);
    drop(writer);

    let bytes = fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("PEXPIREAT"),
        "missing PEXPIREAT record in {text:?}"
    );
    assert!(text.contains(&abs_ms.to_string()));
    let _ = fs::remove_file(&path);
}

#[test]
fn persist_logs_persist_record_to_aof() {
    let path = tmp_aof_path("persist");
    let (engine, writer) = engine_with_aof(&path);

    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let abs_ms = current_epoch_ms() + 60_000;
    engine.expire_at_ms(b"k", abs_ms).unwrap();
    assert!(engine.persist(b"k").unwrap());
    drop(engine);
    drop(writer);

    let bytes = fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains("PERSIST"),
        "missing PERSIST record in {text:?}"
    );
    let _ = fs::remove_file(&path);
}

#[test]
fn sweep_removes_expired_entries_and_logs_del() {
    let path = tmp_aof_path("sweep");
    let (engine, writer) = engine_with_aof(&path);

    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    {
        let mut store = engine.store.write().unwrap();
        if let Some(entry) = store.get_mut(b"k".as_slice()) {
            entry.expire_at = Some(Instant::now() - Duration::from_millis(1));
        }
    }

    let stats = engine.sweep_expired(16).unwrap();
    assert_eq!(stats.evicted, 1);
    assert_eq!(engine.dbsize().unwrap(), 0);

    drop(engine);
    drop(writer);
    let bytes = fs::read(&path).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains("DEL"));
    let _ = fs::remove_file(&path);
}

#[test]
fn sweep_respects_zero_sample() {
    let engine = KvEngine::new();
    let stats = engine.sweep_expired(0).unwrap();
    assert_eq!(stats.examined, 0);
    assert_eq!(stats.evicted, 0);
}

#[test]
fn used_memory_starts_at_zero_and_grows_on_set() {
    let engine = KvEngine::new();
    assert_eq!(engine.used_memory(), 0);
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let after_set = engine.used_memory();
    assert!(after_set >= 2 + PER_ENTRY_OVERHEAD);
}

#[test]
fn used_memory_adjusts_on_overwrite_and_delete() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"short".to_vec()).unwrap();
    let a = engine.used_memory();
    engine.set(b"k".to_vec(), vec![b'x'; 100]).unwrap();
    let b = engine.used_memory();
    assert!(b > a, "grow overwrite should increase used_memory");

    engine.del(b"k").unwrap();
    assert_eq!(engine.used_memory(), 0);
}

#[test]
fn flushdb_resets_used_memory() {
    let engine = KvEngine::new();
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    assert!(engine.used_memory() > 0);
    engine.flushdb().unwrap();
    assert_eq!(engine.used_memory(), 0);
}

#[test]
fn memory_usage_returns_per_entry_bytes_or_none() {
    let engine = KvEngine::new();
    assert_eq!(engine.memory_usage(b"absent").unwrap(), None);
    engine.set(b"k".to_vec(), b"value".to_vec()).unwrap();
    let reported = engine.memory_usage(b"k").unwrap().unwrap();
    assert_eq!(reported, 1 + 5 + PER_ENTRY_OVERHEAD);
}

#[test]
fn memory_usage_expires_key_lazily() {
    let engine = KvEngine::new();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    {
        let mut store = engine.store.write().unwrap();
        if let Some(entry) = store.get_mut(b"k".as_slice()) {
            entry.expire_at = Some(Instant::now() - Duration::from_millis(1));
        }
    }
    assert_eq!(engine.memory_usage(b"k").unwrap(), None);
    assert_eq!(engine.used_memory(), 0);
}

#[test]
fn used_memory_tracks_mset_and_del_many() {
    let engine = KvEngine::new();
    engine
        .mset(vec![
            (b"a".to_vec(), b"1".to_vec()),
            (b"bb".to_vec(), b"22".to_vec()),
        ])
        .unwrap();
    assert_eq!(
        engine.used_memory(),
        (1 + 1 + PER_ENTRY_OVERHEAD) + (2 + 2 + PER_ENTRY_OVERHEAD),
    );
    let removed = engine
        .del_many(&[b"a".to_vec(), b"bb".to_vec(), b"missing".to_vec()])
        .unwrap();
    assert_eq!(removed, 2);
    assert_eq!(engine.used_memory(), 0);
}

#[test]
fn noeviction_refuses_writes_past_max_memory() {
    let engine = KvEngine::new();
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: PER_ENTRY_OVERHEAD + 8,
            policy: EvictionPolicy::NoEviction,
            samples: 5,
        })
        .unwrap();
    engine.set(b"k".to_vec(), b"12345".to_vec()).unwrap();
    // Writing anything now overshoots: expect OOM without touching the
    // existing key.
    let err = engine.set(b"x".to_vec(), b"y".to_vec()).unwrap_err();
    assert!(matches!(err, FerrumError::OutOfMemory));
    assert_eq!(engine.get(b"k").unwrap(), Some(b"12345".to_vec()));
}

#[test]
fn allkeys_lru_evicts_least_recently_used_key() {
    let engine = KvEngine::new();
    // Room for exactly two entries.
    let max = 2 * (1 + 3 + PER_ENTRY_OVERHEAD);
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: max,
            policy: EvictionPolicy::AllKeysLru,
            samples: 10,
        })
        .unwrap();

    engine.set(b"a".to_vec(), b"AAA".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(5));
    engine.set(b"b".to_vec(), b"BBB".to_vec()).unwrap();
    std::thread::sleep(Duration::from_millis(5));

    // Touch `a` so it becomes the newest.
    engine.get(b"a").unwrap();
    std::thread::sleep(Duration::from_millis(5));

    // Third insert must evict: `b` is now the least recently used.
    engine.set(b"c".to_vec(), b"CCC".to_vec()).unwrap();
    assert_eq!(
        engine.get(b"b").unwrap(),
        None,
        "b should have been evicted"
    );
    assert!(engine.exists(b"a").unwrap());
    assert!(engine.exists(b"c").unwrap());
}

#[test]
fn volatile_lru_falls_back_to_oom_without_volatile_keys() {
    let engine = KvEngine::new();
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: PER_ENTRY_OVERHEAD + 2,
            policy: EvictionPolicy::VolatileLru,
            samples: 5,
        })
        .unwrap();
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    let err = engine.set(b"x".to_vec(), b"y".to_vec()).unwrap_err();
    assert!(matches!(err, FerrumError::OutOfMemory));
}

#[test]
fn volatile_ttl_evicts_soonest_expiring() {
    let engine = KvEngine::new();
    let max = 2 * (1 + 1 + PER_ENTRY_OVERHEAD);
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: max,
            policy: EvictionPolicy::VolatileTtl,
            samples: 10,
        })
        .unwrap();
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    let now = current_epoch_ms();
    engine.expire_at_ms(b"a", now + 10_000).unwrap();
    engine.expire_at_ms(b"b", now + 60_000).unwrap();

    engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();
    assert_eq!(
        engine.get(b"a").unwrap(),
        None,
        "shortest-TTL key should evict"
    );
    assert!(engine.exists(b"b").unwrap());
    assert!(engine.exists(b"c").unwrap());
}

#[test]
fn allkeys_random_picks_some_victim_under_pressure() {
    let engine = KvEngine::new();
    let max = 2 * (1 + 1 + PER_ENTRY_OVERHEAD);
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: max,
            policy: EvictionPolicy::AllKeysRandom,
            samples: 5,
        })
        .unwrap();
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();
    // One of the three must have been evicted.
    let survivors = [b"a", b"b", b"c"]
        .iter()
        .filter(|k| engine.exists(k.as_slice()).unwrap())
        .count();
    assert_eq!(survivors, 2);
}

#[test]
fn zero_max_memory_disables_enforcement() {
    let engine = KvEngine::new();
    for i in 0..100 {
        let k = format!("k{i}");
        engine.set(k.into_bytes(), vec![b'x'; 128]).unwrap();
    }
    assert_eq!(engine.dbsize().unwrap(), 100);
}

#[test]
fn allkeys_adaptiveclimb_evicts_lru_end() {
    let engine = KvEngine::new();
    let max = 2 * (1 + 3 + PER_ENTRY_OVERHEAD);
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: max,
            policy: EvictionPolicy::AllKeysAdaptiveClimb,
            samples: 5,
        })
        .unwrap();
    engine.set(b"a".to_vec(), b"AAA".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"BBB".to_vec()).unwrap();
    // Third insert overflows; with no hits the oldest key (a, now at the
    // LRU end) is evicted.
    engine.set(b"c".to_vec(), b"CCC".to_vec()).unwrap();
    assert_eq!(engine.get(b"a").unwrap(), None, "a should be evicted");
    assert!(engine.exists(b"b").unwrap());
    assert!(engine.exists(b"c").unwrap());
    assert_eq!(engine.dbsize().unwrap(), 2);
}

#[test]
fn allkeys_adaptiveclimb_hit_protects_key() {
    let engine = KvEngine::new();
    let max = 2 * (1 + 3 + PER_ENTRY_OVERHEAD);
    engine
        .set_eviction_config(EvictionConfig {
            max_memory: max,
            policy: EvictionPolicy::AllKeysAdaptiveClimb,
            samples: 5,
        })
        .unwrap();
    engine.set(b"a".to_vec(), b"AAA".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"BBB".to_vec()).unwrap();
    // Access `a` so it is promoted toward the MRU end.
    engine.get(b"a").unwrap();
    // Now the overflow evicts the LRU end (b), leaving the promoted `a`.
    engine.set(b"c".to_vec(), b"CCC".to_vec()).unwrap();
    assert!(engine.exists(b"a").unwrap());
    assert_eq!(engine.get(b"b").unwrap(), None, "b should be evicted");
    assert!(engine.exists(b"c").unwrap());
}

#[test]
fn scan_keys_glob_filtering() {
    let engine = KvEngine::new();
    engine.set(b"user:1".to_vec(), b"a".to_vec()).unwrap();
    engine.set(b"user:2".to_vec(), b"b".to_vec()).unwrap();
    engine
        .set(b"config:theme".to_vec(), b"dark".to_vec())
        .unwrap();
    engine.set(b"session:ab".to_vec(), b"c".to_vec()).unwrap();

    // '*' matches everything.
    assert_eq!(engine.scan_keys(b"*").unwrap().len(), 4);

    // Prefix wildcard.
    let users = engine.scan_keys(b"user:*").unwrap();
    assert_eq!(users.len(), 2);

    // Single-character wildcard.
    let sessions = engine.scan_keys(b"session:?b").unwrap();
    assert_eq!(sessions.len(), 1);

    // Character class.
    let either = engine.scan_keys(b"user:[12]").unwrap();
    assert_eq!(either.len(), 2);

    // No match.
    assert_eq!(engine.scan_keys(b"nope:*").unwrap().len(), 0);
}
