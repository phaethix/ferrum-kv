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
        engine.set("name".into(), "ferrum".into()).unwrap();
        engine.set("lang".into(), "rust".into()).unwrap();
        engine.del("lang").unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    let stats = replay(&path, &restored).unwrap();
    assert_eq!(stats.applied, 3);
    assert_eq!(stats.skipped, 0);
    assert!(!stats.truncated_tail);

    assert_eq!(restored.get("name").unwrap(), Some("ferrum".into()));
    assert_eq!(restored.get("lang").unwrap(), None);
    assert_eq!(restored.dbsize().unwrap(), 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn writes_survive_restart_with_everysec_policy() {
    let path = tmp_path("restart-everysec");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::EverySec);
        for i in 0..20 {
            engine.set(format!("k{i}"), format!("v{i}")).unwrap();
        }
        drop(engine);
        // Dropping the writer flushes + fsyncs before returning.
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    for i in 0..20 {
        assert_eq!(
            restored.get(&format!("k{i}")).unwrap(),
            Some(format!("v{i}"))
        );
    }

    let _ = fs::remove_file(&path);
}

#[test]
fn writes_survive_restart_with_no_policy() {
    let path = tmp_path("restart-no");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::No);
        engine.set("k".into(), "v".into()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get("k").unwrap(), Some("v".into()));

    let _ = fs::remove_file(&path);
}

#[test]
fn flushdb_is_replayed_and_resets_state() {
    let path = tmp_path("flushdb");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set("a".into(), "1".into()).unwrap();
        engine.set("b".into(), "2".into()).unwrap();
        engine.flushdb().unwrap();
        engine.set("c".into(), "3".into()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get("a").unwrap(), None);
    assert_eq!(restored.get("b").unwrap(), None);
    assert_eq!(restored.get("c").unwrap(), Some("3".into()));
    assert_eq!(restored.dbsize().unwrap(), 1);

    let _ = fs::remove_file(&path);
}

#[test]
fn read_only_commands_do_not_grow_the_log() {
    let path = tmp_path("readonly");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.get("missing").unwrap();
        engine.exists("missing").unwrap();
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
    let tricky = "line1\r\nline2\r\n";
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set("k".into(), tricky.into()).unwrap();
        drop(engine);
        drop(writer);
    }

    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    assert_eq!(restored.get("k").unwrap(), Some(tricky.into()));

    let _ = fs::remove_file(&path);
}

#[test]
fn partial_tail_is_recovered_and_file_is_truncated() {
    let path = tmp_path("partial-tail");
    {
        let (engine, writer) = engine_with_writer(&path, FsyncPolicy::Always);
        engine.set("a".into(), "1".into()).unwrap();
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
    assert_eq!(restored.get("a").unwrap(), Some("1".into()));
    assert_eq!(restored.get("b").unwrap(), None);

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
        engine.set("a".into(), "1".into()).unwrap();
        drop(engine);
        drop(writer);
    }

    // Simulate server restart: replay, then reopen writer in append mode.
    let restored = KvEngine::new();
    replay(&path, &restored).unwrap();
    let (engine, writer) = {
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());
        let engine = restored.with_aof(Arc::clone(&writer));
        (engine, writer)
    };
    engine.set("b".into(), "2".into()).unwrap();
    drop(engine);
    drop(writer);

    // Final replay should see exactly two commands, no duplicates.
    let final_engine = KvEngine::new();
    let stats = replay(&path, &final_engine).unwrap();
    assert_eq!(stats.applied, 2);
    assert_eq!(final_engine.get("a").unwrap(), Some("1".into()));
    assert_eq!(final_engine.get("b").unwrap(), Some("2".into()));

    let _ = fs::remove_file(&path);
}
