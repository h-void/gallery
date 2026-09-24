//! Persistent runtime logging.
//!
//! Diagnostic lines append to `DATA_DIR/logs/gallery.log` with the timestamp
//! shape the Python-era log used (`%Y-%m-%d %H:%M:%S,mmm [LEVEL] message`), so
//! `product_ui::log_line_timestamp_millis` parses Rust lines without a second
//! format and `/api/logs/tail?source=gallery` serves live runtime output.
//!
//! File writes are best-effort: when `DATA_DIR` is unset (unit tests, ad-hoc
//! runs) or the write fails, the line still goes to stderr, which
//! `fnpack/cmd/main` appends to `data/logs/startup.log`.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// Shared runtime-log name: the health error scan and the logs/tail
/// `gallery` source already point here, and the pre-Rust fossil content is
/// excluded from health by the `GALLERY_LOG_ERROR_WINDOW_HOURS` window.
const LOG_FILE_NAME: &str = "gallery.log";
const ROTATED_LOG_NAME: &str = "gallery.log.1";
const MAX_LOG_BYTES: u64 = 16 * 1024 * 1024;

static WRITE_LOCK: Mutex<()> = Mutex::new(());

#[macro_export]
macro_rules! log_info {
    ($($arg:tt)*) => {
        $crate::logging::log_event("INFO", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_warn {
    ($($arg:tt)*) => {
        $crate::logging::log_event("WARN", &format!($($arg)*))
    };
}

#[macro_export]
macro_rules! log_error {
    ($($arg:tt)*) => {
        $crate::logging::log_event("ERROR", &format!($($arg)*))
    };
}

/// Timestamp prefix shared by every persisted log line.
///
/// `product_ui::log_line_timestamp_millis` only parses 23-character stamps
/// carrying a `,mmm` / `.mmm` fraction. A writer that falls back to a bare
/// `%H:%M:%S` stamp produces lines the health error window cannot date, so
/// they silently inherit the previous line's timestamp and drop out of the
/// recent-errors panel.
pub fn log_timestamp() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S,%3f")
        .to_string()
}

pub fn log_event(level: &str, message: &str) {
    let line = format!("{} [{}] {}\n", log_timestamp(), level, message);
    let Some(dir) = log_dir() else {
        eprint!("{line}");
        return;
    };
    if append_line(&dir, &line).is_err() {
        eprint!("{line}");
    }
}

#[cfg(test)]
thread_local! {
    static TEST_LOG_DIR_OVERRIDE: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
pub struct TestLogDirGuard {
    prev: Option<PathBuf>,
}

#[cfg(test)]
impl TestLogDirGuard {
    pub fn set(path: impl Into<PathBuf>) -> Self {
        let p = path.into();
        let prev = TEST_LOG_DIR_OVERRIDE.with(|cell| cell.replace(Some(p)));
        Self { prev }
    }
}

#[cfg(test)]
impl Drop for TestLogDirGuard {
    fn drop(&mut self) {
        TEST_LOG_DIR_OVERRIDE.with(|cell| {
            cell.replace(self.prev.take());
        });
    }
}

/// Read per call (no caching) so tests can redirect `DATA_DIR` freely.
fn log_dir() -> Option<PathBuf> {
    #[cfg(test)]
    {
        if let Some(dir) = TEST_LOG_DIR_OVERRIDE.with(|cell| cell.borrow().clone()) {
            return (!dir.as_os_str().is_empty()).then_some(dir);
        }
    }
    let data_dir = std::env::var_os("DATA_DIR")?;
    let dir = PathBuf::from(data_dir).join("logs");
    (!dir.as_os_str().is_empty()).then_some(dir)
}

fn append_line(log_dir: &Path, line: &str) -> std::io::Result<()> {
    let _guard = WRITE_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = log_dir.join(LOG_FILE_NAME);
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() > MAX_LOG_BYTES {
            // Not `let _ =`: a rotation that cannot complete must reach the
            // caller, which falls back to stderr for this line.
            rotate(log_dir, &path)?;
        }
    }
    match OpenOptions::new().create(true).append(true).open(&path) {
        Ok(mut file) => file.write_all(line.as_bytes()),
        // create() makes the file, not its parent directory.
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            fs::create_dir_all(log_dir)?;
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)?
                .write_all(line.as_bytes())
        }
        Err(error) => Err(error),
    }
}

/// Move the oversized log aside, and if that cannot be done, make sure the
/// append cannot keep growing it.
///
/// The rotate used to be two `let _ =` calls. When `rename` fails — the file is
/// open in another process without share-delete, the directory is read-only or
/// full — the failure was invisible and every later append kept succeeding on
/// the now-unrotated file, so `MAX_LOG_BYTES` silently became "unlimited". That
/// matters beyond disk use: `/api/logs/tail` and the health error window read a
/// bounded tail of this file, and a log that never rotates makes the *stored*
/// window unbounded instead.
///
/// The order matters. A timestamped name is tried first so a locked
/// `gallery.log.1` cannot disable rotation entirely, and only if that also
/// fails is the file truncated in place, which always works on a file this
/// process can append to. Truncating loses the older half but keeps the cap.
fn rotate(log_dir: &Path, path: &Path) -> std::io::Result<()> {
    let rotated = log_dir.join(ROTATED_LOG_NAME);
    let _ = fs::remove_file(&rotated);
    match fs::rename(path, &rotated) {
        Ok(()) => Ok(()),
        Err(error) => {
            let stamped = log_dir.join(format!(
                "{ROTATED_LOG_NAME}.{}",
                chrono::Local::now().format("%Y%m%d%H%M%S")
            ));
            match fs::rename(path, &stamped) {
                Ok(()) => {
                    eprintln!(
                        "runtime log rotation: {ROTATED_LOG_NAME} is unavailable ({error}); \
                         kept the previous contents as {}",
                        stamped.display()
                    );
                    Ok(())
                }
                Err(stamped_error) => {
                    eprintln!(
                        "runtime log rotation failed ({error}; then {stamped_error}); \
                         truncating {path:?} in place to keep the size cap"
                    );
                    OpenOptions::new().write(true).truncate(true).open(path)?;
                    Ok(())
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::ENV_LOCK;

    #[test]
    fn log_event_appends_timestamped_level_line() {
        // The thread-local guard, not `DATA_DIR`: these tests assert on the
        // exact contents of one log file, and a global `DATA_DIR` is visible to
        // every concurrently running test. Any of them logging (an `ANALYZE`
        // line is enough) would then append into this fixture and break the
        // count and rotation assertions. The override is private to this
        // thread, so nothing else can resolve to this directory.
        let _env_lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("data/logs");
        let _log_dir = TestLogDirGuard::set(&logs);
        log_event("ERROR", "hash: resolve scan candidate 7 failed: boom");
        let content = fs::read_to_string(logs.join(LOG_FILE_NAME)).unwrap();
        assert_eq!(
            content.lines().count(),
            1,
            "one line per event: {content:?}"
        );
        assert!(content.ends_with("[ERROR] hash: resolve scan candidate 7 failed: boom\n"));
        let stamp = content
            .get(..23)
            .expect("line starts with a 23-char timestamp");
        assert!(
            chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S,%3f").is_ok(),
            "timestamp must match the shared log format: {stamp:?}"
        );
    }

    #[test]
    fn log_event_rotates_oversized_log() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("data/logs");
        fs::create_dir_all(&logs).unwrap();
        let current = logs.join(LOG_FILE_NAME);
        let oversized = "x".repeat(MAX_LOG_BYTES as usize + 1);
        fs::write(&current, oversized).unwrap();
        let _log_dir = TestLogDirGuard::set(&logs);
        log_event("INFO", "after rotation");
        let rotated = fs::read_to_string(logs.join(ROTATED_LOG_NAME)).unwrap();
        assert_eq!(rotated.len(), MAX_LOG_BYTES as usize + 1);
        let content = fs::read_to_string(&current).unwrap();
        assert!(
            content.contains("[INFO] after rotation\n"),
            "fresh file holds the new line: {content:?}"
        );
    }

    /// A locked rotation target must not disable rotation.
    ///
    /// With `gallery.log.1` unusable — here it is a directory, which is how a
    /// rename fails deterministically on every platform — the old code dropped
    /// both `let _ =` results and appended to the oversized file forever, so
    /// the 16 MiB cap became "unlimited". The fallback keeps the previous
    /// contents under a timestamped name instead, and the new line still lands
    /// in a fresh `gallery.log`.
    ///
    /// This pins the fallback rather than the discarded error: the `let _ =`
    /// that used to wrap this call only decided whether the append reported the
    /// failure upward, not whether the fallback ran.
    #[test]
    fn log_event_rotates_even_when_the_rotation_target_is_unusable() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let logs = dir.path().join("data/logs");
        fs::create_dir_all(&logs).unwrap();
        let current = logs.join(LOG_FILE_NAME);
        fs::write(&current, "x".repeat(MAX_LOG_BYTES as usize + 1)).unwrap();
        // A directory cannot be replaced by `rename`, and `remove_file` refuses
        // to delete it, so both steps of the preferred path fail.
        fs::create_dir(logs.join(ROTATED_LOG_NAME)).unwrap();
        let _log_dir = TestLogDirGuard::set(&logs);

        log_event("INFO", "after a refused rotation");

        let kept: Vec<PathBuf> = fs::read_dir(&logs)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with("gallery.log.1."))
            })
            .collect();
        assert_eq!(
            kept.len(),
            1,
            "the oversized log must be preserved under a fallback name: {kept:?}"
        );
        assert_eq!(
            fs::read_to_string(&kept[0]).unwrap().len(),
            MAX_LOG_BYTES as usize + 1,
            "the fallback keeps the previous contents whole"
        );
        let content = fs::read_to_string(&current).unwrap();
        assert!(
            content.ends_with("[INFO] after a refused rotation\n"),
            "a fresh file holds the new line: {content:?}"
        );
        assert!(
            content.len() < MAX_LOG_BYTES as usize,
            "the cap must hold even when the preferred target is unusable"
        );
    }

    #[test]
    fn log_event_without_data_dir_stays_stderr_only() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let previous = std::env::var_os("DATA_DIR");
        std::env::remove_var("DATA_DIR");
        let cwd = std::env::current_dir().unwrap();
        let stray = cwd.join("data/logs/gallery.log");
        let existed = stray.exists();
        let result = std::panic::catch_unwind(|| log_event("ERROR", "no data dir"));
        match previous {
            Some(value) => std::env::set_var("DATA_DIR", value),
            None => std::env::remove_var("DATA_DIR"),
        }
        result.expect("logging without DATA_DIR must not panic");
        assert!(
            existed || !stray.exists(),
            "stderr-only fallback must not create data/logs/gallery.log"
        );
    }

    #[test]
    fn test_log_dir_guard_isolates_concurrent_threads() {
        let dir1 = tempfile::tempdir().unwrap();
        let dir2 = tempfile::tempdir().unwrap();
        let path1 = dir1.path().join("logs");
        let path2 = dir2.path().join("logs");

        let p1 = path1.clone();
        let t1 = std::thread::spawn(move || {
            let _guard = TestLogDirGuard::set(&p1);
            for i in 0..10 {
                log_event("INFO", &format!("thread1 message {i}"));
                std::thread::yield_now();
            }
        });

        let p2 = path2.clone();
        let t2 = std::thread::spawn(move || {
            let _guard = TestLogDirGuard::set(&p2);
            for i in 0..10 {
                log_event("WARN", &format!("thread2 message {i}"));
                std::thread::yield_now();
            }
        });

        t1.join().unwrap();
        t2.join().unwrap();

        let log1 = fs::read_to_string(path1.join(LOG_FILE_NAME)).unwrap();
        let log2 = fs::read_to_string(path2.join(LOG_FILE_NAME)).unwrap();

        assert!(log1.contains("thread1 message 0"));
        assert!(log1.contains("thread1 message 9"));
        assert!(!log1.contains("thread2"));

        assert!(log2.contains("thread2 message 0"));
        assert!(log2.contains("thread2 message 9"));
        assert!(!log2.contains("thread1"));
    }
}
