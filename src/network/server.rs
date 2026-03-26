use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

use crate::protocol::parser::{self, Command};
use crate::storage::engine::KvEngine;

/// Handle a single client connection
///
/// Reads commands line-by-line, parses them, executes against the KV engine,
/// and writes back the response.
fn handle_client(mut stream: TcpStream, engine: KvEngine) {
    let peer = stream.peer_addr().unwrap();
    eprintln!("[INFO] Client connected: {peer}");

    let reader = BufReader::new(stream.try_clone().unwrap());

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

                if let Err(e) = writeln!(stream, "{response}") {
                    eprintln!("[ERROR] write failed for {peer}: {e}");
                    break;
                }
            }
            Err(e) => {
                eprintln!("[ERROR] read failed for {peer}: {e}");
                break;
            }
        }
    }

    eprintln!("[INFO] Client disconnected: {peer}");
}

/// Parse and execute a command against the KV engine, returning the response string
fn execute_command(input: &str, engine: &KvEngine) -> String {
    let cmd = match parser::parse(input) {
        Ok(cmd) => cmd,
        Err(e) => return parser::format_response(Err(e)),
    };

    #[rustfmt::skip]
    let resp = match cmd {
        Command::Set { key, value } => { engine.set(key, value); "OK".into() },
        Command::Get { key }        => engine.get(&key).unwrap_or_else(|| "NULL".into()),
        Command::Del { key }        => engine.del(&key).then_some("OK").unwrap_or("NULL").into(),
        Command::Ping               => "PONG".into(),
        Command::Unknown(cmd)       => format!("ERR unknown command: {cmd}"),
    };
    resp
}

/// Start the TCP server and listen for incoming connections
///
/// Each connection is handled in a separate thread with a shared KV engine.
pub fn start(addr: &str, engine: KvEngine) {
    let listener = TcpListener::bind(addr).expect("failed to bind");
    println!("[INFO] FerrumKV listening on {addr}");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                let engine = engine.clone();
                thread::spawn(move || {
                    handle_client(stream, engine);
                });
            }
            Err(e) => {
                eprintln!("[ERROR] connection failed: {e}");
            }
        }
    }
}
