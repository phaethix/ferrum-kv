use std::fmt;
use std::sync::PoisonError;

/// The unified error type used across FerrumKV.
///
/// Variants cover infrastructure failures, protocol validation, storage
/// constraints, and persistence errors.
#[derive(Debug)]
pub enum FerrumError {
    /// Wraps an I/O error from networking, file access, or persistence.
    IoError(std::io::Error),
    /// Indicates that a lock was poisoned because another thread panicked while
    /// holding it.
    LockPoisoned(String),
    /// Indicates an internal invariant violation.
    Internal(String),

    /// Reports empty or malformed client input.
    ParseError(String),
    /// Reports an unknown command name.
    UnknownCommand(String),
    /// Reports an incorrect argument count for a known command.
    WrongArity { cmd: &'static str },

    /// Reports that a key exceeds the configured size limit.
    KeyTooLong { len: usize, max: usize },
    /// Reports that a value exceeds the configured size limit.
    ValueTooLarge { len: usize, max: usize },
    /// Reports that the memory limit was reached under the `noeviction` policy.
    OutOfMemory,

    /// Reports AOF write, fsync, or replay failures.
    PersistenceError(String),
}

impl fmt::Display for FerrumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FerrumError::IoError(e) => write!(f, "io error: {e}"),
            FerrumError::LockPoisoned(msg) => write!(f, "lock poisoned: {msg}"),
            FerrumError::Internal(msg) => write!(f, "internal error: {msg}"),
            FerrumError::ParseError(msg) => write!(f, "{msg}"),
            FerrumError::UnknownCommand(cmd) => write!(f, "unknown command: '{cmd}'"),
            FerrumError::WrongArity { cmd } => {
                write!(f, "wrong number of arguments for '{cmd}' command")
            }
            FerrumError::KeyTooLong { len, max } => {
                write!(f, "key too long ({len} bytes, max {max})")
            }
            FerrumError::ValueTooLarge { len, max } => {
                write!(f, "value too large ({len} bytes, max {max})")
            }
            FerrumError::OutOfMemory => {
                write!(f, "command not allowed when used memory > maxmemory")
            }
            FerrumError::PersistenceError(msg) => write!(f, "persistence error: {msg}"),
        }
    }
}

impl std::error::Error for FerrumError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            FerrumError::IoError(e) => Some(e),
            _ => None,
        }
    }
}

impl From<std::io::Error> for FerrumError {
    fn from(err: std::io::Error) -> Self {
        Self::IoError(err)
    }
}

impl<T> From<PoisonError<T>> for FerrumError {
    fn from(err: PoisonError<T>) -> Self {
        Self::LockPoisoned(err.to_string())
    }
}
