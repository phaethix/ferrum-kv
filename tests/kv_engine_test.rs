//! Integration tests for the KV engine.
//!
//! The RESP2 wire protocol is tested separately via the encoder/parser unit
//! tests and the end-to-end TCP tests; this file deliberately exercises the
//! engine through its public API only, keeping storage semantics decoupled
//! from the protocol format.

use ferrum_kv::error::FerrumError;
use ferrum_kv::storage::engine::{KEY_MAX_BYTES, KvEngine, VALUE_MAX_BYTES};

#[test]
fn set_then_get_returns_stored_value() {
    let engine = KvEngine::new();
    engine.set(b"greeting".to_vec(), b"hello".to_vec()).unwrap();
    assert_eq!(engine.get(b"greeting").unwrap(), Some(b"hello".to_vec()));
}

#[test]
fn get_returns_none_for_missing_key() {
    let engine = KvEngine::new();
    assert_eq!(engine.get(b"missing").unwrap(), None);
}

#[test]
fn del_removes_existing_key_and_returns_true() {
    let engine = KvEngine::new();
    engine.set(b"key".to_vec(), b"value".to_vec()).unwrap();
    assert!(engine.del(b"key").unwrap());
    assert_eq!(engine.get(b"key").unwrap(), None);
}

#[test]
fn del_returns_false_for_missing_key() {
    let engine = KvEngine::new();
    assert!(!engine.del(b"missing").unwrap());
}

#[test]
fn exists_reflects_key_lifecycle() {
    let engine = KvEngine::new();
    assert!(!engine.exists(b"k").unwrap());
    engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
    assert!(engine.exists(b"k").unwrap());
    engine.del(b"k").unwrap();
    assert!(!engine.exists(b"k").unwrap());
}

#[test]
fn dbsize_counts_live_keys() {
    let engine = KvEngine::new();
    assert_eq!(engine.dbsize().unwrap(), 0);
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    assert_eq!(engine.dbsize().unwrap(), 2);
}

#[test]
fn flushdb_clears_all_keys() {
    let engine = KvEngine::new();
    engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    engine.flushdb().unwrap();
    assert_eq!(engine.dbsize().unwrap(), 0);
}

#[test]
fn values_may_contain_whitespace() {
    let engine = KvEngine::new();
    engine
        .set(b"msg".to_vec(), b"hello world".to_vec())
        .unwrap();
    assert_eq!(engine.get(b"msg").unwrap(), Some(b"hello world".to_vec()));
}

#[test]
fn binary_safe_keys_and_values_round_trip() {
    let engine = KvEngine::new();
    let key: Vec<u8> = vec![0x00, 0x01, 0xff, 0xfe];
    let value: Vec<u8> = vec![0x80, b'\r', b'\n', 0x00, 0xc3, 0x28];
    engine.set(key.clone(), value.clone()).unwrap();
    assert_eq!(engine.get(&key).unwrap(), Some(value));
    assert!(engine.exists(&key).unwrap());
    assert!(engine.del(&key).unwrap());
    assert_eq!(engine.get(&key).unwrap(), None);
}

#[test]
fn key_exceeding_limit_is_rejected() {
    let engine = KvEngine::new();
    let big_key = vec![b'k'; KEY_MAX_BYTES + 1];
    let err = engine.set(big_key, b"v".to_vec()).unwrap_err();
    assert!(matches!(err, FerrumError::KeyTooLong { .. }));
}

#[test]
fn value_exceeding_limit_is_rejected() {
    let engine = KvEngine::new();
    let big_value = vec![b'v'; VALUE_MAX_BYTES + 1];
    let err = engine.set(b"k".to_vec(), big_value).unwrap_err();
    assert!(matches!(err, FerrumError::ValueTooLarge { .. }));
}

#[test]
fn concurrent_writers_see_consistent_state() {
    use std::thread;

    let engine = KvEngine::new();
    let mut handles = Vec::new();
    for i in 0..50 {
        let engine = engine.clone();
        handles.push(thread::spawn(move || {
            engine
                .set(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(engine.dbsize().unwrap(), 50);
    for i in 0..50 {
        assert_eq!(
            engine.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }
}
