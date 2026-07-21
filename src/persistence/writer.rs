//! Append-only file writer.
//!
//! The writer serialises mutating commands using the RESP2 encoder from
//! [`super::resp`] and appends them to the configured log file. Three fsync
//! strategies are supported, mirroring Redis' `appendfsync` semantics.
//!
//! The writer is designed to be shared between threads via [`Arc`]. Callers
//! hold an [`Arc<AofWriter>`] and invoke the `append_*` methods concurrently;
//! internal synchronisation serialises access to the underlying file.

use std::fs::{File, OpenOptions};
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use log::warn;

use crate::error::FerrumError;

use super::config::{AofConfig, FsyncPolicy};
use super::resp::encode_command;

/// Maximum size of the in-memory delta buffer used during an AOF rewrite.
///
/// Writes that occur while a rewrite is in flight are copied here so they can
/// be replayed onto the compact snapshot after the atomic swap. If the buffer
/// would exceed this cap the rewrite is aborted and the original AOF is kept.
const DEFAULT_DELTA_CAP: usize = 64 * 1024 * 1024;

/// Writes mutating commands to the AOF log with a configurable fsync policy.
pub struct AofWriter {
    inner: Arc<Mutex<BufWriter<File>>>,
    policy: FsyncPolicy,
    flusher: Option<BackgroundFlusher>,
    /// On-disk path of the AOF, retained so a rewrite can reopen the handle
    /// after the atomic rename+swap.
    path: PathBuf,
    /// Set while a rewrite is capturing the keyspace. Gates delta buffering.
    rewriting: AtomicBool,
    /// Writes that land during a rewrite, replayed onto the compact snapshot.
    delta: Mutex<Vec<u8>>,
    /// Upper bound on `delta`'s byte length (see [`DEFAULT_DELTA_CAP`]).
    delta_cap: usize,
    /// Serialises `append_*` against the rename+swap so no command is in
    /// flight across the moment the file handle is replaced.
    rewrite_lock: Arc<Mutex<()>>,
}

impl AofWriter {
    /// Opens (or creates) the AOF file described by `config` and returns a
    /// ready-to-use writer.
    ///
    /// Existing log contents are preserved; new commands are appended to the
    /// end of the file.
    pub fn open(config: &AofConfig) -> Result<Self, FerrumError> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(config.path())
            .map_err(|e| persistence_error(config.path(), "open", &e))?;

        let inner = Arc::new(Mutex::new(BufWriter::new(file)));
        let flusher = match config.fsync {
            FsyncPolicy::EverySec => Some(BackgroundFlusher::spawn(
                Arc::clone(&inner),
                Duration::from_secs(1),
            )),
            FsyncPolicy::Always | FsyncPolicy::No => None,
        };

        Ok(Self {
            inner,
            policy: config.fsync,
            flusher,
            path: config.path().to_path_buf(),
            rewriting: AtomicBool::new(false),
            delta: Mutex::new(Vec::new()),
            delta_cap: DEFAULT_DELTA_CAP,
            rewrite_lock: Arc::new(Mutex::new(())),
        })
    }

    /// Opens a writer with a custom delta-buffer cap.
    ///
    /// Exposed for tests so the overflow-abort path can be exercised without
    /// materialising 64 MiB of in-flight writes. Production code always uses
    /// [`AofWriter::open`] (the default 64 MiB cap).
    #[cfg(test)]
    pub(crate) fn open_with_delta_cap(config: &AofConfig, cap: usize) -> Result<Self, FerrumError> {
        let mut w = Self::open(config)?;
        w.delta_cap = cap;
        Ok(w)
    }

    /// Appends a `SET key value` entry to the log.
    pub fn append_set(&self, key: &[u8], value: &[u8]) -> Result<(), FerrumError> {
        self.append(&[b"SET", key, value])
    }

    /// Appends a batch of `SET key value` entries as a single write.
    ///
    /// All records are serialised first and then committed to the file in
    /// one `write_all`, which keeps the batch atomic with respect to other
    /// writers: concurrent appenders never observe a partially written
    /// batch. This backs `MSET`, whose multi-key mutation must either
    /// appear entirely in the log or not at all.
    pub fn append_set_many(&self, pairs: &[(Vec<u8>, Vec<u8>)]) -> Result<(), FerrumError> {
        if pairs.is_empty() {
            return Ok(());
        }
        let mut bytes = Vec::new();
        for (k, v) in pairs {
            bytes.extend_from_slice(&encode_command(&[b"SET", k.as_slice(), v.as_slice()]));
        }
        self.write_bytes(&bytes)
    }

    /// Appends a `DEL key` entry to the log.
    pub fn append_del(&self, key: &[u8]) -> Result<(), FerrumError> {
        self.append(&[b"DEL", key])
    }

    /// Appends a `FLUSHDB` entry to the log.
    pub fn append_flushdb(&self) -> Result<(), FerrumError> {
        self.append(&[b"FLUSHDB"])
    }

    /// Appends a `PEXPIREAT key abs_epoch_ms` entry to the log.
    ///
    /// The absolute millisecond timestamp is recorded rather than a relative
    /// offset so replay stays correct regardless of how long the log has been
    /// sitting on disk. Any already-past timestamp encountered during replay
    /// makes the key be dropped immediately.
    pub fn append_pexpireat(&self, key: &[u8], abs_epoch_ms: i64) -> Result<(), FerrumError> {
        let ts = abs_epoch_ms.to_string();
        self.append(&[b"PEXPIREAT", key, ts.as_bytes()])
    }

    /// Appends a `PERSIST key` entry to the log.
    pub fn append_persist(&self, key: &[u8]) -> Result<(), FerrumError> {
        self.append(&[b"PERSIST", key])
    }

    fn append(&self, parts: &[&[u8]]) -> Result<(), FerrumError> {
        let bytes = encode_command(parts);
        self.write_bytes(&bytes)
    }

    fn write_bytes(&self, bytes: &[u8]) -> Result<(), FerrumError> {
        // Hold `rewrite_lock` across the write so the rename+swap in
        // `finish_rewrite` can never observe a command split between the old
        // file and the delta buffer.
        let _rk = self.rewrite_lock.lock()?;
        let mut guard = self.inner.lock()?;
        guard
            .write_all(bytes)
            .map_err(|e| FerrumError::PersistenceError(format!("aof write failed: {e}")))?;

        match self.policy {
            FsyncPolicy::Always => {
                guard
                    .flush()
                    .map_err(|e| FerrumError::PersistenceError(format!("aof flush failed: {e}")))?;
                guard
                    .get_ref()
                    .sync_data()
                    .map_err(|e| FerrumError::PersistenceError(format!("aof fsync failed: {e}")))?;
            }
            FsyncPolicy::EverySec | FsyncPolicy::No => {
                // EverySec delegates durability to the background flusher;
                // No relies on the operating system to schedule flushes.
            }
        }

        // While a rewrite is in flight, mirror these bytes into the delta so
        // they can be replayed onto the compact snapshot. If the buffer would
        // overflow its cap, abort the rewrite (the old AOF is kept intact).
        if self.rewriting.load(Ordering::SeqCst) {
            let mut d = self.delta.lock()?;
            if d.len() + bytes.len() > self.delta_cap {
                warn!(
                    "aof rewrite delta exceeded {} bytes; aborting rewrite (old AOF kept)",
                    self.delta_cap
                );
                self.rewriting.store(false, Ordering::SeqCst);
                d.clear();
            } else {
                d.extend_from_slice(bytes);
            }
        }

        Ok(())
    }

    /// Marks a rewrite as started: clears the delta buffer and flips the
    /// `rewriting` flag so subsequent `append_*` calls buffer into `delta`.
    pub fn begin_rewrite(&self) {
        if let Ok(mut d) = self.delta.lock() {
            d.clear();
        }
        self.rewriting.store(true, Ordering::SeqCst);
    }

    /// Aborts an in-flight rewrite: drops the buffered delta and clears the
    /// `rewriting` flag so the old AOF keeps receiving live appends.
    pub fn abort_rewrite(&self) {
        if let Ok(mut d) = self.delta.lock() {
            d.clear();
        }
        self.rewriting.store(false, Ordering::SeqCst);
    }

    /// True when a rewrite is currently capturing the keyspace.
    pub fn is_rewriting(&self) -> bool {
        self.rewriting.load(Ordering::SeqCst)
    }

    /// Path of the temp file a rewrite should serialise into. Sits alongside
    /// the live AOF (`.aof` → `.rewrite.tmp`) so it inherits the same
    /// directory and permissions.
    pub(crate) fn rewrite_temp_path(&self) -> PathBuf {
        self.path.with_extension("rewrite.tmp")
    }

    /// Completes a rewrite by atomically swapping the compact snapshot in and
    /// replaying any buffered delta onto it.
    ///
    /// Must be called after the temp file has been fully written and fsync'd.
    /// If the rewrite was aborted (delta overflow) before this point, the temp
    /// file is discarded and the original AOF is left untouched.
    pub fn finish_rewrite(&self, temp: &Path) -> Result<(), FerrumError> {
        if !self.rewriting.load(Ordering::SeqCst) {
            // Aborted mid-flight: keep the old AOF, drop the temp file.
            let _ = std::fs::remove_file(temp);
            return Ok(());
        }

        // Atomically replace the live AOF with the compact snapshot.
        std::fs::rename(temp, &self.path).map_err(|e| {
            FerrumError::PersistenceError(format!(
                "aof rename '{}' -> '{}' failed: {e}",
                temp.display(),
                self.path.display()
            ))
        })?;

        // Serialise against appenders: no command may be in flight across the
        // swap of the file handle.
        let _rk = self.rewrite_lock.lock()?;
        let mut inner = self.inner.lock()?;
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| FerrumError::PersistenceError(format!("aof reopen failed: {e}")))?;
        // The background flusher keeps working because it shares this same
        // `Arc<Mutex<BufWriter<File>>>`.
        let _old = std::mem::replace(&mut *inner, BufWriter::new(file));

        // Replay buffered in-flight writes onto the compact snapshot.
        let mut delta_guard = self.delta.lock()?;
        if !delta_guard.is_empty() {
            inner.write_all(&delta_guard).map_err(|e| {
                FerrumError::PersistenceError(format!("aof rewrite delta replay failed: {e}"))
            })?;
            inner.flush().map_err(|e| {
                FerrumError::PersistenceError(format!("aof rewrite delta flush failed: {e}"))
            })?;
            inner.get_ref().sync_data().map_err(|e| {
                FerrumError::PersistenceError(format!("aof rewrite delta fsync failed: {e}"))
            })?;
        }
        delta_guard.clear();
        drop(delta_guard);

        self.rewriting.store(false, Ordering::SeqCst);
        Ok(())
    }
}

impl Drop for AofWriter {
    fn drop(&mut self) {
        if let Some(flusher) = self.flusher.take() {
            flusher.shutdown();
        }

        if let Ok(mut guard) = self.inner.lock() {
            let _ = guard.flush();
            let _ = guard.get_ref().sync_data();
        }
    }
}

/// Builds a persistence error that carries the file path and failing op name.
fn persistence_error(path: &Path, op: &str, err: &std::io::Error) -> FerrumError {
    FerrumError::PersistenceError(format!("aof {op} '{}' failed: {err}", path.display()))
}

/// Background thread that periodically flushes and fsyncs the AOF file.
struct BackgroundFlusher {
    shared: Arc<FlusherShared>,
    handle: Option<JoinHandle<()>>,
}

struct FlusherShared {
    stop: AtomicBool,
    cvar: Condvar,
    lock: Mutex<()>,
}

impl BackgroundFlusher {
    fn spawn(inner: Arc<Mutex<BufWriter<File>>>, interval: Duration) -> Self {
        let shared = Arc::new(FlusherShared {
            stop: AtomicBool::new(false),
            cvar: Condvar::new(),
            lock: Mutex::new(()),
        });

        let thread_shared = Arc::clone(&shared);
        let handle = thread::Builder::new()
            .name("ferrum-aof-fsync".into())
            .spawn(move || run(inner, thread_shared, interval))
            .expect("failed to spawn aof fsync thread");

        Self {
            shared,
            handle: Some(handle),
        }
    }

    fn shutdown(mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        self.shared.cvar.notify_all();
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn run(inner: Arc<Mutex<BufWriter<File>>>, shared: Arc<FlusherShared>, interval: Duration) {
    loop {
        // Wait for `interval` or until shutdown is signalled.
        let guard = match shared.lock.lock() {
            Ok(g) => g,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (_guard, _timeout) = match shared.cvar.wait_timeout(guard, interval) {
            Ok(pair) => pair,
            Err(poisoned) => {
                let pair = poisoned.into_inner();
                (pair.0, pair.1)
            }
        };

        if shared.stop.load(Ordering::SeqCst) {
            break;
        }

        if let Ok(mut guard) = inner.lock() {
            if let Err(e) = guard.flush() {
                warn!("aof flush failed: {e}");
                continue;
            }
            if let Err(e) = guard.get_ref().sync_data() {
                warn!("aof fsync failed: {e}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::thread;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ferrum-aof-{label}-{nanos}-{n}.aof"))
    }

    #[test]
    fn writes_set_in_resp_format_with_always_policy() {
        let path = tmp_path("always-set");
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        {
            let writer = AofWriter::open(&cfg).unwrap();
            writer.append_set(b"name", b"ferrum").unwrap();
        }

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$4\r\nname\r\n$6\r\nferrum\r\n");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn writes_multiple_commands_in_order() {
        let path = tmp_path("always-multi");
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        {
            let writer = AofWriter::open(&cfg).unwrap();
            writer.append_set(b"a", b"1").unwrap();
            writer.append_del(b"a").unwrap();
            writer.append_flushdb().unwrap();
        }

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
            "*1\r\n$7\r\nFLUSHDB\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn reopening_appends_without_truncating() {
        let path = tmp_path("append-reopen");
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        {
            let writer = AofWriter::open(&cfg).unwrap();
            writer.append_set(b"k", b"v1").unwrap();
        }
        {
            let writer = AofWriter::open(&cfg).unwrap();
            writer.append_set(b"k", b"v2").unwrap();
        }

        let bytes = fs::read(&path).unwrap();
        let expected = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv1\r\n",
            "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$2\r\nv2\r\n",
        );
        assert_eq!(bytes, expected.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn no_policy_still_flushes_on_drop() {
        let path = tmp_path("no-drop");
        let cfg = AofConfig::new(&path, FsyncPolicy::No);
        {
            let writer = AofWriter::open(&cfg).unwrap();
            writer.append_set(b"k", b"v").unwrap();
            // `No` may keep data buffered; Drop must flush it out.
        }

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn everysec_background_flusher_persists_writes() {
        let path = tmp_path("everysec");
        let cfg = AofConfig::new(&path, FsyncPolicy::EverySec);
        let writer = AofWriter::open(&cfg).unwrap();
        writer.append_set(b"k", b"v").unwrap();
        // Give the background flusher time to sync at least once.
        thread::sleep(Duration::from_millis(1500));
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n");
        drop(writer);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn concurrent_writers_do_not_interleave_records() {
        let path = tmp_path("concurrent");
        let cfg = AofConfig::new(&path, FsyncPolicy::Always);
        let writer = Arc::new(AofWriter::open(&cfg).unwrap());

        let mut handles = Vec::new();
        for i in 0..16 {
            let w = Arc::clone(&writer);
            handles.push(thread::spawn(move || {
                w.append_set(format!("k{i}").as_bytes(), format!("v{i}").as_bytes())
                    .unwrap();
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
        drop(writer);

        // The file should contain exactly 16 well-formed records, one per key.
        let bytes = fs::read(&path).unwrap();
        let text = String::from_utf8(bytes).unwrap();
        assert_eq!(text.matches("*3\r\n$3\r\nSET\r\n").count(), 16);
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn delta_captures_appends_issued_during_rewrite() {
        let path = tmp_path("delta-capture");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        let writer = AofWriter::open(&cfg).unwrap();

        writer.begin_rewrite();
        assert!(writer.is_rewriting());
        writer.append_set(b"k", b"v").unwrap();

        let d = writer.delta.lock().unwrap();
        assert_eq!(
            &d[..],
            b"*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n",
            "append during rewrite must be mirrored into the delta buffer"
        );
        drop(d);
        writer.abort_rewrite();
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn bounded_delta_overflow_aborts_rewrite_and_keeps_old_aof() {
        let path = tmp_path("delta-overflow");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        // Tiny cap so a single large SET overflows it.
        let writer = AofWriter::open_with_delta_cap(&cfg, 50).unwrap();

        // Seed the old AOF with a record that must survive the aborted rewrite.
        writer.append_set(b"keep", b"me").unwrap();

        writer.begin_rewrite();
        // This SET is larger than the 50-byte cap, so the rewrite aborts and
        // the old AOF keeps receiving live appends.
        writer.append_set(b"bigkey", &[b'x'; 200]).unwrap();

        assert!(
            !writer.is_rewriting(),
            "overflow must abort the rewrite in flight"
        );
        let d = writer.delta.lock().unwrap();
        assert!(d.is_empty(), "delta must be cleared on abort");
        drop(d);

        // The old AOF is intact (the pre-rewrite "keep" record is still there).
        let bytes = fs::read(&path).unwrap();
        assert!(
            bytes.windows(b"$4\r\nkeep\r\n".len()).any(|w| w == b"$4\r\nkeep\r\n"),
            "aborted rewrite must leave the old AOF untouched"
        );
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn finish_rewrite_swaps_handle_and_replays_delta() {
        let path = tmp_path("finish-swap");
        let cfg = AofConfig::new(path.clone(), FsyncPolicy::Always);
        let writer = AofWriter::open(&cfg).unwrap();

        // Pre-rewrite record lands in the old file (not in the delta).
        writer.append_set(b"orig", b"1").unwrap();

        // Begin the rewrite and issue an in-flight write that must be preserved.
        writer.begin_rewrite();
        writer.append_set(b"delta", b"2").unwrap();

        // Simulate the compact snapshot: a fresh file containing only "orig".
        let temp = writer.rewrite_temp_path();
        fs::write(&temp, b"*3\r\n$3\r\nSET\r\n$4\r\norig\r\n$1\r\n1\r\n").unwrap();

        writer.finish_rewrite(&temp).unwrap();
        assert!(!writer.is_rewriting());
        assert!(!temp.exists(), "temp file must be renamed away");

        // The live AOF must now hold BOTH the compact "orig" and the replayed
        // "delta" write — nothing lost across the swap.
        let bytes = fs::read(&path).unwrap();
        assert!(
            bytes.windows(b"$4\r\norig\r\n".len()).any(|w| w == b"$4\r\norig\r\n"),
            "compact snapshot must survive the swap"
        );
        assert!(
            bytes.windows(b"$5\r\ndelta\r\n".len()).any(|w| w == b"$5\r\ndelta\r\n"),
            "in-flight delta write must be replayed onto the new file"
        );
        let _ = fs::remove_file(&path);
    }
}
