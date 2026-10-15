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
use crate::fs_util::safe_canonicalize;
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
    let root = safe_canonicalize(root)?;
    if !root.is_dir() {
        bail!("media root directory is missing");
    }
    let relative_path = validate_relative_directory(relative_path)?;
    let current = root.join(relative_path.replace('/', std::path::MAIN_SEPARATOR_STR));
    let metadata = fs::symlink_metadata(&current)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        bail!("directory not found");
    }
    if !safe_canonicalize(&current)?.starts_with(&root) {
        bail!("directory escapes the configured media root");
    }
    let mut directories = fs::read_dir(&current)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let metadata = fs::symlink_metadata(entry.path()).ok()?;
            let name = entry.file_name().into_string().ok()?;
            (metadata.is_dir() && !metadata.file_type().is_symlink() && !name.starts_with('.'))
                .then_some(name)
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
    path.strip_prefix(&source)
        .and_then(|r| r.strip_prefix('/'))
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
            let canonical = safe_canonicalize(&root_path).ok()?;
            let source_canonical = safe_canonicalize(source).ok()?;
            source_canonical
                .starts_with(&canonical)
                .then_some((index, canonical))
        })
        .max_by_key(|(_, root)| root.components().count())
        .map(|(index, _)| index)
}

fn target_parent_stays_under_root(target: &Path, root: &Path) -> bool {
    let Ok(root) = safe_canonicalize(root) else {
        return false;
    };
    let mut current = Some(target);
    while let Some(path) = current {
        if path.exists() {
            return safe_canonicalize(path)
                .map(|canonical| canonical.starts_with(&root))
                .unwrap_or(false);
        }
        current = path.parent();
    }
    false
}

fn indexed_conflicts(conn: &Connection, target: &str, artist_id: i64) -> Result<Vec<Value>> {
    let mut conflicts = Vec::new();
    // Byte-exact comparisons: COLLATE NOCASE reports case-only distinct paths
    // (`/root/Foo` vs `/root/foo`) as conflicts on case-sensitive media
    // filesystems, and a raw `LIKE target || '/%'` treats `%`/`_` in valid
    // directory names as wildcards. `=`/`substr` with an explicit BINARY
    // collation match canonical path bytes and need no escaping.
    let artist_conflict = conn
        .query_row(
            "SELECT id, path FROM artists WHERE id != ? AND path COLLATE BINARY = ? LIMIT 1",
            params![artist_id, target],
            |row| Ok(json!({"artist_id": row.get::<_, i64>(0)?, "path": row.get::<_, String>(1)?})),
        )
        .optional()?;
    if let Some(conflict) = artist_conflict {
        conflicts.push(conflict);
    }
    let mut stmt = conn.prepare(
        "SELECT id, artist_id, file_path FROM items
         WHERE artist_id != ?
           AND (file_path COLLATE BINARY = ?
                OR substr(file_path, 1, length(?) + 1) = ? || '/')
         LIMIT 20",
    )?;
    let rows = stmt.query_map(params![artist_id, target, target, target], |row| {
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
    let source_canonical = safe_canonicalize(&source)?;
    let target_canonical = safe_canonicalize(&target).unwrap_or_else(|_| target.clone());
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

/// Rewrite `column` for rows under any of several `source → target` prefixes.
///
/// A stored row can name the artist by its real path or by a virtual alias
/// (`/picturesN`), and an imported library may still hold the alias. Matching
/// only the real prefix updated the `artists` row and left every item row on the
/// old tree, so the next scan read the artist as missing and re-created its
/// items. The caller passes both forms for the same reason `folder_archive`'s
/// `update_folder_item_paths` does.
fn update_path_column_multi(
    conn: &Connection,
    table: &str,
    column: &str,
    artist_id: i64,
    prefixes: &[(&str, &str)],
) -> Result<i64> {
    if prefixes.is_empty() {
        return Ok(0);
    }
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
        for (source, target) in prefixes {
            let Some(next) = remap_path(&path, source, target) else {
                continue;
            };
            conn.execute(&update, params![next, id])?;
            changed += 1;
            break;
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

/// Rewrite active recycle entries (recycled/moving) for one artist during a
/// whole-artist move: `original_path` and the snapshot's `file_path` both
/// move from source prefix to target prefix, so a later restore lands inside
/// the moved tree instead of resurrecting the abandoned source directory.
fn update_recycle_entries_for_move(
    conn: &Connection,
    artist_id: i64,
    source: &str,
    target: &str,
) -> Result<i64> {
    if !table_has_column(conn, "recycle_entries", "original_path")?
        || !table_has_column(conn, "recycle_entries", "item_snapshot")?
    {
        return Ok(0);
    }
    let rows = conn
        .prepare(
            "SELECT id, original_path, item_snapshot FROM recycle_entries
             WHERE artist_id=? AND status IN ('recycled','moving')",
        )?
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let update = "UPDATE recycle_entries SET original_path=?, item_snapshot=? WHERE id=?";
    let mut changed = 0;
    for (id, original_path, snapshot_raw) in rows {
        let Some(new_original) = remap_path(&original_path, source, target) else {
            continue;
        };
        // `original_path` and the snapshot `file_path` must remap together:
        // updating only one leaves the entry pointing at two different
        // generations of the tree and permanently unrestorable. Any failure
        // skips the whole entry (it keeps its consistent pre-move paths) and
        // is reported instead of silently half-applied.
        let outcome = (|| -> Result<String> {
            let snapshot: Value = serde_json::from_str(&snapshot_raw)
                .map_err(|error| anyhow!("parse item_snapshot: {error}"))?;
            let object = snapshot
                .as_object()
                .ok_or_else(|| anyhow!("item_snapshot is not an object"))?;
            let file_path = object
                .get("file_path")
                .and_then(Value::as_str)
                .ok_or_else(|| anyhow!("item_snapshot has no file_path"))?;
            let new_file_path = remap_path(file_path, source, target)
                .ok_or_else(|| anyhow!("snapshot file_path does not remap under source prefix"))?;
            let mut encoded = snapshot;
            encoded["file_path"] = Value::String(new_file_path);
            serde_json::to_string(&encoded)
                .map_err(|error| anyhow!("encode item_snapshot: {error}"))
        })();
        let encoded = match outcome {
            Ok(encoded) => encoded,
            Err(error) => {
                log_error!("artist move: recycle entry {id} left unremapped: {error}");
                continue;
            }
        };
        conn.execute(update, params![new_original, encoded, id])?;
        changed += 1;
    }
    Ok(changed)
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

fn force_copy_fallback() -> bool {
    if cfg!(test) {
        std::env::var("GALLERY_TEST_ARTIST_MOVE_COPY").is_ok()
    } else {
        false
    }
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

fn pending_move_intent_path() -> PathBuf {
    let data_dir = std::env::var("DATA_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data"));
    data_dir.join("pending-artist-move.json")
}

/// Persist a pending whole-artist move intent BEFORE touching the filesystem.
///
/// If the process dies between the tree rename and the database rewrite, the
/// intent lets startup reconciliation roll the move forward instead of leaving
/// disk at the target while every DB row still points at the source.
fn write_pending_move_intent(artist_id: i64, source: &str, target: &str) -> Result<()> {
    let path = pending_move_intent_path();
    if path.exists() {
        bail!("a pending artist move intent already exists; restart the service to reconcile it first");
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create data dir {}", parent.display()))?;
    }
    let payload = json!({
        "artist_id": artist_id,
        "source": source,
        "target": target,
        "created_at": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|value| value.as_secs())
            .unwrap_or(0),
    });
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, payload.to_string())?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

fn clear_pending_move_intent() {
    let _ = std::fs::remove_file(pending_move_intent_path());
}

/// Move an intent that could not be resolved out of the way instead of leaving it
/// in place forever.
///
/// `write_pending_move_intent` refuses to start while the intent file exists, and
/// the remedy it prints is a restart — which is exactly what re-enters the branch
/// that kept the file. An ambiguous state (source and target both present, which
/// the product's own copy fallback produces when it is killed between publishing
/// the staging target and retiring the source) therefore blocked every future
/// artist folder move, for every artist, with no way out.
///
/// The evidence is kept for inspection; only the block is lifted.
fn archive_unresolved_move_intent(note: &str) {
    let path = pending_move_intent_path();
    if !path.is_file() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs())
        .unwrap_or(0);
    let archived = path.with_file_name(format!("pending-artist-move.unresolved-{stamp}.json"));
    match std::fs::rename(&path, &archived) {
        Ok(()) => log_error!(
            "artist move intent {note}; archived to {} so later moves are not blocked",
            archived.display()
        ),
        Err(error) => log_error!(
            "artist move intent {note}; could not archive {}: {error}",
            path.display()
        ),
    }
}

/// Shared DB rewrite for a completed whole-artist tree move: repoint the
/// artist row plus every path-bearing table from source to target.
///
/// `roots` supplies the virtual aliases (`/picturesN`) that may name the same
/// artist, so rows stored under an alias move too. Matching only the real prefix
/// committed `artists.path` while leaving those item rows on the abandoned tree.
fn apply_artist_move_db_updates(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    source_text: &str,
    target_text: &str,
) -> Result<i64> {
    // Pair aliases by root index, not by list position: the source and target
    // roots can be declared with different alias counts, and zipping two
    // independent alias lists would pair the wrong virtual root with the target.
    let source_real = roots.real_root_for(source_text);
    let target_real = roots.real_root_for(target_text);
    let mut prefixes: Vec<(&str, &str)> = vec![(source_text, target_text)];
    let mut logical_pairs: Vec<(String, String)> = Vec::new();
    for index in 0..roots.roots.len() {
        let Some(real) = roots.real_root_at(index) else {
            continue;
        };
        if real != source_real {
            continue;
        }
        let alias = roots.roots[index].trim_end_matches(['/', '\\']).to_string();
        if alias.is_empty() || alias == source_text {
            continue;
        }
        // A virtual alias only maps onto the moved tree when this root is the
        // one that is moving; otherwise the alias keeps pointing at its own root.
        let target_alias = if target_real == source_real {
            alias.clone()
        } else {
            target_text.to_string()
        };
        logical_pairs.push((alias, target_alias));
    }
    for (logical_source, logical_target) in &logical_pairs {
        prefixes.push((logical_source.as_str(), logical_target.as_str()));
    }
    let remap_any = |path: &str| -> Option<String> {
        prefixes
            .iter()
            .find_map(|(source, target)| remap_path(path, source, target))
    };
    let transaction = conn.unchecked_transaction()?;
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
            updated += update_path_column_multi(&transaction, table, column, artist_id, &prefixes)?;
        }
    }
    if table_has_column(&transaction, "move_candidates", "artist_id")? {
        updated += update_path_column_multi(
            &transaction,
            "move_candidates",
            "old_path",
            artist_id,
            &prefixes,
        )?;
        updated += update_path_column_multi(
            &transaction,
            "move_candidates",
            "new_path",
            artist_id,
            &prefixes,
        )?;
    }
    if table_has_column(&transaction, "kemono_subscriptions", "target_dir")?
        && table_has_column(&transaction, "kemono_subscriptions", "artist_id")?
    {
        updated += update_path_column_multi(
            &transaction,
            "kemono_subscriptions",
            "target_dir",
            artist_id,
            &prefixes,
        )?;
    }
    if table_has_column(&transaction, "download_publish_jobs", "target_path")? {
        let mut stmt = transaction.prepare("SELECT id, target_path FROM download_publish_jobs")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (id, path) in rows {
            if let Some(next) = remap_any(&path) {
                transaction.execute(
                    "UPDATE download_publish_jobs SET target_path=? WHERE id=?",
                    params![next, id],
                )?;
                updated += 1;
            }
        }
    }
    // `kemono_files.target_path` is the per-resource destination reserved when
    // the resource was first claimed and reused on every retry, and the
    // downloader publishes into *that* path. Leaving it behind made the next
    // attempt rebuild the abandoned source directory next to the artist's new
    // one — a second tree for the same creator, which the scan then registered
    // as a second artist — and split one work between the two.
    if table_has_column(&transaction, "kemono_files", "target_path")? {
        let mut stmt = transaction.prepare("SELECT id, target_path FROM kemono_files")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (id, path) in rows {
            if let Some(next) = remap_any(&path) {
                transaction.execute(
                    "UPDATE kemono_files SET target_path=? WHERE id=?",
                    params![next, id],
                )?;
                updated += 1;
            }
        }
    }
    if table_has_column(&transaction, "netdisk_task_naming", "snapshot")? {
        let mut stmt = transaction.prepare("SELECT task_id, snapshot FROM netdisk_task_naming")?;
        let rows = stmt
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        drop(stmt);
        for (task_id, snap_str) in rows {
            if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&snap_str) {
                let mut changed = false;
                if val.get("artist_id").and_then(|v| v.as_i64()) == Some(artist_id) {
                    if let Some(p) = val.get("artist_path").and_then(|v| v.as_str()) {
                        if let Some(next) = remap_any(p) {
                            val["artist_path"] = serde_json::Value::String(next);
                            changed = true;
                        }
                    }
                    if let Some(p) = val.get("target_dir").and_then(|v| v.as_str()) {
                        if let Some(next) = remap_path(p, source_text, target_text) {
                            val["target_dir"] = serde_json::Value::String(next);
                            changed = true;
                        }
                    }
                }
                if changed {
                    transaction.execute(
                        "UPDATE netdisk_task_naming SET snapshot=?1 WHERE task_id=?2",
                        params![serde_json::to_string(&val)?, task_id],
                    )?;
                    updated += 1;
                }
            }
        }
    }
    // Active recycle entries follow the move so a later restore lands in the
    // new directory instead of rebuilding the abandoned source path.
    updated += update_recycle_entries_for_move(&transaction, artist_id, source_text, target_text)?;
    transaction.commit()?;
    Ok(updated)
}

/// Startup reconciliation for an interrupted whole-artist folder move.
///
/// Called once at writable startup, mirroring recycle reconciliation: an
/// intent whose filesystem rename already happened rolls forward; an intent
/// whose rename never started rolls back (intent dropped); anything ambiguous
/// is archived for manual reconciliation instead of blocking later moves.
pub fn reconcile_pending_artist_move(conn: &Connection) -> Value {
    let path = pending_move_intent_path();
    let raw = match std::fs::read_to_string(&path) {
        Ok(raw) => raw,
        Err(_) => return json!({"reconciled": false}),
    };
    // The media roots come from the same environment the move itself read, so a
    // roll-forward rewrites the virtual aliases as well as the real paths.
    let roots = crate::media_roots::env_media_roots();
    let parsed: Value = match serde_json::from_str(&raw) {
        Ok(value) => value,
        Err(error) => {
            log_error!("artist move intent unreadable, keeping for inspection: {error}");
            return json!({"reconciled": true, "outcome": "unreadable_intent"});
        }
    };
    let Some(artist_id) = parsed["artist_id"].as_i64() else {
        log_error!("artist move intent missing artist_id, keeping for inspection");
        return json!({"reconciled": true, "outcome": "unreadable_intent"});
    };
    let (Some(source), Some(target)) = (parsed["source"].as_str(), parsed["target"].as_str())
    else {
        log_error!("artist move intent missing paths, keeping for inspection");
        return json!({"reconciled": true, "outcome": "unreadable_intent"});
    };
    let source_dir = PathBuf::from(source);
    let target_dir = PathBuf::from(target);
    let outcome = if !source_dir.is_dir() && target_dir.is_dir() {
        match apply_artist_move_db_updates(conn, &roots, artist_id, source, target) {
            Ok(updated) => {
                clear_pending_move_intent();
                log_info!(
                    "artist move rolled forward for artist {artist_id}: {updated} db rows updated"
                );
                "rolled_forward"
            }
            Err(error) => {
                log_error!(
                    "artist move roll-forward failed for artist {artist_id}: {error:#}; intent kept"
                );
                "roll_forward_failed"
            }
        }
    } else if source_dir.is_dir() && !target_dir.is_dir() {
        clear_pending_move_intent();
        log_info!("artist move intent dropped: filesystem was never moved");
        "rolled_back"
    } else {
        // Both trees present, or neither: the intent cannot decide, and keeping
        // the file would block every later artist move while printing a remedy
        // that re-enters this same branch. Archive it and let the operator look.
        archive_unresolved_move_intent(&format!(
            "needs manual reconciliation: source={source} target={target}"
        ));
        "needs_manual_reconciliation"
    };
    json!({"reconciled": true, "outcome": outcome})
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

    // Record the intent before the first irreversible filesystem step so a
    // crash inside move_artist_tree (or mid DB rewrite) is recoverable.
    write_pending_move_intent(artist_id, &source_text, &target_text)?;
    let outcome = move_artist_tree(&source, &target, roots, expected_source_identity);
    let outcome = match outcome {
        Ok(outcome) => outcome,
        Err(error) => {
            clear_pending_move_intent();
            return Err(error);
        }
    };
    let ArtistMoveOutcome {
        renamed,
        staged_source,
        staged_source_identity,
        published_identity,
    } = outcome;

    let result = apply_artist_move_db_updates(conn, roots, artist_id, &source_text, &target_text);
    let updated = match result {
        Ok(updated) => updated,
        Err(error) => {
            clear_pending_move_intent();
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
    // DB rewrite committed: the move is durable, drop the crash-recovery intent.
    clear_pending_move_intent();
    let cleanup_error = staged_source
        .and_then(|staging| remove_owned_dir(&staging, staged_source_identity).err())
        .map(|error| error.to_string());
    Ok(
        json!({"ok": true, "backup": backup, "source": source_text, "target": target_text, "updated_paths": updated, "cleanup_error": cleanup_error, "retained_dirs": prune_retained_directory_records()}),
    )
}
