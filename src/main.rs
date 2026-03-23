use std::io::{BufRead, BufReader, Write};
use std::net::{TcpListener, TcpStream};
use std::thread;

fn handle_client(mut stream: TcpStream) {
    let peer = stream.peer_addr().unwrap();
    println!("[INFO] Client connected: {}", peer);

    let reader = BufReader::new(stream.try_clone().unwrap());

    for line in reader.lines() {
        match line {
            Ok(cmd) => {
                let cmd = cmd.trim();
                println!("[DEBUG] recv: {}", cmd);

                let response = process_command(cmd);

                if let Err(e) = writeln!(stream, "{}", response) {
                    println!("[ERROR] write failed: {}", e);
                    break;
                }
            }
            Err(e) => {
                println!("[ERROR] read failed: {}", e);
                break;
            }
        }
    }

    println!("[INFO] Client disconnected: {}", peer);
}

fn process_command(cmd: &str) -> String {
    match cmd {
        "PING" => "PONG".to_string(),
        "HELLO" => "FerrumKV".to_string(),
        _ => format!("ERR unknown command: {}", cmd),
    }
}

fn main() {
    let addr = "127.0.0.1:6379";
    let listener = TcpListener::bind(addr).expect("failed to bind");

    println!("[INFO] FerrumKV listening on {}", addr);

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                thread::spawn(|| {
                    handle_client(stream);
                });
            }
            Err(e) => {
                println!("[ERROR] connection failed: {}", e);
            }
        }
    }
}
