use std::fmt;

/// Unified error type for FerrumKV
#[derive(Debug)]
pub enum FerrumError {
    /// Network IO errors
    IoError(std::io::Error),
    /// Command parsing errors (malformed input)
    ParseError(String),
    /// Storage operation errors
    StorageError(String),
    /// AOF persistence errors
    PersistenceError(String),
}

impl fmt::Display for FerrumError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            FerrumError::IoError(e) => write!(f, "IO error: {e}"),
            FerrumError::ParseError(msg) => write!(f, "Parse error: {msg}"),
            FerrumError::StorageError(msg) => write!(f, "Storage error: {msg}"),
            FerrumError::PersistenceError(msg) => write!(f, "Persistence error: {msg}"),
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
