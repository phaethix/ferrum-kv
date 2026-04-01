use crate::error::FerrumError;

/// Represents a parsed client command
#[derive(Debug, PartialEq)]
pub enum Command {
    /// SET key value
    Set { key: String, value: String },
    /// GET key
    Get { key: String },
    /// DEL key
    Del { key: String },
    /// PING (health check)
    Ping,
    /// DBSIZE — return the number of keys
    DbSize,
    /// FLUSHDB — remove all keys
    FlushDb,
    /// Unknown or invalid command
    Unknown(String),
}

/// Parse a raw input line into a Command
///
/// Commands are case-insensitive. Arguments are separated by whitespace.
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

    let parts: Vec<&str> = input.splitn(3, ' ').collect();
    let cmd = parts[0].to_uppercase();

    match cmd.as_str() {
        "SET" => {
            if parts.len() < 3 {
                return Err(FerrumError::ParseError(
                    "wrong number of arguments for 'SET' command".into(),
                ));
            }
            Ok(Command::Set {
                key: parts[1].to_string(),
                value: parts[2].to_string(),
            })
        }
        "GET" => {
            if parts.len() < 2 {
                return Err(FerrumError::ParseError(
                    "wrong number of arguments for 'GET' command".into(),
                ));
            }
            Ok(Command::Get {
                key: parts[1].to_string(),
            })
        }
        "DEL" => {
            if parts.len() < 2 {
                return Err(FerrumError::ParseError(
                    "wrong number of arguments for 'DEL' command".into(),
                ));
            }
            Ok(Command::Del {
                key: parts[1].to_string(),
            })
        }
        "PING" => Ok(Command::Ping),
        "DBSIZE" => Ok(Command::DbSize),
        "FLUSHDB" => Ok(Command::FlushDb),
        _ => Ok(Command::Unknown(input.to_string())),
    }
}

/// Format a response string to send back to the client
///
/// Successful results are returned as-is. Errors are formatted with an `ERR` prefix
/// and a human-readable message appropriate for the error category.
pub fn format_response(result: Result<String, FerrumError>) -> String {
    match result {
        Ok(msg) => msg,
        Err(FerrumError::ParseError(msg)) => format!("ERR {msg}"),
        Err(FerrumError::StorageError(msg)) => format!("ERR internal error: {msg}"),
        Err(FerrumError::IoError(e)) => format!("ERR io error: {e}"),
        Err(FerrumError::PersistenceError(msg)) => format!("ERR persistence error: {msg}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let result = parse("SET key");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_set_missing_args() {
        let result = parse("SET");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_get() {
        let cmd = parse("GET name").unwrap();
        assert_eq!(
            cmd,
            Command::Get {
                key: "name".to_string()
            }
        );
    }

    #[test]
    fn test_parse_get_missing_key() {
        let result = parse("GET");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_del() {
        let cmd = parse("DEL name").unwrap();
        assert_eq!(
            cmd,
            Command::Del {
                key: "name".to_string()
            }
        );
    }

    #[test]
    fn test_parse_del_missing_key() {
        let result = parse("DEL");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_ping() {
        let cmd = parse("PING").unwrap();
        assert_eq!(cmd, Command::Ping);
    }

    #[test]
    fn test_parse_ping_case_insensitive() {
        let cmd = parse("ping").unwrap();
        assert_eq!(cmd, Command::Ping);
    }

    #[test]
    fn test_parse_dbsize() {
        let cmd = parse("DBSIZE").unwrap();
        assert_eq!(cmd, Command::DbSize);
    }

    #[test]
    fn test_parse_dbsize_case_insensitive() {
        let cmd = parse("dbsize").unwrap();
        assert_eq!(cmd, Command::DbSize);
    }

    #[test]
    fn test_parse_flushdb() {
        let cmd = parse("FLUSHDB").unwrap();
        assert_eq!(cmd, Command::FlushDb);
    }

    #[test]
    fn test_parse_flushdb_case_insensitive() {
        let cmd = parse("flushdb").unwrap();
        assert_eq!(cmd, Command::FlushDb);
    }

    #[test]
    fn test_parse_unknown_command() {
        let cmd = parse("FOOBAR").unwrap();
        assert_eq!(cmd, Command::Unknown("FOOBAR".to_string()));
    }

    #[test]
    fn test_parse_empty_input() {
        let result = parse("");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_whitespace_only() {
        let result = parse("   ");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_with_leading_trailing_spaces() {
        let cmd = parse("  GET name  ").unwrap();
        assert_eq!(
            cmd,
            Command::Get {
                key: "name".to_string()
            }
        );
    }

    #[test]
    fn test_format_response_ok() {
        let result = format_response(Ok("OK".to_string()));
        assert_eq!(result, "OK");
    }

    #[test]
    fn test_format_response_parse_error() {
        let result = format_response(Err(FerrumError::ParseError(
            "wrong number of arguments for 'SET' command".to_string(),
        )));
        assert_eq!(result, "ERR wrong number of arguments for 'SET' command");
    }

    #[test]
    fn test_format_response_storage_error() {
        let result = format_response(Err(FerrumError::StorageError(
            "lock poisoned".to_string(),
        )));
        assert_eq!(result, "ERR internal error: lock poisoned");
    }

    #[test]
    fn test_format_response_persistence_error() {
        let result = format_response(Err(FerrumError::PersistenceError(
            "disk full".to_string(),
        )));
        assert_eq!(result, "ERR persistence error: disk full");
    }
}
