//! Tiny HTTP/1.1 server powering the FerrumKV dashboard.
//!
//! Design goals: zero extra dependencies, fully self-contained, and easy to
//! test. All routing logic lives in the pure [`route`] function, which takes a
//! parsed [`Request`] plus the shared [`KvEngine`] and returns a [`Response`].
//! The socket plumbing in [`handle_conn`] is a thin wrapper around it, so the
//! handlers can be exercised directly from unit tests.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::error::FerrumError;
use crate::network::server::execute_command;
use crate::network::shutdown::Shutdown;
use crate::protocol::parser::{self, FrameParse};
use crate::storage::engine::{KvEngine, TtlStatus};

/// HTML page served at `/`, embedded at compile time so the binary is fully
/// self-contained — there is no asset directory to deploy alongside it.
const INDEX_HTML: &str = include_str!("index.html");

/// Default page size for the key browser.
const KEYS_PER_PAGE: usize = 50;

/// A parsed HTTP request (method, path, query string and body).
struct Request {
    method: String,
    path: String,
    query: String,
    body: Vec<u8>,
}

/// A response to write back over the socket.
struct Response {
    status: u16,
    content_type: &'static str,
    body: Vec<u8>,
}

impl Response {
    fn ok_json(body: String) -> Self {
        Self {
            status: 200,
            content_type: "application/json; charset=utf-8",
            body: body.into_bytes(),
        }
    }

    fn text_html(body: &'static str) -> Self {
        Self {
            status: 200,
            content_type: "text/html; charset=utf-8",
            body: body.as_bytes().to_vec(),
        }
    }

    fn with_status(status: u16) -> Self {
        Self {
            status,
            content_type: "text/plain; charset=utf-8",
            body: status_text(status).as_bytes().to_vec(),
        }
    }
}

fn status_text(status: u16) -> &'static str {
    match status {
        200 => "OK",
        204 => "No Content",
        400 => "Bad Request",
        404 => "Not Found",
        405 => "Method Not Allowed",
        500 => "Internal Server Error",
        _ => "Unknown",
    }
}

/// Routes a parsed request to a response. This is the entire dashboard logic
/// in one place, kept free of any socket IO so it is trivially testable.
fn route(engine: &KvEngine, start: Instant, req: &Request) -> Response {
    match req.path.as_str() {
        "/" => {
            if req.method == "GET" {
                Response::text_html(INDEX_HTML)
            } else {
                Response::with_status(405)
            }
        }
        "/api/info" => {
            if req.method == "GET" {
                Response::ok_json(info_json(engine, start))
            } else {
                Response::with_status(405)
            }
        }
        "/api/keys" => match req.method.as_str() {
            "GET" => Response::ok_json(keys_json(engine, &req.query)),
            "POST" => create_key(engine, &req.body),
            _ => Response::with_status(405),
        },
        "/api/command" => match req.method.as_str() {
            "POST" => run_command(engine, &req.body),
            _ => Response::with_status(405),
        },
        path if path.starts_with("/api/keys/") => {
            let raw = &path["/api/keys/".len()..];
            let key = url_decode(raw);
            match req.method.as_str() {
                "GET" => key_detail(engine, key.as_bytes()),
                "DELETE" => delete_key(engine, key.as_bytes()),
                "POST" | "PUT" => update_key(engine, key.as_bytes(), &req.body),
                _ => Response::with_status(405),
            }
        }
        _ => Response::with_status(404),
    }
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

fn info_json(engine: &KvEngine, start: Instant) -> String {
    let cfg = engine.eviction_config().unwrap_or_default();
    let (hits, misses) = engine.keyspace_stats();
    let total = engine.dbsize().unwrap_or(0);
    let used = engine.used_memory();
    let (expires, avg_ttl) = engine.expire_stats().unwrap_or((0, 0));
    let ahe = engine.ahe_snapshot();
    let hit_ratio = if hits + misses > 0 {
        hits as f64 / (hits + misses) as f64
    } else {
        0.0
    };
    let uptime = start.elapsed().as_secs();
    format!(
        "{{\"version\":\"{}\",\"uptime_sec\":{},\"keys\":{},\"used_memory\":{},\"max_memory\":{},\"eviction_policy\":\"{}\",\"hits\":{},\"misses\":{},\"hit_ratio\":{:.4},\"ahe_alpha\":{:.3},\"ahe_hit_ratio\":{:.3},\"expires\":{},\"avg_ttl_ms\":{}}}",
        env!("CARGO_PKG_VERSION"),
        uptime,
        total,
        used,
        cfg.max_memory,
        cfg.policy.name(),
        hits,
        misses,
        hit_ratio,
        ahe.alpha,
        ahe.last_hit_ratio,
        expires,
        avg_ttl,
    )
}

fn keys_json(engine: &KvEngine, query: &str) -> String {
    let params = parse_query(query);
    let filter = params
        .get("filter")
        .cloned()
        .unwrap_or_else(|| "*".to_string());
    let limit: usize = params
        .get("limit")
        .and_then(|s| s.parse().ok())
        .unwrap_or(KEYS_PER_PAGE);
    let offset: usize = params
        .get("offset")
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);

    let all = engine.scan_keys(filter.as_bytes()).unwrap_or_default();
    let total = all.len();
    let slice_start = offset.min(total);
    let slice_end = (offset + limit).min(total);

    let mut items = String::new();
    for key in &all[slice_start..slice_end] {
        let key_str = String::from_utf8_lossy(key);
        let ttl = match engine.ttl_ms(key) {
            Ok(TtlStatus::NoExpire) => -1i64,
            Ok(TtlStatus::Millis(ms)) => ms,
            _ => -1,
        };
        items.push_str(&format!(
            "{{\"key\":{},\"type\":\"string\",\"ttl_ms\":{}}},",
            json_string(&key_str),
            ttl
        ));
    }

    format!(
        "{{\"total\":{},\"count\":{},\"offset\":{},\"keys\":[{}]}}",
        total,
        slice_end - slice_start,
        offset,
        items.trim_end_matches(',')
    )
}

fn key_detail(engine: &KvEngine, key: &[u8]) -> Response {
    match engine.get(key) {
        Ok(Some(value)) => {
            let value_str = String::from_utf8_lossy(&value);
            let key_str = String::from_utf8_lossy(key);
            let ttl = match engine.ttl_ms(key) {
                Ok(TtlStatus::NoExpire) => -1i64,
                Ok(TtlStatus::Millis(ms)) => ms,
                _ => -1,
            };
            let body = format!(
                "{{\"key\":{},\"type\":\"string\",\"value\":{},\"ttl_ms\":{}}}",
                json_string(&key_str),
                json_string(&value_str),
                ttl
            );
            Response::ok_json(body)
        }
        Ok(None) => json_error(404, "key not found"),
        Err(e) => json_error(500, &e.to_string()),
    }
}

fn create_key(engine: &KvEngine, body: &[u8]) -> Response {
    let form = parse_query(std::str::from_utf8(body).unwrap_or(""));
    let key = match form.get("key") {
        Some(k) if !k.is_empty() => k.clone(),
        _ => return json_error(400, "missing 'key'"),
    };
    let value = form.get("value").cloned().unwrap_or_default();
    if let Err(e) = engine.set(key.as_bytes().to_vec(), value.as_bytes().to_vec()) {
        return json_error(500, &e.to_string());
    }
    if let Some(ttl_str) = form.get("ttl") {
        apply_ttl(engine, key.as_bytes(), ttl_str);
    }
    Response::ok_json(format!("{{\"ok\":true,\"key\":{}}}", json_string(&key)))
}

fn update_key(engine: &KvEngine, key: &[u8], body: &[u8]) -> Response {
    let form = parse_query(std::str::from_utf8(body).unwrap_or(""));
    let value = match form.get("value") {
        Some(v) => v.clone(),
        None => return json_error(400, "missing 'value'"),
    };
    if let Err(e) = engine.set(key.to_vec(), value.as_bytes().to_vec()) {
        return json_error(500, &e.to_string());
    }
    if let Some(ttl_str) = form.get("ttl") {
        apply_ttl(engine, key, ttl_str);
    }
    Response::ok_json("{\"ok\":true}".to_string())
}

/// Applies a TTL form field to `key`: a positive value sets an expiry, `0`
/// clears any existing expiry, and empty/invalid values are ignored.
fn apply_ttl(engine: &KvEngine, key: &[u8], ttl_str: &str) {
    match ttl_str.parse::<i64>() {
        Ok(ttl) if ttl > 0 => {
            let _ = engine.expire_at_ms(key, now_ms() + ttl * 1000);
        }
        Ok(0) => {
            let _ = engine.persist(key);
        }
        _ => {}
    }
}

fn delete_key(engine: &KvEngine, key: &[u8]) -> Response {
    match engine.del(key) {
        Ok(true) => Response::ok_json("{\"deleted\":true}".to_string()),
        Ok(false) => Response::ok_json("{\"deleted\":false}".to_string()),
        Err(e) => json_error(500, &e.to_string()),
    }
}

/// Executes a raw command line (e.g. `GET mykey`) by reusing the exact same
/// parser and executor the RESP server uses, then decodes the RESP reply into
/// a human-readable string for the console.
fn run_command(engine: &KvEngine, body: &[u8]) -> Response {
    let form = parse_query(std::str::from_utf8(body).unwrap_or(""));
    let command = match form.get("command") {
        Some(c) => c.clone(),
        None => return json_error(400, "missing 'command'"),
    };
    let argv = match tokenize_command(&command) {
        Some(a) if !a.is_empty() => a,
        _ => return json_error(400, "empty command"),
    };
    let frame = encode_resp_array(&argv);
    let parsed = match parser::parse_frame(&frame) {
        Ok(FrameParse::Complete { command, .. }) => command,
        Ok(FrameParse::Invalid { error, .. }) => return json_error(400, &error.to_string()),
        Ok(FrameParse::Incomplete) => return json_error(400, "incomplete command"),
        Err(e) => return json_error(400, &e.to_string()),
    };

    let mut out = Vec::new();
    execute_command(parsed, engine, None, &mut out);
    let response = decode_resp(&out);
    Response::ok_json(format!(
        "{{\"ok\":true,\"response\":{}}}",
        json_string(&response)
    ))
}

// ---------------------------------------------------------------------------
// Socket handling
// ---------------------------------------------------------------------------

/// Handles a single dashboard HTTP connection.
pub async fn handle_conn(
    mut stream: TcpStream,
    engine: Arc<KvEngine>,
    shutdown: Shutdown,
) -> Result<(), FerrumError> {
    let start = Instant::now();
    let mut buf: Vec<u8> = Vec::with_capacity(8192);
    let mut tmp = [0u8; 8192];

    // Phase 1: read until the header block is complete.
    let header_end = loop {
        if shutdown.is_triggered() {
            return Ok(());
        }
        if let Some(pos) = find_subsequence(&buf, b"\r\n\r\n") {
            break pos + 4;
        }
        if buf.len() > 1 << 20 {
            return Ok(());
        }
        let n = tokio::select! {
            _ = shutdown.notified() => return Ok(()),
            r = stream.read(&mut tmp) => r,
        };
        match n {
            Ok(0) => return Ok(()),
            Ok(k) => buf.extend_from_slice(&tmp[..k]),
            Err(e) => return Err(e.into()),
        }
    };

    // Phase 2: keep reading until the declared Content-Length body arrives.
    let content_length = header_value(&buf[..header_end], "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    while buf.len() < header_end + content_length {
        if shutdown.is_triggered() {
            return Ok(());
        }
        let n = tokio::select! {
            _ = shutdown.notified() => return Ok(()),
            r = stream.read(&mut tmp) => r,
        };
        match n {
            Ok(0) => break,
            Ok(k) => buf.extend_from_slice(&tmp[..k]),
            Err(e) => return Err(e.into()),
        }
    }

    let req = match parse_request(&buf) {
        Some(r) => r,
        None => {
            write_status(&mut stream, 400).await;
            return Ok(());
        }
    };
    let resp = route(&engine, start, &req);
    write_response(&mut stream, &resp).await;
    Ok(())
}

fn parse_request(buf: &[u8]) -> Option<Request> {
    let header_end = find_subsequence(buf, b"\r\n\r\n")? + 4;
    let header_block = &buf[..header_end.saturating_sub(4)];
    let mut lines = header_block.split(|&b| b == b'\n');
    let request_line = lines.next()?;
    let mut parts = request_line.split(|&b| b == b' ');
    let method = String::from_utf8_lossy(parts.next()?).into_owned();
    let target = String::from_utf8_lossy(parts.next()?).into_owned();
    let (path, query) = match target.split_once('?') {
        Some((p, q)) => (p.to_string(), q.to_string()),
        None => (target, String::new()),
    };
    let content_length = header_value(header_block, "content-length")
        .and_then(|v| v.parse::<usize>().ok())
        .unwrap_or(0);
    let body = if buf.len() >= header_end + content_length {
        buf[header_end..header_end + content_length].to_vec()
    } else {
        Vec::new()
    };
    Some(Request {
        method,
        path,
        query,
        body,
    })
}

fn header_value(block: &[u8], name: &str) -> Option<String> {
    let name_lower = name.to_ascii_lowercase();
    for line in block.split(|&b| b == b'\n') {
        let line = strip_prefix_byte(line, b'\r');
        if let Some((k, v)) = split_byte(line, b':')
            && trim_ascii(k).eq_ignore_ascii_case(name_lower.as_bytes())
        {
            return Some(String::from_utf8_lossy(trim_ascii(v)).into_owned());
        }
    }
    None
}

/// Splits a byte slice at the first occurrence of `needle`, returning the
/// parts before and after it (without the delimiter).
fn split_byte(slice: &[u8], needle: u8) -> Option<(&[u8], &[u8])> {
    let pos = slice.iter().position(|&b| b == needle)?;
    Some((&slice[..pos], &slice[pos + 1..]))
}

async fn write_response(stream: &mut TcpStream, resp: &Response) {
    let mut out = Vec::with_capacity(resp.body.len() + 64);
    out.extend_from_slice(
        format!("HTTP/1.1 {} {}\r\n", resp.status, status_text(resp.status)).as_bytes(),
    );
    out.extend_from_slice(format!("Content-Type: {}\r\n", resp.content_type).as_bytes());
    out.extend_from_slice(format!("Content-Length: {}\r\n", resp.body.len()).as_bytes());
    out.extend_from_slice(b"Connection: close\r\n\r\n");
    out.extend_from_slice(&resp.body);
    let _ = stream.write_all(&out).await;
    let _ = stream.flush().await;
}

async fn write_status(stream: &mut TcpStream, status: u16) {
    let resp = Response::with_status(status);
    write_response(stream, &resp).await;
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn json_error(status: u16, msg: &str) -> Response {
    Response {
        status,
        content_type: "application/json; charset=utf-8",
        body: format!("{{\"error\":{}}}", json_string(msg)).into_bytes(),
    }
}

/// Escapes a string for safe embedding inside a JSON double-quoted literal.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Parses an `application/x-www-form-urlencoded` (or query string) body.
fn parse_query(q: &str) -> HashMap<String, String> {
    let mut map = HashMap::new();
    for pair in q.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (k, v) = match pair.split_once('=') {
            Some(kv) => kv,
            None => (pair, ""),
        };
        map.insert(url_decode(k), url_decode(v));
    }
    map
}

fn url_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b'%' if i + 2 < bytes.len() => {
                let hi = (bytes[i + 1] as char).to_digit(16);
                let lo = (bytes[i + 2] as char).to_digit(16);
                match (hi, lo) {
                    (Some(h), Some(l)) => {
                        out.push((h * 16 + l) as u8);
                        i += 3;
                    }
                    _ => {
                        out.push(bytes[i]);
                        i += 1;
                    }
                }
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Splits a command line into argv, honouring double quotes and backslash
/// escapes (e.g. `SET "key with space" "val\"ue"`).
fn tokenize_command(line: &str) -> Option<Vec<Vec<u8>>> {
    let bytes = line.as_bytes();
    let mut argv: Vec<Vec<u8>> = Vec::new();
    let mut cur: Vec<u8> = Vec::new();
    let mut in_token = false;
    let mut in_quotes = false;
    let mut i = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if in_quotes {
            match b {
                b'"' => {
                    in_quotes = false;
                    i += 1;
                }
                b'\\' if i + 1 < bytes.len() => {
                    cur.push(bytes[i + 1]);
                    i += 2;
                }
                _ => {
                    cur.push(b);
                    i += 1;
                }
            }
        } else {
            match b {
                b'"' => {
                    in_quotes = true;
                    in_token = true;
                    i += 1;
                }
                b' ' | b'\t' | b'\r' | b'\n' => {
                    if in_token {
                        argv.push(std::mem::take(&mut cur));
                        in_token = false;
                    }
                    i += 1;
                }
                _ => {
                    in_token = true;
                    cur.push(b);
                    i += 1;
                }
            }
        }
    }
    if in_token {
        argv.push(cur);
    }
    if in_quotes {
        return None;
    }
    Some(argv)
}

/// Builds a RESP array-of-bulk-strings frame from argv, reusing the exact
/// wire format the RESP server parses.
fn encode_resp_array(argv: &[Vec<u8>]) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(format!("*{}\r\n", argv.len()).as_bytes());
    for a in argv {
        b.extend_from_slice(format!("${}\r\n", a.len()).as_bytes());
        b.extend_from_slice(a);
        b.extend_from_slice(b"\r\n");
    }
    b
}

/// Decodes a RESP reply (as produced by `execute_command`) into a friendly
/// string for console display.
fn decode_resp(buf: &[u8]) -> String {
    let mut cur = 0usize;
    decode_one(buf, &mut cur).unwrap_or_else(|| String::from_utf8_lossy(buf).into_owned())
}

fn decode_one(buf: &[u8], cur: &mut usize) -> Option<String> {
    if *cur >= buf.len() {
        return None;
    }
    let marker = buf[*cur];
    *cur += 1;
    match marker {
        b'+' => Some(read_line(buf, cur)),
        b'-' => Some(format!("(error) {}", read_line(buf, cur))),
        b':' => Some(read_line(buf, cur)),
        b'$' => {
            let len: i64 = read_line(buf, cur).parse().ok()?;
            if len < 0 {
                return Some("(nil)".to_string());
            }
            let len = len as usize;
            if *cur + len + 2 > buf.len() {
                return None;
            }
            let s = String::from_utf8_lossy(&buf[*cur..*cur + len]).into_owned();
            *cur += len + 2;
            Some(s)
        }
        b'*' => {
            let count: i64 = read_line(buf, cur).parse().ok()?;
            if count < 0 {
                return Some("(empty array)".to_string());
            }
            let mut parts = Vec::with_capacity(count as usize);
            for _ in 0..count {
                parts.push(decode_one(buf, cur)?);
            }
            Some(parts.join("\n"))
        }
        _ => None,
    }
}

fn read_line(buf: &[u8], cur: &mut usize) -> String {
    let start = *cur;
    while *cur < buf.len() && buf[*cur] != b'\r' {
        *cur += 1;
    }
    let s = String::from_utf8_lossy(&buf[start..*cur]).into_owned();
    if *cur < buf.len() {
        *cur += 2; // skip CRLF
    }
    s
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

fn find_subsequence(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    hay.windows(needle.len()).position(|w| w == needle)
}

fn strip_prefix_byte(slice: &[u8], byte: u8) -> &[u8] {
    if !slice.is_empty() && slice[0] == byte {
        &slice[1..]
    } else {
        slice
    }
}

fn trim_ascii(slice: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = slice.len();
    while start < end && slice[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && slice[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &slice[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_engine() -> KvEngine {
        let e = KvEngine::new();
        e.set(b"user:1".to_vec(), b"alice".to_vec()).unwrap();
        e.set(b"user:2".to_vec(), b"bob".to_vec()).unwrap();
        e.set(b"config:theme".to_vec(), b"dark".to_vec()).unwrap();
        e
    }

    fn req(method: &str, path: &str, query: &str, body: &[u8]) -> Request {
        Request {
            method: method.to_string(),
            path: path.to_string(),
            query: query.to_string(),
            body: body.to_vec(),
        }
    }

    fn body_of(resp: &Response) -> String {
        String::from_utf8_lossy(&resp.body).into_owned()
    }

    #[test]
    fn serves_index_html() {
        let e = make_engine();
        let resp = route(&e, Instant::now(), &req("GET", "/", "", b""));
        assert_eq!(resp.status, 200);
        assert!(body_of(&resp).contains("<!DOCTYPE html"));
    }

    #[test]
    fn info_reports_keys() {
        let e = make_engine();
        let resp = route(&e, Instant::now(), &req("GET", "/api/info", "", b""));
        let body = body_of(&resp);
        assert!(body.contains("\"keys\":3"));
        assert!(body.contains("\"eviction_policy\""));
    }

    #[test]
    fn keys_list_and_filter() {
        let e = make_engine();
        let resp = route(
            &e,
            Instant::now(),
            &req("GET", "/api/keys", "filter=user:*", b""),
        );
        let body = body_of(&resp);
        assert!(body.contains("user:1"));
        assert!(body.contains("user:2"));
        assert!(!body.contains("config:theme"));
    }

    #[test]
    fn create_get_update_delete_roundtrip() {
        let e = make_engine();
        let create = route(
            &e,
            Instant::now(),
            &req("POST", "/api/keys", "", b"key=tmp&value=hello&ttl=120"),
        );
        assert_eq!(create.status, 200);

        let got = route(&e, Instant::now(), &req("GET", "/api/keys/tmp", "", b""));
        assert_eq!(got.status, 200);
        let body = body_of(&got);
        assert!(body.contains("\"value\":\"hello\""));
        let ttl_idx = body.find("\"ttl_ms\":").unwrap() + 9;
        let rest = &body[ttl_idx..];
        let num_end = rest
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(rest.len());
        let ttl_val: i64 = rest[..num_end].parse().unwrap();
        assert!(
            (119_000..=120_000).contains(&ttl_val),
            "expected ttl near 120000ms, got {ttl_val}"
        );

        let upd = route(
            &e,
            Instant::now(),
            &req("PUT", "/api/keys/tmp", "", b"value=world"),
        );
        assert_eq!(upd.status, 200);

        let del = route(&e, Instant::now(), &req("DELETE", "/api/keys/tmp", "", b""));
        assert!(body_of(&del).contains("\"deleted\":true"));

        let missing = route(&e, Instant::now(), &req("GET", "/api/keys/tmp", "", b""));
        assert_eq!(missing.status, 404);
    }

    #[test]
    fn command_console_executes() {
        let e = make_engine();
        let resp = route(
            &e,
            Instant::now(),
            &req("POST", "/api/command", "", b"command=GET%20user:1"),
        );
        let body = body_of(&resp);
        assert!(body.contains("\"ok\":true"));
        assert!(body.contains("alice"));
    }

    #[test]
    fn unknown_path_is_404() {
        let e = make_engine();
        let resp = route(&e, Instant::now(), &req("GET", "/nope", "", b""));
        assert_eq!(resp.status, 404);
    }

    #[test]
    fn method_not_allowed() {
        let e = make_engine();
        let resp = route(&e, Instant::now(), &req("DELETE", "/api/info", "", b""));
        assert_eq!(resp.status, 405);
    }
}
