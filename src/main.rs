use ferrum_kv::storage::engine::KvEngine;

fn main() {
    let addr = "127.0.0.1:6380";
    let engine = KvEngine::new();

    if let Err(e) = ferrum_kv::network::server::start(addr, engine) {
        eprintln!("[FATAL] server error: {e}");
        std::process::exit(1);
    }
}
