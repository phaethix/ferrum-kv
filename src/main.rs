use std::env;
use std::net::{SocketAddr, TcpListener};
use std::process::ExitCode;
use std::sync::Arc;
use std::thread;

use log::{error, info, warn};

use ferrum_kv::network::server::ServerConfig;
use ferrum_kv::network::shutdown::Shutdown;
use ferrum_kv::persistence::AofWriter;
use ferrum_kv::storage::engine::KvEngine;

use crate::cli::{CliArgs, Invocation, USAGE};

mod cli;

/// Environment variable consulted by `env_logger`. We prefer a project-specific
/// name so that users embedding FerrumKV as a library can keep their own
/// `RUST_LOG` unaffected; we still fall back to `RUST_LOG` for convenience.
const LOG_ENV: &str = "FERRUM_LOG";

fn main() -> ExitCode {
    let args = match CliArgs::parse(env::args().skip(1)) {
        Ok(Invocation::Run(args)) => args,
        Ok(Invocation::Help) => {
            println!("{USAGE}");
            return ExitCode::SUCCESS;
        }
        Err(msg) => {
            // Logger not yet initialised: write straight to stderr so the
            // user sees the problem even if `RUST_LOG` is misconfigured.
            eprintln!("ferrum-kv: {msg}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    init_logger(args.loglevel());

    let engine = match build_engine(&args) {
        Ok(engine) => engine,
        Err(code) => return code,
    };

    // Bind the listener up front so that `--addr :0` is resolved before we
    // install the signal handler that needs the concrete address.
    let listener = match TcpListener::bind(&args.addr) {
        Ok(l) => l,
        Err(e) => {
            error!("failed to bind {}: {}", args.addr, e);
            return ExitCode::FAILURE;
        }
    };
    let local = match listener.local_addr() {
        Ok(a) => a,
        Err(e) => {
            error!("local_addr failed: {e}");
            return ExitCode::FAILURE;
        }
    };
    info!("FerrumKV listening on {local}");

    let shutdown = Shutdown::new();
    if let Err(e) = install_signal_handlers(shutdown.clone(), local) {
        error!("failed to install signal handlers: {e}");
        return ExitCode::FAILURE;
    }

    let server_config = ServerConfig {
        client_timeout: args.client_timeout(),
        max_clients: args
            .max_clients()
            .unwrap_or_else(|| ServerConfig::default().max_clients),
    };
    match server_config.client_timeout {
        Some(d) => info!("client idle timeout: {}s", d.as_secs()),
        None => info!("client idle timeout: disabled"),
    }
    if server_config.max_clients == 0 {
        info!("maxclients: unlimited");
    } else {
        info!("maxclients: {}", server_config.max_clients);
    }

    if let Err(e) =
        ferrum_kv::network::server::run_listener(listener, engine, shutdown, server_config)
    {
        error!("server error: {e}");
        return ExitCode::FAILURE;
    }

    info!("shutdown complete");
    ExitCode::SUCCESS
}

/// Initialises `env_logger` with the following precedence (highest wins):
///
/// 1. `FERRUM_LOG` — project-specific, takes precedence.
/// 2. `RUST_LOG`   — the standard `env_logger` convention.
/// 3. `--loglevel` / `loglevel` directive from the config file.
/// 4. `info`       — sensible default for a server binary.
///
/// Environment variables win so that operators can always crank the log
/// level up (or down) without editing the config file.
fn init_logger(cli_level: Option<&str>) {
    let filter = env::var(LOG_ENV)
        .or_else(|_| env::var("RUST_LOG"))
        .unwrap_or_else(|_| cli_level.unwrap_or("info").to_string());
    // `try_init` so re-invocations (e.g. in tests) do not panic.
    let _ = env_logger::Builder::new()
        .parse_filters(&filter)
        .format_timestamp_secs()
        .try_init();
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
                info!(
                    "AOF replay: applied={} skipped={} truncated_tail={}",
                    stats.applied, stats.skipped, stats.truncated_tail
                );
            }
        }
        Err(e) => {
            error!("AOF replay failed: {e}");
            return Err(ExitCode::FAILURE);
        }
    }

    match AofWriter::open(&aof_config) {
        Ok(writer) => {
            info!(
                "AOF enabled: path={} fsync={:?}",
                aof_config.path().display(),
                aof_config.fsync
            );
            engine = engine.with_aof(Arc::new(writer));
            Ok(engine)
        }
        Err(e) => {
            error!("failed to open AOF file: {e}");
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
                warn!("received {name}, initiating graceful shutdown");
                shutdown.trigger();
                Shutdown::wake_listener(wake_addr);
            }
        })?;
    Ok(())
}
