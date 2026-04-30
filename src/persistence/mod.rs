//! AOF persistence subsystem.
//!
//! This module implements FerrumKV's append-only file (AOF) persistence:
//! a RESP2 encoder used to serialize mutating commands, configuration types
//! that describe where the log lives and how aggressively it is synced, and
//! (in later commits) the writer and replay loader that drive the log.

pub mod config;

mod resp;
