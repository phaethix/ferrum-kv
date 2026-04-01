use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::error::FerrumError;
use crate::protocol::parser::{self, Command};
use crate::storage::engine::KvEngine;

/// Handle a single client connection
///
/// Reads commands line-by-line, parses them, executes against the KV engine,
/// and writes back the response.
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

/// Parse and execute a command against the KV engine, returning the response string
pub fn execute_command(input: &str, engine: &KvEngine) -> String {
    let cmd = match parser::parse(input) {
        Ok(cmd) => cmd,
        Err(e) => return parser::format_response(Err(e)),
    };

    let result = match cmd {
        Command::Set { key, value } => engine.set(key, value).map(|_| "OK".to_string()),
        Command::Get { key } => engine.get(&key).map(|v| v.unwrap_or_else(|| "NULL".into())),
        Command::Del { key } => engine
            .del(&key)
            .map(|deleted| if deleted { "OK" } else { "NULL" }.into()),
        Command::Ping => Ok("PONG".to_string()),
        Command::DbSize => engine.dbsize().map(|n| n.to_string()),
        Command::FlushDb => engine.flushdb().map(|()| "OK".to_string()),
        Command::Unknown(cmd) => Ok(format!("ERR unknown command: {cmd}")),
    };

    parser::format_response(result)
}

/// Start the TCP server and listen for incoming connections
///
/// Each connection is handled in a separate thread with a shared KV engine.
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
