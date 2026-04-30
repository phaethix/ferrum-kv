use crate::error::FerrumError;

/// Represents a parsed command received from a client.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `SET key value`
    Set { key: String, value: String },
    /// `GET key`
    Get { key: String },
    /// `DEL key`
    Del { key: String },
    /// `EXISTS key`
    Exists { key: String },
    /// `PING [message]`
    Ping { msg: Option<String> },
    /// `DBSIZE`, which returns the number of keys.
    DbSize,
    /// `FLUSHDB`, which removes all keys.
    FlushDb,
}

/// Parse a raw input line into a Command.
///
/// Commands are case-insensitive. Arguments are separated by a single space;
/// the `SET` command treats everything after the key as the value (so values
/// may contain inner spaces).
///
/// # Examples
/// ```
/// use ferrum_kv::protocol::parser::{parse, Command};
///
/// let cmd = parse("SET name ferrum").unwrap();
/// assert_eq!(cmd, Command::Set { key: "name".to_string(), value: "ferrum".to_string() });
/// ```
pub fn parse(input: &str) -> Result<Command, FerrumError> {
    let input = input.trim();

    if input.is_empty() {
        return Err(FerrumError::ParseError("empty command".into()));
    }

    // Inspect the command name first. `SET` requires a three-way split to keep
    // the value intact, while the remaining commands can use a generic splitter.
    let first_space = input.find(' ');
    let cmd_name = match first_space {
        Some(idx) => &input[..idx],
        None => input,
    };
    let cmd = cmd_name.to_uppercase();

    match cmd.as_str() {
        "SET" => {
            let parts: Vec<&str> = input.splitn(3, ' ').collect();
            if parts.len() < 3 || parts[1].is_empty() || parts[2].is_empty() {
                return Err(FerrumError::WrongArity { cmd: "SET" });
            }
            Ok(Command::Set {
                key: parts[1].to_string(),
                value: parts[2].to_string(),
            })
        }
        "GET" => {
            let key = single_key_arg(input, "GET")?;
            Ok(Command::Get { key })
        }
        "DEL" => {
            let key = single_key_arg(input, "DEL")?;
            Ok(Command::Del { key })
        }
        "EXISTS" => {
            let key = single_key_arg(input, "EXISTS")?;
            Ok(Command::Exists { key })
        }
        "PING" => {
            // `PING` accepts an optional message argument that is echoed back.
            match first_space {
                None => Ok(Command::Ping { msg: None }),
                Some(idx) => {
                    let rest = input[idx + 1..].trim_start();
                    if rest.is_empty() {
                        Ok(Command::Ping { msg: None })
                    } else {
                        Ok(Command::Ping {
                            msg: Some(rest.to_string()),
                        })
                    }
                }
            }
        }
        "DBSIZE" => {
            ensure_no_args(input, "DBSIZE")?;
            Ok(Command::DbSize)
        }
        "FLUSHDB" => {
            ensure_no_args(input, "FLUSHDB")?;
            Ok(Command::FlushDb)
        }
        other => Err(FerrumError::UnknownCommand(other.to_string())),
    }
}

/// Parses a single-key command (`GET`, `DEL`, or `EXISTS`).
fn single_key_arg(input: &str, cmd: &'static str) -> Result<String, FerrumError> {
    let parts: Vec<&str> = input.splitn(2, ' ').collect();
    if parts.len() < 2 || parts[1].trim().is_empty() {
        return Err(FerrumError::WrongArity { cmd });
    }
    let key = parts[1].trim();
    // Reject extra tokens such as `GET a b`.
    if key.contains(' ') {
        return Err(FerrumError::WrongArity { cmd });
    }
    Ok(key.to_string())
}

/// Validates that a no-argument command has no trailing tokens.
fn ensure_no_args(input: &str, cmd: &'static str) -> Result<(), FerrumError> {
    let trimmed_rest = match input.find(' ') {
        None => return Ok(()),
        Some(idx) => input[idx + 1..].trim(),
    };
    if trimmed_rest.is_empty() {
        Ok(())
    } else {
        Err(FerrumError::WrongArity { cmd })
    }
}

/// Formats a `Result` into the wire response sent back to the client.
///
/// Errors are prefixed by category:
/// - protocol or validation errors → `ERR ...`
/// - future type mismatches        → `WRONGTYPE ...`
/// - memory-limit rejections       → `OOM ...`
pub fn format_response(result: Result<String, FerrumError>) -> String {
    match result {
        Ok(msg) => msg,
        Err(FerrumError::OutOfMemory) => {
            "OOM command not allowed when used memory > maxmemory".to_string()
        }
        Err(e) => format!("ERR {e}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Tests for `SET`.

    #[test]
    fn test_parse_set() {
        let cmd = parse("SET name ferrum").unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: "name".to_string(),
                value: "ferrum".to_string()
            }
        );
    }

    #[test]
    fn test_parse_set_case_insensitive() {
        let cmd = parse("set Name Value").unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: "Name".to_string(),
                value: "Value".to_string()
            }
        );
    }

    #[test]
    fn test_parse_set_value_with_spaces() {
        let cmd = parse("SET key hello world").unwrap();
        assert_eq!(
            cmd,
            Command::Set {
                key: "key".to_string(),
                value: "hello world".to_string()
            }
        );
    }

    #[test]
    fn test_parse_set_missing_value() {
        let err = parse("SET key").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "SET" }));
    }

    #[test]
    fn test_parse_set_missing_args() {
        let err = parse("SET").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "SET" }));
    }

    // Tests for `GET`, `DEL`, and `EXISTS`.

    #[test]
    fn test_parse_get() {
        let cmd = parse("GET name").unwrap();
        assert_eq!(cmd, Command::Get { key: "name".into() });
    }

    #[test]
    fn test_parse_get_missing_key() {
        let err = parse("GET").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "GET" }));
    }

    #[test]
    fn test_parse_get_extra_args_rejected() {
        let err = parse("GET a b").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "GET" }));
    }

    #[test]
    fn test_parse_del() {
        let cmd = parse("DEL name").unwrap();
        assert_eq!(cmd, Command::Del { key: "name".into() });
    }

    #[test]
    fn test_parse_del_missing_key() {
        let err = parse("DEL").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "DEL" }));
    }

    #[test]
    fn test_parse_exists() {
        let cmd = parse("EXISTS name").unwrap();
        assert_eq!(cmd, Command::Exists { key: "name".into() });
    }

    #[test]
    fn test_parse_exists_case_insensitive() {
        let cmd = parse("exists NAME").unwrap();
        assert_eq!(cmd, Command::Exists { key: "NAME".into() });
    }

    #[test]
    fn test_parse_exists_missing_key() {
        let err = parse("EXISTS").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "EXISTS" }));
    }

    // Tests for `PING`.

    #[test]
    fn test_parse_ping() {
        let cmd = parse("PING").unwrap();
        assert_eq!(cmd, Command::Ping { msg: None });
    }

    #[test]
    fn test_parse_ping_case_insensitive() {
        let cmd = parse("ping").unwrap();
        assert_eq!(cmd, Command::Ping { msg: None });
    }

    #[test]
    fn test_parse_ping_with_message() {
        let cmd = parse("PING hello world").unwrap();
        assert_eq!(
            cmd,
            Command::Ping {
                msg: Some("hello world".into())
            }
        );
    }

    #[test]
    fn test_parse_ping_trailing_space_no_message() {
        let cmd = parse("PING   ").unwrap();
        assert_eq!(cmd, Command::Ping { msg: None });
    }

    // Tests for `DBSIZE` and `FLUSHDB`.

    #[test]
    fn test_parse_dbsize() {
        assert_eq!(parse("DBSIZE").unwrap(), Command::DbSize);
        assert_eq!(parse("dbsize").unwrap(), Command::DbSize);
    }

    #[test]
    fn test_parse_dbsize_rejects_args() {
        let err = parse("DBSIZE extra").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "DBSIZE" }));
    }

    #[test]
    fn test_parse_flushdb() {
        assert_eq!(parse("FLUSHDB").unwrap(), Command::FlushDb);
        assert_eq!(parse("flushdb").unwrap(), Command::FlushDb);
    }

    #[test]
    fn test_parse_flushdb_rejects_args() {
        let err = parse("FLUSHDB extra").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "FLUSHDB" }));
    }

    // Tests for unknown or empty input.

    #[test]
    fn test_parse_unknown_command() {
        let err = parse("FOOBAR").unwrap_err();
        assert!(matches!(err, FerrumError::UnknownCommand(ref c) if c == "FOOBAR"));
    }

    #[test]
    fn test_parse_empty_input() {
        assert!(parse("").is_err());
    }

    #[test]
    fn test_parse_whitespace_only() {
        assert!(parse("   ").is_err());
    }

    #[test]
    fn test_parse_with_leading_trailing_spaces() {
        let cmd = parse("  GET name  ").unwrap();
        assert_eq!(cmd, Command::Get { key: "name".into() });
    }

    // Tests for `format_response`.

    #[test]
    fn test_format_response_ok() {
        assert_eq!(format_response(Ok("OK".to_string())), "OK");
    }

    #[test]
    fn test_format_response_wrong_arity() {
        let out = format_response(Err(FerrumError::WrongArity { cmd: "SET" }));
        assert_eq!(out, "ERR wrong number of arguments for 'SET' command");
    }

    #[test]
    fn test_format_response_unknown_command() {
        let out = format_response(Err(FerrumError::UnknownCommand("FOO".into())));
        assert_eq!(out, "ERR unknown command: 'FOO'");
    }

    #[test]
    fn test_format_response_key_too_long() {
        let out = format_response(Err(FerrumError::KeyTooLong {
            len: 70000,
            max: 65536,
        }));
        assert_eq!(out, "ERR key too long (70000 bytes, max 65536)");
    }

    #[test]
    fn test_format_response_value_too_large() {
        let out = format_response(Err(FerrumError::ValueTooLarge {
            len: 20_000_000,
            max: 16_777_216,
        }));
        assert_eq!(out, "ERR value too large (20000000 bytes, max 16777216)");
    }

    #[test]
    fn test_format_response_oom() {
        let out = format_response(Err(FerrumError::OutOfMemory));
        assert_eq!(out, "OOM command not allowed when used memory > maxmemory");
    }

    #[test]
    fn test_format_response_lock_poisoned() {
        let out = format_response(Err(FerrumError::LockPoisoned("boom".into())));
        assert_eq!(out, "ERR lock poisoned: boom");
    }

    #[test]
    fn test_format_response_persistence_error() {
        let out = format_response(Err(FerrumError::PersistenceError("disk full".to_string())));
        assert_eq!(out, "ERR persistence error: disk full");
    }
}
