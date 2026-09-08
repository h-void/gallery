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

/// Read per call (no caching) so tests can redirect `DATA_DIR` freely.
fn log_dir() -> Option<PathBuf> {
    let data_dir = std::env::var_os("DATA_DIR")?;
    let dir = PathBuf::from(data_dir).join("logs");
    (!dir.as_os_str().is_empty()).then_some(dir)
}

fn append_line(log_dir: &Path, line: &str) -> std::io::Result<()> {
    let _guard = WRITE_LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let path = log_dir.join(LOG_FILE_NAME);
    if let Ok(metadata) = fs::metadata(&path) {
        if metadata.len() > MAX_LOG_BYTES {
            let rotated = log_dir.join(ROTATED_LOG_NAME);
            let _ = fs::remove_file(&rotated);
            let _ = fs::rename(&path, &rotated);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{EnvVar, ENV_LOCK};

    #[test]
    fn log_event_appends_timestamped_level_line() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = EnvVar::set("DATA_DIR", dir.path().join("data"));
        log_event("ERROR", "hash: resolve scan candidate 7 failed: boom");
        let content =
            fs::read_to_string(dir.path().join("data/logs/gallery.log")).unwrap();
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
        let _data_dir = EnvVar::set("DATA_DIR", dir.path().join("data"));
        log_event("INFO", "after rotation");
        let rotated = fs::read_to_string(logs.join(ROTATED_LOG_NAME)).unwrap();
        assert_eq!(rotated.len(), MAX_LOG_BYTES as usize + 1);
        let content = fs::read_to_string(&current).unwrap();
        assert!(
            content.ends_with("[INFO] after rotation\n"),
            "fresh file holds the new line: {content:?}"
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
}
