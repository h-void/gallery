use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

#[cfg(target_os = "linux")]
use crate::folder_archive::{
    create_authorized_directory_no_follow, open_authorized_directory_no_follow, open_dir_at,
};
use crate::folder_archive::{
    create_db_backup, rename_directory_under_authorized_roots_no_overwrite_expected,
};
use crate::media_roots::{normalize_slashes, path_under_authorized_roots, MediaRoots};

fn validate_relative_destination(raw: &str) -> Result<String> {
    let value = raw.replace('\\', "/").trim().to_string();
    if value.is_empty() || value.starts_with('/') || value.contains(':') {
        bail!("destination folder must be relative");
    }
    let value = value.trim_matches('/').to_string();
    let mut parts = Vec::new();
    for part in value.split('/') {
        if part.is_empty() || part == "." || part == ".." {
            bail!("destination folder contains an invalid segment");
        }
        parts.push(part);
    }
    Ok(parts.join("/"))
}

fn validate_relative_directory(raw: &str) -> Result<String> {
    let value = raw.replace('\\', "/").trim().to_string();
    if value.starts_with('/') || value.contains(':') {
        bail!("directory path must be relative");
    }
    let value = value.trim_matches('/');
    if value.is_empty() {
        return Ok(String::new());
    }
    if value
        .split('/')
        .any(|part| part.is_empty() || matches!(part, "." | ".."))
    {
        bail!("directory path contains an invalid segment");
    }
    Ok(value.to_string())
}

pub fn list_media_root_directories(
    roots: &MediaRoots,
    root_index: usize,
    relative_path: &str,
) -> Result<Value> {
    let root = roots
        .real_root_at(root_index)
        .ok_or_else(|| anyhow!("media root not found"))?;
    let root = PathBuf::from(root).canonicalize()?;
    if !root.is_dir() {
        bail!("media root directory is missing");
    }
    let relative_path = validate_relative_directory(relative_path)?;
    let current = root.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let metadata = fs::symlink_metadata(&current)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("directory not found");
    }
    if !current.canonicalize()?.starts_with(&root) {
        bail!("directory escapes the configured media root");
    }
    let mut directories = fs::read_dir(&current)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            (metadata.is_dir() && !metadata.file_type().is_symlink())
                .then(|| entry.file_name().into_string().ok())
                .flatten()
        })
        .collect::<Vec<_>>();
    directories.sort_by_key(|name| name.to_lowercase());
    Ok(json!({
        "root_index": root_index,
        "path": relative_path,
        "directories": directories,
    }))
}

fn path_text(path: &Path) -> String {
    normalize_slashes(&path.to_string_lossy())
}

fn source_prefix(path: &str) -> String {
    path.trim_end_matches(['/', '\\']).to_string()
}

fn remap_path(path: &str, source: &str, target: &str) -> Option<String> {
    let path = normalize_slashes(path);
    let source = source_prefix(&normalize_slashes(source));
    if path == source {
        return Some(target.to_string());
    }
    path.strip_prefix(&(source.clone() + "/"))
        .map(|suffix| format!("{target}/{suffix}"))
}

fn artist_row(conn: &Connection, artist_id: i64) -> Result<(String, String)> {
    conn.query_row(
        "SELECT name, path FROM artists WHERE id=?",
        params![artist_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
    .optional()?
    .ok_or_else(|| anyhow!("artist not found"))
}

fn root_for_source(source: &Path, roots: &MediaRoots) -> Option<usize> {
    roots
        .real_paths
        .iter()
        .enumerate()
        .filter_map(|(index, root)| {
            let root_path = PathBuf::from(root);
            let canonical = root_path.canonicalize().ok()?;
            let source_canonical = source.canonicalize().ok()?;
            source_canonical
                .starts_with(&canonical)
                .then_some((index, canonical))
        })
        .max_by_key(|(_, root)| root.components().count())
        .map(|(index, _)| index)
}

fn target_parent_stays_under_root(target: &Path, root: &Path) -> bool {
    let Ok(root) = root.canonicalize() else {
        return false;
    };
    let mut current = Some(target);
    while let Some(path) = current {
        if path.exists() {
            return path
                .canonicalize()
                .map(|canonical| canonical.starts_with(&root))
                .unwrap_or(false);
        }
        current = path.parent();
    }
    false
}

fn indexed_conflicts(conn: &Connection, target: &str, artist_id: i64) -> Result<Vec<Value>> {
    let mut conflicts = Vec::new();
    let artist_conflict = conn
        .query_row(
            "SELECT id, path FROM artists WHERE id != ? AND path = ? COLLATE NOCASE LIMIT 1",
            params![artist_id, target],
            |row| Ok(json!({"artist_id": row.get::<_, i64>(0)?, "path": row.get::<_, String>(1)?})),
        )
        .optional()?;
    if let Some(conflict) = artist_conflict {
        conflicts.push(conflict);
    }
    let mut stmt = conn.prepare(
        "SELECT id, artist_id, file_path FROM items WHERE artist_id != ? AND (file_path = ? COLLATE NOCASE OR file_path LIKE ? || '/%') LIMIT 20",
    )?;
    let rows = stmt.query_map(params![artist_id, target, target], |row| {
        Ok(json!({"item_id": row.get::<_, i64>(0)?, "artist_id": row.get::<_, i64>(1)?, "path": row.get::<_, String>(2)?}))
    })?;
    for row in rows {
        conflicts.push(row?);
    }
    Ok(conflicts)
}

pub fn preview_artist_folder_move(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    root_index: usize,
    destination: &str,
) -> Result<Value> {
    let (name, source_db) = artist_row(conn, artist_id)?;
    let source = roots.map_to_real(&source_db)?;
    if !source.is_dir() || fs::symlink_metadata(&source)?.file_type().is_symlink() {
        bail!("artist source directory is missing");
    }
    if !path_under_authorized_roots(&source, roots) {
        bail!("artist source is outside configured media roots");
    }
    let relative = validate_relative_destination(destination)?;
    let root = roots
        .real_root_at(root_index)
        .ok_or_else(|| anyhow!("media root not found"))?;
    let root_path = PathBuf::from(root);
    if !root_path.is_dir() {
        bail!("media root directory is missing");
    }
    let target = root_path.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    if !target_parent_stays_under_root(&target, &root_path) {
        bail!("destination escapes the configured media root");
    }
    let source_canonical = source.canonicalize()?;
    let target_canonical = target.canonicalize().unwrap_or_else(|_| target.clone());
    if target_canonical == source_canonical || target_canonical.starts_with(&source_canonical) {
        bail!("destination cannot be the source or inside it");
    }
    let conflicts = indexed_conflicts(conn, &path_text(&target), artist_id)?;
    let item_summary: (i64, i64) = conn.query_row(
        "SELECT COUNT(*), COALESCE(SUM(file_size), 0) FROM items WHERE artist_id=? AND missing=0",
        params![artist_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let target_exists = target.exists();
    let inferred_root = root_for_source(&source, roots);
    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "artist_name": name,
        "source": path_text(&source),
        "source_identity": dir_identity(&source).map(|(dev, ino)| json!([dev, ino])),
        "source_root_index": inferred_root,
        "target_root_index": root_index,
        "target_root": root,
        "destination": relative,
        "target": path_text(&target),
        "target_exists": target_exists,
        "item_count": item_summary.0,
        "total_size": item_summary.1,
        "conflicts": conflicts,
        "can_execute": !target_exists && conflicts.is_empty(),
    }))
}

#[cfg(any(not(target_os = "linux"), test))]
fn copy_dir_recursive(source: &Path, target: &Path) -> Result<()> {
    fs::create_dir(target)?;
    copy_dir_contents(source, target)
}

#[cfg(any(not(target_os = "linux"), test))]
fn copy_dir_contents(source: &Path, target: &Path) -> Result<()> {
    for entry in fs::read_dir(source)? {
        let entry = entry?;
        let src = entry.path();
        let dst = target.join(entry.file_name());
        let metadata = fs::symlink_metadata(&src)?;
        if metadata.is_dir() {
            copy_dir_recursive(&src, &dst)?;
        } else if metadata.is_file() {
            let mut output = fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&dst)
                .with_context(|| format!("create {}", dst.display()))?;
            let mut input =
                fs::File::open(&src).with_context(|| format!("open {}", src.display()))?;
            std::io::copy(&mut input, &mut output)
                .with_context(|| format!("copy {}", src.display()))?;
        } else {
            bail!("unsupported filesystem entry {}", src.display());
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn copy_directory_contents_no_follow(
    source_parent: std::os::raw::c_int,
    target_parent: std::os::raw::c_int,
) -> Result<()> {
    use std::ffi::CString;
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    const ENOTDIR: i32 = 20;
    let source = PathBuf::from("/proc/self/fd").join(source_parent.to_string());
    for entry in fs::read_dir(source)? {
        let name = entry?.file_name();
        let printable = name.to_string_lossy().to_string();
        let name = CString::new(name.as_bytes())
            .map_err(|_| anyhow!("directory entry contains a NUL byte"))?;
        match open_dir_at(source_parent, &name) {
            Ok(source_child) => {
                let target_child = create_directory_at_no_follow(target_parent, &name)
                    .with_context(|| format!("create staging directory {printable}"))?;
                copy_directory_contents_no_follow(
                    source_child.as_raw_fd(),
                    target_child.as_raw_fd(),
                )?;
            }
            Err(error) if error.raw_os_error() == Some(ENOTDIR) => {
                copy_regular_file_at(source_parent, target_parent, &name)
                    .with_context(|| format!("copy staging file {printable}"))?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn create_directory_at_no_follow(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::raw::{c_char, c_int, c_uint};

    unsafe extern "C" {
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    if unsafe { mkdirat(parent, name.as_ptr(), 0o755) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    open_dir_at(parent, name)
}

#[cfg(target_os = "linux")]
fn copy_regular_file_at(
    source_parent: std::os::raw::c_int,
    target_parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> Result<()> {
    let mut source = open_regular_file_at_no_follow(source_parent, name)?;
    if !source.metadata()?.is_file() {
        bail!("source entry is not a regular file");
    }
    let mut target = create_regular_file_at_no_follow(target_parent, name)?;
    std::io::copy(&mut source, &mut target)?;
    Ok(())
}

#[cfg(target_os = "linux")]
fn open_regular_file_at_no_follow(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_RDONLY: c_int = 0;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_NOFOLLOW: c_int = 0o400000;
    const O_NONBLOCK: c_int = 0o4000;
    let fd = unsafe {
        openat(
            parent,
            name.as_ptr(),
            O_RDONLY | O_CLOEXEC | O_NOFOLLOW | O_NONBLOCK,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn create_regular_file_at_no_follow(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::fs::File> {
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_WRONLY: c_int = 1;
    const O_CREAT: c_int = 0o100;
    const O_EXCL: c_int = 0o200;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_NOFOLLOW: c_int = 0o400000;
    let fd = unsafe {
        openat(
            parent,
            name.as_ptr(),
            O_WRONLY | O_CREAT | O_EXCL | O_CLOEXEC | O_NOFOLLOW,
            0o644,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn copy_artist_tree_to_staging(source: &Path, staging: &Path, roots: &MediaRoots) -> Result<()> {
    let source = open_authorized_directory_no_follow(source, roots)?;
    let staging = create_authorized_directory_no_follow(staging, roots)?;
    copy_directory_contents_no_follow(source.as_raw_fd(), staging.as_raw_fd())
}

#[cfg(not(target_os = "linux"))]
fn copy_artist_tree_to_staging(source: &Path, staging: &Path, _roots: &MediaRoots) -> Result<()> {
    copy_dir_recursive(source, staging)
}

fn update_path_column(
    conn: &Connection,
    table: &str,
    column: &str,
    artist_id: i64,
    source: &str,
    target: &str,
) -> Result<i64> {
    let query = format!("SELECT id, {column} FROM {table} WHERE artist_id=?");
    let mut stmt = conn.prepare(&query)?;
    let rows = stmt
        .query_map(params![artist_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    let update = format!("UPDATE {table} SET {column}=? WHERE id=?");
    let mut changed = 0;
    for (id, path) in rows {
        if let Some(next) = remap_path(&path, source, target) {
            conn.execute(&update, params![next, id])?;
            changed += 1;
        }
    }
    Ok(changed)
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let names = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(names.iter().any(|name| name == column))
}

#[derive(Debug)]
struct ArtistMoveOutcome {
    renamed: bool,
    staged_source: Option<PathBuf>,
    staged_source_identity: Option<(u64, u64)>,
    published_identity: Option<(u64, u64)>,
}

fn dir_identity(path: &Path) -> Option<(u64, u64)> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        std::fs::symlink_metadata(path)
            .ok()
            .filter(|metadata| metadata.is_dir())
            .map(|metadata| (metadata.dev(), metadata.ino()))
    }
    #[cfg(not(unix))]
    {
        let _ = path;
        None
    }
}

/// Remove a directory only while it still carries the identity this operation
/// published, so a concurrently recreated path is never cleaned up.
fn remove_owned_dir(path: &Path, expected: Option<(u64, u64)>) -> std::io::Result<()> {
    if expected.is_some() && dir_identity(path) != expected {
        return Err(std::io::Error::other(
            "operation-owned directory identity changed before cleanup",
        ));
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(std::io::Error::other(
            "operation-owned directory changed before cleanup",
        ));
    }
    fs::remove_dir_all(path)
}

#[cfg(test)]
fn force_copy_fallback() -> bool {
    std::env::var("GALLERY_TEST_ARTIST_MOVE_COPY").is_ok()
}

#[cfg(not(test))]
fn force_copy_fallback() -> bool {
    false
}

fn copy_fallback_allowed(error: &std::io::Error) -> bool {
    // EXDEV on Linux, ERROR_NOT_SAME_DEVICE on Windows. Other failures (for
    // example O_NOFOLLOW rejecting a raced symlink) are safety failures, not
    // an invitation to retry through ordinary pathname copy operations.
    matches!(error.raw_os_error(), Some(18) | Some(17))
}

/// When a move has to leave a directory behind for manual reconciliation, the
/// path used to live only inside the error text and was lost with the toast.
/// Persist a small JSON record under DATA_DIR/retained-dirs so maintenance can
/// discover these directories later.
fn record_retained_directory(retained: &Path, source: &Path, target: &Path, reason: &str) {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let records = Path::new(&data_dir).join("retained-dirs");
    if std::fs::create_dir_all(&records).is_err() {
        return;
    }
    let recorded_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    let record = serde_json::json!({
        "retained_path": retained.to_string_lossy(),
        "source": source.to_string_lossy(),
        "target": target.to_string_lossy(),
        "reason": reason,
        "recorded_at": recorded_at,
    });
    let _ = std::fs::write(
        records.join(format!("{}.json", uuid::Uuid::new_v4().simple())),
        record.to_string(),
    );
}

/// Drop bookkeeping for retained directories that have since been removed by
/// an operator; returns how many live records remain.
fn prune_retained_directory_records() -> usize {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let records = Path::new(&data_dir).join("retained-dirs");
    let Ok(entries) = std::fs::read_dir(&records) else {
        return 0;
    };
    let mut live = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|extension| extension.to_str()) != Some("json") {
            continue;
        }
        let retained = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
            .and_then(|record| {
                record
                    .get("retained_path")
                    .and_then(|value| value.as_str())
                    .map(str::to_owned)
            });
        match retained {
            Some(retained) if Path::new(&retained).exists() => live += 1,
            _ => {
                let _ = std::fs::remove_file(&path);
            }
        }
    }
    live
}

/// Move the artist tree without overwriting anything another process created:
/// publish via a no-replace rename, or copy to private staging and publish the
/// staging directory with a no-replace rename before retiring the source.
fn move_artist_tree(
    source: &Path,
    target: &Path,
    roots: &MediaRoots,
    expected_source_identity: Option<(u64, u64)>,
) -> Result<ArtistMoveOutcome> {
    let forced_copy = force_copy_fallback();
    let rename_outcome = if forced_copy {
        Err(std::io::Error::other("forced copy fallback"))
    } else {
        rename_directory_under_authorized_roots_no_overwrite_expected(
            source,
            target,
            roots,
            expected_source_identity,
        )
    };
    match rename_outcome {
        Ok(()) => Ok(ArtistMoveOutcome {
            renamed: true,
            staged_source: None,
            staged_source_identity: None,
            published_identity: dir_identity(target),
        }),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            bail!("destination appeared between preview and execute; refusing to proceed")
        }
        Err(error) if forced_copy || copy_fallback_allowed(&error) => {
            if !path_under_authorized_roots(source, roots)
                || !path_under_authorized_roots(target, roots)
            {
                return Err(anyhow!("copy fallback path escaped configured media roots"));
            }
            let staging_target =
                target.with_file_name(format!(".gallery-copy-{}", uuid::Uuid::new_v4().simple()));
            if let Err(error) = copy_artist_tree_to_staging(source, &staging_target, roots) {
                record_retained_directory(&staging_target, source, target, "partial_copy_staging");
                return Err(error.context(format!(
                    "copy staging retained for manual reconciliation: {}",
                    staging_target.display()
                )));
            }
            let staging_target_identity = dir_identity(&staging_target);
            if let Err(error) = rename_directory_under_authorized_roots_no_overwrite_expected(
                &staging_target,
                target,
                roots,
                staging_target_identity,
            ) {
                if error.kind() == std::io::ErrorKind::AlreadyExists {
                    record_retained_directory(
                        &staging_target,
                        source,
                        target,
                        "unpublished_staging",
                    );
                    bail!(
                        "destination appeared between preview and execute; staging retained for manual reconciliation: {}",
                        staging_target.display()
                    );
                }
                return Err(error.into());
            }
            let published_identity = dir_identity(target);
            let staging_source =
                source.with_file_name(format!(".gallery-move-{}", uuid::Uuid::new_v4().simple()));
            if let Err(error) = rename_directory_under_authorized_roots_no_overwrite_expected(
                source,
                &staging_source,
                roots,
                expected_source_identity,
            ) {
                if let Err(cleanup_error) = remove_owned_dir(target, published_identity) {
                    record_retained_directory(target, source, target, "published_target_retained");
                    return Err(anyhow!(
                        "source retirement failed: {error}; published target retained for manual reconciliation: {cleanup_error}"
                    ));
                }
                return Err(error.into());
            }
            let staged_source_identity = dir_identity(&staging_source);
            Ok(ArtistMoveOutcome {
                renamed: false,
                staged_source: Some(staging_source),
                staged_source_identity,
                published_identity,
            })
        }
        Err(error) => Err(error.into()),
    }
}

pub fn execute_artist_folder_move(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    root_index: usize,
    destination: &str,
) -> Result<Value> {
    let preview = preview_artist_folder_move(conn, roots, artist_id, root_index, destination)?;
    if !preview["can_execute"].as_bool().unwrap_or(false) {
        bail!("artist folder move has a target or indexed path conflict");
    }
    let source = PathBuf::from(preview["source"].as_str().unwrap_or_default());
    let target = PathBuf::from(preview["target"].as_str().unwrap_or_default());
    let expected_source_identity =
        preview["source_identity"]
            .as_array()
            .and_then(|identity| match identity.as_slice() {
                [dev, ino] => Some((dev.as_u64()?, ino.as_u64()?)),
                _ => None,
            });
    let source_text = path_text(&source);
    let target_text = path_text(&target);
    let backup = create_db_backup(conn)?;

    let outcome = move_artist_tree(&source, &target, roots, expected_source_identity)?;
    let ArtistMoveOutcome {
        renamed,
        staged_source,
        staged_source_identity,
        published_identity,
    } = outcome;

    let transaction = conn.unchecked_transaction()?;
    let result = (|| -> Result<i64> {
        transaction.execute(
            "UPDATE artists SET path=?, missing=0, missing_at=NULL WHERE id=?",
            params![target_text, artist_id],
        )?;
        let mut updated = 0;
        for (table, column) in [
            ("items", "file_path"),
            ("scan_seen", "file_path"),
            ("scan_candidates", "file_path"),
            ("artist_link_documents", "file_path"),
        ] {
            if table_has_column(&transaction, table, "artist_id")?
                && table_has_column(&transaction, table, column)?
            {
                updated += update_path_column(
                    &transaction,
                    table,
                    column,
                    artist_id,
                    &source_text,
                    &target_text,
                )?;
            }
        }
        if table_has_column(&transaction, "move_candidates", "artist_id")? {
            updated += update_path_column(
                &transaction,
                "move_candidates",
                "old_path",
                artist_id,
                &source_text,
                &target_text,
            )?;
            updated += update_path_column(
                &transaction,
                "move_candidates",
                "new_path",
                artist_id,
                &source_text,
                &target_text,
            )?;
        }
        transaction.commit()?;
        Ok(updated)
    })();
    let updated = match result {
        Ok(updated) => updated,
        Err(error) => {
            // Roll back only through no-replace renames: a concurrently
            // recreated source must never be clobbered; if safe rollback is
            // impossible, report a manual-reconciliation outcome instead.
            if renamed {
                if let Err(rollback_error) =
                    rename_directory_under_authorized_roots_no_overwrite_expected(
                        &target,
                        &source,
                        roots,
                        published_identity,
                    )
                {
                    record_retained_directory(
                        &target,
                        &source,
                        &target,
                        "rollback_retained_target",
                    );
                    return Err(anyhow!(
                        "database update failed: {error}; filesystem rollback failed and needs manual reconciliation: {rollback_error}"
                    ));
                }
            } else {
                let mut rollback_errors = Vec::new();
                if let Some(staging) = staged_source.as_ref() {
                    if let Err(rollback_error) =
                        rename_directory_under_authorized_roots_no_overwrite_expected(
                            staging,
                            &source,
                            roots,
                            staged_source_identity,
                        )
                    {
                        rollback_errors.push(format!(
                            "source rollback failed (staged source: {}): {rollback_error}",
                            staging.display()
                        ));
                    }
                }
                if let Err(cleanup_error) = remove_owned_dir(&target, published_identity) {
                    rollback_errors.push(format!("published target retained: {cleanup_error}"));
                }
                if !rollback_errors.is_empty() {
                    if let Some(staging) = staged_source.as_ref().filter(|s| s.is_dir()) {
                        record_retained_directory(
                            staging,
                            &source,
                            &target,
                            "rollback_retained_staging",
                        );
                    }
                    if target.is_dir() {
                        record_retained_directory(
                            &target,
                            &source,
                            &target,
                            "rollback_retained_target",
                        );
                    }
                    return Err(anyhow!(
                        "database update failed: {error}; filesystem rollback needs manual reconciliation: {}",
                        rollback_errors.join("; ")
                    ));
                }
            }
            return Err(error);
        }
    };
    let cleanup_error = staged_source
        .and_then(|staging| remove_owned_dir(&staging, staged_source_identity).err())
        .map(|error| error.to_string());
    Ok(
        json!({"ok": true, "backup": backup, "source": source_text, "target": target_text, "updated_paths": updated, "cleanup_error": cleanup_error, "retained_dirs": prune_retained_directory_records()}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn retained_directory_records_are_persisted_and_pruned() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let retained = dir.path().join("leftover");
        fs::create_dir_all(&retained).unwrap();
        record_retained_directory(
            &retained,
            &dir.path().join("s"),
            &dir.path().join("t"),
            "test",
        );

        assert_eq!(prune_retained_directory_records(), 1);

        fs::remove_dir_all(&retained).unwrap();
        assert_eq!(prune_retained_directory_records(), 0);
        let records_dir = dir.path().join("data").join("retained-dirs");
        let remaining: Vec<_> = fs::read_dir(&records_dir)
            .map(|entries| entries.filter_map(|entry| entry.ok()).collect())
            .unwrap_or_default();
        assert!(remaining.is_empty(), "pruned records must be removed");
    }

    #[test]
    fn occupied_publish_target_records_retained_staging() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _forced = crate::test_support::EnvVar::set("GALLERY_TEST_ARTIST_MOVE_COPY", "1");
        let dir = tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let source = dir.path().join("Artist");
        fs::create_dir_all(source.join("work")).unwrap();
        fs::write(source.join("work").join("image.jpg"), b"image").unwrap();
        let target = dir.path().join("Moved");
        fs::create_dir_all(&target).unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["root".into()],
        );

        let error = move_artist_tree(&source, &target, &roots, None)
            .expect_err("occupied target must refuse the publish");

        assert!(error.to_string().contains("staging retained"));
        let records_dir = dir.path().join("data").join("retained-dirs");
        let records: Vec<_> = fs::read_dir(&records_dir)
            .map(|entries| {
                entries
                    .filter_map(|entry| entry.ok())
                    .filter(|entry| {
                        entry
                            .path()
                            .extension()
                            .and_then(|extension| extension.to_str())
                            == Some("json")
                    })
                    .collect()
            })
            .unwrap_or_default();
        assert_eq!(records.len(), 1, "the retained staging must be recorded");
        let record: serde_json::Value =
            serde_json::from_str(&fs::read_to_string(records[0].path()).unwrap()).unwrap();
        let recorded = Path::new(record["retained_path"].as_str().unwrap());
        assert!(recorded.is_dir(), "record must point at the staging copy");
        assert!(recorded
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(".gallery-copy-")));
    }

    #[test]
    fn copy_fallback_publishes_tree_and_retires_source_to_staging() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _forced = crate::test_support::EnvVar::set("GALLERY_TEST_ARTIST_MOVE_COPY", "1");
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        fs::create_dir_all(source.join("work")).unwrap();
        fs::write(source.join("work").join("image.jpg"), b"image").unwrap();
        let target = dir.path().join("Moved");
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["root".into()],
        );

        let outcome = move_artist_tree(&source, &target, &roots, None).unwrap();

        assert!(!outcome.renamed);
        assert!(target.join("work").join("image.jpg").is_file());
        assert!(!source.exists());
        let staged = outcome.staged_source.as_ref().unwrap();
        assert!(staged.is_dir());
        assert!(staged
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with(".gallery-move-")));
        let leftovers: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .is_some_and(|n| n.starts_with(".gallery-copy-"))
            })
            .collect();
        assert!(
            leftovers.is_empty(),
            "staging target must be published, not left behind"
        );
    }

    #[test]
    fn copy_fallback_never_overwrites_or_cleans_up_appeared_target() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _forced = crate::test_support::EnvVar::set("GALLERY_TEST_ARTIST_MOVE_COPY", "1");
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("image.jpg"), b"image").unwrap();
        let target = dir.path().join("Moved");
        fs::create_dir_all(&target).unwrap();
        fs::write(target.join("foreign.txt"), b"foreign").unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["root".into()],
        );

        let error = move_artist_tree(&source, &target, &roots, None).unwrap_err();

        assert!(
            error.to_string().contains("destination appeared"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(target.join("foreign.txt")).unwrap(), b"foreign");
        assert!(source.join("image.jpg").is_file());
        // A failed publish retains private staging rather than recursively
        // deleting a pathname that another process may have replaced.
        assert!(fs::read_dir(dir.path()).unwrap().any(|entry| {
            entry.ok().is_some_and(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.starts_with(".gallery-copy-"))
            })
        }));
    }

    #[test]
    fn db_failure_rollback_restores_renamed_source() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        let file = source.join("work").join("image.jpg");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"image").unwrap();
        let conn = Connection::open(dir.path().join("gallery.db")).unwrap();
        conn.execute_batch("CREATE TABLE artists(id INTEGER PRIMARY KEY,name TEXT,path TEXT UNIQUE,missing INTEGER DEFAULT 0,missing_at REAL); CREATE TABLE items(id INTEGER PRIMARY KEY,artist_id INTEGER,file_path TEXT UNIQUE,file_size INTEGER,missing INTEGER DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO artists(id,name,path) VALUES(1,'Artist',?)",
            params![source.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items(id,artist_id,file_path,file_size,missing) VALUES(1,1,?,5,0)",
            params![file.to_string_lossy()],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_items_update BEFORE UPDATE ON items
             BEGIN SELECT RAISE(ABORT, 'stop'); END;",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().into()],
            vec!["root".into()],
        );

        let error = execute_artist_folder_move(&conn, &roots, 1, 0, "Moved").unwrap_err();

        assert!(
            error.to_string().contains("stop"),
            "unexpected error: {error}"
        );
        assert!(source.join("work").join("image.jpg").is_file());
        assert!(!dir.path().join("Moved").exists());
    }

    #[test]
    fn db_failure_with_recreated_source_requires_manual_reconciliation() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        let file = source.join("work").join("image.jpg");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"image").unwrap();
        let conn = Connection::open(dir.path().join("gallery.db")).unwrap();
        conn.execute_batch("CREATE TABLE artists(id INTEGER PRIMARY KEY,name TEXT,path TEXT UNIQUE,missing INTEGER DEFAULT 0,missing_at REAL); CREATE TABLE items(id INTEGER PRIMARY KEY,artist_id INTEGER,file_path TEXT UNIQUE,file_size INTEGER,missing INTEGER DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO artists(id,name,path) VALUES(1,'Artist',?)",
            params![source.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items(id,artist_id,file_path,file_size,missing) VALUES(1,1,?,5,0)",
            params![file.to_string_lossy()],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_items_update BEFORE UPDATE ON items
             BEGIN SELECT RAISE(ABORT, 'stop'); END;",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().into()],
            vec!["root".into()],
        );

        // Another process recreates the source path right after the artist row
        // update starts: rollback must refuse to clobber it.
        let recreated = source.clone();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "artists",
                    ..
                }
            ) {
                fs::create_dir_all(&recreated).unwrap();
                fs::write(recreated.join("foreign.txt"), b"foreign").unwrap();
            }
            Authorization::Allow
        }));
        let error = execute_artist_folder_move(&conn, &roots, 1, 0, "Moved").unwrap_err();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);

        assert!(
            error.to_string().contains("manual reconciliation"),
            "unexpected error: {error}"
        );
        assert_eq!(fs::read(source.join("foreign.txt")).unwrap(), b"foreign");
        assert!(
            dir.path()
                .join("Moved")
                .join("work")
                .join("image.jpg")
                .is_file(),
            "moved tree must stay available for manual reconciliation"
        );
    }

    #[test]
    fn copy_dir_recursive_refuses_to_overwrite_existing_file() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(&src).unwrap();
        fs::create_dir_all(&dst).unwrap();
        fs::write(src.join("a.jpg"), b"source").unwrap();
        fs::write(dst.join("a.jpg"), b"existing").unwrap();

        let result = copy_dir_recursive(&src, &dst);
        assert!(result.is_err());
        // Original target file must be untouched.
        assert_eq!(fs::read(dst.join("a.jpg")).unwrap(), b"existing");
    }

    #[test]
    fn lists_only_direct_non_symlink_directories_under_the_selected_root() {
        let dir = tempdir().unwrap();
        fs::create_dir_all(dir.path().join("Alpha").join("Child")).unwrap();
        fs::create_dir_all(dir.path().join("Beta")).unwrap();
        fs::write(dir.path().join("file.jpg"), b"file").unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().into()],
            vec!["root".into()],
        );

        let root_listing = list_media_root_directories(&roots, 0, "").unwrap();
        assert_eq!(root_listing["directories"], json!(["Alpha", "Beta"]));
        let child_listing = list_media_root_directories(&roots, 0, "Alpha").unwrap();
        assert_eq!(child_listing["directories"], json!(["Child"]));
        assert!(list_media_root_directories(&roots, 0, "../outside").is_err());
    }

    #[test]
    fn copy_dir_recursive_succeeds_when_target_has_no_conflicting_files() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src");
        let dst = dir.path().join("dst");
        fs::create_dir_all(src.join("sub")).unwrap();
        fs::write(src.join("a.jpg"), b"file-a").unwrap();
        fs::write(src.join("sub").join("b.jpg"), b"file-b").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();
        assert_eq!(fs::read(dst.join("a.jpg")).unwrap(), b"file-a");
        assert_eq!(fs::read(dst.join("sub").join("b.jpg")).unwrap(), b"file-b");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn move_artist_tree_rejects_symlinked_target_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let source = root.join("Artist");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(source.join("image.jpg"), b"image").unwrap();
        symlink(&outside, root.join("replaced-parent")).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );
        let target = root.join("replaced-parent").join("Moved");

        assert!(move_artist_tree(&source, &target, &roots, None).is_err());
        assert!(source.join("image.jpg").is_file());
        assert!(!outside.join("Moved").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn move_artist_tree_rejects_symlinked_source_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let parent = root.join("source-parent");
        let source = parent.join("Artist");
        let original_parent = root.join("source-parent-original");
        let outside = dir.path().join("outside");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("image.jpg"), b"original").unwrap();
        fs::create_dir_all(outside.join("Artist")).unwrap();
        fs::write(outside.join("Artist").join("foreign.jpg"), b"foreign").unwrap();
        fs::rename(&parent, &original_parent).unwrap();
        symlink(&outside, &parent).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );

        assert!(move_artist_tree(&source, &root.join("Moved"), &roots, None).is_err());
        assert_eq!(
            fs::read(original_parent.join("Artist").join("image.jpg")).unwrap(),
            b"original"
        );
        assert_eq!(
            fs::read(outside.join("Artist").join("foreign.jpg")).unwrap(),
            b"foreign"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn move_artist_tree_rejects_source_replaced_after_preview() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let source = root.join("Artist");
        let replaced_source = root.join("Artist-before-replacement");
        let target = root.join("Moved");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("image.jpg"), b"original").unwrap();
        let expected = dir_identity(&source);
        fs::rename(&source, &replaced_source).unwrap();
        fs::create_dir(&source).unwrap();
        fs::write(source.join("foreign.jpg"), b"foreign").unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );

        assert!(move_artist_tree(&source, &target, &roots, expected).is_err());
        assert_eq!(fs::read(source.join("foreign.jpg")).unwrap(), b"foreign");
        assert_eq!(
            fs::read(replaced_source.join("image.jpg")).unwrap(),
            b"original"
        );
        assert!(!target.exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copy_staging_refuses_existing_directory() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let source = root.join("Artist");
        let staging = root.join(".gallery-copy-existing");
        fs::create_dir_all(&source).unwrap();
        fs::write(source.join("image.jpg"), b"image").unwrap();
        fs::create_dir_all(&staging).unwrap();
        fs::write(staging.join("foreign.txt"), b"foreign").unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );

        assert!(copy_artist_tree_to_staging(&source, &staging, &roots).is_err());
        assert_eq!(fs::read(staging.join("foreign.txt")).unwrap(), b"foreign");
        assert!(source.join("image.jpg").is_file());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn copy_staging_rejects_symlinked_child() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let source = root.join("Artist");
        let staging = root.join(".gallery-copy-safe");
        let outside = dir.path().join("outside.jpg");
        fs::create_dir_all(&source).unwrap();
        fs::write(&outside, b"outside").unwrap();
        symlink(&outside, source.join("escape.jpg")).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );

        assert!(copy_artist_tree_to_staging(&source, &staging, &roots).is_err());
        assert_eq!(fs::read(&outside).unwrap(), b"outside");
        assert!(!staging.join("escape.jpg").exists());
    }

    #[cfg(unix)]
    #[test]
    fn remove_owned_dir_keeps_replacement() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let path = dir.path().join("owned");
        let outside = dir.path().join("outside");
        fs::create_dir(&path).unwrap();
        let identity = dir_identity(&path);
        fs::remove_dir(&path).unwrap();
        fs::create_dir(&outside).unwrap();
        fs::write(outside.join("foreign.txt"), b"foreign").unwrap();
        symlink(&outside, &path).unwrap();

        assert!(remove_owned_dir(&path, identity).is_err());
        assert_eq!(fs::read(outside.join("foreign.txt")).unwrap(), b"foreign");
    }

    #[test]
    fn previews_and_rejects_existing_target() {
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        let target = dir.path().join("Moved");
        fs::create_dir_all(&source).unwrap();
        fs::create_dir_all(&target).unwrap();
        let conn = Connection::open(dir.path().join("gallery.db")).unwrap();
        conn.execute_batch("CREATE TABLE artists(id INTEGER PRIMARY KEY,name TEXT,path TEXT,missing INTEGER DEFAULT 0,missing_at REAL); CREATE TABLE items(id INTEGER PRIMARY KEY,artist_id INTEGER,file_path TEXT,file_size INTEGER,missing INTEGER DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO artists(id,name,path) VALUES(1,'Artist',?)",
            params![source.to_string_lossy()],
        )
        .unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().into()],
            vec!["root".into()],
        );
        let value = preview_artist_folder_move(&conn, &roots, 1, 0, "Moved").unwrap();
        assert_eq!(value["target_exists"], true);
        assert_eq!(value["can_execute"], false);
    }

    #[test]
    fn executes_move_and_updates_indexed_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let source = dir.path().join("Artist");
        let file = source.join("work").join("image.jpg");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(&file, b"image").unwrap();
        let conn = Connection::open(dir.path().join("gallery.db")).unwrap();
        conn.execute_batch("CREATE TABLE artists(id INTEGER PRIMARY KEY,name TEXT,path TEXT UNIQUE,missing INTEGER DEFAULT 0,missing_at REAL); CREATE TABLE items(id INTEGER PRIMARY KEY,artist_id INTEGER,file_path TEXT UNIQUE,file_size INTEGER,missing INTEGER DEFAULT 0);").unwrap();
        conn.execute(
            "INSERT INTO artists(id,name,path) VALUES(1,'Artist',?)",
            params![source.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items(id,artist_id,file_path,file_size,missing) VALUES(1,1,?,5,0)",
            params![file.to_string_lossy()],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().into()],
            vec!["root".into()],
        );

        let result = execute_artist_folder_move(&conn, &roots, 1, 0, "Moved").unwrap();
        let target = dir.path().join("Moved");
        assert_eq!(result["ok"], true);
        assert!(target.join("work").join("image.jpg").is_file());
        assert!(!source.exists());
        let artist_path: String = conn
            .query_row("SELECT path FROM artists WHERE id=1", [], |row| row.get(0))
            .unwrap();
        let item_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(artist_path, path_text(&target));
        assert_eq!(item_path, path_text(&target.join("work").join("image.jpg")));
        assert!(Path::new(result["backup"].as_str().unwrap()).is_file());
    }
}
