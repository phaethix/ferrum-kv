//! Active expiration sweeper.
//!
//! Runs on its own thread, periodically sampling keys from the engine and
//! removing those whose TTL has elapsed. The sampler mirrors Redis' adaptive
//! strategy: sample 20 keys every 100ms; when more than 25% of the sample
//! was already expired, run another round immediately instead of sleeping.
//!
//! The thread is cooperatively shut down via [`Shutdown`], matching the rest
//! of the server.

use std::sync::Arc;
use std::sync::{Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use log::{debug, warn};

use crate::network::shutdown::Shutdown;
use crate::storage::engine::KvEngine;

/// Default sample size per sweep tick.
pub const DEFAULT_SAMPLE: usize = 20;

/// Default interval between sweep ticks.
pub const DEFAULT_INTERVAL: Duration = Duration::from_millis(100);

/// Spawns the background expiration sweeper.
///
/// Returns a [`ExpireSweeperHandle`] whose [`ExpireSweeperHandle::shutdown`]
/// waits for the worker thread to exit. The worker also self-terminates when
/// the shared [`Shutdown`] flag flips, so callers that forward the server's
/// shutdown signal do not need to call the handle explicitly — dropping it
/// will still join the thread.
pub fn spawn(engine: KvEngine, shutdown: Shutdown) -> ExpireSweeperHandle {
    spawn_with(engine, shutdown, DEFAULT_SAMPLE, DEFAULT_INTERVAL)
}

/// Testing-oriented variant that accepts custom sampling parameters.
pub fn spawn_with(
    engine: KvEngine,
    shutdown: Shutdown,
    sample: usize,
    interval: Duration,
) -> ExpireSweeperHandle {
    let wait = Arc::new(SleepWaker::default());
    let wait_clone = Arc::clone(&wait);
    let shutdown_clone = shutdown.clone();

    let handle = thread::Builder::new()
        .name("ferrum-expire".into())
        .spawn(move || run(engine, shutdown_clone, sample, interval, wait_clone))
        .expect("failed to spawn ferrum-expire thread");

    ExpireSweeperHandle {
        handle: Some(handle),
        wait,
    }
}

/// Joins the sweeper thread on drop, surfacing any thread panic as a warning.
pub struct ExpireSweeperHandle {
    handle: Option<JoinHandle<()>>,
    wait: Arc<SleepWaker>,
}

impl ExpireSweeperHandle {
    /// Wakes the sweeper and blocks until it exits.
    pub fn shutdown(mut self) {
        self.wait.wake();
        if let Some(handle) = self.handle.take()
            && let Err(e) = handle.join()
        {
            warn!("ferrum-expire thread panicked: {e:?}");
        }
    }
}

impl Drop for ExpireSweeperHandle {
    fn drop(&mut self) {
        self.wait.wake();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

/// Interruptible sleep primitive shared between the sweeper and its owner.
#[derive(Default)]
struct SleepWaker {
    lock: Mutex<bool>,
    cvar: Condvar,
}

impl SleepWaker {
    fn wake(&self) {
        if let Ok(mut guard) = self.lock.lock() {
            *guard = true;
            self.cvar.notify_all();
        }
    }

    fn sleep(&self, timeout: Duration) {
        let guard = match self.lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (mut guard, _) = match self.cvar.wait_timeout(guard, timeout) {
            Ok(pair) => pair,
            Err(poisoned) => {
                let pair = poisoned.into_inner();
                (pair.0, pair.1)
            }
        };
        // Reset so the next tick sleeps again unless explicitly woken.
        *guard = false;
    }
}

fn run(
    engine: KvEngine,
    shutdown: Shutdown,
    sample: usize,
    interval: Duration,
    wait: Arc<SleepWaker>,
) {
    debug!("ferrum-expire: started (sample={sample}, interval={interval:?})");
    while !shutdown.is_triggered() {
        // Iterate the adaptive loop a small bounded number of times so one
        // busy sweep cannot starve other readers holding the write lock.
        for _ in 0..16 {
            match engine.sweep_expired(sample) {
                Ok(stats) => {
                    if !stats.should_continue() {
                        break;
                    }
                }
                Err(e) => {
                    warn!("ferrum-expire: sweep failed: {e}");
                    break;
                }
            }
        }
        wait.sleep(interval);
    }
    debug!("ferrum-expire: stopped");
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn sweeper_removes_expired_keys_in_the_background() {
        let engine = KvEngine::new();
        engine.set(b"k".to_vec(), b"v".to_vec()).unwrap();
        // Install a 10 ms TTL.
        let now = crate::storage::engine::current_epoch_ms();
        engine.expire_at_ms(b"k", now + 10).unwrap();

        let shutdown = Shutdown::new();
        let handle = spawn_with(
            engine.clone(),
            shutdown.clone(),
            8,
            Duration::from_millis(5),
        );

        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            if !engine.exists(b"k").unwrap() {
                break;
            }
            thread::sleep(Duration::from_millis(5));
        }

        shutdown.trigger();
        handle.shutdown();
        assert!(
            !engine.exists(b"k").unwrap(),
            "sweeper failed to remove expired key"
        );
    }

    #[test]
    fn sweeper_honours_shutdown_flag() {
        let engine = KvEngine::new();
        let shutdown = Shutdown::new();
        let handle = spawn_with(engine, shutdown.clone(), 8, Duration::from_millis(50));
        shutdown.trigger();
        handle.shutdown();
        // Absence of a hang here is the assertion.
    }
}
