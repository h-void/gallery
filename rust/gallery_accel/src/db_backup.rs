//! SQLite online database backup and backup retention pruning.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::json;

use crate::folder_archive::rename_dir_no_overwrite;
use crate::fs_util::safe_canonicalize;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub(crate) fn backup_retention() -> usize {
    std::env::var("DB_BACKUP_RETENTION")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(8)
        .max(1)
}

pub(crate) fn prune_backup_root(root: &Path, retention: usize) -> Result<usize> {
    std::fs::create_dir_all(root)?;
    let root = safe_canonicalize(root)?;
    let mut entries = std::fs::read_dir(&root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            let hidden = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with('.'));
            let real_directory = entry
                .file_type()
                .ok()
                .is_some_and(|file_type| file_type.is_dir() && !file_type.is_symlink());
            (!hidden && real_directory).then_some(path)
        })
        .filter_map(|path| safe_canonicalize(path).ok())
        .filter(|path| path.starts_with(&root))
        .collect::<Vec<_>>();
    entries.sort();
    let remove_count = entries.len().saturating_sub(retention.max(1));
    for path in entries.into_iter().take(remove_count) {
        if path.starts_with(&root) {
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(remove_count)
}

pub fn create_db_backup(conn: &Connection) -> Result<String> {
    create_db_backup_in(conn, Path::new(""))
}

/// Lifecycle housekeeping backups live in their own `housekeeping/` subtree so
/// routine missing-item cleanup can never prune the pre-archive backup an undo
/// depends on.
pub fn create_housekeeping_db_backup(conn: &Connection) -> Result<String> {
    create_db_backup_in(conn, Path::new("housekeeping"))
}

fn create_db_backup_in(conn: &Connection, subdir: &Path) -> Result<String> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let mut root = PathBuf::from(data_dir).join("db-backups");
    if !subdir.as_os_str().is_empty() {
        root.push(subdir);
    }
    std::fs::create_dir_all(&root)?;
    let label = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let mut index = 0usize;
    let (dir, temp_dir) = loop {
        let suffix = if index == 0 {
            String::new()
        } else {
            format!("-{index}")
        };
        let dir = root.join(format!("{label}{suffix}"));
        let temp_dir = root.join(format!(
            ".{label}{suffix}.tmp-{}-{}",
            std::process::id(),
            index
        ));
        if dir.exists() {
            index += 1;
            continue;
        }
        match std::fs::create_dir(&temp_dir) {
            Ok(()) => break (dir, temp_dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                index += 1;
            }
            Err(error) => return Err(error.into()),
        }
    };
    let result = (|| -> Result<PathBuf> {
        let dest = temp_dir.join("gallery.db");
        let mut dst = Connection::open(&dest)?;
        let backup = rusqlite::backup::Backup::new(conn, &mut dst)?;
        backup.run_to_completion(5, std::time::Duration::from_millis(0), None)?;
        drop(backup);
        dst.close().map_err(|(_, error)| error)?;
        std::fs::write(
            temp_dir.join("metadata.json"),
            json!({"created_at": now(), "label": label}).to_string(),
        )?;
        let mut published = dir;
        let mut publish_index = index;
        loop {
            match rename_dir_no_overwrite(&temp_dir, &published) {
                Ok(()) => break,
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    publish_index += 1;
                    published = root.join(format!("{label}-{publish_index}"));
                }
                Err(error) => return Err(error.into()),
            }
        }
        Ok(published.join("gallery.db"))
    })();
    let dest = match result {
        Ok(dest) => dest,
        Err(error) => {
            let _ = std::fs::remove_dir_all(&temp_dir);
            return Err(error);
        }
    };
    let _ = prune_backup_root(&root, backup_retention());
    Ok(dest.display().to_string())
}
