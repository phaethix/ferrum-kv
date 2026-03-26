use ferrum_kv::protocol::parser::{self, Command};
use ferrum_kv::storage::engine::KvEngine;

#[test]
fn test_set_then_get_via_engine() {
    let engine = KvEngine::new();

    let cmd = parser::parse("SET greeting hello").unwrap();
    if let Command::Set { key, value } = cmd {
        engine.set(key, value);
    }

    let cmd = parser::parse("GET greeting").unwrap();
    if let Command::Get { key } = cmd {
        assert_eq!(engine.get(&key), Some("hello".to_string()));
    }
}

#[test]
fn test_del_via_engine() {
    let engine = KvEngine::new();
    engine.set("key".to_string(), "value".to_string());

    let cmd = parser::parse("DEL key").unwrap();
    if let Command::Del { key } = cmd {
        assert!(engine.del(&key));
        assert_eq!(engine.get(&key), None);
    }
}

#[test]
fn test_ping_command() {
    let cmd = parser::parse("PING").unwrap();
    assert_eq!(cmd, Command::Ping);
}
