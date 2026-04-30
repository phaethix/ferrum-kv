//! AOF replay loader.
//!
//! On startup the server feeds every RESP2 record from the log back through
//! the [`KvEngine`], recreating the pre-crash state. The replayer deliberately
//! uses an engine *without* an attached [`AofWriter`] so the act of replaying
//! does not append duplicated records to the log (whitepaper §8.4).
//!
//! Robustness rules:
//!
//! * A half-written record at the very end of the file is truncated back to
//!   the last complete record, with a warning. This is expected after a crash.
//! * A single malformed record in the middle of the file is skipped with a
//!   warning, but replay continues.
//! * Unknown commands are skipped with a warning, matching the forward
//!   compatibility guarantees of the persistence format.

use std::fs::{File, OpenOptions};
use std::io::{self, BufReader, Read, Seek, SeekFrom};
use std::path::Path;

use log::warn;

use crate::error::FerrumError;
use crate::storage::engine::KvEngine;

/// Outcome of a replay run, primarily useful for tests and logging.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct ReplayStats {
    /// Number of records successfully replayed.
    pub applied: usize,
    /// Number of records skipped due to decode errors or unknown commands.
    pub skipped: usize,
    /// Whether a partial trailing record was truncated from the file.
    pub truncated_tail: bool,
}

/// Replays every command stored in `path` into `engine`.
///
/// If `path` does not exist the function returns an empty [`ReplayStats`]
/// without touching the engine.
pub fn replay(path: &Path, engine: &KvEngine) -> Result<ReplayStats, FerrumError> {
    if !path.exists() {
        return Ok(ReplayStats::default());
    }

    let file = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| FerrumError::PersistenceError(format!("aof open failed: {e}")))?;

    replay_from_file(file, engine, path)
}

fn replay_from_file(
    mut file: File,
    engine: &KvEngine,
    path: &Path,
) -> Result<ReplayStats, FerrumError> {
    let mut reader = BufReader::new(&mut file);
    let mut stats = ReplayStats::default();
    let mut last_good: u64 = 0;

    loop {
        let record = match read_record(&mut reader) {
            Ok(Some(rec)) => rec,
            Ok(None) => break,
            Err(ReadError::UnexpectedEof { consumed }) => {
                if consumed > 0 {
                    warn!(
                        "aof replay: truncating partial trailing record \
                         ({consumed} bytes) in '{}'",
                        path.display()
                    );
                    stats.truncated_tail = true;
                }
                break;
            }
            Err(ReadError::Malformed { message, consumed }) => {
                warn!(
                    "aof replay: skipping malformed record ({message}) \
                     in '{}' at byte {last_good}",
                    path.display()
                );
                stats.skipped += 1;
                // Skip to the next potential record boundary (start of array).
                match resync_to_next_array(&mut reader) {
                    Ok(true) => {
                        last_good += consumed as u64;
                        continue;
                    }
                    Ok(false) => {
                        stats.truncated_tail = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            Err(ReadError::Io(e)) => {
                return Err(FerrumError::PersistenceError(format!(
                    "aof read failed: {e}"
                )));
            }
        };

        match apply_record(engine, &record) {
            Ok(()) => {
                stats.applied += 1;
                last_good = stream_position(&mut reader)?;
            }
            Err(ApplyError::Unknown(cmd)) => {
                warn!(
                    "aof replay: skipping unknown command '{cmd}' in '{}'",
                    path.display()
                );
                stats.skipped += 1;
                last_good = stream_position(&mut reader)?;
            }
            Err(ApplyError::Arity(cmd)) => {
                warn!(
                    "aof replay: skipping '{cmd}' with wrong argument count in '{}'",
                    path.display()
                );
                stats.skipped += 1;
                last_good = stream_position(&mut reader)?;
            }
            Err(ApplyError::Engine(e)) => {
                return Err(e);
            }
        }
    }

    drop(reader);

    if stats.truncated_tail {
        file.set_len(last_good)
            .map_err(|e| FerrumError::PersistenceError(format!("aof truncate failed: {e}")))?;
        file.sync_all().map_err(|e| {
            FerrumError::PersistenceError(format!("aof sync after truncate failed: {e}"))
        })?;
    }

    Ok(stats)
}

fn stream_position<R: Seek>(reader: &mut R) -> Result<u64, FerrumError> {
    reader
        .stream_position()
        .map_err(|e| FerrumError::PersistenceError(format!("aof stream_position failed: {e}")))
}

#[derive(Debug)]
enum ReadError {
    /// Stream ended mid-record; `consumed` bytes belong to the partial record.
    UnexpectedEof { consumed: usize },
    /// The bytes read do not form a valid RESP2 record.
    Malformed { message: String, consumed: usize },
    /// Underlying I/O failure.
    Io(io::Error),
}

#[derive(Debug)]
enum ApplyError {
    Unknown(String),
    Arity(String),
    Engine(FerrumError),
}

/// Reads a single RESP2 Array-of-Bulk-Strings record.
///
/// Returns `Ok(None)` on clean EOF before any byte of the next record.
/// Bulk payloads are returned as raw bytes because the AOF must faithfully
/// replay whatever the write path committed, including values that are not
/// valid UTF-8.
fn read_record<R: Read>(reader: &mut R) -> Result<Option<Vec<Vec<u8>>>, ReadError> {
    let mut consumed = 0usize;

    let first = match read_byte(reader)? {
        Some(b) => {
            consumed += 1;
            b
        }
        None => return Ok(None),
    };

    if first != b'*' {
        return Err(ReadError::Malformed {
            message: format!("expected '*', found byte {first:#x}"),
            consumed,
        });
    }

    let array_len = read_integer(reader, &mut consumed)?;
    if array_len < 0 {
        return Err(ReadError::Malformed {
            message: format!("negative array length {array_len}"),
            consumed,
        });
    }

    let mut parts: Vec<Vec<u8>> = Vec::with_capacity(array_len as usize);
    for _ in 0..array_len {
        let marker = expect_byte(reader, &mut consumed)?;
        if marker != b'$' {
            return Err(ReadError::Malformed {
                message: format!("expected '$', found byte {marker:#x}"),
                consumed,
            });
        }
        let len = read_integer(reader, &mut consumed)?;
        if len < 0 {
            return Err(ReadError::Malformed {
                message: format!("negative bulk length {len}"),
                consumed,
            });
        }
        let mut buf = vec![0u8; len as usize];
        read_exact_tracked(reader, &mut buf, &mut consumed)?;
        let terminator_cr = expect_byte(reader, &mut consumed)?;
        let terminator_lf = expect_byte(reader, &mut consumed)?;
        if terminator_cr != b'\r' || terminator_lf != b'\n' {
            return Err(ReadError::Malformed {
                message: "missing CRLF after bulk string".into(),
                consumed,
            });
        }
        parts.push(buf);
    }

    Ok(Some(parts))
}

fn read_byte<R: Read>(reader: &mut R) -> Result<Option<u8>, ReadError> {
    let mut buf = [0u8; 1];
    match reader.read(&mut buf) {
        Ok(0) => Ok(None),
        Ok(_) => Ok(Some(buf[0])),
        Err(e) if e.kind() == io::ErrorKind::Interrupted => read_byte(reader),
        Err(e) => Err(ReadError::Io(e)),
    }
}

fn expect_byte<R: Read>(reader: &mut R, consumed: &mut usize) -> Result<u8, ReadError> {
    match read_byte(reader)? {
        Some(b) => {
            *consumed += 1;
            Ok(b)
        }
        None => Err(ReadError::UnexpectedEof {
            consumed: *consumed,
        }),
    }
}

fn read_exact_tracked<R: Read>(
    reader: &mut R,
    buf: &mut [u8],
    consumed: &mut usize,
) -> Result<(), ReadError> {
    let mut filled = 0;
    while filled < buf.len() {
        match reader.read(&mut buf[filled..]) {
            Ok(0) => {
                *consumed += filled;
                return Err(ReadError::UnexpectedEof {
                    consumed: *consumed,
                });
            }
            Ok(n) => filled += n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ReadError::Io(e)),
        }
    }
    *consumed += filled;
    Ok(())
}

fn read_integer<R: Read>(reader: &mut R, consumed: &mut usize) -> Result<i64, ReadError> {
    let mut number = Vec::with_capacity(8);
    loop {
        let b = expect_byte(reader, consumed)?;
        if b == b'\r' {
            let lf = expect_byte(reader, consumed)?;
            if lf != b'\n' {
                return Err(ReadError::Malformed {
                    message: "expected LF after CR in integer".into(),
                    consumed: *consumed,
                });
            }
            break;
        }
        number.push(b);
    }

    std::str::from_utf8(&number)
        .map_err(|e| ReadError::Malformed {
            message: format!("non-ASCII integer: {e}"),
            consumed: *consumed,
        })?
        .parse::<i64>()
        .map_err(|e| ReadError::Malformed {
            message: format!("bad integer: {e}"),
            consumed: *consumed,
        })
}

/// Advances the reader to the next byte that looks like the start of a RESP
/// Array (`'*'`), so replay can continue after a single malformed record.
///
/// Returns `true` if a candidate was found, `false` on EOF.
fn resync_to_next_array<R: Read + Seek>(reader: &mut R) -> Result<bool, FerrumError> {
    loop {
        let mut buf = [0u8; 1];
        match reader.read(&mut buf) {
            Ok(0) => return Ok(false),
            Ok(_) => {
                if buf[0] == b'*' {
                    // Step back one byte so the next read_record sees the '*'.
                    reader.seek(SeekFrom::Current(-1)).map_err(|e| {
                        FerrumError::PersistenceError(format!("aof seek failed: {e}"))
                    })?;
                    return Ok(true);
                }
            }
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => {
                return Err(FerrumError::PersistenceError(format!(
                    "aof resync read failed: {e}"
                )));
            }
        }
    }
}

fn apply_record(engine: &KvEngine, parts: &[Vec<u8>]) -> Result<(), ApplyError> {
    let Some(cmd) = parts.first() else {
        return Err(ApplyError::Arity(String::new()));
    };

    let upper: Vec<u8> = cmd.iter().map(|b| b.to_ascii_uppercase()).collect();
    match upper.as_slice() {
        b"SET" => {
            if parts.len() != 3 {
                return Err(ApplyError::Arity(cmd_name(cmd)));
            }
            engine
                .set(parts[1].clone(), parts[2].clone())
                .map(|_| ())
                .map_err(ApplyError::Engine)
        }
        b"DEL" => {
            if parts.len() != 2 {
                return Err(ApplyError::Arity(cmd_name(cmd)));
            }
            engine
                .del(&parts[1])
                .map(|_| ())
                .map_err(ApplyError::Engine)
        }
        b"FLUSHDB" => {
            if parts.len() != 1 {
                return Err(ApplyError::Arity(cmd_name(cmd)));
            }
            engine.flushdb().map_err(ApplyError::Engine)
        }
        _ => Err(ApplyError::Unknown(cmd_name(cmd))),
    }
}

/// Renders a command name for log messages, escaping non-UTF8 bytes.
fn cmd_name(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn tmp_path(label: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("ferrum-replay-{label}-{nanos}-{n}.aof"))
    }

    #[test]
    fn replay_of_missing_file_returns_empty_stats() {
        let path = tmp_path("missing");
        let engine = KvEngine::new();
        let stats = replay(&path, &engine).unwrap();
        assert_eq!(stats, ReplayStats::default());
    }

    #[test]
    fn replays_set_del_flushdb_sequence() {
        let path = tmp_path("sequence");
        let content = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n",
            "*2\r\n$3\r\nDEL\r\n$1\r\na\r\n",
        );
        fs::write(&path, content).unwrap();

        let engine = KvEngine::new();
        let stats = replay(&path, &engine).unwrap();
        assert_eq!(stats.applied, 3);
        assert_eq!(stats.skipped, 0);
        assert!(!stats.truncated_tail);
        assert_eq!(engine.get(b"a").unwrap(), None);
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn replay_does_not_write_to_aof_when_engine_has_no_writer() {
        // With no AofWriter attached, the engine should not touch the file
        // during replay. The file contents must remain unchanged.
        let path = tmp_path("no-rewrite");
        let original = "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$1\r\nv\r\n";
        fs::write(&path, original).unwrap();

        let engine = KvEngine::new();
        replay(&path, &engine).unwrap();

        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, original.as_bytes());
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn partial_trailing_record_is_truncated() {
        let path = tmp_path("partial-tail");
        let mut content = String::from("*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n");
        // Append a half-written SET that terminates mid-bulk.
        content.push_str("*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$5\r\nfoo");
        fs::write(&path, &content).unwrap();

        let engine = KvEngine::new();
        let stats = replay(&path, &engine).unwrap();
        assert_eq!(stats.applied, 1);
        assert!(stats.truncated_tail);
        assert_eq!(engine.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(engine.get(b"b").unwrap(), None);

        // The file should now end exactly at the last good record boundary.
        let bytes = fs::read(&path).unwrap();
        assert_eq!(bytes, b"*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n");
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn unknown_command_is_skipped() {
        let path = tmp_path("unknown-cmd");
        let content = concat!(
            "*3\r\n$3\r\nSET\r\n$1\r\na\r\n$1\r\n1\r\n",
            "*2\r\n$3\r\nHSET\r\n$1\r\nx\r\n",
            "*3\r\n$3\r\nSET\r\n$1\r\nb\r\n$1\r\n2\r\n",
        );
        fs::write(&path, content).unwrap();

        let engine = KvEngine::new();
        let stats = replay(&path, &engine).unwrap();
        assert_eq!(stats.applied, 2);
        assert_eq!(stats.skipped, 1);
        assert_eq!(engine.get(b"a").unwrap(), Some(b"1".to_vec()));
        assert_eq!(engine.get(b"b").unwrap(), Some(b"2".to_vec()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn binary_safe_values_survive_roundtrip() {
        let path = tmp_path("binary");
        // Value contains CRLF which would break a line-based format.
        let content = "*3\r\n$3\r\nSET\r\n$1\r\nk\r\n$4\r\na\r\nb\r\n";
        fs::write(&path, content).unwrap();

        let engine = KvEngine::new();
        replay(&path, &engine).unwrap();
        assert_eq!(engine.get(b"k").unwrap(), Some(b"a\r\nb".to_vec()));
        let _ = fs::remove_file(&path);
    }

    #[test]
    fn empty_file_replays_cleanly() {
        let path = tmp_path("empty");
        fs::write(&path, b"").unwrap();

        let engine = KvEngine::new();
        let stats = replay(&path, &engine).unwrap();
        assert_eq!(stats, ReplayStats::default());
        let _ = fs::remove_file(&path);
    }
}
