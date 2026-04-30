//! Command-line argument parsing for the `ferrum-kv` binary.
//!
//! Kept as a binary-local module (not re-exported from `lib.rs`) because it
//! only serves the server executable. Unit tests live alongside the parser
//! so they can exercise private fields directly.

use std::path::PathBuf;

use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};

pub const DEFAULT_ADDR: &str = "127.0.0.1:6380";

pub const USAGE: &str =
    "usage: ferrum-kv [--addr HOST:PORT] [--aof-path PATH] [--appendfsync always|everysec|no]";

/// Outcome of parsing `argv`.
///
/// `Help` is a first-class variant so `parse` stays pure and the caller
/// decides whether to print usage and exit.
#[derive(Debug)]
pub enum Invocation {
    Run(CliArgs),
    Help,
}

/// Parsed command-line arguments for the FerrumKV server.
#[derive(Debug)]
pub struct CliArgs {
    pub addr: String,
    aof_path: Option<PathBuf>,
    appendfsync: Option<FsyncPolicy>,
}

impl CliArgs {
    /// Parses an iterator of arguments (already stripped of `argv[0]`).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, String> {
        let mut iter = args.into_iter();
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
                "-h" | "--help" => return Ok(Invocation::Help),
                other => return Err(format!("unrecognised argument: '{other}'")),
            }
        }

        if appendfsync.is_some() && aof_path.is_none() {
            return Err("--appendfsync requires --aof-path to enable AOF".to_string());
        }

        Ok(Invocation::Run(Self {
            addr,
            aof_path,
            appendfsync,
        }))
    }

    /// Returns the AOF configuration if the user enabled persistence.
    pub fn aof_config(&self) -> Option<AofConfig> {
        self.aof_path
            .as_ref()
            .map(|path| AofConfig::new(path.clone(), self.appendfsync.unwrap_or_default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Invocation, String> {
        CliArgs::parse(args.iter().map(|s| s.to_string()))
    }

    fn parse_run(args: &[&str]) -> CliArgs {
        match parse(args).unwrap() {
            Invocation::Run(a) => a,
            Invocation::Help => panic!("expected Run, got Help"),
        }
    }

    #[test]
    fn defaults_when_no_args_given() {
        let args = parse_run(&[]);
        assert_eq!(args.addr, DEFAULT_ADDR);
        assert!(args.aof_config().is_none());
    }

    #[test]
    fn parses_aof_path_with_default_fsync() {
        let args = parse_run(&["--aof-path", "ferrum.aof"]);
        let cfg = args.aof_config().unwrap();
        assert_eq!(cfg.path(), std::path::Path::new("ferrum.aof"));
        assert_eq!(cfg.fsync, FsyncPolicy::EverySec);
    }

    #[test]
    fn parses_explicit_appendfsync() {
        let args = parse_run(&["--aof-path", "a.aof", "--appendfsync", "always"]);
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
        let args = parse_run(&["--addr", "0.0.0.0:1234"]);
        assert_eq!(args.addr, "0.0.0.0:1234");
    }

    #[test]
    fn help_flag_returns_help_variant() {
        assert!(matches!(parse(&["--help"]).unwrap(), Invocation::Help));
        assert!(matches!(parse(&["-h"]).unwrap(), Invocation::Help));
    }
}
