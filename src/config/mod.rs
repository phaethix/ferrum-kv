//! Configuration file support.
//!
//! FerrumKV accepts a Redis-style configuration file: a plain-text file with
//! one `directive value[ value...]` pair per line, lines starting with `#`
//! treated as comments, blank lines ignored. This is deliberately simpler
//! than TOML/YAML so a seasoned Redis operator can reuse their muscle memory
//! and so we avoid pulling in a serde/toml dependency for a file with a
//! dozen entries.
//!
//! The parsed representation lives in [`FileConfig`]. Each field is `Option`
//! so that "not set in the file" is distinguishable from "set to the default";
//! the CLI layer then overrides any field the user supplied on the command
//! line before feeding the result into runtime configuration.

pub mod file;

pub use file::{FileConfig, FileConfigError};
