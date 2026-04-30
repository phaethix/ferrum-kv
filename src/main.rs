use std::env;
use std::process::ExitCode;
use std::sync::Arc;

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

    if let Err(e) = ferrum_kv::network::server::start(&args.addr, engine) {
        eprintln!("[FATAL] server error: {e}");
        return ExitCode::FAILURE;
    }

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
