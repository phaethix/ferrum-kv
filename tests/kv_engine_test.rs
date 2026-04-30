//! Integration tests for the KV engine.
//!
//! The RESP2 wire protocol is tested separately via the encoder/parser unit
//! tests and the forthcoming end-to-end TCP tests; this file deliberately
//! exercises the engine through its public API only, keeping storage
//! semantics decoupled from the protocol format.

use ferrum_kv::error::FerrumError;
use ferrum_kv::storage::engine::{KEY_MAX_BYTES, KvEngine, VALUE_MAX_BYTES};

#[test]
fn set_then_get_returns_stored_value() {
    let engine = KvEngine::new();
    engine.set("greeting".into(), "hello".into()).unwrap();
    assert_eq!(engine.get("greeting").unwrap(), Some("hello".into()));
}

#[test]
fn get_returns_none_for_missing_key() {
    let engine = KvEngine::new();
    assert_eq!(engine.get("missing").unwrap(), None);
}

#[test]
fn del_removes_existing_key_and_returns_true() {
    let engine = KvEngine::new();
    engine.set("key".into(), "value".into()).unwrap();
    assert!(engine.del("key").unwrap());
    assert_eq!(engine.get("key").unwrap(), None);
}

#[test]
fn del_returns_false_for_missing_key() {
    let engine = KvEngine::new();
    assert!(!engine.del("missing").unwrap());
}

#[test]
fn exists_reflects_key_lifecycle() {
    let engine = KvEngine::new();
    assert!(!engine.exists("k").unwrap());
    engine.set("k".into(), "v".into()).unwrap();
    assert!(engine.exists("k").unwrap());
    engine.del("k").unwrap();
    assert!(!engine.exists("k").unwrap());
}

#[test]
fn dbsize_counts_live_keys() {
    let engine = KvEngine::new();
    assert_eq!(engine.dbsize().unwrap(), 0);
    engine.set("a".into(), "1".into()).unwrap();
    engine.set("b".into(), "2".into()).unwrap();
    assert_eq!(engine.dbsize().unwrap(), 2);
}

#[test]
fn flushdb_clears_all_keys() {
    let engine = KvEngine::new();
    engine.set("a".into(), "1".into()).unwrap();
    engine.set("b".into(), "2".into()).unwrap();
    engine.flushdb().unwrap();
    assert_eq!(engine.dbsize().unwrap(), 0);
}

#[test]
fn values_may_contain_whitespace() {
    let engine = KvEngine::new();
    engine.set("msg".into(), "hello world".into()).unwrap();
    assert_eq!(engine.get("msg").unwrap(), Some("hello world".into()));
}

#[test]
fn key_exceeding_limit_is_rejected() {
    let engine = KvEngine::new();
    let big_key = "k".repeat(KEY_MAX_BYTES + 1);
    let err = engine.set(big_key, "v".into()).unwrap_err();
    assert!(matches!(err, FerrumError::KeyTooLong { .. }));
}

#[test]
fn value_exceeding_limit_is_rejected() {
    let engine = KvEngine::new();
    let big_value = "v".repeat(VALUE_MAX_BYTES + 1);
    let err = engine.set("k".into(), big_value).unwrap_err();
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
            engine.set(format!("k{i}"), format!("v{i}")).unwrap();
        }));
    }
    for h in handles {
        h.join().unwrap();
    }

    assert_eq!(engine.dbsize().unwrap(), 50);
    for i in 0..50 {
        assert_eq!(engine.get(&format!("k{i}")).unwrap(), Some(format!("v{i}")));
    }
}
