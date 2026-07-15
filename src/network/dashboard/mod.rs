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

pub mod http;

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
pub fn serve(addr: &str, engine: KvEngine, shutdown: Shutdown) -> Result<(), FerrumError> {
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
        info!("FerrumKV dashboard available at http://{local}");

        loop {
            tokio::select! {
                biased;
                _ = shutdown.notified() => break,
                accepted = listener.accept() => {
                    match accepted {
                        Ok((stream, _peer)) => {
                            let engine = Arc::clone(&engine);
                            let shutdown = shutdown.clone();
                            tokio::spawn(async move {
                                if let Err(e) = http::handle_conn(stream, engine, shutdown).await {
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
