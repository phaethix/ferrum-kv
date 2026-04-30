//! Minimal RESP2 encoder used by the AOF writer.
//!
//! The encoder writes an "Array of Bulk Strings" exactly as specified by
//! RESP2, which is the on-disk format mandated by the whitepaper (§8.2).
//! Only the subset required by FerrumKV's AOF is implemented here; a full
//! codec will be introduced alongside the RESP protocol support.

/// Encodes a command and its arguments as a RESP2 Array of Bulk Strings.
///
/// The first element is the command name; remaining elements are arguments.
/// Each bulk string is length-prefixed, making the encoding binary-safe.
///
/// # Examples
///
/// ```ignore
/// let bytes = encode_command(&["SET", "name", "ferrum"]);
/// assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n");
/// ```
pub(crate) fn encode_command(parts: &[&str]) -> Vec<u8> {
    // Estimate capacity to avoid repeated reallocations for typical payloads.
    let payload_len: usize = parts.iter().map(|p| p.len()).sum();
    let mut buf = Vec::with_capacity(16 + payload_len + parts.len() * 8);

    buf.extend_from_slice(b"*");
    buf.extend_from_slice(parts.len().to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");

    for part in parts {
        buf.extend_from_slice(b"$");
        buf.extend_from_slice(part.len().to_string().as_bytes());
        buf.extend_from_slice(b"\r\n");
        buf.extend_from_slice(part.as_bytes());
        buf.extend_from_slice(b"\r\n");
    }

    buf
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encodes_set_command() {
        let encoded = encode_command(&["SET", "name", "ferrum"]);
        assert_eq!(
            encoded,
            b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n"
        );
    }

    #[test]
    fn encodes_del_command() {
        let encoded = encode_command(&["DEL", "name"]);
        assert_eq!(encoded, b"*2\r\n$3\r\nDEL\r\n$4\r\nname\r\n");
    }

    #[test]
    fn encodes_flushdb_command() {
        let encoded = encode_command(&["FLUSHDB"]);
        assert_eq!(encoded, b"*1\r\n$7\r\nFLUSHDB\r\n");
    }

    #[test]
    fn encodes_values_containing_crlf() {
        let encoded = encode_command(&["SET", "k", "a\r\nb"]);
        assert_eq!(encoded, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4\r\na\r\nb\r\n");
    }

    #[test]
    fn encodes_empty_value() {
        let encoded = encode_command(&["SET", "k", ""]);
        assert_eq!(encoded, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$0\r\n\r\n");
    }

    #[test]
    fn encodes_large_value_length_prefix() {
        let value = "x".repeat(1024);
        let encoded = encode_command(&["SET", "k", &value]);
        assert!(encoded.starts_with(b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1024\r\n"));
        assert!(encoded.ends_with(b"\r\n"));
    }
}
