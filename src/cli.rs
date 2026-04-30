//! Command-line argument parsing for the `ferrum-kv` binary.
//!
//! Kept as a binary-local module (not re-exported from `lib.rs`) because it
//! only serves the server executable. Unit tests live alongside the parser
//! so they can exercise private fields directly.

use std::path::PathBuf;
use std::time::Duration;

use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};

pub const DEFAULT_ADDR: &str = "127.0.0.1:6380";

pub const USAGE: &str = concat!(
    "usage: ferrum-kv [--addr HOST:PORT] [--aof-path PATH]\n",
    "                 [--appendfsync always|everysec|no]\n",
    "                 [--client-timeout SECONDS] [--maxclients N]"
);

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
    /// Per-connection idle timeout. `None` (CLI value `0`) disables the
    /// timeout entirely, matching Redis' `timeout 0`.
    client_timeout: Option<Duration>,
    /// Concurrent client connection cap. `None` means "take the default";
    /// the value `0` explicitly disables the limit.
    max_clients: Option<usize>,
}

impl CliArgs {
    /// Parses an iterator of arguments (already stripped of `argv[0]`).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, String> {
        let mut iter = args.into_iter();
        let mut addr = DEFAULT_ADDR.to_string();
        let mut aof_path: Option<PathBuf> = None;
        let mut appendfsync: Option<FsyncPolicy> = None;
        let mut client_timeout: Option<Duration> = None;
        let mut max_clients: Option<usize> = None;

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
                "--client-timeout" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--client-timeout requires a value".to_string())?;
                    client_timeout = parse_timeout_seconds(&value)?;
                }
                "--maxclients" => {
                    let value = iter
                        .next()
                        .ok_or_else(|| "--maxclients requires a value".to_string())?;
                    max_clients = Some(value.parse().map_err(|_| {
                        format!("invalid --maxclients: '{value}' is not a non-negative integer")
                    })?);
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
            client_timeout,
            max_clients,
        }))
    }

    /// Returns the AOF configuration if the user enabled persistence.
    pub fn aof_config(&self) -> Option<AofConfig> {
        self.aof_path
            .as_ref()
            .map(|path| AofConfig::new(path.clone(), self.appendfsync.unwrap_or_default()))
    }

    /// Returns the idle timeout applied to every accepted connection, if any.
    pub fn client_timeout(&self) -> Option<Duration> {
        self.client_timeout
    }

    /// Returns the `max_clients` override if the user supplied one.
    pub fn max_clients(&self) -> Option<usize> {
        self.max_clients
    }
}

/// Parses a non-negative integer number of seconds.
///
/// `0` is treated as "disabled" and returns `Ok(None)`, matching Redis'
/// convention for the `timeout` directive.
fn parse_timeout_seconds(raw: &str) -> Result<Option<Duration>, String> {
    let secs: u64 = raw
        .parse()
        .map_err(|_| format!("invalid --client-timeout: '{raw}' is not a non-negative integer"))?;
    if secs == 0 {
        Ok(None)
    } else {
        Ok(Some(Duration::from_secs(secs)))
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

    #[test]
    fn client_timeout_defaults_to_none() {
        assert!(parse_run(&[]).client_timeout().is_none());
    }

    #[test]
    fn client_timeout_zero_is_disabled() {
        assert!(
            parse_run(&["--client-timeout", "0"])
                .client_timeout()
                .is_none()
        );
    }

    #[test]
    fn client_timeout_positive_is_parsed() {
        assert_eq!(
            parse_run(&["--client-timeout", "30"]).client_timeout(),
            Some(std::time::Duration::from_secs(30))
        );
    }

    #[test]
    fn client_timeout_rejects_non_integer() {
        let err = parse(&["--client-timeout", "abc"]).unwrap_err();
        assert!(err.contains("--client-timeout"));
    }

    #[test]
    fn client_timeout_rejects_negative() {
        let err = parse(&["--client-timeout", "-1"]).unwrap_err();
        assert!(err.contains("--client-timeout"));
    }

    #[test]
    fn max_clients_defaults_to_none() {
        assert!(parse_run(&[]).max_clients().is_none());
    }

    #[test]
    fn max_clients_positive_is_parsed() {
        assert_eq!(parse_run(&["--maxclients", "128"]).max_clients(), Some(128));
    }

    #[test]
    fn max_clients_zero_means_unlimited() {
        assert_eq!(parse_run(&["--maxclients", "0"]).max_clients(), Some(0));
    }

    #[test]
    fn max_clients_rejects_non_integer() {
        let err = parse(&["--maxclients", "many"]).unwrap_err();
        assert!(err.contains("--maxclients"));
    }
}
