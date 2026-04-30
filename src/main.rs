use std::env;
use std::net::{SocketAddr, TcpListener};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::persistence::AofWriter;
use ferrum_kv::storage::engine::KvEngine;

use crate::cli::{CliArgs, Invocation, USAGE};

mod cli;

fn main() -> ExitCode {
    let args = match CliArgs::parse(env::args().skip(1)) {
        Ok(Invocation::Run(args)) => args,
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            eprintln!("[FATAL] {msg}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let engine = match build_engine(&args) {
        Ok(engine) => engine,
        Err(code) => return code,
    };

    // Bind the listener up front so that `--addr :0` is resolved before we
    // install the signal handler that needs the concrete address.
    let listener = match TcpListener::bind(&args.addr) {
        Ok(l) => l,
        Err(e) => {
            eprintln!("[FATAL] failed to bind {}: {}", args.addr, e);
            return ExitCode::FAILURE;
        }
    };
    let local = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            eprintln!("[FATAL] local_addr failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    eprintln!("[INFO] FerrumKV listening on {local}");

    let shutdown = Shutdown::new();
    if let Err(e) = install_signal_handlers(shutdown.clone(), local) {
        eprintln!("[FATAL] failed to install signal handlers: {e}");
        return ExitCode::FAILURE;
    }

    if let Err(e) = ferrum_kv::network::server::run_listener(listener, engine, shutdown) {
        eprintln!("[FATAL] server error: {e}");
        return ExitCode::FAILURE;
    }

    eprintln!("[INFO] shutdown complete");
    ExitCode::SUCCESS
}

/// Builds a `KvEngine`, replaying any existing AOF and attaching a writer
/// if persistence is enabled. Returns an `ExitCode` on fatal setup errors.
fn build_engine(args: &CliArgs) -> Result<KvEngine, ExitCode> {
    let mut engine = KvEngine::new();

    let Some(aof_config) = args.aof_config() else {
        return Ok(engine);
    };

    match ferrum_kv::persistence::replay(aof_config.path(), &engine) {
        Ok(stats) => {
            if stats.applied > 0 || stats.skipped > 0 || stats.truncated_tail {
                eprintln!(
                    "[INFO] AOF replay: applied={} skipped={} truncated_tail={}",
                    stats.applied, stats.skipped, stats.truncated_tail
                );
            }
        }
        Err(e) => {
            eprintln!("[FATAL] AOF replay failed: {e}");
            return Err(ExitCode::FAILURE);
        }
    }

    match AofWriter::open(&aof_config) {
        Ok(writer) => {
            eprintln!(
                "[INFO] AOF enabled: path={} fsync={:?}",
                aof_config.path().display(),
                aof_config.fsync
            );
            engine = engine.with_aof(Arc::new(writer));
            Ok(engine)
        }
        Err(e) => {
            eprintln!("[FATAL] failed to open AOF file: {e}");
            Err(ExitCode::FAILURE)
        }
    }
}

/// Installs SIGINT/SIGTERM handlers that flip the shared shutdown flag and
/// self-connect to the listener so the blocked `accept` returns immediately.
fn install_signal_handlers(shutdown: Shutdown, wake_addr: SocketAddr) -> std::io::Result<()> {
    use signal_hook::consts::{SIGINT, SIGTERM};
    use signal_hook::iterator::Signals;

    let mut signals = Signals::new([SIGINT, SIGTERM])?;
    thread::Builder::new()
        .name("ferrum-signal".into())
        .spawn(move || {
            // Block until either SIGINT or SIGTERM arrives; once we have
            // observed a signal we stop polling — one is enough to start the
            // shutdown sequence.
            if let Some(sig) = signals.forever().next() {
                let name = match sig {
                    SIGINT => "SIGINT",
                    SIGTERM => "SIGTERM",
                    _ => "signal",
                };
                eprintln!("[INFO] received {name}, initiating graceful shutdown");
                shutdown.trigger();
                Shutdown::wake_listener(wake_addr);
            }
        })?;
    Ok(())
}
