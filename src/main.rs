use std::env;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;

use ferrum_kv::persistence::AofWriter;
use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};
use ferrum_kv::storage::engine::KvEngine;

const DEFAULT_ADDR: &str = "127.0.0.1:6380";

fn main() -> ExitCode {
    let args = match CliArgs::parse(env::args().skip(1)) {
        Ok(args) => args,
        Err(msg) => {
            eprintln!("[FATAL] {msg}");
            eprintln!("{USAGE}");
            return ExitCode::from(2);
        }
    };

    let mut engine = KvEngine::new();

    if let Some(aof_config) = args.aof_config() {
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
                return ExitCode::FAILURE;
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
            }
            Err(e) => {
                eprintln!("[FATAL] failed to open AOF file: {e}");
                return ExitCode::FAILURE;
            }
        }
    }

    if let Err(e) = ferrum_kv::network::server::start(&args.addr, engine) {
        eprintln!("[FATAL] server error: {e}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

const USAGE: &str =
    "usage: ferrum-kv [--addr HOST:PORT] [--aof-path PATH] [--appendfsync always|everysec|no]";

/// Parsed command-line arguments for the FerrumKV server.
#[derive(Debug)]
struct CliArgs {
    addr: String,
    aof_path: Option<PathBuf>,
    appendfsync: Option<FsyncPolicy>,
}

impl CliArgs {
    fn parse<I: Iterator<Item = String>>(mut iter: I) -> Result<Self, String> {
        let mut addr = DEFAULT_ADDR.to_string();
        let mut aof_path: Option<PathBuf> = None;
        let mut appendfsync: Option<FsyncPolicy> = None;

        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--addr" => {
                    addr = iter
                        .next()
                        .ok_or_else(|| "--addr requires a value".to_string())?;
                }
                "--aof-path" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--aof-path requires a value".to_string())?;
                    aof_path = Some(PathBuf::from(value));
                }
                "--appendfsync" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--appendfsync requires a value".to_string())?;
                    appendfsync = Some(
                        FsyncPolicy::parse(&value)
                            .map_err(|e| format!("invalid --appendfsync: {e}"))?,
                    );
                }
                "-h" | "--help" => {
                    println!("{USAGE}");
                    std::process::exit(0);
                }
                other => return Err(format!("unrecognised argument: '{other}'")),
            }
        }

        if appendfsync.is_some() && aof_path.is_none() {
            return Err("--appendfsync requires --aof-path to enable AOF".to_string());
        }

        Ok(Self {
            addr,
            aof_path,
            appendfsync,
        })
    }

    /// Returns the AOF configuration if the user enabled persistence.
    fn aof_config(&self) -> Option<AofConfig> {
        self.aof_path
            .as_ref()
            .map(|path| AofConfig::new(path.clone(), self.appendfsync.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CliArgs, String> {
        CliArgs::parse(args.iter().map(|s| s.to_string()))
    }

    #[test]
    fn defaults_when_no_args_given() {
        let args = parse(&[]).unwrap();
        assert_eq!(args.addr, DEFAULT_ADDR);
        assert!(args.aof_config().is_none());
    }

    #[test]
    fn parses_aof_path_with_default_fsync() {
        let args = parse(&["--aof-path", "ferrum.aof"]).unwrap();
        let cfg = args.aof_config().unwrap();
        assert_eq!(cfg.path(), std::path::Path::new("ferrum.aof"));
        assert_eq!(cfg.fsync, FsyncPolicy::EverySec);
    }

    #[test]
    fn parses_explicit_appendfsync() {
        let args = parse(&["--aof-path", "a.aof", "--appendfsync", "always"]).unwrap();
        assert_eq!(args.aof_config().unwrap().fsync, FsyncPolicy::Always);
    }

    #[test]
    fn rejects_appendfsync_without_aof_path() {
        let err = parse(&["--appendfsync", "always"]).unwrap_err();
        assert!(err.contains("requires --aof-path"));
    }

    #[test]
    fn rejects_unknown_appendfsync_value() {
        let err = parse(&["--aof-path", "a.aof", "--appendfsync", "weekly"]).unwrap_err();
        assert!(err.contains("invalid --appendfsync"));
    }

    #[test]
    fn rejects_unknown_flag() {
        let err = parse(&["--nope"]).unwrap_err();
        assert!(err.contains("unrecognised"));
    }

    #[test]
    fn parses_custom_addr() {
        let args = parse(&["--addr", "0.0.0.0:1234"]).unwrap();
        assert_eq!(args.addr, "0.0.0.0:1234");
    }
}
