//! Command-line argument parsing for the `ferrum-kv` binary.
//!
//! Configuration precedence (highest wins):
//!
//! 1. Command-line flags (`--addr`, `--aof-path`, …)
//! 2. Directives from the config file pointed to by `--config`
//! 3. Built-in defaults ([`DEFAULT_ADDR`], disabled timeout, etc.)
//!
//! The two-pass design keeps `parse` pure: it first scans `argv` into a
//! [`RawFlags`] bag without applying defaults, then — if `--config` was
//! supplied — loads the file and fills in any field `argv` left empty.

use std::path::PathBuf;
use std::time::Duration;

use ferrum_kv::config::{FileConfig, FileConfigError};
use ferrum_kv::persistence::config::{AofConfig, FsyncPolicy};
use ferrum_kv::storage::eviction::{EvictionConfig, EvictionPolicy};

pub const DEFAULT_ADDR: &str = "127.0.0.1:6380";

pub const USAGE: &str = concat!(
    "usage: ferrum-kv [--config PATH] [--addr HOST:PORT] [--aof-path PATH]\n",
    "                 [--appendfsync always|everysec|no]\n",
    "                 [--client-timeout SECONDS] [--maxclients N]\n",
    "                 [--maxmemory BYTES] [--maxmemory-policy POLICY]\n",
    "                 [--maxmemory-samples N]\n",
    "                 [--loglevel off|error|warn|info|debug|trace]"
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
    /// Per-connection idle timeout. `None` (value `0`) disables the timeout
    /// entirely, matching Redis' `timeout 0`.
    client_timeout: Option<Duration>,
    /// Concurrent client connection cap. `None` means "take the default";
    /// the value `0` explicitly disables the limit.
    max_clients: Option<usize>,
    /// Explicit `loglevel` requested via `--loglevel` or the config file.
    /// The caller may still override this with `FERRUM_LOG`/`RUST_LOG`.
    loglevel: Option<String>,
    /// Memory ceiling in bytes. `None` keeps the built-in default of
    /// zero (disabled); `Some(0)` explicitly disables enforcement.
    max_memory: Option<u64>,
    /// Eviction policy when `maxmemory` is reached.
    max_memory_policy: Option<EvictionPolicy>,
    /// How many random candidates each eviction round considers.
    max_memory_samples: Option<usize>,
}

/// Raw, un-merged values taken verbatim from the command line.
///
/// Every field is `Option` to preserve the distinction between "the user
/// passed the flag" and "the user did not". We need that distinction to
/// implement the precedence rules between CLI, file and defaults.
#[derive(Debug, Default)]
struct RawFlags {
    config_path: Option<PathBuf>,
    addr: Option<String>,
    bind: Option<String>, // not a CLI flag yet; reserved so bind+port from file can be combined
    port: Option<u16>,    // ditto
    aof_path: Option<PathBuf>,
    appendfsync: Option<FsyncPolicy>,
    client_timeout: Option<Option<Duration>>, // outer Some = flag was passed, inner None = disabled
    max_clients: Option<usize>,
    loglevel: Option<String>,
    max_memory: Option<u64>,
    max_memory_policy: Option<EvictionPolicy>,
    max_memory_samples: Option<usize>,
    /// Whether AOF was explicitly enabled via the config file's `appendonly yes`.
    /// CLI `--aof-path` implies enabled; this field only carries the file's
    /// intent so that a later merge step can decide.
    _appendonly_from_file: Option<bool>,
}

impl CliArgs {
    /// Parses an iterator of arguments (already stripped of `argv[0]`).
    pub fn parse<I: IntoIterator<Item = String>>(args: I) -> Result<Invocation, String> {
        let raw = match scan_argv(args)? {
            ScanOutcome::Help => return Ok(Invocation::Help),
            ScanOutcome::Flags(f) => f,
        };

        let file_cfg = if let Some(path) = raw.config_path.as_deref() {
            Some(FileConfig::load(path).map_err(file_err_to_cli)?)
        } else {
            None
        };

        let merged = merge(raw, file_cfg.as_ref())?;
        Ok(Invocation::Run(merged))
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

    /// Returns the explicit log level requested via CLI or config file.
    pub fn loglevel(&self) -> Option<&str> {
        self.loglevel.as_deref()
    }

    /// Returns the resolved eviction configuration. Defaults (unlimited
    /// memory, `noeviction`, 5 samples) apply when neither CLI nor config
    /// file specifies a value.
    pub fn eviction_config(&self) -> EvictionConfig {
        let default = EvictionConfig::default();
        EvictionConfig {
            max_memory: self.max_memory.unwrap_or(default.max_memory),
            policy: self.max_memory_policy.unwrap_or(default.policy),
            samples: self.max_memory_samples.unwrap_or(default.samples),
        }
    }
}

enum ScanOutcome {
    Help,
    Flags(RawFlags),
}

/// First pass: walk `argv` and record every flag we recognise without
/// applying defaults. This keeps the merge step trivial.
fn scan_argv<I: IntoIterator<Item = String>>(args: I) -> Result<ScanOutcome, String> {
    let mut iter = args.into_iter();
    let mut raw = RawFlags::default();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--config" => {
                let value = take_value(&mut iter, "--config")?;
                raw.config_path = Some(PathBuf::from(value));
            }
            "--addr" => {
                let value = take_value(&mut iter, "--addr")?;
                raw.addr = Some(value);
            }
            "--aof-path" => {
                let value = take_value(&mut iter, "--aof-path")?;
                raw.aof_path = Some(PathBuf::from(value));
            }
            "--appendfsync" => {
                let value = take_value(&mut iter, "--appendfsync")?;
                raw.appendfsync = Some(
                    FsyncPolicy::parse(&value)
                        .map_err(|e| format!("invalid --appendfsync: {e}"))?,
                );
            }
            "--client-timeout" => {
                let value = take_value(&mut iter, "--client-timeout")?;
                raw.client_timeout = Some(parse_timeout_seconds(&value)?);
            }
            "--maxclients" => {
                let value = take_value(&mut iter, "--maxclients")?;
                raw.max_clients = Some(value.parse().map_err(|_| {
                    format!("invalid --maxclients: '{value}' is not a non-negative integer")
                })?);
            }
            "--loglevel" => {
                let value = take_value(&mut iter, "--loglevel")?;
                raw.loglevel = Some(validate_loglevel(&value)?);
            }
            "--maxmemory" => {
                let value = take_value(&mut iter, "--maxmemory")?;
                raw.max_memory =
                    Some(parse_bytes(&value).map_err(|e| format!("invalid --maxmemory: {e}"))?);
            }
            "--maxmemory-policy" => {
                let value = take_value(&mut iter, "--maxmemory-policy")?;
                let name = value.to_ascii_lowercase();
                raw.max_memory_policy =
                    Some(EvictionPolicy::from_name(&name).ok_or_else(|| {
                        format!(
                            "invalid --maxmemory-policy '{value}' (expected noeviction, \
                             allkeys-lru, volatile-lru, allkeys-lfu, volatile-lfu, \
                             allkeys-random, volatile-random, volatile-ttl, \
                             allkeys-ahe, volatile-ahe)"
                        )
                    })?);
            }
            "--maxmemory-samples" => {
                let value = take_value(&mut iter, "--maxmemory-samples")?;
                raw.max_memory_samples = Some(value.parse().map_err(|_| {
                    format!("invalid --maxmemory-samples: '{value}' is not a non-negative integer")
                })?);
            }
            "-h" | "--help" => return Ok(ScanOutcome::Help),
            other => return Err(format!("unrecognised argument: '{other}'")),
        }
    }

    Ok(ScanOutcome::Flags(raw))
}

/// Merges `raw` flags with an optional [`FileConfig`], applying defaults for
/// any remaining holes and validating cross-field invariants.
fn merge(raw: RawFlags, file: Option<&FileConfig>) -> Result<CliArgs, String> {
    // --- addr ---------------------------------------------------------------
    let addr = if let Some(a) = raw.addr {
        a
    } else if let Some(file) = file {
        compose_addr_from_file(file)
    } else {
        DEFAULT_ADDR.to_string()
    };

    // --- aof -----------------------------------------------------------------
    // CLI always wins. If the file says `appendonly yes` but gives no
    // filename we mirror Redis' default `appendonly.aof`.
    let aof_path = raw.aof_path.or_else(|| match file {
        Some(f) if f.appendonly.unwrap_or(false) => Some(
            f.appendfilename
                .clone()
                .unwrap_or_else(|| PathBuf::from("appendonly.aof")),
        ),
        _ => None,
    });
    let appendfsync = raw.appendfsync.or_else(|| file.and_then(|f| f.appendfsync));

    if appendfsync.is_some() && aof_path.is_none() {
        return Err(
            "appendfsync requires AOF to be enabled (via --aof-path or 'appendonly yes')"
                .to_string(),
        );
    }

    // --- timeout ------------------------------------------------------------
    let client_timeout = match raw.client_timeout {
        Some(v) => v,
        None => file.and_then(|f| f.timeout_secs).and_then(|s| {
            if s == 0 {
                None
            } else {
                Some(Duration::from_secs(s))
            }
        }),
    };

    // --- maxclients ---------------------------------------------------------
    let max_clients = raw.max_clients.or_else(|| file.and_then(|f| f.max_clients));

    // --- loglevel -----------------------------------------------------------
    let loglevel = raw
        .loglevel
        .or_else(|| file.and_then(|f| f.loglevel.clone()));

    // --- maxmemory family ---------------------------------------------------
    let max_memory = raw.max_memory.or_else(|| file.and_then(|f| f.max_memory));
    let max_memory_policy = raw
        .max_memory_policy
        .or_else(|| file.and_then(|f| f.max_memory_policy));
    let max_memory_samples = raw
        .max_memory_samples
        .or_else(|| file.and_then(|f| f.max_memory_samples));

    Ok(CliArgs {
        addr,
        aof_path,
        appendfsync,
        client_timeout,
        max_clients,
        loglevel,
        max_memory,
        max_memory_policy,
        max_memory_samples,
    })
}

/// Builds a `host:port` pair from the file's `bind`/`port` directives.
///
/// If only one of the two is present the missing half falls back to the
/// [`DEFAULT_ADDR`] component so users can override just the port without
/// restating the host.
fn compose_addr_from_file(file: &FileConfig) -> String {
    let default_host = "127.0.0.1";
    let default_port: u16 = 6380;
    let host = file.bind.as_deref().unwrap_or(default_host);
    // `bind` may contain several space-separated addresses; we honour the
    // first one for the listener — mirroring Redis' behaviour.
    let host = host.split_whitespace().next().unwrap_or(default_host);
    let port = file.port.unwrap_or(default_port);
    format!("{host}:{port}")
}

fn file_err_to_cli(e: FileConfigError) -> String {
    e.to_string()
}

fn take_value<I: Iterator<Item = String>>(iter: &mut I, flag: &str) -> Result<String, String> {
    iter.next()
        .ok_or_else(|| format!("{flag} requires a value"))
}

fn validate_loglevel(raw: &str) -> Result<String, String> {
    let lower = raw.to_ascii_lowercase();
    if matches!(
        lower.as_str(),
        "off" | "error" | "warn" | "info" | "debug" | "trace"
    ) {
        Ok(lower)
    } else {
        Err(format!(
            "invalid --loglevel '{raw}' (expected off|error|warn|info|debug|trace)"
        ))
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

/// Parses a Redis-style byte size (`100mb`, `1gb`, plain integer, …).
fn parse_bytes(raw: &str) -> Result<u64, String> {
    let lower = raw.trim().to_ascii_lowercase();
    let (num, factor): (&str, u64) = if let Some(s) = lower.strip_suffix("gb") {
        (s, 1024 * 1024 * 1024)
    } else if let Some(s) = lower.strip_suffix("mb") {
        (s, 1024 * 1024)
    } else if let Some(s) = lower.strip_suffix("kb") {
        (s, 1024)
    } else if let Some(s) = lower.strip_suffix('g') {
        (s, 1024 * 1024 * 1024)
    } else if let Some(s) = lower.strip_suffix('m') {
        (s, 1024 * 1024)
    } else if let Some(s) = lower.strip_suffix('k') {
        (s, 1024)
    } else if let Some(s) = lower.strip_suffix('b') {
        (s, 1)
    } else {
        (lower.as_str(), 1)
    };
    let num: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("'{raw}' is not a byte size"))?;
    num.checked_mul(factor)
        .ok_or_else(|| format!("'{raw}' overflows u64"))
}

// Marker to silence the dead_code lint on the currently-reserved fields.
// The struct itself is module-private; this impl is cheap.
impl RawFlags {
    #[allow(dead_code)]
    fn _touch(&self) {
        let _ = (&self.bind, &self.port, &self._appendonly_from_file);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::Path;

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
        assert!(args.loglevel().is_none());
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
        assert!(err.contains("requires AOF"));
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
            Some(Duration::from_secs(30))
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

    #[test]
    fn loglevel_flag_is_parsed_and_lowercased() {
        assert_eq!(
            parse_run(&["--loglevel", "DEBUG"]).loglevel(),
            Some("debug")
        );
    }

    #[test]
    fn loglevel_rejects_unknown_value() {
        let err = parse(&["--loglevel", "verbose"]).unwrap_err();
        assert!(err.contains("invalid --loglevel"));
    }

    // --- Config file integration tests -------------------------------------

    struct TempConf {
        path: PathBuf,
    }

    impl TempConf {
        fn new(name: &str, body: &str) -> Self {
            let mut dir = std::env::temp_dir();
            dir.push(format!("ferrum-cli-{name}-{}.conf", std::process::id()));
            fs::write(&dir, body).expect("write temp conf");
            Self { path: dir }
        }
    }

    impl Drop for TempConf {
        fn drop(&mut self) {
            let _ = fs::remove_file(&self.path);
        }
    }

    #[test]
    fn config_file_supplies_addr_from_bind_and_port() {
        let conf = TempConf::new("bind-port", "bind 0.0.0.0\nport 7000\n");
        let args = parse_run(&["--config", conf.path.to_str().unwrap()]);
        assert_eq!(args.addr, "0.0.0.0:7000");
    }

    #[test]
    fn cli_addr_overrides_config_file() {
        let conf = TempConf::new("override", "bind 0.0.0.0\nport 7000\n");
        let args = parse_run(&[
            "--config",
            conf.path.to_str().unwrap(),
            "--addr",
            "127.0.0.1:9999",
        ]);
        assert_eq!(args.addr, "127.0.0.1:9999");
    }

    #[test]
    fn config_file_enables_aof_via_appendonly() {
        let conf = TempConf::new(
            "aof",
            "appendonly yes\nappendfilename \"ferrum.aof\"\nappendfsync always\n",
        );
        let args = parse_run(&["--config", conf.path.to_str().unwrap()]);
        let aof = args.aof_config().expect("aof enabled");
        assert_eq!(aof.path(), Path::new("ferrum.aof"));
        assert_eq!(aof.fsync, FsyncPolicy::Always);
    }

    #[test]
    fn config_file_appendonly_without_filename_falls_back_to_default() {
        let conf = TempConf::new("aof-default", "appendonly yes\n");
        let args = parse_run(&["--config", conf.path.to_str().unwrap()]);
        let aof = args.aof_config().expect("aof enabled");
        assert_eq!(aof.path(), Path::new("appendonly.aof"));
    }

    #[test]
    fn config_file_timeout_and_maxclients_are_applied() {
        let conf = TempConf::new("misc", "timeout 45\nmaxclients 512\nloglevel warn\n");
        let args = parse_run(&["--config", conf.path.to_str().unwrap()]);
        assert_eq!(args.client_timeout(), Some(Duration::from_secs(45)));
        assert_eq!(args.max_clients(), Some(512));
        assert_eq!(args.loglevel(), Some("warn"));
    }

    #[test]
    fn cli_timeout_overrides_config_file_timeout() {
        let conf = TempConf::new("timeout-override", "timeout 100\n");
        let args = parse_run(&[
            "--config",
            conf.path.to_str().unwrap(),
            "--client-timeout",
            "5",
        ]);
        assert_eq!(args.client_timeout(), Some(Duration::from_secs(5)));
    }

    #[test]
    fn missing_config_file_reports_error() {
        let err = parse(&["--config", "/definitely/does/not/exist/xyz.conf"]).unwrap_err();
        assert!(err.contains("failed to read config"));
    }

    #[test]
    fn maxmemory_flag_accepts_byte_suffixes() {
        let args = parse_run(&["--maxmemory", "10mb"]);
        assert_eq!(args.eviction_config().max_memory, 10 * 1024 * 1024);
    }

    #[test]
    fn maxmemory_policy_flag_is_parsed() {
        let args = parse_run(&["--maxmemory-policy", "allkeys-lru"]);
        assert_eq!(args.eviction_config().policy, EvictionPolicy::AllKeysLru);
    }

    #[test]
    fn maxmemory_samples_flag_is_parsed() {
        let args = parse_run(&["--maxmemory-samples", "20"]);
        assert_eq!(args.eviction_config().samples, 20);
    }

    #[test]
    fn maxmemory_rejects_unknown_policy() {
        let err = parse(&["--maxmemory-policy", "wishful"]).unwrap_err();
        assert!(err.contains("--maxmemory-policy"));
    }

    #[test]
    fn cli_maxmemory_overrides_config_file() {
        let conf = TempConf::new(
            "maxmem",
            "maxmemory 1kb\nmaxmemory-policy allkeys-lru\nmaxmemory-samples 3\n",
        );
        let args = parse_run(&[
            "--config",
            conf.path.to_str().unwrap(),
            "--maxmemory",
            "2mb",
        ]);
        let cfg = args.eviction_config();
        assert_eq!(cfg.max_memory, 2 * 1024 * 1024);
        assert_eq!(cfg.policy, EvictionPolicy::AllKeysLru);
        assert_eq!(cfg.samples, 3);
    }
}
