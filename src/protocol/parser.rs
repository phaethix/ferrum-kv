use crate::error::FerrumError;

/// Represents a parsed command received from a client.
///
/// Keys and values are kept as raw bytes so that binary-safe payloads survive
/// unchanged through the protocol layer.
#[derive(Debug, PartialEq)]
pub enum Command {
    /// `SET key value`
    Set { key: Vec<u8>, value: Vec<u8> },
    /// `GET key`
    Get { key: Vec<u8> },
    /// `DEL key`
    Del { key: Vec<u8> },
    /// `EXISTS key`
    Exists { key: Vec<u8> },
    /// `PING [message]`
    Ping { msg: Option<Vec<u8>> },
    /// `DBSIZE`, which returns the number of keys.
    DbSize,
    /// `FLUSHDB`, which removes all keys.
    FlushDb,
}

/// Outcome of attempting to parse a single RESP2 frame from a byte buffer.
///
/// The parser is incremental: callers feed it whatever bytes have been read so
/// far, and receive back either a fully decoded command plus the number of
/// bytes it consumed, or an `Incomplete` signal indicating that more data must
/// be read before parsing can succeed.
///
/// Command-level errors (unknown commands, wrong arity) are reported via
/// [`FrameParse::Invalid`] rather than a parser error so that the caller can
/// reply with `-ERR …` and keep the connection open: the frame itself was
/// syntactically valid, only its contents are rejected.
#[derive(Debug)]
pub enum FrameParse {
    /// A command was decoded; `consumed` bytes may be removed from the buffer.
    Complete { command: Command, consumed: usize },
    /// A frame was consumed but its contents are not a valid command.
    Invalid { error: FerrumError, consumed: usize },
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

    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(count);
    for _ in 0..count {
        match read_bulk_string(buf, cursor)? {
            Some((s, next)) => {
                parts.push(s);
                cursor = next;
            }
            None => return Ok(FrameParse::Incomplete),
        }
    }

    let command = match build_command(parts) {
        Ok(cmd) => cmd,
        Err(e) => {
            return Ok(FrameParse::Invalid {
                error: e,
                consumed: cursor,
            });
        }
    };
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
/// Bulk strings are binary-safe on the wire and the parser preserves that
/// guarantee by returning the payload as raw bytes without any UTF-8
/// validation. Callers that need a textual interpretation (e.g. command-name
/// lookup) perform the conversion themselves.
fn read_bulk_string(buf: &[u8], start: usize) -> Result<Option<(Vec<u8>, usize)>, FerrumError> {
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

    let payload = buf[after_len..payload_end].to_vec();
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
fn build_command(parts: Vec<Vec<u8>>) -> Result<Command, FerrumError> {
    let mut iter = parts.into_iter();
    // `parse_frame` already rejected empty arrays, so the first element exists.
    let name = iter.next().expect("array has at least one element");
    let args: Vec<Vec<u8>> = iter.collect();

    let upper = ascii_uppercase(&name);
    match upper.as_slice() {
        b"SET" => {
            if args.len() != 2 {
                return Err(FerrumError::WrongArity { cmd: "SET" });
            }
            let mut it = args.into_iter();
            Ok(Command::Set {
                key: it.next().unwrap(),
                value: it.next().unwrap(),
            })
        }
        b"GET" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "GET" });
            }
            Ok(Command::Get {
                key: args.into_iter().next().unwrap(),
            })
        }
        b"DEL" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "DEL" });
            }
            Ok(Command::Del {
                key: args.into_iter().next().unwrap(),
            })
        }
        b"EXISTS" => {
            if args.len() != 1 {
                return Err(FerrumError::WrongArity { cmd: "EXISTS" });
            }
            Ok(Command::Exists {
                key: args.into_iter().next().unwrap(),
            })
        }
        b"PING" => match args.len() {
            0 => Ok(Command::Ping { msg: None }),
            1 => Ok(Command::Ping {
                msg: Some(args.into_iter().next().unwrap()),
            }),
            _ => Err(FerrumError::WrongArity { cmd: "PING" }),
        },
        b"DBSIZE" => {
            if !args.is_empty() {
                return Err(FerrumError::WrongArity { cmd: "DBSIZE" });
            }
            Ok(Command::DbSize)
        }
        b"FLUSHDB" => {
            if !args.is_empty() {
                return Err(FerrumError::WrongArity { cmd: "FLUSHDB" });
            }
            Ok(Command::FlushDb)
        }
        _ => Err(FerrumError::UnknownCommand(
            String::from_utf8_lossy(&name).into_owned(),
        )),
    }
}

/// Returns the ASCII-uppercased copy of `bytes`; non-ASCII bytes are preserved.
fn ascii_uppercase(bytes: &[u8]) -> Vec<u8> {
    bytes.iter().map(|b| b.to_ascii_uppercase()).collect()
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
            FrameParse::Invalid { error, .. } => {
                panic!("expected complete frame, got Invalid: {error}")
            }
            FrameParse::Incomplete => panic!("expected complete frame, got Incomplete"),
        }
    }

    fn assert_incomplete(buf: &[u8]) {
        match parse_frame(buf).unwrap() {
            FrameParse::Incomplete => {}
            other => panic!("expected Incomplete, got {other:?}"),
        }
    }

    #[test]
    fn parses_set_command() {
        let cmd = parse_exact(b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: b"name".to_vec(),
                value: b"ferrum".to_vec(),
            }
        );
    }

    #[test]
    fn parses_get_command() {
        let cmd = parse_exact(b"*2\r\n$3\r\nGET\r\n$4\r\nname\r\n");
        assert_eq!(
            cmd,
            Command::Get {
                key: b"name".to_vec()
            }
        );
    }

    #[test]
    fn parses_del_command() {
        let cmd = parse_exact(b"*2\r\n$3\r\nDEL\r\n$1\r\nk\r\n");
        assert_eq!(cmd, Command::Del { key: b"k".to_vec() });
    }

    #[test]
    fn parses_exists_command() {
        let cmd = parse_exact(b"*2\r\n$6\r\nEXISTS\r\n$4\r\nname\r\n");
        assert_eq!(
            cmd,
            Command::Exists {
                key: b"name".to_vec()
            }
        );
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
                msg: Some(b"hello".to_vec())
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
                key: b"k".to_vec(),
                value: b"v".to_vec(),
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
                key: b"k".to_vec(),
                value: b"a\r\nb".to_vec(),
            }
        );
    }

    #[test]
    fn accepts_empty_value() {
        let cmd = parse_exact(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
        assert_eq!(
            cmd,
            Command::Set {
                key: b"k".to_vec(),
                value: Vec::new(),
            }
        );
    }

    #[test]
    fn accepts_non_utf8_payloads() {
        // Bulk strings must survive arbitrary byte sequences, including values
        // that are not valid UTF-8 and keys that embed NUL bytes.
        let mut frame: Vec<u8> = Vec::new();
        frame.extend_from_slice(b"*3\r\n$3\r\nSET\r\n$3\r\n");
        frame.extend_from_slice(&[0x00, 0xff, 0x01]);
        frame.extend_from_slice(b"\r\n$5\r\n");
        frame.extend_from_slice(&[0x80, 0x00, 0xc3, 0x28, 0xfe]);
        frame.extend_from_slice(b"\r\n");

        let cmd = parse_exact(&frame);
        assert_eq!(
            cmd,
            Command::Set {
                key: vec![0x00, 0xff, 0x01],
                value: vec![0x80, 0x00, 0xc3, 0x28, 0xfe],
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
                key: b"a".to_vec(),
                value: b"1".to_vec(),
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
                key: b"b".to_vec(),
                value: b"2".to_vec(),
            }
        );
    }

    #[test]
    fn returns_incomplete_for_empty_buffer() {
        assert_incomplete(b"");
    }

    #[test]
    fn returns_incomplete_for_partial_header() {
        assert_incomplete(b"*3\r");
        assert_incomplete(b"*3");
    }

    #[test]
    fn returns_incomplete_for_partial_bulk_header() {
        assert_incomplete(b"*2\r\n$3\r\nGET\r\n$4\r\nna");
    }

    #[test]
    fn returns_incomplete_when_trailing_crlf_missing() {
        // Payload bytes are present but the trailing CRLF is not.
        assert_incomplete(b"*2\r\n$3\r\nGET\r\n$4\r\nname");
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
    fn reports_wrong_arity_as_invalid_frame() {
        // SET with only the key: frame is well-formed, command is rejected.
        let (err, consumed) = match parse_frame(b"*2\r\n$3\r\nSET\r\n$1\r\nk\r\n").unwrap() {
            FrameParse::Invalid { error, consumed } => (error, consumed),
            other => panic!("expected Invalid, got {other:?}"),
        };
        assert!(matches!(err, FerrumError::WrongArity { cmd: "SET" }));
        // The full frame was consumed so the connection can keep going.
        assert_eq!(consumed, b"*2\r\n$3\r\nSET\r\n$1\r\nk\r\n".len());
    }

    #[test]
    fn reports_unknown_command_as_invalid_frame() {
        let (err, consumed) = match parse_frame(b"*1\r\n$6\r\nFOOBAR\r\n").unwrap() {
            FrameParse::Invalid { error, consumed } => (error, consumed),
            other => panic!("expected Invalid, got {other:?}"),
        };
        assert!(matches!(err, FerrumError::UnknownCommand(ref c) if c == "FOOBAR"));
        assert_eq!(consumed, b"*1\r\n$6\r\nFOOBAR\r\n".len());
    }

    #[test]
    fn rejects_oversized_array() {
        // Count larger than MAX_ARRAY_ELEMENTS should fail immediately without
        // trying to allocate or read elements.
        let err = parse_frame(b"*1000000\r\n").unwrap_err();
        assert!(matches!(err, FerrumError::ParseError(_)));
    }
}
