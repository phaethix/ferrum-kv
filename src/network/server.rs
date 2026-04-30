use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::error::FerrumError;
use crate::protocol::encoder;
use crate::protocol::parser::{self, Command, FrameParse};
use crate::storage::engine::KvEngine;

/// Initial capacity of each per-connection read buffer.
///
/// Most Redis commands fit into well under 1 KiB; starting small keeps idle
/// connections cheap while letting the buffer grow naturally for bulk values.
const READ_BUF_INITIAL: usize = 1024;

/// Upper bound on how much unparsed data we are willing to buffer per client.
///
/// If the buffer exceeds this size without a complete frame the client is
/// sending something malformed or abusive; we reply with an error and close.
const READ_BUF_MAX: usize = 16 * 1024 * 1024;

/// Size of each `read()` scratch buffer.
const READ_CHUNK: usize = 8 * 1024;

/// Handles a single client connection using the RESP2 protocol.
///
/// Bytes are read into a per-connection buffer and fed to [`parser::parse_frame`]
/// in a loop: every complete frame is executed against the shared engine and
/// its reply is written back using the RESP2 encoders. Partial frames remain
/// in the buffer until the next `read` fills them in, so requests that span
/// multiple packets are handled transparently.
fn handle_client(mut stream: TcpStream, engine: KvEngine) -> Result<(), FerrumError> {
    let peer = stream.peer_addr()?;
    eprintln!("[INFO] Client connected: {peer}");

    let mut inbuf: Vec<u8> = Vec::with_capacity(READ_BUF_INITIAL);
    let mut chunk = [0u8; READ_CHUNK];
    let mut outbuf: Vec<u8> = Vec::with_capacity(256);

    loop {
        let n = match stream.read(&mut chunk) {
            Ok(0) => break, // Orderly EOF: client closed the connection.
            Ok(n) => n,
            Err(e) => {
                eprintln!("[ERROR] read failed for {peer}: {e}");
                return Err(e.into());
            }
        };
        inbuf.extend_from_slice(&chunk[..n]);

        // Drain as many complete frames as the buffer currently holds.
        loop {
            match parser::parse_frame(&inbuf) {
                Ok(FrameParse::Complete { command, consumed }) => {
                    outbuf.clear();
                    execute_command(command, &engine, &mut outbuf);
                    if let Err(e) = stream.write_all(&outbuf) {
                        eprintln!("[ERROR] write failed for {peer}: {e}");
                        return Err(e.into());
                    }
                    inbuf.drain(..consumed);
                }
                Ok(FrameParse::Invalid { error, consumed }) => {
                    // The frame was well-formed; the command itself was
                    // invalid (unknown, wrong arity, etc). Reply with -ERR
                    // and keep the connection open so the client can retry.
                    outbuf.clear();
                    write_ferrum_error(&mut outbuf, &error);
                    if let Err(e) = stream.write_all(&outbuf) {
                        eprintln!("[ERROR] write failed for {peer}: {e}");
                        return Err(e.into());
                    }
                    inbuf.drain(..consumed);
                }
                Ok(FrameParse::Incomplete) => break,
                Err(e) => {
                    // Protocol-level error: the byte stream itself cannot be
                    // resynchronised, so reply with -ERR and close.
                    outbuf.clear();
                    write_ferrum_error(&mut outbuf, &e);
                    let _ = stream.write_all(&outbuf);
                    eprintln!("[WARN] protocol error from {peer}: {e}");
                    return Ok(());
                }
            }
        }

        if inbuf.len() > READ_BUF_MAX {
            outbuf.clear();
            encoder::encode_error(&mut outbuf, "ERR request too large");
            let _ = stream.write_all(&outbuf);
            eprintln!("[WARN] request buffer overflow from {peer}");
            return Ok(());
        }
    }

    eprintln!("[INFO] Client disconnected: {peer}");
    Ok(())
}

/// Executes a parsed command against the engine and appends its RESP2 reply
/// to `out`.
///
/// Replies follow the Redis conventions:
///
/// - `SET`, `FLUSHDB`                         → `+OK`
/// - `GET` (hit)                              → bulk string
/// - `GET` (miss)                             → null bulk (`$-1`)
/// - `DEL`, `EXISTS`, `DBSIZE`                → integer
/// - `PING` (no arg)                          → `+PONG`
/// - `PING msg`                               → bulk string echoing `msg`
/// - engine/persistence failures              → `-ERR` / `-OOM`
pub fn execute_command(cmd: Command, engine: &KvEngine, out: &mut Vec<u8>) {
    match cmd {
        Command::Set { key, value } => match engine.set(key, value) {
            Ok(_) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Get { key } => match engine.get(&key) {
            Ok(Some(v)) => encoder::encode_bulk_string(out, v.as_bytes()),
            Ok(None) => encoder::encode_null_bulk(out),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Del { key } => match engine.del(&key) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Exists { key } => match engine.exists(&key) {
            Ok(true) => encoder::encode_integer(out, 1),
            Ok(false) => encoder::encode_integer(out, 0),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::Ping { msg } => match msg {
            None => encoder::encode_simple_string(out, "PONG"),
            Some(m) => encoder::encode_bulk_string(out, m.as_bytes()),
        },
        Command::DbSize => match engine.dbsize() {
            Ok(n) => encoder::encode_integer(out, n as i64),
            Err(e) => write_ferrum_error(out, &e),
        },
        Command::FlushDb => match engine.flushdb() {
            Ok(()) => encoder::encode_simple_string(out, "OK"),
            Err(e) => write_ferrum_error(out, &e),
        },
    }
}

/// Serialises a [`FerrumError`] into a RESP2 error reply.
///
/// The `OOM` variant is emitted with the `OOM` prefix mandated by the Redis
/// protocol; everything else uses `ERR`. CR/LF characters inside error
/// messages are replaced with spaces because Simple errors are single-line.
fn write_ferrum_error(out: &mut Vec<u8>, err: &FerrumError) {
    let (prefix, detail) = match err {
        FerrumError::OutOfMemory => ("OOM", err.to_string()),
        _ => ("ERR", err.to_string()),
    };
    let mut sanitised = String::with_capacity(prefix.len() + 1 + detail.len());
    sanitised.push_str(prefix);
    sanitised.push(' ');
    for ch in detail.chars() {
        if ch == '\r' || ch == '\n' {
            sanitised.push(' ');
        } else {
            sanitised.push(ch);
        }
    }
    encoder::encode_error(out, &sanitised);
}

/// Starts the TCP server and listens for incoming connections.
///
/// Each accepted connection is handled on a separate thread that shares the
/// same [`KvEngine`] instance.
pub fn start(addr: &str, engine: KvEngine) -> Result<(), FerrumError> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("[INFO] FerrumKV listening on {addr}");
    run_listener(listener, engine)
}

/// Runs the accept loop on an already-bound [`TcpListener`].
///
/// Split out from [`start`] so that tests (and future embeddings) can bind
/// their own listener — for example to port `0` to obtain an OS-assigned
/// ephemeral port — and drive the server from there.
pub fn run_listener(listener: TcpListener, engine: KvEngine) -> Result<(), FerrumError> {
    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = engine.clone();
                thread::spawn(move || {
                    if let Err(e) = handle_client(stream, engine) {
                        eprintln!("[ERROR] client handler error: {e}");
                    }
                });
            }
            Err(e) => {
                eprintln!("[ERROR] connection failed: {e}");
            }
        }
    }

    Ok(())
}
