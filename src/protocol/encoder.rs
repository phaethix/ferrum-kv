//! RESP2 response encoder.
//!
//! Each function appends one RESP2 reply element to the caller-supplied
//! buffer. Callers compose a full response by issuing writes on a single
//! [`Vec<u8>`] and flushing it to the socket in one `write_all` call, which
//! matches how Redis serialises replies and avoids per-element syscalls.
//!
//! The encoder is deliberately byte-oriented: bulk payloads take `&[u8]` so
//! that binary values survive round-trips unchanged, including values that
//! embed `\r\n`.

/// Appends a RESP2 Simple String (`+<text>\r\n`).
///
/// The caller must guarantee that `text` contains neither `\r` nor `\n`;
/// Simple Strings are defined to be a single line. Longer or binary payloads
/// must use [`encode_bulk_string`] instead.
pub fn encode_simple_string(buf: &mut Vec<u8>, text: &str) {
    debug_assert!(
        !text.as_bytes().contains(&b'\r') && !text.as_bytes().contains(&b'\n'),
        "simple strings must not contain CR or LF",
    );
    buf.reserve(text.len() + 3);
    buf.push(b'+');
    buf.extend_from_slice(text.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// Appends a RESP2 Error (`-<message>\r\n`).
///
/// The message is expected to be a single line. Conventionally the first
/// token is an uppercase prefix such as `ERR`, `WRONGTYPE`, or `OOM`.
pub fn encode_error(buf: &mut Vec<u8>, message: &str) {
    debug_assert!(
        !message.as_bytes().contains(&b'\r') && !message.as_bytes().contains(&b'\n'),
        "error messages must not contain CR or LF",
    );
    buf.reserve(message.len() + 3);
    buf.push(b'-');
    buf.extend_from_slice(message.as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// Appends a RESP2 Integer (`:<n>\r\n`).
pub fn encode_integer(buf: &mut Vec<u8>, n: i64) {
    buf.push(b':');
    // `itoa` would be faster, but the std formatter keeps the dependency
    // footprint small and is negligible for reply-sized numbers.
    buf.extend_from_slice(n.to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
}

/// Appends a RESP2 Bulk String (`$<len>\r\n<payload>\r\n`).
///
/// The payload is written verbatim, so values containing CR/LF bytes are
/// round-tripped intact. An empty slice produces `$0\r\n\r\n`, which is
/// distinct from the null bulk emitted by [`encode_null_bulk`].
pub fn encode_bulk_string(buf: &mut Vec<u8>, payload: &[u8]) {
    let len_str = payload.len().to_string();
    buf.reserve(1 + len_str.len() + 2 + payload.len() + 2);
    buf.push(b'$');
    buf.extend_from_slice(len_str.as_bytes());
    buf.extend_from_slice(b"\r\n");
    buf.extend_from_slice(payload);
    buf.extend_from_slice(b"\r\n");
}

/// Appends a RESP2 Null Bulk String (`$-1\r\n`).
///
/// This is the standard reply for `GET` against a missing key and for any
/// other command whose contract calls for a null bulk reply.
pub fn encode_null_bulk(buf: &mut Vec<u8>) {
    buf.extend_from_slice(b"$-1\r\n");
}

/// Appends a RESP2 Array header (`*<count>\r\n`).
///
/// Callers follow this with exactly `count` further RESP2 elements to form a
/// complete array reply. Used by commands such as `MGET` that return a
/// heterogeneous sequence of bulk strings and null bulks.
pub fn encode_array_header(buf: &mut Vec<u8>, count: usize) {
    buf.push(b'*');
    buf.extend_from_slice(count.to_string().as_bytes());
    buf.extend_from_slice(b"\r\n");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_string_ok() {
        let mut buf = Vec::new();
        encode_simple_string(&mut buf, "OK");
        assert_eq!(buf, b"+OK\r\n");
    }

    #[test]
    fn simple_string_pong() {
        let mut buf = Vec::new();
        encode_simple_string(&mut buf, "PONG");
        assert_eq!(buf, b"+PONG\r\n");
    }

    #[test]
    fn error_with_err_prefix() {
        let mut buf = Vec::new();
        encode_error(&mut buf, "ERR unknown command: 'FOO'");
        assert_eq!(buf, b"-ERR unknown command: 'FOO'\r\n");
    }

    #[test]
    fn error_with_wrongtype_prefix() {
        let mut buf = Vec::new();
        encode_error(&mut buf, "WRONGTYPE expected string");
        assert_eq!(buf, b"-WRONGTYPE expected string\r\n");
    }

    #[test]
    fn integer_positive() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, 42);
        assert_eq!(buf, b":42\r\n");
    }

    #[test]
    fn integer_zero() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, 0);
        assert_eq!(buf, b":0\r\n");
    }

    #[test]
    fn integer_negative() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, -7);
        assert_eq!(buf, b":-7\r\n");
    }

    #[test]
    fn integer_large_values() {
        let mut buf = Vec::new();
        encode_integer(&mut buf, i64::MAX);
        assert_eq!(buf, format!(":{}\r\n", i64::MAX).as_bytes());

        buf.clear();
        encode_integer(&mut buf, i64::MIN);
        assert_eq!(buf, format!(":{}\r\n", i64::MIN).as_bytes());
    }

    #[test]
    fn bulk_string_basic() {
        let mut buf = Vec::new();
        encode_bulk_string(&mut buf, b"ferrum");
        assert_eq!(buf, b"$6\r\nferrum\r\n");
    }

    #[test]
    fn bulk_string_empty() {
        let mut buf = Vec::new();
        encode_bulk_string(&mut buf, b"");
        assert_eq!(buf, b"$0\r\n\r\n");
    }

    #[test]
    fn bulk_string_binary_safe_across_crlf() {
        let mut buf = Vec::new();
        encode_bulk_string(&mut buf, b"a\r\nb");
        assert_eq!(buf, b"$4\r\na\r\nb\r\n");
    }

    #[test]
    fn bulk_string_contains_null_byte() {
        let mut buf = Vec::new();
        encode_bulk_string(&mut buf, b"\x00\x01\x02");
        assert_eq!(buf, b"$3\r\n\x00\x01\x02\r\n");
    }

    #[test]
    fn bulk_string_large_payload_length_prefix() {
        let payload = vec![b'x'; 1024];
        let mut buf = Vec::new();
        encode_bulk_string(&mut buf, &payload);
        assert!(buf.starts_with(b"$1024\r\n"));
        assert!(buf.ends_with(b"\r\n"));
        assert_eq!(buf.len(), 1024 + b"$1024\r\n".len() + 2);
    }

    #[test]
    fn null_bulk_shape() {
        let mut buf = Vec::new();
        encode_null_bulk(&mut buf);
        assert_eq!(buf, b"$-1\r\n");
    }

    #[test]
    fn null_bulk_differs_from_empty_bulk() {
        let mut null = Vec::new();
        encode_null_bulk(&mut null);
        let mut empty = Vec::new();
        encode_bulk_string(&mut empty, b"");
        assert_ne!(null, empty);
    }

    #[test]
    fn array_header_basic() {
        let mut buf = Vec::new();
        encode_array_header(&mut buf, 3);
        assert_eq!(buf, b"*3\r\n");
    }

    #[test]
    fn array_header_zero() {
        let mut buf = Vec::new();
        encode_array_header(&mut buf, 0);
        assert_eq!(buf, b"*0\r\n");
    }

    #[test]
    fn encoders_can_be_chained_into_one_buffer() {
        // Simulates composing a full reply for a hypothetical multi-element
        // response. In practice each command produces exactly one element, but
        // the encoder must support sequential writes without any separators
        // beyond what each call emits itself.
        let mut buf = Vec::new();
        encode_simple_string(&mut buf, "OK");
        encode_integer(&mut buf, 3);
        encode_bulk_string(&mut buf, b"hi");
        encode_null_bulk(&mut buf);
        encode_error(&mut buf, "ERR boom");
        assert_eq!(buf, b"+OK\r\n:3\r\n$2\r\nhi\r\n$-1\r\n-ERR boom\r\n");
    }
}
