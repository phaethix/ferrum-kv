//! Built-in web dashboard for FerrumKV.
//!
//! The dashboard is a self-contained HTTP server (no external dependencies
//! beyond `tokio`, which the KV server already uses) that exposes a small
//! JSON API plus a single-page web UI. Point a browser at its address and you
//! get key browsing, inline editing, live stats and a command console — no
//! separate desktop client required.
//!
//! The HTTP layer is deliberately tiny: a hand-written HTTP/1.1 server whose
//! request handling is factored into the pure [`http::route`] function so it
//! can be unit-tested without opening a socket.
//!
//! ## Access control
//!
//! The dashboard can execute arbitrary commands (including `FLUSHDB` and
//! `CONFIG SET requirepass`), and it deliberately bypasses the RESP
//! `requirepass` check. Binding it to loopback is therefore enforced: a
//! non-loopback address requires `--dashboard-token`, and every `/api/*`
//! request must then present that token.

pub mod http;

use std::net::ToSocketAddrs;
use std::sync::Arc;

use log::info;
use tokio::net::TcpListener;
use tokio::runtime::Builder;

use crate::error::FerrumError;
use crate::network::shutdown::Shutdown;
use crate::storage::engine::KvEngine;

/// Starts the dashboard HTTP server and blocks until `shutdown` triggers.
///
/// Runs on its own multi-threaded tokio runtime (separate from the RESP
/// server's runtime) so the two never contend for the same worker pool. The
/// caller typically spawns this on a dedicated OS thread next to
/// [`crate::network::server::run_listener`].
///
/// `token` is the shared secret required on `/api/*` requests. It is mandatory
/// for non-loopback binds; see [`check_bind_policy`].
pub fn serve(
    addr: &str,
    engine: KvEngine,
    shutdown: Shutdown,
    token: Option<Arc<str>>,
) -> Result<(), FerrumError> {
    check_bind_policy(addr, token.as_deref())?;

    let engine = Arc::new(engine);
    let runtime = Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("ferrum-dashboard")
        .enable_all()
        .build()
        .map_err(|e| FerrumError::Internal(format!("failed to build dashboard runtime: {e}")))?;

    runtime.block_on(async move {
        let listener = TcpListener::bind(addr).await?;
        let local = listener.local_addr()?;
        if token.is_some() {
            info!("FerrumKV dashboard available at http://{local} (token required)");
        } else {
            info!("FerrumKV dashboard available at http://{local}");
        }

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            let engine = Arc::clone(&engine);
                            let shutdown = shutdown.clone();
                            let token = token.clone();
                            tokio::spawn(async move {
                                if let Err(e) = http::handle_conn(stream, engine, shutdown, token).await {
                                    log::debug!("dashboard connection ended: {e}");
                                }
                            });
                        }
                        Err(e) => log::warn!("dashboard accept error: {e}"),
                    }
                }
            }
        }
        info!("dashboard shutting down");
        Ok::<(), FerrumError>(())
    })
}

/// Refuses to expose the dashboard on a non-loopback address without a token.
///
/// The dashboard is an unauthenticated admin surface by design (it exists to
/// make the engine inspectable out of the box). Allowing it onto `0.0.0.0`
/// without a secret would turn a debugging tool into a remote takeover vector,
/// so that combination is rejected at startup instead of warned about.
fn check_bind_policy(addr: &str, token: Option<&str>) -> Result<(), FerrumError> {
    if token.is_some() {
        return Ok(());
    }
    let addrs = addr
        .to_socket_addrs()
        .map_err(|e| FerrumError::Internal(format!("invalid dashboard address '{addr}': {e}")))?;
    for resolved in addrs {
        if !resolved.ip().is_loopback() {
            return Err(FerrumError::Internal(format!(
                "refusing to bind the dashboard to non-loopback address {resolved} \
                 without a token; pass --dashboard-token <secret> or use 127.0.0.1"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_without_token_is_allowed() {
        assert!(check_bind_policy("127.0.0.1:6381", None).is_ok());
        assert!(check_bind_policy("localhost:6381", None).is_ok());
        assert!(check_bind_policy("[::1]:6381", None).is_ok());
    }

    #[test]
    fn non_loopback_without_token_is_rejected() {
        assert!(check_bind_policy("0.0.0.0:6381", None).is_err());
        assert!(check_bind_policy("192.0.2.10:6381", None).is_err());
    }

    #[test]
    fn non_loopback_with_token_is_allowed() {
        assert!(check_bind_policy("0.0.0.0:6381", Some("s3cret")).is_ok());
    }
}
