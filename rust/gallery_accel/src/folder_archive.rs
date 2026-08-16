//! Folder archive plan list + execute (pure Rust product path).

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::{path_under_authorized_roots, MediaRoots};

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[cfg(target_os = "linux")]
pub(crate) fn rename_dir_no_overwrite(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::c_int;
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: c_int = -100;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    renameat2_no_replace(AT_FDCWD, &source, AT_FDCWD, &target)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn rename_dir_no_overwrite(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(source, target)
}

/// Rename a directory only through no-follow handles rooted in configured
/// media paths, with an optional source identity captured during preview.
#[cfg(target_os = "linux")]
pub(crate) fn rename_directory_under_authorized_roots_no_overwrite_expected(
    source: &Path,
    target: &Path,
    roots: &MediaRoots,
    expected_identity: Option<(u64, u64)>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let (source_root, source_relative) = authorized_root_relative(source, roots)?;
    let (target_root, target_relative) = authorized_root_relative(target, roots)?;
    let source_name = source_relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = target_relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_root_fd = open_absolute_dir(&source_root)?;
    let target_root_fd = open_absolute_dir(&target_root)?;
    let source_parent = open_relative_dir(
        &source_root_fd,
        source_relative.parent().unwrap_or_else(|| Path::new("")),
        false,
    )?;
    let target_parent = open_relative_dir(
        &target_root_fd,
        target_relative.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let source_name = std::ffi::CString::new(source_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = std::ffi::CString::new(target_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_dir = open_dir_at(source_parent.as_raw_fd(), &source_name)?;
    let source_metadata = std::fs::File::from(source_dir.try_clone()?).metadata()?;
    if expected_identity
        .is_some_and(|(dev, ino)| source_metadata.dev() != dev || source_metadata.ino() != ino)
    {
        return Err(std::io::Error::other(
            "source directory identity changed after preview",
        ));
    }

    renameat2_no_replace(
        source_parent.as_raw_fd(),
        &source_name,
        target_parent.as_raw_fd(),
        &target_name,
    )?;
    let actual =
        std::fs::File::from(open_dir_at(target_parent.as_raw_fd(), &target_name)?).metadata()?;
    if source_metadata.dev() == actual.dev() && source_metadata.ino() == actual.ino() {
        return Ok(());
    }
    match renameat2_no_replace(
        target_parent.as_raw_fd(),
        &target_name,
        source_parent.as_raw_fd(),
        &source_name,
    ) {
        Ok(()) => Err(std::io::Error::other(
            "renamed directory identity changed during operation",
        )),
        Err(rollback_error) => Err(std::io::Error::other(format!(
            "renamed directory identity changed during operation; rollback failed: {rollback_error}"
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn rename_directory_under_authorized_roots_no_overwrite_expected(
    source: &Path,
    target: &Path,
    roots: &MediaRoots,
    _expected_identity: Option<(u64, u64)>,
) -> std::io::Result<()> {
    if !path_under_authorized_roots(source, roots) || !path_under_authorized_roots(target, roots) {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    rename_dir_no_overwrite(source, target)
}

#[cfg(target_os = "linux")]
fn authorized_root_relative(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let (root, relative) = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|root| {
            let root = PathBuf::from(root).canonicalize().ok()?;
            let relative = path.strip_prefix(&root).ok()?.to_path_buf();
            Some((root, relative))
        })
        .filter(|(_, relative)| {
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        })
        .max_by_key(|(root, _)| root.components().count())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    Ok((root, relative))
}

#[cfg(target_os = "linux")]
pub(crate) struct AuthorizedDirectory {
    fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl AuthorizedDirectory {
    pub(crate) fn as_raw_fd(&self) -> std::os::raw::c_int {
        use std::os::fd::AsRawFd;

        self.fd.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_authorized_directory_no_follow(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<AuthorizedDirectory> {
    let (root, relative) = authorized_root_relative(path, roots)?;
    let root_fd = open_absolute_dir(&root)?;
    let fd = open_relative_dir(&root_fd, &relative, false)?;
    Ok(AuthorizedDirectory { fd })
}

#[cfg(target_os = "linux")]
pub(crate) fn create_authorized_directory_no_follow(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<AuthorizedDirectory> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    let (root, relative) = authorized_root_relative(path, roots)?;
    let name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let root_fd = open_absolute_dir(&root)?;
    let parent = open_relative_dir(
        &root_fd,
        relative.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    if unsafe { mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = open_dir_at(parent.as_raw_fd(), &name)?;
    Ok(AuthorizedDirectory { fd })
}

#[cfg(target_os = "linux")]
fn renameat2_no_replace(
    source_parent: std::os::raw::c_int,
    source: &std::ffi::CStr,
    target_parent: std::os::raw::c_int,
    target: &std::ffi::CStr,
) -> std::io::Result<()> {
    use std::os::raw::{c_char, c_int, c_uint};

    unsafe extern "C" {
        fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    const RENAME_NOREPLACE: c_uint = 1;
    if unsafe {
        renameat2(
            source_parent,
            source.as_ptr(),
            target_parent,
            target.as_ptr(),
            RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
struct PreparedArtistRename {
    artist: PathBuf,
    target: PathBuf,
    source_parent: std::os::fd::OwnedFd,
    target_parent: std::os::fd::OwnedFd,
    source_name: std::ffi::CString,
    target_name: std::ffi::CString,
    source_dir: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl PreparedArtistRename {
    fn execute(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        renameat2_no_replace(
            self.source_parent.as_raw_fd(),
            &self.source_name,
            self.target_parent.as_raw_fd(),
            &self.target_name,
        )?;
        if let Err(error) = self.verify_target() {
            return match renameat2_no_replace(
                self.target_parent.as_raw_fd(),
                &self.target_name,
                self.source_parent.as_raw_fd(),
                &self.source_name,
            ) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(std::io::Error::other(format!(
                    "renamed directory verification failed: {error}; rollback failed: {rollback_error}"
                ))),
            };
        }
        Ok(())
    }

    fn verify_target(&self) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let expected = std::fs::File::from(self.source_dir.try_clone()?).metadata()?;
        let artist = open_absolute_dir(&self.artist)?;
        let actual = open_relative_dir(&artist, &self.target, false)?;
        let actual = std::fs::File::from(actual).metadata()?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(std::io::Error::other(
                "renamed directory identity changed during operation",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_dir_at(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_RDONLY: c_int = 0;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    let fd = unsafe {
        openat(
            parent,
            name.as_ptr(),
            O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
fn open_absolute_dir(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_RDONLY: c_int = 0;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    if !path.is_absolute() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let slash = std::ffi::CString::new("/").unwrap();
    let fd = unsafe {
        open(
            slash.as_ptr(),
            O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
        )
    };
    if fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let root = unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) };
    open_relative_dir(&root, path.strip_prefix("/").unwrap_or(path), false)
}

#[cfg(target_os = "linux")]
fn open_relative_dir(
    root: &std::os::fd::OwnedFd,
    relative: &Path,
    create: bool,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    let dot = std::ffi::CString::new(".").unwrap();
    let mut current = open_dir_at(root.as_raw_fd(), &dot)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        };
        let name = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let next = match open_dir_at(current.as_raw_fd(), &name) {
            Ok(fd) => fd,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                let created = unsafe { mkdirat(current.as_raw_fd(), name.as_ptr(), 0o755) };
                if created != 0 {
                    let mkdir_error = std::io::Error::last_os_error();
                    if mkdir_error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(mkdir_error);
                    }
                }
                open_dir_at(current.as_raw_fd(), &name)?
            }
            Err(error) => return Err(error),
        };
        current = next;
    }
    Ok(current)
}

#[cfg(target_os = "linux")]
fn prepare_artist_dir_rename(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<PreparedArtistRename> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let artist = artist.canonicalize()?;
    let authorized_root = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|root| PathBuf::from(root).canonicalize().ok())
        .filter(|root| artist.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let authorized_fd = open_absolute_dir(&authorized_root)?;
    let artist_relative = artist
        .strip_prefix(&authorized_root)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let artist_fd = open_relative_dir(&authorized_fd, artist_relative, false)?;

    let source = Path::new(source);
    let target = Path::new(target);
    let source_name = source
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = target
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_parent = open_relative_dir(
        &artist_fd,
        source.parent().unwrap_or_else(|| Path::new("")),
        false,
    )?;
    let target_parent = open_relative_dir(
        &artist_fd,
        target.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let source_name = std::ffi::CString::new(source_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = std::ffi::CString::new(target_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_dir = open_dir_at(source_parent.as_raw_fd(), &source_name)?;

    Ok(PreparedArtistRename {
        artist,
        target: target.to_path_buf(),
        source_parent,
        target_parent,
        source_name,
        target_name,
        source_dir,
    })
}

#[cfg(target_os = "linux")]
fn rename_artist_dir_no_overwrite(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {
    prepare_artist_dir_rename(artist, roots, source, target)?.execute()
}

#[cfg(not(target_os = "linux"))]
fn rename_artist_dir_no_overwrite(
    artist: &Path,
    _roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {
    let source = artist.join(source);
    let target = artist.join(target);
    if let Some(parent) = target.parent() {
        ensure_target_parent_under_artist(parent, artist)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::Other, error))?;
    }
    rename_dir_no_overwrite(&source, &target)
}

fn rollback_undo_folder_rename(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {
    #[cfg(test)]
    if FAIL_UNDO_ROLLBACK.swap(false, std::sync::atomic::Ordering::SeqCst) {
        return Err(std::io::Error::other("forced undo rollback failure"));
    }
    rename_artist_dir_no_overwrite(artist, roots, source, target)
}

#[cfg(test)]
static FAIL_UNDO_ROLLBACK: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

pub(crate) fn backup_retention() -> usize {
    std::env::var("DB_BACKUP_RETENTION")
        .ok()
        .and_then(|value| value.trim().parse::<usize>().ok())
        .unwrap_or(8)
        .max(1)
}

pub(crate) fn prune_backup_root(root: &Path, retention: usize) -> Result<usize> {
    std::fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
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
        .filter_map(|path| path.canonicalize().ok())
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

/// Reject absolute paths and traversal segments for artist-relative folders.
pub(crate) fn validate_relative_folder(folder: &str) -> Result<String> {
    let raw = folder.replace('\\', "/").trim().to_string();
    if raw.is_empty() {
        return Err(anyhow!("Bad folder path"));
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(anyhow!("Bad folder path"));
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err(anyhow!("Bad folder path"));
    }
    if raw.starts_with("//") || raw.starts_with("\\\\") {
        return Err(anyhow!("Bad folder path"));
    }
    let mut parts = Vec::new();
    for part in raw.trim_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        // Reject "." and ".." explicitly (do not silently strip).
        if part == "." || part == ".." {
            return Err(anyhow!("Bad folder path"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(anyhow!("Bad folder path"));
    }
    Ok(parts.join("/"))
}

fn path_under_artist(path: &Path, artist: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let artist = artist
        .canonicalize()
        .unwrap_or_else(|_| artist.to_path_buf());
    path.starts_with(&artist)
}

fn target_parent_under_artist(parent: &Path, artist: &Path) -> bool {
    let mut current = Some(parent);
    while let Some(candidate) = current {
        if candidate.exists() {
            return candidate.is_dir() && path_under_artist(candidate, artist);
        }
        current = candidate.parent();
    }
    false
}

fn folder_db_path(artist_path: &str, folder: &str) -> String {
    PathBuf::from(artist_path)
        .join(folder)
        .to_string_lossy()
        .replace('\\', "/")
}

fn update_folder_item_paths(
    tx: &rusqlite::Transaction<'_>,
    artist_id: i64,
    logical_source: &str,
    real_source: &str,
    logical_target: &str,
    real_target: &str,
) -> Result<i64> {
    let logical_prefix = format!("{}/", logical_source.trim_end_matches('/'));
    let real_prefix = format!("{}/", real_source.trim_end_matches('/'));
    Ok(tx.execute(
        "UPDATE items SET file_path=CASE
           WHEN file_path=?1 OR instr(file_path, ?2)=1
             THEN ?3 || substr(file_path, length(?1) + 1)
           ELSE ?4 || substr(file_path, length(?5) + 1)
         END
         WHERE artist_id=?6 AND (
           file_path=?1 OR instr(file_path, ?2)=1 OR
           file_path=?5 OR instr(file_path, ?7)=1
         )",
        params![
            real_source,
            real_prefix,
            real_target,
            logical_target,
            logical_source,
            artist_id,
            logical_prefix,
        ],
    )? as i64)
}

#[cfg(not(target_os = "linux"))]
fn ensure_target_parent_under_artist(parent: &Path, artist: &Path) -> Result<()> {
    let relative = parent
        .strip_prefix(artist)
        .map_err(|_| anyhow!("target parent is outside the artist directory"))?;
    if !artist.is_dir() || !path_under_artist(artist, artist) {
        bail!("artist directory is unavailable or outside its real path");
    }
    let mut current = artist.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("target parent contains an invalid path component");
        };
        current.push(component);
        if !current.exists() {
            match std::fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !current.is_dir() || !path_under_artist(&current, artist) {
            bail!("target parent leaves the artist directory");
        }
    }
    Ok(())
}

pub fn ensure_folder_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS folder_rename_plans (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL,
            source_folder TEXT NOT NULL,
            original_folder_name TEXT NOT NULL DEFAULT '',
            original_title TEXT NOT NULL DEFAULT '',
            parsed_date TEXT NOT NULL DEFAULT '',
            selected_tag_ids TEXT NOT NULL DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'needs_tags',
            file_count INTEGER NOT NULL DEFAULT 0,
            total_size INTEGER NOT NULL DEFAULT 0,
            max_mtime REAL NOT NULL DEFAULT 0,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            confirmed_at REAL,
            confirmation_source TEXT NOT NULL DEFAULT '',
            target_folder TEXT NOT NULL DEFAULT '',
            executed_at REAL,
            execution_log TEXT NOT NULL DEFAULT '[]',
            plan_kind TEXT NOT NULL DEFAULT 'rename_folder',
            split_actions TEXT NOT NULL DEFAULT '[]',
            UNIQUE(artist_id, source_folder)
        );
        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        ",
    )?;
    let has_snapshot: bool = conn
        .prepare("PRAGMA table_info(folder_rename_plans)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "format_snapshot");
    if !has_snapshot {
        conn.execute(
            "ALTER TABLE folder_rename_plans ADD COLUMN format_snapshot TEXT NOT NULL DEFAULT '{}'",
            [],
        )?;
    }
    conn.execute(
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='', updated_at=?
         WHERE status IN ('confirmed','ready')
           AND (
             execution_log LIKE '%failed%'
             OR execution_log LIKE '%source_missing%'
             OR execution_log LIKE '%target_exists%'
             OR execution_log LIKE '%bad_folder_path%'
             OR execution_log LIKE '%db_update_failed%'
             OR execution_log LIKE '%outside_artist%'
             OR execution_log LIKE '%\"status\":\"error\"%'
           )",
        params![now()],
    )?;
    // Drop obsolete auto-archive summary only; plans and media paths stay untouched.
    purge_folder_rename_auto_last_run(conn)?;
    Ok(())
}

pub fn purge_folder_rename_auto_last_run(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM app_settings WHERE key='folder_rename_auto_last_run'",
        [],
    )?;
    Ok(())
}

pub(crate) fn archive_failure_message(reason: &str) -> &'static str {
    match reason {
        "backup_failed" => "数据库备份失败",
        "source_missing" => "来源文件夹不存在",
        "target_exists" => "目标已存在",
        "target_parent_failed" => "无法创建目标文件夹",
        "target_inside_source" => "目标文件夹不能位于来源文件夹内",
        "bad_folder_path" => "文件夹路径无效",
        "db_update_failed" => "数据库路径更新失败",
        "rollback_failed" => "执行回滚失败，需要人工核对",
        "outside_artist" => "路径不在画师目录内",
        "execution_failed" => "执行失败",
        _ => "整理失败",
    }
}

/// Record a failed attempt as manual review so automatic runs do not retry it.
pub(crate) fn record_plan_execution_failure(
    conn: &Connection,
    plan_id: i64,
    reason: &str,
    source: &str,
    target: &str,
    extra: Option<Value>,
) -> Result<()> {
    let mut entry = json!({
        "at": now(),
        "status": "failed",
        "reason": reason,
        "message": archive_failure_message(reason),
        "source": source,
        "target": target,
        "automatic": true,
    });
    if let Some(Value::Object(map)) = extra {
        if let Some(object) = entry.as_object_mut() {
            for (key, value) in map {
                object.insert(key, value);
            }
        }
    }
    conn.execute(
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=? AND status='confirmed'",
        params![json!([entry]).to_string(), now(), plan_id],
    )?;
    Ok(())
}

fn record_undo_reconciliation_failure(
    conn: &Connection,
    plan_id: i64,
    source: &str,
    target: &str,
    backup: &str,
    error: &anyhow::Error,
    rollback_error: &std::io::Error,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let (status, execution_log): (String, String) = tx.query_row(
        "SELECT status, execution_log FROM folder_rename_plans WHERE id=?",
        params![plan_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status != "executed" {
        bail!("plan state changed while recording undo reconciliation failure");
    }
    let mut log = serde_json::from_str::<Value>(&execution_log)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    log.push(json!({
        "at": now(),
        "status": "failed",
        "reason": "rollback_failed",
        "message": archive_failure_message("rollback_failed"),
        "operation": "folder_rename_undo",
        "source": source,
        "target": target,
        "backup": backup,
        "error": error.to_string(),
        "rollback_error": rollback_error.to_string(),
        "automatic": false,
    }));
    let changed = tx.execute(
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=? AND status='executed' AND execution_log=?",
        params![Value::Array(log).to_string(), now(), plan_id, execution_log,],
    )?;
    if changed != 1 {
        bail!("plan state changed while recording undo reconciliation failure");
    }
    tx.commit()?;
    Ok(())
}

pub fn auto_discover_artist_folder_plans(conn: &Connection, artist_id: i64) -> Result<()> {
    ensure_folder_schema(conn)?;
    let query_only = conn
        .query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        != 0;
    if query_only {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT folder_name, COUNT(*), MIN(COALESCE(date, ''))
         FROM items
         WHERE artist_id=? AND COALESCE(missing, 0)=0 AND folder_name != ''
         GROUP BY folder_name",
    )?;
    let folders: Vec<(String, i64, String)> = stmt
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();

    for (folder, file_count, min_date) in folders {
        if folder.trim().is_empty() {
            continue;
        }
        let mut parsed_date = crate::media_type::extract_date_from_folder(&folder);
        if parsed_date.is_empty() && min_date.len() >= 10 && !min_date.starts_with("0000") {
            parsed_date = min_date[..10].to_string();
        }

        let mut item_stmt = conn.prepare(
            "SELECT i.id, (
                SELECT json_group_array(it.tag_id)
                FROM (SELECT it.tag_id FROM item_tags it WHERE it.item_id = i.id ORDER BY it.tag_id) it
             )
             FROM items i
             WHERE i.artist_id=? AND i.folder_name=? AND COALESCE(i.missing, 0)=0
             ORDER BY i.id",
        )?;
        let items: Vec<(i64, Vec<i64>)> = item_stmt
            .query_map(params![artist_id, folder], |row| {
                let item_id: i64 = row.get(0)?;
                let raw: Option<String> = row.get(1)?;
                let tags: Vec<i64> = raw
                    .and_then(|s| serde_json::from_str::<Vec<i64>>(&s).ok())
                    .unwrap_or_default();
                Ok((item_id, tags))
            })?
            .filter_map(|r| r.ok())
            .collect();

        let total_items = items.len();
        if total_items == 0 {
            continue;
        }

        let has_any_tags = items.iter().any(|(_, tags)| !tags.is_empty());
        if !has_any_tags {
            // Folders with no tags must not appear in the pending organize list.
            let _ = conn.execute(
                "DELETE FROM folder_rename_plans WHERE artist_id=? AND source_folder=? AND status NOT IN ('confirmed', 'executed')",
                params![artist_id, folder],
            );
            continue;
        }

        let first_tags = &items[0].1;
        let all_tags_identical = !first_tags.is_empty() && items.iter().all(|(_, tags)| tags == first_tags);
        let selected_tag_ids_json = if all_tags_identical {
            serde_json::to_string(first_tags)?
        } else {
            let mut union_set = std::collections::BTreeSet::new();
            for (_, tags) in &items {
                for tid in tags {
                    union_set.insert(*tid);
                }
            }
            serde_json::to_string(&union_set.into_iter().collect::<Vec<_>>())?
        };

        let initial_status = if all_tags_identical { "draft" } else { "inconsistent_tags" };

        let existing: Option<(i64, String, String, String, i64, String)> = conn
            .query_row(
                "SELECT id, status, parsed_date, selected_tag_ids, file_count, target_folder
                 FROM folder_rename_plans
                 WHERE artist_id=? AND source_folder=?",
                params![artist_id, folder],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?;

        match existing {
            None => {
                let _ = conn.execute(
                    "INSERT INTO folder_rename_plans
                     (artist_id, source_folder, original_folder_name, original_title, parsed_date,
                      selected_tag_ids, status, file_count, target_folder, execution_log, plan_kind,
                      split_actions, confirmation_source, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, '', '[]', 'rename_folder', '[]', '', ?)",
                    params![
                        artist_id,
                        folder,
                        folder,
                        folder,
                        parsed_date,
                        selected_tag_ids_json,
                        initial_status,
                        file_count,
                        now()
                    ],
                );
            }
            Some((plan_id, status, old_date, old_tags, old_count, old_target)) => {
                if status != "confirmed" && status != "executed" {
                    let new_status = if all_tags_identical {
                        if status == "inconsistent_tags" { "draft" } else { status.as_str() }
                    } else {
                        "inconsistent_tags"
                    };
                    let new_target = if !all_tags_identical { "" } else { old_target.as_str() };
                    let final_date = if old_date.is_empty() {
                        parsed_date
                    } else {
                        old_date
                    };
                    let _ = conn.execute(
                        "UPDATE folder_rename_plans
                         SET selected_tag_ids=?, parsed_date=?, file_count=?, status=?, target_folder=?, updated_at=?
                         WHERE id=?",
                        params![selected_tag_ids_json, final_date, file_count, new_status, new_target, now(), plan_id],
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn list_folder_renames(conn: &Connection, artist_id: Option<i64>) -> Result<Value> {
    ensure_folder_schema(conn)?;
    if let Some(aid) = artist_id {
        let _ = auto_discover_artist_folder_plans(conn, aid);
    }
    let mut sql = String::from(
        "SELECT id, artist_id, source_folder, target_folder, status, plan_kind, file_count,
                selected_tag_ids, parsed_date, execution_log, confirmed_at, executed_at
         FROM folder_rename_plans",
    );
    let mut plans = Vec::new();
    if let Some(aid) = artist_id {
        sql.push_str(" WHERE artist_id=? ORDER BY id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![aid], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    } else {
        sql.push_str(" ORDER BY id DESC LIMIT 500");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    }
    Ok(json!({"plans": plans, "total": plans.len()}))
}

fn map_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "artist_id": r.get::<_, i64>(1)?,
        "source_folder": r.get::<_, String>(2)?,
        "target_folder": r.get::<_, String>(3)?,
        "status": r.get::<_, String>(4)?,
        "plan_kind": r.get::<_, String>(5)?,
        "file_count": r.get::<_, i64>(6)?,
        "selected_tag_ids": r.get::<_, String>(7)?,
        "parsed_date": r.get::<_, String>(8)?,
        "execution_log": r.get::<_, String>(9)?,
        "confirmed_at": r.get::<_, Option<f64>>(10)?,
        "executed_at": r.get::<_, Option<f64>>(11)?,
    }))
}

pub fn upsert_folder_rename_plans(
    conn: &Connection,
    artist_id: i64,
    plans: &[Value],
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    let artist_exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM artists WHERE id=?)",
        [artist_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !artist_exists {
        bail!("invalid folder rename plan: artist does not exist");
    }
    let mut upserted = 0i64;
    for plan in plans {
        let source_raw = plan
            .get("source_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if source_raw.is_empty() {
            bail!("invalid folder rename plan: source_folder is required");
        }
        let source = validate_relative_folder(source_raw).with_context(|| {
            format!("invalid folder rename plan: invalid source_folder {source_raw:?}")
        })?;
        let target_raw = plan
            .get("target_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let target = if target_raw.is_empty() {
            String::new()
        } else {
            validate_relative_folder(target_raw).with_context(|| {
                format!("invalid folder rename plan: invalid target_folder {target_raw:?}")
            })?
        };
        let status = plan
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("needs_tags");
        if !matches!(status, "draft" | "needs_tags" | "ready" | "manual_review" | "inconsistent_tags") {
            bail!("invalid folder rename plan: invalid editable status");
        }
        let existing_status: Option<String> = tx
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE artist_id=? AND source_folder=?",
                params![artist_id, source],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(ref st) = existing_status {
            if st == "inconsistent_tags" && !target.is_empty() {
                bail!("invalid folder rename plan: folder tags are inconsistent, cannot modify target");
            }
        }
        let parsed_tags = match plan.get("selected_tag_ids") {
            Some(Value::String(value)) => serde_json::from_str(value).with_context(|| {
                "invalid folder rename plan: selected_tag_ids must be a JSON array"
            })?,
            Some(Value::Array(value)) => Value::Array(value.clone()),
            Some(_) => {
                bail!("invalid folder rename plan: selected_tag_ids must be an array")
            }
            None => Value::Array(Vec::new()),
        };
        let tag_values = parsed_tags.as_array().ok_or_else(|| {
            anyhow!("invalid folder rename plan: selected_tag_ids must be an array")
        })?;
        let mut tag_ids = Vec::with_capacity(tag_values.len());
        let mut seen = HashSet::new();
        for value in tag_values {
            let tag_id = value.as_i64().filter(|id| *id > 0).ok_or_else(|| {
                anyhow!(
                    "invalid folder rename plan: selected_tag_ids must contain positive integers"
                )
            })?;
            if !seen.insert(tag_id) {
                continue;
            }
            let owned = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE id=? AND artist_id=?)",
                params![tag_id, artist_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !owned {
                bail!(
                    "invalid folder rename plan: tag {tag_id} does not belong to artist {artist_id}"
                );
            }
            tag_ids.push(tag_id);
        }
        let tags = serde_json::to_string(&tag_ids)?;
        upserted += tx.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status, selected_tag_ids, updated_at)
             VALUES (?,?,?,?,?,?)
             ON CONFLICT(artist_id, source_folder) DO UPDATE SET
               target_folder=excluded.target_folder,
               status=excluded.status,
               selected_tag_ids=excluded.selected_tag_ids,
               updated_at=excluded.updated_at
             WHERE folder_rename_plans.status NOT IN ('confirmed','executed')",
            params![artist_id, source, target, status, tags, now()],
        )? as i64;
    }
    tx.commit()?;
    Ok(json!({"ok": true, "upserted": upserted}))
}

pub fn set_folder_rename_auto(conn: &Connection, enabled: bool) -> Result<Value> {
    ensure_folder_schema(conn)?;
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('folder_rename_auto_enabled', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![if enabled { "1" } else { "0" }, now()],
    )?;
    conn.execute(
        "DELETE FROM app_settings WHERE key='folder_rename_auto'",
        [],
    )?;
    purge_folder_rename_auto_last_run(conn)?;
    Ok(json!({"enabled": enabled}))
}

pub fn folder_rename_auto_enabled(conn: &Connection) -> Result<bool> {
    let query_only = conn.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))? != 0;
    if query_only {
        let table_exists = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='app_settings')",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !table_exists {
            return Ok(false);
        }
    } else {
        ensure_folder_schema(conn)?;
    }
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='folder_rename_auto_enabled'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(value) = value {
        return Ok(matches!(value.trim(), "1" | "true" | "yes" | "on"));
    }
    let legacy: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='folder_rename_auto'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let enabled = legacy
        .as_deref()
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if legacy.is_some() && !query_only {
        set_folder_rename_auto(conn, enabled)?;
    }
    Ok(enabled)
}

/// Execute confirmed plans for an artist: online SQLite backup then rename folders + update item paths.
pub fn execute_folder_renames(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    dry_run: bool,
) -> Result<Value> {
    execute_folder_renames_with_backup(conn, roots, artist_id, dry_run, None)
}

pub fn execute_folder_renames_with_backup(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    dry_run: bool,
    backup_override: Option<&str>,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let artist_path: String = conn.query_row(
        "SELECT path FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let artist_root = roots.map_to_real(&artist_path)?;
    let plans: Vec<(i64, String, String)> = conn
        .prepare(
            "SELECT id, source_folder, target_folder FROM folder_rename_plans
             WHERE artist_id=? AND status='confirmed' AND target_folder != ''",
        )?
        .query_map(params![artist_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if !path_under_authorized_roots(&artist_root, roots) {
        let results = plans
            .iter()
            .map(|(id, source, target)| {
                if !dry_run {
                    let _ = record_plan_execution_failure(
                        conn,
                        *id,
                        "outside_artist",
                        source,
                        target,
                        None,
                    );
                }
                json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": "outside_artist",
                    "source": source,
                    "target": target,
                })
            })
            .collect::<Vec<_>>();
        return Ok(json!({
            "ok": false,
            "dry_run": dry_run,
            "backup": "",
            "results": results,
        }));
    }

    let mut backup_path = backup_override.unwrap_or("").to_string();
    if !dry_run && !plans.is_empty() && backup_path.is_empty() {
        match create_db_backup(conn) {
            Ok(path) => backup_path = path,
            Err(error) => {
                for (id, source, target) in &plans {
                    let _ = record_plan_execution_failure(
                        conn,
                        *id,
                        "backup_failed",
                        source,
                        target,
                        Some(json!({"error": error.to_string()})),
                    );
                }
                return Ok(json!({
                    "ok": false,
                    "dry_run": false,
                    "backup": "",
                    "results": plans.into_iter().map(|(id, source, target)| json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": "backup_failed",
                        "source": source,
                        "target": target,
                        "error": error.to_string()
                    })).collect::<Vec<_>>()
                }));
            }
        }
    }

    let mut executed = Vec::new();
    for (id, source_raw, target_raw) in plans {
        let source = match validate_relative_folder(&source_raw) {
            Ok(v) => v,
            Err(_) => {
                if !dry_run {
                    let _ = record_plan_execution_failure(
                        conn,
                        id,
                        "bad_folder_path",
                        &source_raw,
                        &target_raw,
                        None,
                    );
                }
                executed
                    .push(json!({"plan_id": id, "status": "error", "reason": "bad_folder_path"}));
                continue;
            }
        };
        let target = match validate_relative_folder(&target_raw) {
            Ok(v) => v,
            Err(_) => {
                if !dry_run {
                    let _ = record_plan_execution_failure(
                        conn,
                        id,
                        "bad_folder_path",
                        &source,
                        &target_raw,
                        None,
                    );
                }
                executed
                    .push(json!({"plan_id": id, "status": "error", "reason": "bad_folder_path"}));
                continue;
            }
        };
        let src = artist_root.join(&source);
        let dst = artist_root.join(&target);
        let src_s = src.to_string_lossy().replace('\\', "/");
        let dst_s = dst.to_string_lossy().replace('\\', "/");
        let src_logical = folder_db_path(&artist_path, &source);
        let dst_logical = folder_db_path(&artist_path, &target);

        // Revalidate: source exists, target free.
        if !src.is_dir() {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "source_missing",
                    &source,
                    &target,
                    None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "source_missing"}));
            continue;
        }
        if dst.exists() {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "target_exists",
                    &source,
                    &target,
                    None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "target_exists"}));
            continue;
        }
        if target == source || target.starts_with(&format!("{source}/")) {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "target_inside_source",
                    &source,
                    &target,
                    None,
                );
            }
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "target_inside_source"
            }));
            continue;
        }
        // Safety: stay under artist path.
        if !path_under_artist(&src, &artist_root)
            || !target_parent_under_artist(dst.parent().unwrap_or(&dst), &artist_root)
        {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "outside_artist",
                    &source,
                    &target,
                    None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "outside_artist"}));
            continue;
        }
        if dry_run {
            executed.push(
                json!({"plan_id": id, "status": "dry_run", "source": source, "target": target}),
            );
            continue;
        }
        // Rename first, then DB in one transaction. On DB failure, restore folder
        // AND reverse any partial item path rewrite.
        if let Err(error) = rename_artist_dir_no_overwrite(&artist_root, roots, &source, &target) {
            let _ = record_plan_execution_failure(
                conn,
                id,
                "execution_failed",
                &source,
                &target,
                Some(json!({"error": error.to_string()})),
            );
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "execution_failed",
                "error": error.to_string(),
            }));
            continue;
        }
        let db_result = (|| -> Result<i64> {
            let tx = conn.unchecked_transaction()?;
            let updated_items = update_folder_item_paths(
                &tx,
                artist_id,
                &src_logical,
                &src_s,
                &dst_logical,
                &dst_s,
            )?;
            let log = json!([{
                "at": now(),
                "status": "executed",
                "source": src_s,
                "target": dst_s,
                "backup": backup_path,
                "updated_items": updated_items
            }]);
            let changed = tx.execute(
                "UPDATE folder_rename_plans SET status='executed', executed_at=?, execution_log=?, updated_at=?
                 WHERE id=? AND status='confirmed'
                   AND source_folder=? AND target_folder=?",
                params![now(), log.to_string(), now(), id, source_raw, target_raw],
            )?;
            if changed != 1 {
                bail!("stale_state");
            }
            tx.commit()?;
            Ok(updated_items)
        })();
        if let Err(err) = db_result {
            let stale = err.to_string().contains("stale_state");
            if let Err(rollback_error) =
                rename_artist_dir_no_overwrite(&artist_root, roots, &target, &source)
            {
                let detail = json!({
                    "error": err.to_string(),
                    "rollback_error": rollback_error.to_string(),
                });
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "rollback_failed",
                    &source,
                    &target,
                    Some(detail.clone()),
                );
                executed.push(json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": "rollback_failed",
                    "error": detail,
                }));
                continue;
            }
            if !stale {
                let _ = record_plan_execution_failure(
                    conn,
                    id,
                    "db_update_failed",
                    &source,
                    &target,
                    Some(json!({"error": err.to_string()})),
                );
            }
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": if stale { "stale_state" } else { "db_update_failed" },
                "error": err.to_string(),
            }));
            continue;
        }
        executed
            .push(json!({"plan_id": id, "status": "executed", "source": source, "target": target}));
    }
    Ok(json!({
        "ok": true,
        "dry_run": dry_run,
        "backup": backup_path,
        "results": executed
    }))
}

pub fn undo_folder_rename_plan(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let plan: Option<(i64, String, String, String, String)> = conn
        .query_row(
            "SELECT artist_id, source_folder, target_folder, status, execution_log
             FROM folder_rename_plans WHERE id=?",
            params![plan_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((artist_id, source_raw, target_raw, status, execution_log)) = plan else {
        bail!("plan_not_found");
    };
    if status != "executed" {
        bail!("plan_not_executed");
    }
    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(artist_path) = artist_path else {
        bail!("artist_missing");
    };
    let artist_root = roots.map_to_real(&artist_path)?;
    if !artist_root.is_dir() {
        bail!("artist_missing");
    }
    if !path_under_authorized_roots(&artist_root, roots) {
        bail!("outside_artist");
    }
    let source = validate_relative_folder(&source_raw).map_err(|_| anyhow!("outside_artist"))?;
    let target = validate_relative_folder(&target_raw).map_err(|_| anyhow!("outside_artist"))?;
    let source_path = artist_root.join(&source);
    let target_path = artist_root.join(&target);
    if !target_path.exists() {
        bail!("target_missing");
    }
    if !target_path.is_dir() {
        bail!("target_not_directory");
    }
    if source_path.exists() {
        bail!("source_exists");
    }
    if !path_under_artist(&target_path, &artist_root)
        || !target_parent_under_artist(source_path.parent().unwrap_or(&source_path), &artist_root)
    {
        bail!("outside_artist");
    }

    let backup = create_db_backup(conn)?;
    rename_artist_dir_no_overwrite(&artist_root, roots, &target, &source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!("source_exists")
        } else {
            error.into()
        }
    })?;

    let target_db = target_path.to_string_lossy().replace('\\', "/");
    let source_db = source_path.to_string_lossy().replace('\\', "/");
    let target_logical = folder_db_path(&artist_path, &target);
    let source_logical = folder_db_path(&artist_path, &source);
    let db_result = (|| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        let updated_items = update_folder_item_paths(
            &tx,
            artist_id,
            &target_logical,
            &target_db,
            &source_logical,
            &source_db,
        )?;
        let mut log = serde_json::from_str::<Value>(&execution_log)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        log.push(json!({
            "at": now(),
            "status": "reverted",
            "reason": "folder_rename_undo",
            "source": target_db,
            "target": source_db,
            "updated_items": updated_items,
            "backup": backup,
        }));
        let changed = tx.execute(
            "UPDATE folder_rename_plans
             SET status='reverted', executed_at=NULL, execution_log=?, updated_at=?
             WHERE id=? AND status='executed'
               AND source_folder=? AND target_folder=? AND execution_log=?",
            params![
                Value::Array(log).to_string(),
                now(),
                plan_id,
                source_raw,
                target_raw,
                execution_log,
            ],
        )?;
        if changed != 1 {
            bail!("stale_state");
        }
        tx.commit()?;
        Ok(updated_items)
    })();

    match db_result {
        Ok(updated_items) => Ok(json!({
            "ok": true,
            "status": "reverted",
            "reason": "folder_rename_undo",
            "plan_id": plan_id,
            "source": target,
            "target": source,
            "updated_items": updated_items,
            "backup": backup,
        })),
        Err(error) => match rollback_undo_folder_rename(&artist_root, roots, &source, &target) {
            Ok(()) => Err(error),
            Err(rollback_error) => {
                record_undo_reconciliation_failure(
                    conn,
                    plan_id,
                    &target_db,
                    &source_db,
                    &backup,
                    &error,
                    &rollback_error,
                )
                .with_context(|| {
                    format!(
                        "reconciliation_required: database update failed: {error}; filesystem rollback failed: {rollback_error}; failure state could not be persisted"
                    )
                })?;
                bail!(
                    "reconciliation_required: database update failed: {error}; filesystem rollback failed: {rollback_error}"
                )
            }
        },
    }
}

/// Generate target folder names for discovered draft plans without requiring a
/// manual "更新整理项" click. Plans with conflicts (target exists,
/// inconsistent tags, etc.) are skipped and stay manual. Shared by the
/// post-scan auto archive and the per-artist manual auto run.
pub fn auto_name_artist_draft_plans(
    conn: &Connection,
    roots: &MediaRoots,
    artists: &[i64],
) -> Result<usize> {
    let mut auto_named = 0usize;
    for artist_id in artists {
        match crate::archive_profiles::preview_folder_rename_template(
            conn,
            roots,
            *artist_id,
            None,
            None,
            None,
            None,
        ) {
            Ok(preview) => {
                let profile = preview["profile"].clone();
                let format_source = preview["format_source"].as_str().unwrap_or("");
                let snapshot =
                    crate::archive_format::rule_snapshot(&profile, format_source).to_string();
                if let Some(plans) = preview["plans"].as_array() {
                    for plan in plans {
                        let Some(id) = plan["id"].as_i64() else { continue };
                        let Some(target) = plan["target_folder"].as_str() else { continue };
                        if target.is_empty() {
                            continue;
                        }
                        let has_conflicts = plan["conflicts"]
                            .as_array()
                            .map(|conflicts| !conflicts.is_empty())
                            .unwrap_or(true);
                        if has_conflicts {
                            continue;
                        }
                        let changed = conn.execute(
                            "UPDATE folder_rename_plans
                             SET target_folder=?, format_snapshot=?, status='ready',
                                 confirmed_at=NULL, confirmation_source='',
                                 updated_at=strftime('%s','now')
                             WHERE id=? AND artist_id=? AND status='draft'",
                            params![target, snapshot, id, artist_id],
                        )?;
                        auto_named += changed as usize;
                    }
                }
            }
            Err(_) => {
                // Artist path missing / outside roots / no usable profile:
                // leave the plans as-is for manual review.
            }
        }
    }
    Ok(auto_named)
}

/// Run automatic archive only after a successful full-library scan.
/// Returns an immediate summary for the caller; does not persist a last_run summary.
pub fn run_folder_rename_auto_after_full_scan(
    conn: &Connection,
    roots: &MediaRoots,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    purge_folder_rename_auto_last_run(conn)?;
    if !folder_rename_auto_enabled(conn)? {
        return Ok(json!({
            "ok": true,
            "status": "disabled",
            "scope": "full",
            "artist_id": Value::Null,
            "at": now(),
            "reason": "disabled",
            "backup": "",
            "executed_count": 0,
            "skipped_count": 0,
            "failed_count": 0,
            "actions": [],
            "skipped": [],
            "failed": [],
            "errors": []
        }));
    }
    let artists = conn
        .prepare("SELECT id FROM artists WHERE COALESCE(missing, 0)=0 ORDER BY id")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if artists.is_empty() {
        return Ok(json!({
            "ok": true,
            "status": "no_actions",
            "scope": "full",
            "artist_id": Value::Null,
            "at": now(),
            "reason": "no_artists",
            "backup": "",
            "executed_count": 0,
            "skipped_count": 0,
            "failed_count": 0,
            "actions": [],
            "skipped": [],
            "failed": [],
            "errors": []
        }));
    }
    // Auto-archive must not require a manual "更新整理项" click: generate the
    // target folder names for discovered draft plans before confirming them,
    // so a scan + auto execute becomes fully automatic. Plans with conflicts
    // (target exists, inconsistent tags, etc.) are skipped and stay manual.
    let auto_named = auto_name_artist_draft_plans(conn, roots, &artists)?;
    let mut executed_count = 0i64;
    let mut skipped_count = 0i64;
    let mut failed_count = 0i64;
    let mut actions = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let auto_confirmed = conn.execute(
        "UPDATE folder_rename_plans
         SET status='confirmed', confirmed_at=?, confirmation_source='auto', updated_at=?
         WHERE status='ready' AND target_folder != ''
           AND EXISTS (
             SELECT 1 FROM artists
             WHERE artists.id=folder_rename_plans.artist_id
               AND COALESCE(artists.missing, 0)=0
           )",
        params![now(), now()],
    )?;
    let mut executable = Vec::new();
    for artist_id in artists {
        let plans: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans
             WHERE artist_id=? AND status='confirmed' AND target_folder != ''",
            params![artist_id],
            |row| row.get(0),
        )?;
        if plans == 0 {
            skipped_count += 1;
            continue;
        }
        executable.push(artist_id);
    }
    let backup = if executable.is_empty() {
        String::new()
    } else {
        match create_db_backup(conn) {
            Ok(path) => path,
            Err(error) => {
                let error_text = error.to_string();
                let mut backup_failures = Vec::new();
                for artist_id in &executable {
                    let plans: Vec<(i64, String, String)> = conn
                        .prepare(
                            "SELECT id, source_folder, target_folder FROM folder_rename_plans
                             WHERE artist_id=? AND status='confirmed' AND target_folder != ''",
                        )?
                        .query_map(params![artist_id], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    for (plan_id, source, target) in plans {
                        record_plan_execution_failure(
                            conn,
                            plan_id,
                            "backup_failed",
                            &source,
                            &target,
                            Some(json!({"error": error_text})),
                        )?;
                        backup_failures.push(json!({
                            "artist_id": artist_id,
                            "plan_id": plan_id,
                            "status": "error",
                            "reason": "backup_failed",
                            "source": source,
                            "target": target,
                            "error": error_text,
                        }));
                    }
                }
                failed_count += backup_failures.len() as i64;
                errors.extend(backup_failures.iter().cloned());
                failed.extend(backup_failures);
                if skipped_count > 0 {
                    skipped.push(json!({
                        "reason": "no_confirmed_plans",
                        "count": skipped_count
                    }));
                }
                return Ok(json!({
                    "ok": true,
                    "status": "failed",
                    "scope": "full",
                    "artist_id": Value::Null,
                    "at": now(),
                    "backup": "",
                    "executed_count": executed_count,
                    "skipped_count": skipped_count,
                    "failed_count": failed_count,
                    "actions": actions,
                    "skipped": skipped,
                    "failed": failed,
                    "errors": errors
                }));
            }
        }
    };
    for artist_id in executable {
        match execute_folder_renames_with_backup(conn, roots, artist_id, false, Some(&backup)) {
            Ok(executed) => {
                if let Some(rows) = executed["results"].as_array() {
                    for row in rows {
                        if row["status"] == "executed" {
                            executed_count += 1;
                            actions.push(row.clone());
                        } else {
                            failed_count += 1;
                            failed.push(row.clone());
                            errors.push(row.clone());
                        }
                    }
                }
            }
            Err(error) => {
                failed_count += 1;
                let failure = json!({
                    "artist_id": artist_id,
                    "auto_confirmed": auto_confirmed,
                    "error": error.to_string()
                });
                failed.push(failure.clone());
                errors.push(failure);
            }
        }
    }
    if skipped_count > 0 {
        skipped.push(json!({
            "reason": "no_confirmed_plans",
            "count": skipped_count
        }));
    }
    let status = if failed_count > 0 && executed_count > 0 {
        "partial"
    } else if failed_count > 0 {
        "failed"
    } else if executed_count > 0 {
        "executed"
    } else if skipped_count > 0 {
        "skipped"
    } else {
        "no_actions"
    };
    Ok(json!({
        "ok": true,
        "status": status,
        "scope": "full",
        "artist_id": Value::Null,
        "at": now(),
        "backup": backup,
        "auto_named": auto_named,
        "executed_count": executed_count,
        "skipped_count": skipped_count,
        "failed_count": failed_count,
        "actions": actions,
        "skipped": skipped,
        "failed": failed,
        "errors": errors
    }))
}

pub fn create_db_backup(conn: &Connection) -> Result<String> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let root = PathBuf::from(data_dir).join("db-backups");
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

pub(crate) struct PlanPathCheck {
    pub artist_id: i64,
    pub source_folder: String,
    pub target_folder: String,
    pub source_exists: bool,
    pub target_exists: bool,
    pub reason: Option<String>,
}

pub(crate) fn check_plan_paths(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
) -> Result<PlanPathCheck> {
    let (artist_id, artist_path, source_raw, target_raw): (i64, String, String, String) = conn
        .query_row(
            "SELECT p.artist_id, a.path, p.source_folder, p.target_folder
             FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
             WHERE p.id=?",
            params![plan_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?
        .ok_or_else(|| anyhow!("plan not found"))?;
    let source = match validate_relative_folder(&source_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source_raw,
                target_folder: target_raw,
                source_exists: false,
                target_exists: false,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let target = match validate_relative_folder(&target_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source,
                target_folder: target_raw,
                source_exists: false,
                target_exists: false,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let artist_root = roots.map_to_real(&artist_path)?;
    let source_path = artist_root.join(&source);
    let target_path = artist_root.join(&target);
    let source_exists = source_path.is_dir();
    let target_exists = target_path.exists();
    let reason = if target == source || target.starts_with(&format!("{source}/")) {
        Some("target_inside_source".into())
    } else if !path_under_authorized_roots(&artist_root, roots) {
        Some("outside_artist".into())
    } else if !path_under_artist(&source_path, &artist_root)
        || !target_parent_under_artist(target_path.parent().unwrap_or(&target_path), &artist_root)
    {
        Some("outside_artist".into())
    } else if !source_exists {
        Some("source_missing".into())
    } else if target_exists {
        Some("target_exists".into())
    } else {
        None
    };
    Ok(PlanPathCheck {
        artist_id,
        source_folder: source,
        target_folder: target,
        source_exists,
        target_exists,
        reason,
    })
}

pub fn recheck_plan(conn: &Connection, roots: &MediaRoots, plan_id: i64) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let check = check_plan_paths(conn, roots, plan_id)?;
    Ok(json!({
        "id": plan_id,
        "artist_id": check.artist_id,
        "status": if check.reason.is_none() { "ready" } else { "blocked" },
        "source_folder": check.source_folder,
        "target_folder": check.target_folder,
        "source_exists": check.source_exists,
        "target_exists": check.target_exists,
        "error": check.reason,
        "rechecked": true
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn test_roots(root: &Path) -> MediaRoots {
        let root = root.to_string_lossy().into_owned();
        MediaRoots {
            roots: vec![root.clone()],
            labels: vec!["root".into()],
            real_paths: vec![root],
        }
    }

    fn create_archive_db(path: &Path, artist: &Path, with_items: bool) -> Connection {
        let conn = Connection::open(path).unwrap();
        conn.execute_batch("CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);")
            .unwrap();
        if with_items {
            conn.execute_batch(
                "CREATE TABLE items (
                    id INTEGER PRIMARY KEY,
                    artist_id INTEGER,
                    file_path TEXT,
                    file_name TEXT,
                    missing INTEGER DEFAULT 0
                );",
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO artists VALUES (1, 'artist', ?)",
            [artist.to_string_lossy().as_ref()],
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn
    }

    fn create_plan_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             INSERT INTO artists VALUES (1, 'one', '/one'), (2, 'two', '/two');
             INSERT INTO tags VALUES (1, 1, 'first'), (2, 1, 'second'), (3, 2, 'other');",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn upsert_is_atomic_and_rejects_execution_state_forgery() {
        let conn = create_plan_db();
        let plans = vec![
            json!({"source_folder": "one", "target_folder": "target", "status": "ready"}),
            json!({"source_folder": "two", "target_folder": "target", "status": "executed"}),
        ];

        assert!(upsert_folder_rename_plans(&conn, 1, &plans).is_err());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM folder_rename_plans", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn upsert_empty_source_rolls_back_the_batch() {
        let conn = create_plan_db();
        let plans = vec![
            json!({"source_folder": "valid", "status": "draft"}),
            json!({"source_folder": "", "status": "draft"}),
        ];

        assert!(upsert_folder_rename_plans(&conn, 1, &plans).is_err());
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM folder_rename_plans", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn auto_discover_filters_untagged_and_checks_tag_consistency() {
        let conn = create_plan_db();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY, artist_id INTEGER, folder_name TEXT, missing INTEGER DEFAULT 0, date TEXT);
             CREATE TABLE IF NOT EXISTS item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO items VALUES (1, 1, 'no_tags', 0, '2026-01-01'), (2, 1, 'no_tags', 0, '2026-01-01');
             INSERT INTO items VALUES (3, 1, 'inconsistent', 0, '2026-01-01'), (4, 1, 'inconsistent', 0, '2026-01-01');
             INSERT INTO item_tags VALUES (3, 1);
             INSERT INTO items VALUES (5, 1, 'consistent', 0, '2026-01-01'), (6, 1, 'consistent', 0, '2026-01-01');
             INSERT INTO item_tags VALUES (5, 1), (6, 1);"
        ).unwrap();

        auto_discover_artist_folder_plans(&conn, 1).unwrap();

        let no_tags_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans WHERE source_folder='no_tags'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(no_tags_count, 0);

        let inconsistent_status: String = conn.query_row(
            "SELECT status FROM folder_rename_plans WHERE source_folder='inconsistent'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(inconsistent_status, "inconsistent_tags");

        let consistent_status: (String, String) = conn.query_row(
            "SELECT status, selected_tag_ids FROM folder_rename_plans WHERE source_folder='consistent'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(consistent_status.0, "draft");
        assert_eq!(serde_json::from_str::<Value>(&consistent_status.1).unwrap(), json!([1]));
    }

    #[test]
    fn upsert_preserves_executed_plans_and_normalizes_tag_json() {
        let conn = create_plan_db();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (1, 'done', 'target', 'executed', '[9]')",
            [],
        )
        .unwrap();

        let result = upsert_folder_rename_plans(
            &conn,
            1,
            &[
                json!({"source_folder": "done", "target_folder": "changed", "status": "ready"}),
                json!({"source_folder": "new", "target_folder": "next", "status": "ready", "selected_tag_ids": "[1,2]"}),
            ],
        )
        .unwrap();

        assert_eq!(result["upserted"], 1);
        let executed: (String, String, String) = conn
            .query_row(
                "SELECT target_folder, status, selected_tag_ids
                 FROM folder_rename_plans WHERE source_folder='done'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(executed, ("target".into(), "executed".into(), "[9]".into()));
        let tags: String = conn
            .query_row(
                "SELECT selected_tag_ids FROM folder_rename_plans WHERE source_folder='new'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&tags).unwrap(), json!([1, 2]));
    }

    #[test]
    fn upsert_validates_and_deduplicates_selected_tags() {
        let conn = create_plan_db();
        let invalid_plans = [
            json!({"source_folder": "bad-shape", "selected_tag_ids": [1, "2"]}),
            json!({"source_folder": "bad-id", "selected_tag_ids": [0]}),
            json!({"source_folder": "missing", "selected_tag_ids": [99]}),
            json!({"source_folder": "cross-artist", "selected_tag_ids": [3]}),
        ];

        for plan in invalid_plans {
            assert!(upsert_folder_rename_plans(&conn, 1, &[plan]).is_err());
        }
        let result = upsert_folder_rename_plans(
            &conn,
            1,
            &[json!({"source_folder": "valid", "selected_tag_ids": [2, 1, 2]})],
        )
        .unwrap();

        assert_eq!(result["upserted"], 1);
        let tags: String = conn
            .query_row(
                "SELECT selected_tag_ids FROM folder_rename_plans WHERE source_folder='valid'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(serde_json::from_str::<Value>(&tags).unwrap(), json!([2, 1]));
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM folder_rename_plans", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 1);
    }

    #[test]
    fn backups_are_unique_and_publish_only_complete_directories() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let conn = Connection::open_in_memory().unwrap();
        conn.execute("CREATE TABLE sample (value TEXT)", [])
            .unwrap();
        conn.execute("INSERT INTO sample VALUES ('one')", [])
            .unwrap();

        let first = create_db_backup(&conn).unwrap();
        conn.execute("INSERT INTO sample VALUES ('two')", [])
            .unwrap();
        let second = create_db_backup(&conn).unwrap();

        assert_ne!(first, second);
        assert!(Path::new(&first).is_file());
        assert!(Path::new(&second).is_file());
        let backup_root = dir.path().join("db-backups");
        assert!(!std::fs::read_dir(backup_root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry.file_name().to_string_lossy().starts_with('.')));
    }

    #[test]
    fn backup_pruning_never_removes_active_temporary_directories() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("db-backups");
        let active = root.join(".20260813-000000.tmp-active");
        let old = root.join("20260812-000000");
        std::fs::create_dir_all(&active).unwrap();
        std::fs::create_dir_all(&old).unwrap();

        prune_backup_root(&root, 1).unwrap();

        assert!(active.is_dir());
        assert!(old.is_dir());
    }

    #[test]
    fn execute_rejects_artist_outside_authorized_roots() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("outside").join("artist");
        std::fs::create_dir_all(artist.join("source")).unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'source', 'target', 'confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let authorized = dir.path().join("authorized");
        std::fs::create_dir_all(&authorized).unwrap();

        let result = execute_folder_renames(&conn, &test_roots(&authorized), 1, false).unwrap();

        assert_eq!(result["ok"], false);
        assert_eq!(result["results"][0]["reason"], "outside_artist");
        assert!(artist.join("source").is_dir());
        assert!(!artist.join("target").exists());
    }

    #[test]
    fn execute_does_not_run_unconfirmed_ready_plans() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'source', 'target', 'ready')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let result = execute_folder_renames(&conn, &test_roots(dir.path()), 1, false).unwrap();

        assert_eq!(result["results"], json!([]));
        assert!(source.join("a.jpg").is_file());
        assert!(!artist.join("target").exists());
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "ready");
    }

    #[test]
    fn undo_restores_folder_paths_and_records_history() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let target = artist.join("2026").join("renamed");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        let target_path = target.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'a.jpg')",
            [format!("{target_path}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, executed_at, execution_log)
             VALUES (1, 'original', '2026/renamed', 'executed', 1, '[]')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let result = undo_folder_rename_plan(&conn, &test_roots(dir.path()), 1).unwrap();

        assert_eq!(result["status"], "reverted");
        assert_eq!(result["reason"], "folder_rename_undo");
        assert!(artist.join("original").join("a.jpg").is_file());
        assert!(!target.exists());
        let path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(path.ends_with("/artist/original/a.jpg"), "{path}");
        let plan: (String, Option<f64>, String) = conn
            .query_row(
                "SELECT status, executed_at, execution_log FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(plan.0, "reverted");
        assert!(plan.1.is_none());
        assert!(plan.2.contains("folder_rename_undo"));
    }

    #[test]
    fn undo_refuses_occupied_source_without_changing_target_or_db() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(artist.join("original")).unwrap();
        let target = artist.join("renamed");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, executed_at)
             VALUES (1, 'original', 'renamed', 'executed', 1)",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let error = undo_folder_rename_plan(&conn, &test_roots(dir.path()), 1).unwrap_err();

        assert!(error.to_string().contains("source_exists"));
        assert!(target.join("a.jpg").is_file());
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "executed");
    }

    #[test]
    fn undo_restores_target_when_database_update_fails() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let target = artist.join("renamed");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, false);
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, executed_at)
             VALUES (1, 'original', 'renamed', 'executed', 1)",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let error = undo_folder_rename_plan(&conn, &test_roots(dir.path()), 1).unwrap_err();

        assert!(error.to_string().contains("no such table: items"));
        assert!(target.join("a.jpg").is_file());
        assert!(!artist.join("original").exists());
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "executed");
    }

    #[test]
    fn undo_stale_plan_rolls_back_filesystem_and_database() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let target = artist.join("renamed");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        let target_path = target.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'a.jpg')",
            [format!("{target_path}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, executed_at)
             VALUES (1, 'original', 'renamed', 'executed', 1)",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER stale_plan_after_item_update AFTER UPDATE ON items
             BEGIN
               UPDATE folder_rename_plans SET execution_log='[\"external\"]' WHERE id=1;
             END;",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let error = undo_folder_rename_plan(&conn, &test_roots(dir.path()), 1).unwrap_err();

        assert!(error.to_string().contains("stale_state"));
        assert!(target.join("a.jpg").is_file());
        assert!(!artist.join("original").exists());
        let item_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(item_path, format!("{target_path}/a.jpg"));
        let plan: (String, String) = conn
            .query_row(
                "SELECT status, execution_log FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(plan, ("executed".into(), "[]".into()));
    }

    #[test]
    fn undo_rollback_failure_moves_plan_to_manual_review() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let target = artist.join("renamed");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        let target_path = target.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'a.jpg')",
            [format!("{target_path}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, executed_at, execution_log)
             VALUES (1, 'original', 'renamed', 'executed', 1,
                     '[{\"status\":\"executed\",\"reason\":\"folder_rename_execute\"}]')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER stale_plan_for_failed_undo AFTER UPDATE ON items
             BEGIN
               UPDATE folder_rename_plans SET execution_log='[\"external\"]' WHERE id=1;
             END;",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        FAIL_UNDO_ROLLBACK.store(true, std::sync::atomic::Ordering::SeqCst);

        let error = undo_folder_rename_plan(&conn, &test_roots(dir.path()), 1).unwrap_err();

        assert!(error.to_string().contains("reconciliation_required"));
        assert!(artist.join("original").join("a.jpg").is_file());
        assert!(!target.exists());
        let plan: (String, String) = conn
            .query_row(
                "SELECT status, execution_log FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(plan.0, "manual_review");
        let log: Value = serde_json::from_str(&plan.1).unwrap();
        assert_eq!(log.as_array().unwrap().len(), 2);
        assert_eq!(log[0]["status"], "executed");
        assert_eq!(log[1]["reason"], "rollback_failed");
        assert_eq!(log[1]["operation"], "folder_rename_undo");
        assert!(log[1]["rollback_error"]
            .as_str()
            .unwrap()
            .contains("forced undo rollback failure"));
    }

    #[test]
    fn execute_renames_folder_and_updates_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let src = artist.join("old");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.jpg"), b"x").unwrap();
        let db_path = dir.path().join("g.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists VALUES (1,'a',?)", params![ap])
            .unwrap();
        let fp = src.join("a.jpg").to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO items VALUES (1,1,?,'a.jpg',0)", params![fp])
            .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1,'old','nested/new','confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };
        let out = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(out["ok"], true);
        assert!(artist.join("nested").join("new").join("a.jpg").is_file());
        let new_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(new_path.contains("/nested/new/"));
    }

    #[test]
    fn execute_and_undo_map_legacy_virtual_artist_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("old");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("a.jpg"), b"x").unwrap();
        let db_path = dir.path().join("gallery.db");
        let conn = create_archive_db(&db_path, &artist, true);
        conn.execute("UPDATE artists SET path='/pictures1/artist' WHERE id=1", [])
            .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/artist/old/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'old', 'new', 'confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["virtual".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };

        let executed = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(executed["results"][0]["status"], "executed");
        assert!(artist.join("new").join("a.jpg").is_file());
        let moved_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(moved_path, "/pictures1/artist/new/a.jpg");

        let reverted = undo_folder_rename_plan(&conn, &roots, 1).unwrap();
        assert_eq!(reverted["status"], "reverted");
        assert!(artist.join("old").join("a.jpg").is_file());
        let restored_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(restored_path, "/pictures1/artist/old/a.jpg");
    }

    #[test]
    fn execute_stale_plan_rolls_back_without_overwriting_newer_state() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("old");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("a.jpg"), b"x").unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        let source_path = source.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'a.jpg')",
            [format!("{source_path}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, execution_log)
             VALUES (1, 'old', 'new', 'confirmed', '[]')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "CREATE TRIGGER stale_plan_after_execute_item_update AFTER UPDATE ON items
             BEGIN
               UPDATE folder_rename_plans
               SET status='draft', execution_log='[\"external\"]' WHERE id=1;
             END;",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let result = execute_folder_renames(&conn, &test_roots(dir.path()), 1, false).unwrap();

        assert_eq!(result["results"][0]["reason"], "stale_state");
        assert!(source.join("a.jpg").is_file());
        assert!(!artist.join("new").exists());
        let item_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(item_path, format!("{source_path}/a.jpg"));
        let plan: (String, String) = conn
            .query_row(
                "SELECT status, execution_log FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(plan, ("confirmed".into(), "[]".into()));
    }

    #[test]
    fn execution_outside_authorized_roots_moves_plan_to_manual_review() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists VALUES (1, 'a', ?)",
            ["/tmp/nonexistent-gallery-artist"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status, confirmed_at, confirmation_source)
             VALUES (1, 'missing', '2026/target', 'confirmed', 1, 'auto')",
            [],
        )
        .unwrap();

        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new(),
        };
        let result = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(result["ok"], false);
        let row = conn
            .query_row(
                "SELECT status, confirmed_at, confirmation_source, execution_log FROM folder_rename_plans",
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Option<f64>>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                },
            )
            .unwrap();
        assert_eq!(row.0, "manual_review");
        assert_eq!(row.1, None);
        assert_eq!(row.2, "");
        assert!(row.3.contains("outside_artist"));
    }

    #[test]
    fn recheck_reports_current_source_and_target_state() {
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(artist.join("source")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);")
            .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists VALUES (1, 'a', ?)",
            [artist.to_string_lossy().as_ref()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'source', 'target', 'manual_review')",
            [],
        )
        .unwrap();

        let roots = test_roots(dir.path());
        let result = recheck_plan(&conn, &roots, 1).unwrap();

        assert_eq!(result["status"], "ready");
        assert_eq!(result["source_exists"], true);
        assert_eq!(result["target_exists"], false);

        conn.execute(
            "UPDATE folder_rename_plans SET target_folder='source/nested' WHERE id=1",
            [],
        )
        .unwrap();
        let nested = recheck_plan(&conn, &roots, 1).unwrap();
        assert_eq!(nested["status"], "blocked");
        assert_eq!(nested["error"], "target_inside_source");
    }

    #[cfg(unix)]
    #[test]
    fn target_parent_rejects_symlink_escape() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let outside = dir.path().join("outside");
        std::fs::create_dir(&artist).unwrap();
        std::fs::create_dir(&outside).unwrap();
        symlink(&outside, artist.join("escape")).unwrap();

        assert!(!target_parent_under_artist(
            &artist.join("escape").join("nested"),
            &artist,
        ));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn prepared_rename_rejects_parent_replaced_by_symlink() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let outside = dir.path().join("outside");
        let source = artist.join("source");
        let target_parent = artist.join("target-parent");
        let detached_parent = artist.join("detached-parent");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target_parent).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(source.join("a.jpg"), b"x").unwrap();
        let roots = test_roots(dir.path());
        let prepared =
            prepare_artist_dir_rename(&artist, &roots, "source", "target-parent/renamed").unwrap();
        std::fs::rename(&target_parent, &detached_parent).unwrap();
        symlink(&outside, &target_parent).unwrap();

        let error = prepared.execute().unwrap_err();

        assert!(error.to_string().contains("symbolic link") || error.raw_os_error().is_some());
        assert!(source.join("a.jpg").is_file());
        assert!(!outside.join("renamed").exists());
        assert!(!detached_parent.join("renamed").exists());
    }

    #[test]
    fn rejects_traversal_folder() {
        assert!(validate_relative_folder("../etc").is_err());
        assert!(validate_relative_folder("/abs").is_err());
        assert!(validate_relative_folder("a/../b").is_err());
        assert!(validate_relative_folder("a/./b").is_err());
        assert!(validate_relative_folder("./b").is_err());
        assert_eq!(validate_relative_folder("2024/ok").unwrap(), "2024/ok");
    }

    #[test]
    fn migrates_legacy_auto_setting_to_canonical_key() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto', '1')",
            [],
        )
        .unwrap();
        assert!(folder_rename_auto_enabled(&conn).unwrap());
        let canonical: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto_enabled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "1");
        assert!(conn
            .query_row::<String, _, _>(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto'",
                [],
                |row| row.get(0)
            )
            .is_err());
    }

    #[test]
    fn auto_archive_returns_disabled_run_counts_without_summary_setting() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_last_run', '{\"legacy\":true}')",
            [],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new().clone(),
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["status"], "disabled");
        assert_eq!(result["scope"], "full");
        assert_eq!(result["executed_count"], 0);
        assert_eq!(result["skipped_count"], 0);
        assert_eq!(result["failed_count"], 0);
        for key in ["actions", "skipped", "failed", "errors"] {
            assert!(result[key].is_array(), "missing array: {key}");
        }
        assert!(conn
            .query_row::<String, _, _>(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto_last_run'",
                [],
                |row| row.get(0)
            )
            .is_err());
    }

    #[test]
    fn auto_archive_does_not_store_one_result_per_artist_without_plans() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_enabled', '1')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO artists (id, name, path) VALUES (1, 'a', '/a'), (2, 'b', '/b');",
        )
        .unwrap();
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new().clone(),
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["status"], "skipped");
        assert_eq!(result["skipped_count"], 2);
        assert!(result["results"].is_null());
        assert!(result["skipped"].as_array().unwrap().len() <= 1);
    }

    #[test]
    fn auto_archive_backup_failure_moves_confirmed_plans_to_manual_review() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(artist.join("source")).unwrap();
        let data_file = dir.path().join("data-file");
        std::fs::write(&data_file, b"not a directory").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_enabled', '1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'a', ?, 0)",
            [artist.to_string_lossy().as_ref()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'source', 'target', 'ready')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_file);
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new(),
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["status"], "failed");
        assert_eq!(result["failed_count"], 1);
        assert_eq!(result["failed"][0]["reason"], "backup_failed");
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "manual_review");
    }

    #[test]
    fn auto_archive_generates_target_for_draft_plan_and_executes() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("2026-01-05 测试");
        std::fs::create_dir_all(&source).unwrap();
        let tag_ids = json!([7]).to_string();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT DEFAULT '', missing INTEGER DEFAULT 0);",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_enabled', '1')",
            [],
        )
        .unwrap();
        let artist_path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'a', ?, 0)",
            [&artist_path],
        )
        .unwrap();
        conn.execute("INSERT INTO tags (id, name) VALUES (7, '测试')", [])
            .unwrap();
        let settings = json!({"version":1,"active_profile_id":"standard","profiles":[{"id":"standard","name":"Standard","template":"{year}/{date}-{tags}","collision_strategy":"suffix"}],"artist_profile_ids":{}});
        crate::archive_profiles::set_folder_rename_format_settings(&conn, &settings, None)
            .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, original_title, parsed_date, selected_tag_ids, status, file_count)
             VALUES (1, '2026-01-05 测试', '2026-01-05 测试', '2026-01-05', ?, 'draft', 2)",
            [&tag_ids],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["executed_count"], 1);
        assert_eq!(result["auto_named"], 1);
        let row: (String, String, String) = conn
            .query_row(
                "SELECT target_folder, status, confirmation_source FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert!(row.0.ends_with("2026-01-05-测试"), "target: {}", row.0);
        assert_eq!(row.1, "executed");
        assert_eq!(row.2, "auto");
        assert!(artist.join("2026/2026-01-05-测试").is_dir());
        assert!(!artist.join("2026-01-05 测试").exists());
    }

    #[test]
    fn auto_archive_keeps_conflicted_draft_plan_manual() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("2026-01-05 测试");
        let taken = artist.join("2026/2026-01-05-测试");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&taken).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT);",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_enabled', '1')",
            [],
        )
        .unwrap();
        let artist_path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'a', ?, 0)",
            [&artist_path],
        )
        .unwrap();
        conn.execute("INSERT INTO tags (id, name) VALUES (7, '测试')", [])
            .unwrap();
        let settings = json!({"version":1,"active_profile_id":"standard","profiles":[{"id":"standard","name":"Standard","template":"{year}/{date}-{tags}","collision_strategy":"reject"}],"artist_profile_ids":{}});
        crate::archive_profiles::set_folder_rename_format_settings(&conn, &settings, None)
            .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, original_title, parsed_date, selected_tag_ids, status, file_count)
             VALUES (1, '2026-01-05 测试', '2026-01-05 测试', '2026-01-05', '[7]', 'draft', 2)",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["executed_count"], 0);
        assert_eq!(result["auto_named"], 0);
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "draft");
    }

    #[test]
    fn execute_rejects_traversal_plan() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(&artist).unwrap();
        let db_path = dir.path().join("g.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists VALUES (1,'a',?)", params![ap])
            .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1,'../escape','new','confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };
        let out = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(out["results"][0]["reason"], "bad_folder_path");
    }
}
