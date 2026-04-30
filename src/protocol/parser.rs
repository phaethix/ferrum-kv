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

// ---------------------------------------------------------------------------
// Legacy line-protocol parser (kept during the RESP2 migration; will be
// removed once the network layer switches over).
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// RESP2 frame parser
// ---------------------------------------------------------------------------

/// Outcome of attempting to parse a single RESP2 frame from a byte buffer.
///
/// The parser is incremental: callers feed it whatever bytes have been read so
/// far, and receive back either a fully decoded command plus the number of
/// bytes it consumed, or an `Incomplete` signal indicating that more data must
/// be read before parsing can succeed.
#[derive(Debug, PartialEq)]
pub enum FrameParse {
    /// A command was decoded; `consumed` bytes may be removed from the buffer.
    Complete { command: Command, consumed: usize },
    /// The buffer does not yet contain a full frame.
    Incomplete,
}

/// Attempts to decode a single RESP2 command frame from `buf`.
///
/// Only the `Array of Bulk Strings` form is accepted, matching how every
/// mainstream Redis client (including `redis-cli` and `redis-benchmark`)
/// serialises requests. Legacy inline commands are rejected with a parse error
/// so that malformed input fails fast rather than being silently misread.
///
/// Returns [`FrameParse::Incomplete`] when the buffer ends mid-frame, allowing
/// the caller to append more bytes and retry without losing state. On protocol
/// errors (malformed length prefix, missing CRLF, wrong type marker, etc.) a
/// [`FerrumError::ParseError`] is returned and the connection should be
/// terminated.
pub fn parse_frame(buf: &[u8]) -> Result<FrameParse, FerrumError> {
    // Every RESP2 request starts with `*<count>\r\n`.
    if buf.is_empty() {
        return Ok(FrameParse::Incomplete);
    }
    if buf[0] != b'*' {
        return Err(FerrumError::ParseError(format!(
            "expected '*' at start of RESP array, got {:?}",
            buf[0] as char
        )));
    }

    let mut cursor = 1;
    let (count, next) = match read_integer_line(buf, cursor)? {
        Some(v) => v,
        None => return Ok(FrameParse::Incomplete),
    };
    cursor = next;

    if count < 1 {
        return Err(FerrumError::ParseError(format!(
            "RESP array must contain at least one element, got {count}"
        )));
    }
    let count = count as usize;
    if count > MAX_ARRAY_ELEMENTS {
        return Err(FerrumError::ParseError(format!(
            "RESP array too large ({count} elements, max {MAX_ARRAY_ELEMENTS})"
        )));
    }

    let mut parts: Vec<String> = Vec::with_capacity(count);
    for _ in 0..count {
        match read_bulk_string(buf, cursor)? {
            Some((s, next)) => {
                parts.push(s);
                cursor = next;
            }
            None => return Ok(FrameParse::Incomplete),
        }
    }

    let command = build_command(parts)?;
    Ok(FrameParse::Complete {
        command,
        consumed: cursor,
    })
}

/// Upper bound on the number of elements in a single array frame.
///
/// Chosen generously enough for any command FerrumKV implements while still
/// rejecting obviously malicious inputs that try to force huge allocations.
const MAX_ARRAY_ELEMENTS: usize = 1024;

/// Upper bound on the byte length of a single bulk string.
///
/// Mirrors Redis' `proto-max-bulk-len` default of 512 MiB, though in practice
/// FerrumKV will enforce its own lower key/value limits in the storage layer.
const MAX_BULK_LEN: i64 = 512 * 1024 * 1024;

/// Reads an ASCII integer terminated by CRLF starting at `start`.
///
/// Returns `Ok(None)` when the line has not yet been fully received, so the
/// caller can wait for more bytes. Returns `Ok(Some((value, next)))` on
/// success, where `next` points just past the trailing `\n`.
fn read_integer_line(buf: &[u8], start: usize) -> Result<Option<(i64, usize)>, FerrumError> {
    let Some(crlf) = find_crlf(buf, start) else {
        return Ok(None);
    };
    let slice = &buf[start..crlf];
    let text = std::str::from_utf8(slice)
        .map_err(|_| FerrumError::ParseError("non-ASCII length prefix".into()))?;
    let value: i64 = text
        .parse()
        .map_err(|_| FerrumError::ParseError(format!("invalid integer: {text:?}")))?;
    Ok(Some((value, crlf + 2)))
}

/// Reads a single `$<len>\r\n<payload>\r\n` bulk string starting at `start`.
///
/// Bulk strings are binary-safe on the wire; however, this intermediate stage
/// still materialises them as [`String`]. The conversion will be replaced with
/// a byte-owned representation once the storage engine accepts `Vec<u8>`.
fn read_bulk_string(buf: &[u8], start: usize) -> Result<Option<(String, usize)>, FerrumError> {
    if start >= buf.len() {
        return Ok(None);
    }
    if buf[start] != b'$' {
        return Err(FerrumError::ParseError(format!(
            "expected '$' at start of bulk string, got {:?}",
            buf[start] as char
        )));
    }

    let (len, after_len) = match read_integer_line(buf, start + 1)? {
        Some(v) => v,
        None => return Ok(None),
    };

    if len < 0 {
        // Null bulk strings are legal in RESP2 responses but never in requests.
        return Err(FerrumError::ParseError(
            "null bulk string is not allowed in requests".into(),
        ));
    }
    if len > MAX_BULK_LEN {
        return Err(FerrumError::ParseError(format!(
            "bulk string too large ({len} bytes)"
        )));
    }

    let len = len as usize;
    let payload_end = after_len + len;
    // Need payload bytes plus the trailing CRLF.
    if buf.len() < payload_end + 2 {
        return Ok(None);
    }
    if &buf[payload_end..payload_end + 2] != b"\r\n" {
        return Err(FerrumError::ParseError(
            "bulk string not terminated by CRLF".into(),
        ));
    }

    let payload = std::str::from_utf8(&buf[after_len..payload_end])
        .map_err(|_| FerrumError::ParseError("non-UTF-8 bulk string".into()))?
        .to_string();

    Ok(Some((payload, payload_end + 2)))
}

/// Locates the next CRLF sequence starting at `from`.
fn find_crlf(buf: &[u8], from: usize) -> Option<usize> {
    if from >= buf.len() {
        return None;
    }
    buf[from..]
        .windows(2)
        .position(|w| w == b"\r\n")
        .map(|pos| pos + from)
}

/// Assembles a [`Command`] from the decoded argv of a RESP array.
fn build_command(parts: Vec<String>) -> Result<Command, FerrumError> {
    let mut iter = parts.into_iter();
    // `parse_frame` already rejected empty arrays, so the first element exists.
    let name = iter.next().expect("array has at least one element");
    let args: Vec<String> = iter.collect();

    match name.to_ascii_uppercase().as_str() {
        "SET" => {
            if args.len() != 2 {
                return Err(FerrumError::WrongArity { cmd: "SET" });
            }
            let mut it = args.into_iter();
            Ok(Command::Set {
                key: it.next().unwrap(),
                value: it.next().unwrap(),
            })
        }
        "GET" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "GET" });
            }
            Ok(Command::Get {
                key: args.into_iter().next().unwrap(),
            })
        }
        "DEL" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "DEL" });
            }
            Ok(Command::Del {
                key: args.into_iter().next().unwrap(),
            })
        }
        "EXISTS" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "EXISTS" });
            }
            Ok(Command::Exists {
                key: args.into_iter().next().unwrap(),
            })
        }
        "PING" => match args.len() {
            0 => Ok(Command::Ping { msg: None }),
            1 => Ok(Command::Ping {
                msg: Some(args.into_iter().next().unwrap()),
            }),
            _ => Err(FerrumError::WrongArity { cmd: "PING" }),
        },
        "DBSIZE" => {
            if !args.is_empty() {
                return Err(FerrumError::WrongArity { cmd: "DBSIZE" });
            }
            Ok(Command::DbSize)
        }
        "FLUSHDB" => {
            if !args.is_empty() {
                return Err(FerrumError::WrongArity { cmd: "FLUSHDB" });
            }
            Ok(Command::FlushDb)
        }
        other => Err(FerrumError::UnknownCommand(other.to_string())),
    }
}

#[cfg(test)]
mod frame_tests {
    use super::*;

    fn parse_exact(buf: &[u8]) -> Command {
        match parse_frame(buf).unwrap() {
            FrameParse::Complete { command, consumed } => {
                assert_eq!(
                    consumed,
                    buf.len(),
                    "parser did not consume the full buffer"
                );
                command
            }
            FrameParse::Incomplete => panic!("expected complete frame, got Incomplete"),
        }
    }

    #[test]
    fn parses_set_command() {
        let cmd = parse_exact(b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: "name".into(),
                value: "ferrum".into(),
            }
        );
    }

    #[test]
    fn parses_get_command() {
        let cmd = parse_exact(b"*2\r\n$3\r\nGET\r\n$4\r\nname\r\n");
        assert_eq!(cmd, Command::Get { key: "name".into() });
    }

    #[test]
    fn parses_del_command() {
        let cmd = parse_exact(b"*2\r\n$3\r\nDEL\r\n$1\r\nk\r\n");
        assert_eq!(cmd, Command::Del { key: "k".into() });
    }

    #[test]
    fn parses_exists_command() {
        let cmd = parse_exact(b"*2\r\n$6\r\nEXISTS\r\n$4\r\nname\r\n");
        assert_eq!(cmd, Command::Exists { key: "name".into() });
    }

    #[test]
    fn parses_ping_without_argument() {
        let cmd = parse_exact(b"*1\r\n$4\r\nPING\r\n");
        assert_eq!(cmd, Command::Ping { msg: None });
    }

    #[test]
    fn parses_ping_with_argument() {
        let cmd = parse_exact(b"*2\r\n$4\r\nPING\r\n$5\r\nhello\r\n");
        assert_eq!(
            cmd,
            Command::Ping {
                msg: Some("hello".into())
            }
        );
    }

    #[test]
    fn parses_dbsize_and_flushdb() {
        assert_eq!(parse_exact(b"*1\r\n$6\r\nDBSIZE\r\n"), Command::DbSize);
        assert_eq!(parse_exact(b"*1\r\n$7\r\nFLUSHDB\r\n"), Command::FlushDb);
    }

    #[test]
    fn command_name_is_case_insensitive() {
        let cmd = parse_exact(b"*3\r\n$3\r\nset\r\n$1\r\nk\r\n$1\r\nv\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: "k".into(),
                value: "v".into(),
            }
        );
    }

    #[test]
    fn value_is_binary_safe_across_crlf() {
        // A four-byte value "a\r\nb" must round-trip intact.
        let cmd = parse_exact(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4\r\na\r\nb\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: "k".into(),
                value: "a\r\nb".into(),
            }
        );
    }

    #[test]
    fn accepts_empty_value() {
        let cmd = parse_exact(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: "k".into(),
                value: String::new(),
            }
        );
    }

    #[test]
    fn reports_trailing_bytes_as_unconsumed() {
        // Two full SET frames in a row: we should only consume the first.
        let buf =
            b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n";
        let FrameParse::Complete { command, consumed } = parse_frame(buf).unwrap() else {
            panic!("expected complete frame");
        };
        assert_eq!(
            command,
            Command::Set {
                key: "a".into(),
                value: "1".into(),
            }
        );
        // First frame length: "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n" = 27 bytes.
        assert_eq!(consumed, 27);

        // Parsing the remainder yields the second command.
        let rest = &buf[consumed..];
        let cmd2 = parse_exact(rest);
        assert_eq!(
            cmd2,
            Command::Set {
                key: "b".into(),
                value: "2".into(),
            }
        );
    }

    #[test]
    fn returns_incomplete_for_empty_buffer() {
        assert_eq!(parse_frame(b"").unwrap(), FrameParse::Incomplete);
    }

    #[test]
    fn returns_incomplete_for_partial_header() {
        assert_eq!(parse_frame(b"*3\r").unwrap(), FrameParse::Incomplete);
        assert_eq!(parse_frame(b"*3").unwrap(), FrameParse::Incomplete);
    }

    #[test]
    fn returns_incomplete_for_partial_bulk_header() {
        assert_eq!(
            parse_frame(b"*2\r\n$3\r\nGET\r\n$4\r\nna").unwrap(),
            FrameParse::Incomplete,
        );
    }

    #[test]
    fn returns_incomplete_when_trailing_crlf_missing() {
        // Payload bytes are present but the trailing CRLF is not.
        assert_eq!(
            parse_frame(b"*2\r\n$3\r\nGET\r\n$4\r\nname").unwrap(),
            FrameParse::Incomplete,
        );
    }

    #[test]
    fn rejects_inline_command() {
        let err = parse_frame(b"PING\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_missing_crlf_after_payload() {
        // Payload length says 4 but the two bytes following are not CRLF.
        let err = parse_frame(b"*2\r\n$3\r\nGET\r\n$4\r\nnameXX").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_non_ascii_length_prefix() {
        let err = parse_frame(b"*abc\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_negative_array_count() {
        let err = parse_frame(b"*-1\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_zero_array_count() {
        let err = parse_frame(b"*0\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_null_bulk_string_in_request() {
        let err = parse_frame(b"*1\r\n$-1\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }

    #[test]
    fn rejects_wrong_arity_via_resp() {
        // SET with only the key.
        let err = parse_frame(b"*2\r\n$3\r\nSET\r\n$1\r\nk\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::WrongArity { cmd: "SET" }));
    }

    #[test]
    fn rejects_unknown_command_via_resp() {
        let err = parse_frame(b"*1\r\n$6\r\nFOOBAR\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::UnknownCommand(ref c) if c == "FOOBAR"));
    }

    #[test]
    fn rejects_oversized_array() {
        // Count larger than MAX_ARRAY_ELEMENTS should fail immediately without
        // trying to allocate or read elements.
        let err = parse_frame(b"*1000000\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }
}
