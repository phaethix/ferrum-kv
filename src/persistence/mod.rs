//! AOF persistence subsystem.
//!
//! This module implements FerrumKV's append-only file (AOF) persistence:
//! a RESP2 encoder used to serialize mutating commands, configuration types
//! that describe where the log lives and how aggressively it is synced, and
//! the writer and replay loader that drive the log.

pub mod config;
pub mod replay;
mod resp;
mod writer;

pub use replay::{ReplayStats, replay};
pub use writer::AofWriter;
