use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::error::FerrumError;
use crate::protocol::parser::{self, Command};
use crate::storage::engine::KvEngine;

/// Handles a single client connection.
///
/// The function reads commands line by line, executes them against the shared
/// engine, and writes each response back to the client.
fn handle_client(stream: TcpStream, engine: KvEngine) -> Result<(), FerrumError> {
    let peer = stream.peer_addr()?;
    eprintln!("[INFO] Client connected: {peer}");

    let reader = BufReader::new(stream.try_clone()?);
    let mut writer = stream;

    for line in reader.lines() {
        match line {
            Ok(input) => {
                let input = input.trim().to_string();
                if input.is_empty() {
                    continue;
                }
                eprintln!("[DEBUG] recv from {peer}: {input}");

                let response = execute_command(&input, &engine);
                eprintln!("[DEBUG] resp to {peer}: {response}");

                if let Err(e) = writeln!(writer, "{response}") {
                    eprintln!("[ERROR] write failed for {peer}: {e}");
                    return Err(e.into());
                }
            }
            Err(e) => {
                eprintln!("[ERROR] read failed for {peer}: {e}");
                return Err(e.into());
            }
        }
    }

    eprintln!("[INFO] Client disconnected: {peer}");
    Ok(())
}

/// Parses and executes a command against the KV engine.
///
/// Response values follow the simple line protocol used by this project:
/// integer-like counts are rendered as decimal strings, missing values produce
/// `NULL`, and `PING` returns `PONG` or the supplied message.
pub fn execute_command(input: &str, engine: &KvEngine) -> String {
    let cmd = match parser::parse(input) {
        Ok(cmd) => cmd,
        Err(e) => return parser::format_response(Err(e)),
    };

    let result: Result<String, FerrumError> = match cmd {
        Command::Set { key, value } => engine.set(key, value).map(|_| "OK".to_string()),
        Command::Get { key } => engine.get(&key).map(|v| v.unwrap_or_else(|| "NULL".into())),
        Command::Del { key } => engine
            .del(&key)
            .map(|deleted| if deleted { "1" } else { "0" }.to_string()),
        Command::Exists { key } => engine
            .exists(&key)
            .map(|found| if found { "1" } else { "0" }.to_string()),
        Command::Ping { msg } => Ok(msg.unwrap_or_else(|| "PONG".to_string())),
        Command::DbSize => engine.dbsize().map(|n| n.to_string()),
        Command::FlushDb => engine.flushdb().map(|()| "OK".to_string()),
    };

    parser::format_response(result)
}

/// Starts the TCP server and listens for incoming connections.
///
/// Each accepted connection is handled on a separate thread that shares the
/// same [`KvEngine`] instance.
pub fn start(addr: &str, engine: KvEngine) -> Result<(), FerrumError> {
    let listener = TcpListener::bind(addr)?;
    eprintln!("[INFO] FerrumKV listening on {addr}");

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
