use ferrum_kv::protocol::parser::{self, Command};
use ferrum_kv::storage::engine::KvEngine;

#[test]
fn test_set_then_get_via_engine() {
    let engine = KvEngine::new();

    let cmd = parser::parse("SET greeting hello").unwrap();
    let Command::Set { key, value } = cmd else {
        panic!("expected Set command")
    };
    engine.set(key, value);

    let cmd = parser::parse("GET greeting").unwrap();
    let Command::Get { key } = cmd else {
        panic!("expected Get command")
    };
    assert_eq!(engine.get(&key), Some("hello".to_string()));
}

#[test]
fn test_del_via_engine() {
    let engine = KvEngine::new();
    engine.set("key".to_string(), "value".to_string());

    let cmd = parser::parse("DEL key").unwrap();
    let Command::Del { key } = cmd else {
        panic!("expected Del command")
    };
    assert!(engine.del(&key));
    assert_eq!(engine.get(&key), None);
}

#[test]
fn test_ping_command() {
    let cmd = parser::parse("PING").unwrap();
    assert_eq!(cmd, Command::Ping);
}
