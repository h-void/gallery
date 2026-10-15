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
