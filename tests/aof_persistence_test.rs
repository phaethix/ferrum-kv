//! End-to-end tests for the AOF subsystem.
//!
//! These tests exercise the full persistence lifecycle from the public API:
//! write commands through [`KvEngine`] with an [`AofWriter`] attached, close
//! the writer, and then replay the log into a fresh engine to confirm the
//! state is recovered exactly.

use std::fs;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use ferrum_kv::persistence::AofWriter;
use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};
use ferrum_kv::persistence::replay;
use ferrum_kv::storage::engine::KvEngine;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_path(label: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let n = COUNTER.fetch_add(1, Ordering::SeqCst);
    std::env::temp_dir().join(format!("ferrum-aof-it-{label}-{nanos}-{n}.aof"))
}

fn engine_with_writer(path: &PathBuf, policy: FsyncPolicy) -> (KvEngine, Arc<AofWriter>) {
    let cfg = AofConfig::new(path, policy);
    let writer = Arc::new(AofWriter::open(&cfg).unwrap());
    let engine = KvEngine::new().with_aof(Arc::clone(&writer));
    (engine, writer)
}

#[test]
fn writes_survive_restart_with_always_policy() {
    let path = tmp_path("restart-always");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(b"name".to_vec(), b"ferrum".to_vec()).unwrap();
        engine.set(b"lang".to_vec(), b"rust".to_vec()).unwrap();
        engine.del(b"lang").unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    let stats = replay(&path, &restored).unwrap();
    assert_eq!(stats.applied, 3);
    assert_eq!(stats.skipped, 0);
    assert!(!stats.truncated_tail);

    assert_eq!(restored.get(b"name").unwrap(), Some(b"ferrum".to_vec()));
    assert_eq!(restored.get(b"lang").unwrap(), None);
    assert_eq!(restored.dbsize().unwrap(), 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn writes_survive_restart_with_everysec_policy() {
    let path = tmp_path("restart-everysec");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::EverySec);
        for i in 0..20 {
            engine
                .set(format!("k{i}").into_bytes(), format!("v{i}").into_bytes())
                .unwrap();
        }
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    for i in 0..20 {
        assert_eq!(
            restored.get(format!("k{i}").as_bytes()).unwrap(),
            Some(format!("v{i}").into_bytes())
        );
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn writes_survive_restart_with_no_policy() {
    let path = tmp_path("restart-no");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::No);
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get(b"k").unwrap(), Some(b"v".to_vec()));

    let _ = fs::remove_file(&path);
}

#[test]
fn flushdb_is_replayed_and_resets_state() {
    let path = tmp_path("flushdb");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
        engine.flushdb().unwrap();
        engine.set(b"c".to_vec(), b"3".to_vec()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get(b"a").unwrap(), None);
    assert_eq!(restored.get(b"b").unwrap(), None);
    assert_eq!(restored.get(b"c").unwrap(), Some(b"3".to_vec()));
    assert_eq!(restored.dbsize().unwrap(), 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn read_only_commands_do_not_grow_the_log() {
    let path = tmp_path("readonly");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.get(b"missing").unwrap();
        engine.exists(b"missing").unwrap();
        engine.dbsize().unwrap();
        drop(engine);
        drop(writer);
    }

    let bytes = fs::read(&path).unwrap();
    assert!(
        bytes.is_empty(),
        "read-only commands must not append to AOF"
    );
    let _ = fs::remove_file(&path);
}

#[test]
fn values_containing_crlf_round_trip_through_aof() {
    let path = tmp_path("crlf");
    let tricky: &[u8] = b"line1\r\nline2\r\n";
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(b"k".to_vec(), tricky.to_vec()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get(b"k").unwrap(), Some(tricky.to_vec()));

    let _ = fs::remove_file(&path);
}

#[test]
fn binary_keys_and_values_round_trip_through_aof() {
    let path = tmp_path("binary");
    let key: Vec<u8> = vec![0x00, 0xff, 0x01, b'\r', b'\n'];
    let value: Vec<u8> = vec![0x80, 0x00, 0xc3, 0x28, 0xfe, 0x01];
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(key.clone(), value.clone()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    let stats = replay(&path, &restored).unwrap();
    assert_eq!(stats.applied, 1);
    assert_eq!(restored.get(&key).unwrap(), Some(value));

    let _ = fs::remove_file(&path);
}

#[test]
fn partial_tail_is_recovered_and_file_is_truncated() {
    let path = tmp_path("partial-tail");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        drop(engine);
        drop(writer);
    }

    // Simulate a crash mid-write by appending a truncated SET at the end.
    {
        use std::fs::OpenOptions;
        use std::io::Write;
        let mut f = OpenOptions::new().append(true).open(&path).unwrap();
        f.write_all(b"*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$5\r\nfoo")
            .unwrap();
    }

    let restored = KvEngine::new();
    let stats = replay(&path, &restored).unwrap();
    assert!(stats.truncated_tail);
    assert_eq!(stats.applied, 1);
    assert_eq!(restored.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(restored.get(b"b").unwrap(), None);

    // Second replay should be a clean pass over the truncated file.
    let second = KvEngine::new();
    let stats2 = replay(&path, &second).unwrap();
    assert!(!stats2.truncated_tail);
    assert_eq!(stats2.applied, 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn reopening_writer_does_not_duplicate_prior_records() {
    let path = tmp_path("reopen");

    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set(b"a".to_vec(), b"1".to_vec()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    let (engine, writer) = {
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = restored.with_aof(Arc::clone(&writer));
        (engine, writer)
    };
    engine.set(b"b".to_vec(), b"2".to_vec()).unwrap();
    drop(engine);
    drop(writer);

    let final_engine = KvEngine::new();
    let stats = replay(&path, &final_engine).unwrap();
    assert_eq!(stats.applied, 2);
    assert_eq!(final_engine.get(b"a").unwrap(), Some(b"1".to_vec()));
    assert_eq!(final_engine.get(b"b").unwrap(), Some(b"2".to_vec()));

    let _ = fs::remove_file(&path);
}
