//! Configuration types for the AOF subsystem.

use std::path::{Path, PathBuf};

/// Policy that controls how often AOF writes are flushed to disk.
///
/// The variants mirror Redis' `appendfsync` configuration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FsyncPolicy {
    /// Call `fsync` after every write. Highest durability, lowest throughput.
    Always,
    /// Flush once per second from a background thread. This is the default.
    EverySec,
    /// Never call `fsync` explicitly; rely on the operating system.
    No,
}

impl Default for FsyncPolicy {
    fn default() -> Self {
        Self::EverySec
    }
}

impl FsyncPolicy {
    /// Parses the textual form used on the command line and in config files.
    ///
    /// Accepts `always`, `everysec`, and `no` (case-insensitive).
    pub fn parse(input: &str) -> Result<Self, ParseFsyncPolicyError> {
        match input.trim().to_ascii_lowercase().as_str() {
            "always" => Ok(Self::Always),
            "everysec" => Ok(Self::EverySec),
            "no" => Ok(Self::No),
            other => Err(ParseFsyncPolicyError(other.to_string())),
        }
    }
}

/// Error returned by [`FsyncPolicy::parse`] when the input is unrecognised.
#[derive(Debug, PartialEq, Eq)]
pub struct ParseFsyncPolicyError(pub String);

impl std::fmt::Display for ParseFsyncPolicyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "invalid appendfsync value '{}', expected one of always|everysec|no",
            self.0
        )
    }
}

impl std::error::Error for ParseFsyncPolicyError {}

/// Runtime configuration for the AOF writer and replayer.
#[derive(Debug, Clone)]
pub struct AofConfig {
    /// Path of the AOF file on disk.
    pub path: PathBuf,
    /// Fsync policy used by the writer.
    pub fsync: FsyncPolicy,
}

impl AofConfig {
    /// Creates a new configuration with the given path and fsync policy.
    pub fn new(path: impl Into<PathBuf>, fsync: FsyncPolicy) -> Self {
        Self {
            path: path.into(),
            fsync,
        }
    }

    /// Returns the AOF file path.
    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Default for AofConfig {
    fn default() -> Self {
        Self {
            path: PathBuf::from("ferrum.aof"),
            fsync: FsyncPolicy::default(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fsync_policy_parses_known_values() {
        assert_eq!(FsyncPolicy::parse("always").unwrap(), FsyncPolicy::Always);
        assert_eq!(
            FsyncPolicy::parse("EVERYSEC").unwrap(),
            FsyncPolicy::EverySec
        );
        assert_eq!(FsyncPolicy::parse("  no  ").unwrap(), FsyncPolicy::No);
    }

    #[test]
    fn fsync_policy_rejects_unknown_values() {
        let err = FsyncPolicy::parse("sometimes").unwrap_err();
        assert_eq!(err, ParseFsyncPolicyError("sometimes".to_string()));
    }

    #[test]
    fn aof_config_default_is_everysec() {
        let cfg = AofConfig::default();
        assert_eq!(cfg.fsync, FsyncPolicy::EverySec);
        assert_eq!(cfg.path(), Path::new("ferrum.aof"));
    }
}
