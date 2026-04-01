use ferrum_kv::network::server::execute_command;
use ferrum_kv::protocol::parser::{self, Command};
use ferrum_kv::storage::engine::KvEngine;

// ─── Parser + Engine integration tests ───

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
fn test_ping_command() {
    let cmd = parser::parse("PING").unwrap();
    assert_eq!(cmd, Command::Ping);
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

// ─── End-to-end command execution tests ───
// These test the full pipeline: raw input → parse → engine → response string

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
fn test_e2e_del_existing() {
    let engine = KvEngine::new();
    execute_command("SET key value", &engine);
    assert_eq!(execute_command("DEL key", &engine), "OK");
    assert_eq!(execute_command("GET key", &engine), "NULL");
}

#[test]
fn test_e2e_del_nonexistent() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("DEL missing", &engine), "NULL");
}

#[test]
fn test_e2e_ping() {
    let engine = KvEngine::new();
    assert_eq!(execute_command("PING", &engine), "PONG");
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
    assert_eq!(execute_command("dbsize", &engine), "1");
    assert_eq!(execute_command("flushdb", &engine), "OK");
}

// ─── Error handling tests ───

#[test]
fn test_e2e_unknown_command() {
    let engine = KvEngine::new();
    let resp = execute_command("FOOBAR", &engine);
    assert!(resp.starts_with("ERR unknown command:"));
}

#[test]
fn test_e2e_set_missing_value() {
    let engine = KvEngine::new();
    let resp = execute_command("SET key", &engine);
    assert!(resp.starts_with("ERR"));
    assert!(resp.contains("wrong number of arguments"));
}

#[test]
fn test_e2e_get_missing_key() {
    let engine = KvEngine::new();
    let resp = execute_command("GET", &engine);
    assert!(resp.starts_with("ERR"));
    assert!(resp.contains("wrong number of arguments"));
}

#[test]
fn test_e2e_del_missing_key() {
    let engine = KvEngine::new();
    let resp = execute_command("DEL", &engine);
    assert!(resp.starts_with("ERR"));
    assert!(resp.contains("wrong number of arguments"));
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

// ─── Concurrent access test ───

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
        assert_eq!(execute_command(&format!("GET k{i}"), &engine), format!("v{i}"));
    }
}
