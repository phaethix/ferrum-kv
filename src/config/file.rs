//! Redis-style configuration file parser.
//!
//! Grammar (informal):
//! ```text
//! line        := comment | blank | directive
//! comment     := '#' .*
//! blank       := whitespace*
//! directive   := key (whitespace+ value)+  whitespace*
//! ```
//!
//! Values are taken verbatim after the first whitespace gap; unknown
//! directives are a parse error so typos surface early rather than silently
//! being ignored.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use crate::persistence::config::FsyncPolicy;
use crate::storage::eviction::EvictionPolicy;

/// Parsed configuration file contents.
///
/// Every field is `Option`: `None` means "the file did not specify a value
/// for this directive" and the CLI layer is free to either apply a default
/// or require the user to pass the flag explicitly.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct FileConfig {
    pub bind: Option<String>,
    pub port: Option<u16>,
    /// Idle timeout in seconds. `0` means "disabled" (matches Redis).
    pub timeout_secs: Option<u64>,
    pub max_clients: Option<usize>,
    pub appendonly: Option<bool>,
    pub appendfilename: Option<PathBuf>,
    pub appendfsync: Option<FsyncPolicy>,
    /// Log level string accepted by `env_logger` (e.g. `info`, `debug`).
    pub loglevel: Option<String>,
    /// Memory ceiling in bytes. `0` disables enforcement.
    pub max_memory: Option<u64>,
    /// Eviction policy selected when `maxmemory` is reached.
    pub max_memory_policy: Option<EvictionPolicy>,
    /// Number of keys inspected per eviction round.
    pub max_memory_samples: Option<usize>,
}

/// Error type for configuration file parsing.
#[derive(Debug)]
pub enum FileConfigError {
    /// The file could not be read (missing, permission denied, …).
    Io(std::io::Error, PathBuf),
    /// A line failed to parse. `line_no` is 1-based.
    Parse {
        path: PathBuf,
        line_no: usize,
        message: String,
    },
}

impl fmt::Display for FileConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FileConfigError::Io(e, path) => {
                write!(f, "failed to read config '{}': {e}", path.display())
            }
            FileConfigError::Parse {
                path,
                line_no,
                message,
            } => write!(f, "config '{}' line {line_no}: {message}", path.display()),
        }
    }
}

impl std::error::Error for FileConfigError {}

impl FileConfig {
    /// Loads and parses the given configuration file.
    pub fn load(path: &Path) -> Result<Self, FileConfigError> {
        let text =
            fs::read_to_string(path).map_err(|e| FileConfigError::Io(e, path.to_path_buf()))?;
        Self::parse(&text, path)
    }

    /// Parses the given text as a config file.
    ///
    /// `source` is used only for error messages; callers that are not reading
    /// from disk can pass an arbitrary placeholder path.
    pub fn parse(text: &str, source: &Path) -> Result<Self, FileConfigError> {
        let mut cfg = FileConfig::default();
        for (idx, raw_line) in text.lines().enumerate() {
            let line_no = idx + 1;
            let trimmed = raw_line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }

            let mut parts = trimmed.splitn(2, char::is_whitespace);
            let key = parts.next().unwrap_or("");
            let value = parts.next().map(str::trim).unwrap_or("");
            if value.is_empty() {
                return Err(parse_err(
                    source,
                    line_no,
                    format!("directive '{key}' requires a value"),
                ));
            }

            apply_directive(&mut cfg, key, value)
                .map_err(|message| parse_err(source, line_no, message))?;
        }
        Ok(cfg)
    }
}

fn parse_err(path: &Path, line_no: usize, message: String) -> FileConfigError {
    FileConfigError::Parse {
        path: path.to_path_buf(),
        line_no,
        message,
    }
}

fn apply_directive(cfg: &mut FileConfig, key: &str, value: &str) -> Result<(), String> {
    // Redis directives are case-insensitive; normalise once here.
    let key_lower = key.to_ascii_lowercase();
    match key_lower.as_str() {
        "bind" => {
            cfg.bind = Some(value.to_string());
        }
        "port" => {
            cfg.port = Some(parse_u16(value, "port")?);
        }
        "timeout" => {
            cfg.timeout_secs = Some(parse_u64(value, "timeout")?);
        }
        "maxclients" => {
            cfg.max_clients = Some(parse_usize(value, "maxclients")?);
        }
        "appendonly" => {
            cfg.appendonly = Some(parse_bool(value)?);
        }
        "appendfilename" => {
            let unquoted = strip_quotes(value);
            cfg.appendfilename = Some(PathBuf::from(unquoted));
        }
        "appendfsync" => {
            cfg.appendfsync =
                Some(FsyncPolicy::parse(value).map_err(|e| format!("invalid appendfsync: {e}"))?);
        }
        "loglevel" => {
            // Validate against the set env_logger understands; reject typos
            // so the user hears about them at startup rather than silently
            // getting the default filter.
            let normalised = value.to_ascii_lowercase();
            if !matches!(
                normalised.as_str(),
                "off" | "error" | "warn" | "info" | "debug" | "trace"
            ) {
                return Err(format!(
                    "invalid loglevel '{value}' (expected off|error|warn|info|debug|trace)"
                ));
            }
            cfg.loglevel = Some(normalised);
        }
        "maxmemory" => {
            cfg.max_memory = Some(parse_bytes(value, "maxmemory")?);
        }
        "maxmemory-policy" => {
            let name = value.to_ascii_lowercase();
            cfg.max_memory_policy = Some(EvictionPolicy::from_name(&name).ok_or_else(|| {
                format!(
                    "invalid maxmemory-policy '{value}' (expected one of noeviction, \
                         allkeys-lru, volatile-lru, allkeys-random, volatile-random, \
                         volatile-ttl)"
                )
            })?);
        }
        "maxmemory-samples" => {
            cfg.max_memory_samples = Some(parse_usize(value, "maxmemory-samples")?);
        }
        other => return Err(format!("unknown directive '{other}'")),
    }
    Ok(())
}

fn parse_u16(raw: &str, name: &str) -> Result<u16, String> {
    raw.parse()
        .map_err(|_| format!("invalid {name}: '{raw}' is not a u16"))
}

fn parse_u64(raw: &str, name: &str) -> Result<u64, String> {
    raw.parse()
        .map_err(|_| format!("invalid {name}: '{raw}' is not a non-negative integer"))
}

fn parse_usize(raw: &str, name: &str) -> Result<usize, String> {
    raw.parse()
        .map_err(|_| format!("invalid {name}: '{raw}' is not a non-negative integer"))
}

fn parse_bool(raw: &str) -> Result<bool, String> {
    match raw.to_ascii_lowercase().as_str() {
        "yes" | "true" | "on" | "1" => Ok(true),
        "no" | "false" | "off" | "0" => Ok(false),
        other => Err(format!("expected yes/no, got '{other}'")),
    }
}

fn strip_quotes(raw: &str) -> &str {
    let bytes = raw.as_bytes();
    if bytes.len() >= 2
        && ((bytes[0] == b'"' && bytes[bytes.len() - 1] == b'"')
            || (bytes[0] == b'\'' && bytes[bytes.len() - 1] == b'\''))
    {
        &raw[1..raw.len() - 1]
    } else {
        raw
    }
}

/// Parses byte sizes in Redis-friendly form: a bare integer, or an integer
/// followed by one of the suffixes `b`, `k`, `kb`, `m`, `mb`, `g`, `gb`
/// (case-insensitive, no space between number and suffix).
fn parse_bytes(raw: &str, name: &str) -> Result<u64, String> {
    let lower = raw.trim().to_ascii_lowercase();
    let (num, factor): (&str, u64) = if let Some(stripped) = lower.strip_suffix("gb") {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix("mb") {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix("kb") {
        (stripped, 1024)
    } else if let Some(stripped) = lower.strip_suffix('g') {
        (stripped, 1024 * 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('m') {
        (stripped, 1024 * 1024)
    } else if let Some(stripped) = lower.strip_suffix('k') {
        (stripped, 1024)
    } else if let Some(stripped) = lower.strip_suffix('b') {
        (stripped, 1)
    } else {
        (lower.as_str(), 1)
    };
    let num: u64 = num
        .trim()
        .parse()
        .map_err(|_| format!("invalid {name}: '{raw}' is not a byte size"))?;
    num.checked_mul(factor)
        .ok_or_else(|| format!("invalid {name}: '{raw}' overflows u64"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(text: &str) -> Result<FileConfig, FileConfigError> {
        FileConfig::parse(text, Path::new("test.conf"))
    }

    #[test]
    fn empty_file_yields_default_config() {
        assert_eq!(parse("").unwrap(), FileConfig::default());
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let cfg = parse("# heading\n\n   \n# another\n").unwrap();
        assert_eq!(cfg, FileConfig::default());
    }

    #[test]
    fn bind_and_port_are_parsed() {
        let cfg = parse("bind 0.0.0.0\nport 6380\n").unwrap();
        assert_eq!(cfg.bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(cfg.port, Some(6380));
    }

    #[test]
    fn bind_accepts_whitespace_separated_addresses() {
        // Redis' `bind` accepts multiple addresses on one line; we keep the
        // whole remainder verbatim for forward compatibility.
        let cfg = parse("bind 127.0.0.1 ::1\n").unwrap();
        assert_eq!(cfg.bind.as_deref(), Some("127.0.0.1 ::1"));
    }

    #[test]
    fn timeout_and_maxclients() {
        let cfg = parse("timeout 30\nmaxclients 128\n").unwrap();
        assert_eq!(cfg.timeout_secs, Some(30));
        assert_eq!(cfg.max_clients, Some(128));
    }

    #[test]
    fn appendonly_and_fsync() {
        let cfg = parse(
            "appendonly yes\n\
             appendfilename \"ferrum.aof\"\n\
             appendfsync everysec\n",
        )
        .unwrap();
        assert_eq!(cfg.appendonly, Some(true));
        assert_eq!(cfg.appendfilename, Some(PathBuf::from("ferrum.aof")));
        assert_eq!(cfg.appendfsync, Some(FsyncPolicy::EverySec));
    }

    #[test]
    fn loglevel_is_lowercased_and_validated() {
        assert_eq!(
            parse("loglevel DEBUG\n").unwrap().loglevel.as_deref(),
            Some("debug")
        );
        let err = parse("loglevel verbose\n").unwrap_err();
        assert!(err.to_string().contains("invalid loglevel"));
    }

    #[test]
    fn unknown_directive_is_rejected() {
        let err = parse("glorp 42\n").unwrap_err();
        assert!(err.to_string().contains("unknown directive"));
    }

    #[test]
    fn missing_value_is_rejected() {
        let err = parse("port\n").unwrap_err();
        assert!(err.to_string().contains("requires a value"));
    }

    #[test]
    fn invalid_port_is_rejected() {
        let err = parse("port banana\n").unwrap_err();
        assert!(err.to_string().contains("invalid port"));
    }

    #[test]
    fn appendonly_accepts_common_truthy_forms() {
        for v in ["yes", "YES", "true", "on", "1"] {
            let cfg = parse(&format!("appendonly {v}\n")).unwrap();
            assert_eq!(cfg.appendonly, Some(true), "value: {v}");
        }
        for v in ["no", "false", "off", "0"] {
            let cfg = parse(&format!("appendonly {v}\n")).unwrap();
            assert_eq!(cfg.appendonly, Some(false), "value: {v}");
        }
    }

    #[test]
    fn key_is_case_insensitive() {
        let cfg = parse("PORT 12345\n").unwrap();
        assert_eq!(cfg.port, Some(12345));
    }

    #[test]
    fn maxmemory_accepts_byte_and_suffix_forms() {
        assert_eq!(parse("maxmemory 0\n").unwrap().max_memory, Some(0));
        assert_eq!(parse("maxmemory 1024\n").unwrap().max_memory, Some(1024));
        assert_eq!(parse("maxmemory 2k\n").unwrap().max_memory, Some(2 * 1024));
        assert_eq!(
            parse("maxmemory 100mb\n").unwrap().max_memory,
            Some(100 * 1024 * 1024)
        );
        assert_eq!(
            parse("maxmemory 1GB\n").unwrap().max_memory,
            Some(1024 * 1024 * 1024)
        );
    }

    #[test]
    fn maxmemory_rejects_nonsense() {
        assert!(parse("maxmemory abc\n").is_err());
        assert!(parse("maxmemory 10tb\n").is_err());
    }

    #[test]
    fn maxmemory_policy_is_parsed_and_validated() {
        assert_eq!(
            parse("maxmemory-policy allkeys-lru\n")
                .unwrap()
                .max_memory_policy,
            Some(EvictionPolicy::AllKeysLru),
        );
        assert!(parse("maxmemory-policy bogus\n").is_err());
    }

    #[test]
    fn maxmemory_samples_is_parsed() {
        assert_eq!(
            parse("maxmemory-samples 12\n").unwrap().max_memory_samples,
            Some(12),
        );
    }
}
