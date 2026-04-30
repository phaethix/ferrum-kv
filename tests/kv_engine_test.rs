use ferrum_kv::network::server::execute_command;
use ferrum_kv::protocol::parser::{self, Command};
use ferrum_kv::storage::engine::{KEY_MAX_BYTES, KvEngine, VALUE_MAX_BYTES};

// Parser and engine integration tests.

#[test]
fn test_set_then_get_via_engine() {
    let engine = KvEngine::new();

    let cmd = parser::parse("SET greeting hello").unwrap();
    let Command::Set { key, value } = cmd else {
        panic!("expected Set command")
    };
    engine.set(key, value).unwrap();

    let cmd = parser::parse("GET greeting").unwrap();
    let Command::Get { key } = cmd else {
        panic!("expected Get command")
    };
    assert_eq!(engine.get(&key).unwrap(), Some("hello".to_string()));
}

#[test]
fn test_del_via_engine() {
    let engine = KvEngine::new();
    engine.set("key".to_string(), "value".to_string()).unwrap();

    let cmd = parser::parse("DEL key").unwrap();
    let Command::Del { key } = cmd else {
        panic!("expected Del command")
    };
    assert!(engine.del(&key).unwrap());
    assert_eq!(engine.get(&key).unwrap(), None);
}

#[test]
fn test_exists_via_engine() {
    let engine = KvEngine::new();
    engine.set("key".into(), "value".into()).unwrap();

    let cmd = parser::parse("EXISTS key").unwrap();
    let Command::Exists { key } = cmd else {
        panic!("expected Exists command")
    };
    assert!(engine.exists(&key).unwrap());
}

#[test]
fn test_ping_command() {
    let cmd = parser::parse("PING").unwrap();
    assert_eq!(cmd, Command::Ping { msg: None });
}

#[test]
fn test_dbsize_via_engine() {
    let engine = KvEngine::new();
    assert_eq!(engine.dbsize().unwrap(), 0);

    engine.set("a".to_string(), "1".to_string()).unwrap();
    engine.set("b".to_string(), "2".to_string()).unwrap();
    assert_eq!(engine.dbsize().unwrap(), 2);
}

#[test]
fn test_flushdb_via_engine() {
    let engine = KvEngine::new();
    engine.set("a".to_string(), "1".to_string()).unwrap();
    engine.set("b".to_string(), "2".to_string()).unwrap();
    engine.flushdb().unwrap();
    assert_eq!(engine.dbsize().unwrap(), 0);
}

// End-to-end command execution tests.
// These tests cover the full pipeline: raw input, parsing, execution, and response formatting.

#[test]
fn test_e2e_set_get() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("SET name ferrum", &engine), "OK");
    assert_eq!(execute_command("GET name", &engine), "ferrum");
}

#[test]
fn test_e2e_get_nonexistent() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("GET missing", &engine), "NULL");
}

#[test]
fn test_e2e_del_existing_returns_one() {
    let engine = KvEngine::new();
    execute_command("SET key value", &engine);
    assert_eq!(execute_command("DEL key", &engine), "1");
    assert_eq!(execute_command("GET key", &engine), "NULL");
}

#[test]
fn test_e2e_del_nonexistent_returns_zero() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("DEL missing", &engine), "0");
}

#[test]
fn test_e2e_exists() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("EXISTS missing", &engine), "0");
    execute_command("SET k v", &engine);
    assert_eq!(execute_command("EXISTS k", &engine), "1");
    execute_command("DEL k", &engine);
    assert_eq!(execute_command("EXISTS k", &engine), "0");
}

#[test]
fn test_e2e_ping() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("PING", &engine), "PONG");
}

#[test]
fn test_e2e_ping_with_message() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("PING hello world", &engine), "hello world");
}

#[test]
fn test_e2e_dbsize() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("DBSIZE", &engine), "0");
    execute_command("SET a 1", &engine);
    execute_command("SET b 2", &engine);
    assert_eq!(execute_command("DBSIZE", &engine), "2");
}

#[test]
fn test_e2e_flushdb() {
    let engine = KvEngine::new();
    execute_command("SET a 1", &engine);
    execute_command("SET b 2", &engine);
    assert_eq!(execute_command("FLUSHDB", &engine), "OK");
    assert_eq!(execute_command("DBSIZE", &engine), "0");
}

#[test]
fn test_e2e_set_value_with_spaces() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("SET msg hello world", &engine), "OK");
    assert_eq!(execute_command("GET msg", &engine), "hello world");
}

#[test]
fn test_e2e_case_insensitive() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("set name ferrum", &engine), "OK");
    assert_eq!(execute_command("get name", &engine), "ferrum");
    assert_eq!(execute_command("ping", &engine), "PONG");
    assert_eq!(execute_command("exists name", &engine), "1");
    assert_eq!(execute_command("dbsize", &engine), "1");
    assert_eq!(execute_command("flushdb", &engine), "OK");
}

// Error handling tests.

#[test]
fn test_e2e_unknown_command() {
    let engine = KvEngine::new();
    let resp = execute_command("FOOBAR", &engine);
    assert_eq!(resp, "ERR unknown command: 'FOOBAR'");
}

#[test]
fn test_e2e_set_missing_value() {
    let engine = KvEngine::new();
    let resp = execute_command("SET key", &engine);
    assert_eq!(resp, "ERR wrong number of arguments for 'SET' command");
}

#[test]
fn test_e2e_get_missing_key() {
    let engine = KvEngine::new();
    let resp = execute_command("GET", &engine);
    assert_eq!(resp, "ERR wrong number of arguments for 'GET' command");
}

#[test]
fn test_e2e_del_missing_key() {
    let engine = KvEngine::new();
    let resp = execute_command("DEL", &engine);
    assert_eq!(resp, "ERR wrong number of arguments for 'DEL' command");
}

#[test]
fn test_e2e_exists_missing_key() {
    let engine = KvEngine::new();
    let resp = execute_command("EXISTS", &engine);
    assert_eq!(resp, "ERR wrong number of arguments for 'EXISTS' command");
}

#[test]
fn test_e2e_empty_input() {
    let engine = KvEngine::new();
    let resp = execute_command("", &engine);
    assert!(resp.starts_with("ERR"));
    assert!(resp.contains("empty command"));
}

#[test]
fn test_e2e_whitespace_only() {
    let engine = KvEngine::new();
    let resp = execute_command("   ", &engine);
    assert!(resp.starts_with("ERR"));
}

#[test]
fn test_e2e_set_key_too_long() {
    let engine = KvEngine::new();
    let big_key = "k".repeat(KEY_MAX_BYTES + 1);
    let resp = execute_command(&format!("SET {big_key} v"), &engine);
    assert!(resp.starts_with("ERR key too long"));
}

#[test]
fn test_e2e_set_value_too_large() {
    let engine = KvEngine::new();
    let big_value = "v".repeat(VALUE_MAX_BYTES + 1);
    let resp = execute_command(&format!("SET k {big_value}"), &engine);
    assert!(resp.starts_with("ERR value too large"));
}

// Concurrent access tests.

#[test]
fn test_e2e_concurrent_writes() {
    use std::thread;

    let engine = KvEngine::new();
    let mut handles = vec![];

    for i in 0..50 {
        let engine = engine.clone();
        handles.push(thread::spawn(move || {
            execute_command(&format!("SET k{i} v{i}"), &engine);
        }));
    }

    for handle in handles {
        handle.join().unwrap();
    }

    assert_eq!(execute_command("DBSIZE", &engine), "50");

    for i in 0..50 {
        assert_eq!(
            execute_command(&format!("GET k{i}"), &engine),
            format!("v{i}")
        );
    }
}
