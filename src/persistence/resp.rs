//! Minimal RESP2 encoder used by the AOF writer.
//!
//! The encoder writes an "Array of Bulk Strings" exactly as specified by
//! RESP2, which is the on-disk format mandated by the whitepaper (§8.2).
//! Only the subset required by FerrumKV's AOF is implemented here; a full
//! codec will be introduced alongside the RESP protocol support.

/// Encodes a command and its arguments as a RESP2 Array of Bulk Strings.
///
/// The first element is the command name; remaining elements are arguments.
/// Each bulk string is length-prefixed, making the encoding binary-safe for
/// arbitrary byte sequences including NUL bytes and embedded CRLF.
///
/// # Examples
///
/// ```ignore
/// let bytes = encode_command(&[b"SET", b"name", b"ferrum"]);
/// assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n");
/// ```
pub(crate) fn encode_command(parts: &[&[u8]]) -> Vec<u8> {
    let payload_len: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(16 + payload_len + parts.len() * 8);

    buf.push(b'*');
    buf.extend_from_slice(parts.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");

    for part in parts {
        buf.push(b'$');
        buf.extend_from_slice(part.len().to_string().as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(part);
        buf.extend_from_slice(b"\r\n");
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::parser::{Command, FrameParse, parse_frame};

    /// Decodes an AOF-encoded frame back through the wire-protocol parser and
    /// asserts the resulting [`Command`] matches the one that produced the
    /// bytes. This is the cross-cutting invariant flagged in FERRUM-R3: the AOF
    /// on-disk format and the wire protocol must share identical RESP2 frame
    /// semantics, even though replay uses a separate `read_record` (Seek +
    /// precise `consumed`) rather than `parse_frame`.
    fn round_trip_via_parser(parts: &[&[u8]]) -> Command {
        let bytes = encode_command(parts);
        match parse_frame(&bytes) {
            Ok(FrameParse::Complete { command, consumed }) => {
                assert_eq!(consumed, bytes.len(), "parser left trailing bytes");
                command
            }
            other => panic!("parse_frame did not yield a complete command: {other:?}"),
        }
    }

    #[test]
    fn aof_frame_is_wire_compatible_set() {
        let cmd = round_trip_via_parser(&[b"SET", b"name", b"ferrum"]);
        assert_eq!(
            cmd,
            Command::Set {
                key: b"name".to_vec(),
                value: b"ferrum".to_vec()
            }
        );
    }

    #[test]
    fn aof_frame_is_wire_compatible_del_multi_key() {
        let cmd = round_trip_via_parser(&[b"DEL", b"k1", b"k2", b"k3"]);
        assert_eq!(
            cmd,
            Command::Del {
                keys: vec![b"k1".to_vec(), b"k2".to_vec(), b"k3".to_vec()]
            }
        );
    }

    #[test]
    fn aof_frame_is_wire_compatible_flushdb() {
        let cmd = round_trip_via_parser(&[b"FLUSHDB"]);
        assert_eq!(cmd, Command::FlushDb);
    }

    #[test]
    fn aof_frame_is_wire_compatible_mset() {
        let cmd = round_trip_via_parser(&[b"MSET", b"a", b"1", b"b", b"2"]);
        assert_eq!(
            cmd,
            Command::MSet {
                pairs: vec![
                    (b"a".to_vec(), b"1".to_vec()),
                    (b"b".to_vec(), b"2".to_vec())
                ]
            }
        );
    }

    #[test]
    fn aof_frame_is_wire_compatible_binary_safe() {
        // NUL + embedded CRLF + high bytes must survive both encode_command and
        // parse_frame identically — the same invariant the binary_safe_* tests
        // guard on the wire path.
        let key: &[u8] = &[0x00, b'\r', b'\n', 0xff];
        let value: &[u8] = &[0x80, 0x00, 0xc3, b'\r', b'\n'];
        let cmd = round_trip_via_parser(&[b"SET", key, value]);
        assert_eq!(
            cmd,
            Command::Set {
                key: key.to_vec(),
                value: value.to_vec()
            }
        );
    }

    #[test]
    fn aof_frame_is_wire_compatible_pexpireat() {
        let cmd = round_trip_via_parser(&[b"PEXPIREAT", b"k", b"1700000000000"]);
        assert_eq!(
            cmd,
            Command::PExpireAt {
                key: b"k".to_vec(),
                abs_epoch_ms: 1_700_000_000_000
            }
        );
    }

    #[test]
    fn encodes_set_command() {
        let encoded = encode_command(&[b"SET", b"name", b"ferrum"]);
        assert_eq!(
            encoded,
            b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n"
        );
    }

    #[test]
    fn encodes_del_command() {
        let encoded = encode_command(&[b"DEL", b"name"]);
        assert_eq!(encoded, b"*2\r\n$3\r\nDEL\r\n$4\r\nname\r\n");
    }

    #[test]
    fn encodes_flushdb_command() {
        let encoded = encode_command(&[b"FLUSHDB"]);
        assert_eq!(encoded, b"*1\r\n$7\r\nFLUSHDB\r\n");
    }

    #[test]
    fn encodes_values_containing_crlf() {
        let encoded = encode_command(&[b"SET", b"k", b"a\r\nb"]);
        assert_eq!(encoded, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4\r\na\r\nb\r\n");
    }

    #[test]
    fn encodes_empty_value() {
        let encoded = encode_command(&[b"SET", b"k", b""]);
        assert_eq!(encoded, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
    }

    #[test]
    fn encodes_values_with_nul_and_high_bytes() {
        let value: &[u8] = &[0x00, 0x01, 0xff, 0xfe, 0x80];
        let encoded = encode_command(&[b"SET", b"k", value]);
        let expected: &[u8] = b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$5\r\n\x00\x01\xff\xfe\x80\r\n";
        assert_eq!(encoded, expected);
    }

    #[test]
    fn encodes_large_value_length_prefix() {
        let value = vec![b'x'; 1024];
        let encoded = encode_command(&[b"SET", b"k", &value]);
        assert!(encoded.starts_with(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1024\r\n"));
        assert!(encoded.ends_with(b"\r\n"));
    }
}
