//! Folder archive plan list + execute (pure Rust product path).

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::archive_format::{self, RenderContext};
use crate::archive_profiles;
use crate::media_roots::{path_under_authorized_roots, MediaRoots};
use crate::media_serve::{
    move_file_from_authorized_path_no_overwrite, move_file_to_authorized_path_no_overwrite,
};

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
        "stale_split_plan" => "文件日期、标签或路径已变化",
        "missing_split_actions" => "没有可执行的拆分文件",
        "execution_failed" => "执行失败",
        _ => "整理失败",
    }
}

/// Demote a plan with a failure log entry without forcing manual review;
/// used when a confirmed target no longer matches the current item dates.
fn demote_plan_with_log(
    conn: &Connection,
    plan_id: i64,
    status: &str,
    target: &str,
    reason: &str,
    source: &str,
) -> Result<()> {
    let entry = json!({
        "at": now(),
        "status": "failed",
        "reason": reason,
        "message": archive_failure_message(reason),
        "source": source,
        "target": target,
        "automatic": true,
    });
    conn.execute(
        "UPDATE folder_rename_plans
         SET status=?, target_folder=?, confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=?",
        params![status, target, json!([entry]).to_string(), now(), plan_id],
    )?;
    Ok(())
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

/// Clear stale confirmation on unexecuted plans whose source folder items had
/// their effective date changed by a manual date batch. Confirmed plans are
/// demoted to `ready` and their stale target cleared so neither the manual
/// apply flow nor auto-run can execute a target that no longer matches the
/// item dates; the fixed target builder re-derives targets on next preview.
pub fn invalidate_plans_after_item_date_change(
    conn: &Connection,
    artist_id: i64,
    folders: &[String],
) -> Result<i64> {
    if folders.is_empty() {
        return Ok(0);
    }
    let placeholders = std::iter::repeat("?")
        .take(folders.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut params: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Real(now()),
        rusqlite::types::Value::Integer(artist_id),
    ];
    params.extend(
        folders
            .iter()
            .map(|folder| rusqlite::types::Value::Text(folder.clone())),
    );
    let affected = conn.execute(
        &format!(
            "UPDATE folder_rename_plans
             SET confirmed_at=NULL, confirmation_source='',
                 status=CASE WHEN status='confirmed' THEN 'ready' ELSE status END,
                 target_folder=CASE WHEN status='confirmed' THEN '' ELSE target_folder END,
                 updated_at=?
             WHERE artist_id=? AND source_folder IN ({placeholders})
               AND status NOT IN ('executed','reverted')"
        ),
        rusqlite::params_from_iter(params.iter()),
    )?;
    Ok(affected as i64)
}

/// Effective raw date of one active item: manual override, else detected date,
/// else the legacy canonical date. Mirrors `effective_display_date`.
fn item_effective_date(
    manual_date: Option<&str>,
    detected_date: &str,
    legacy_date: &str,
) -> String {
    manual_date
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if detected_date.is_empty() {
                None
            } else {
                Some(detected_date.to_string())
            }
        })
        .unwrap_or_else(|| legacy_date.to_string())
}

/// `YYYY-MM-DD` or `YYYY-MM` key from a manual/recognized effective date.
/// A full day is preserved whenever the source carries one.
fn effective_date_key(raw: &str) -> Option<String> {
    let digits = raw
        .trim()
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() >= 8 {
        Some(format!(
            "{}-{}-{}",
            &digits[..4],
            &digits[4..6],
            &digits[6..8]
        ))
    } else if digits.len() == 6 {
        Some(format!("{}-{}", &digits[..4], &digits[4..]))
    } else {
        None
    }
}

/// Distinct effective dates of the active items in one source folder.
fn effective_item_dates(conn: &Connection, artist_id: i64, folder: &str) -> Result<Vec<String>> {
    let artist_path = artist_plan_path(conn, artist_id)?;
    let mut dates = Vec::new();
    let mut stmt = conn.prepare(
        "SELECT file_path, folder_name, manual_date, detected_date, date FROM items
         WHERE artist_id=? AND COALESCE(missing, 0)=0",
    )?;
    let rows = stmt.query_map(params![artist_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (file_path, folder_name, manual_date, detected_date, legacy_date) = row?;
        if source_folder_for_item(&artist_path, &file_path, &folder_name).as_deref() != Some(folder)
        {
            continue;
        }
        let raw = item_effective_date(manual_date.as_deref(), &detected_date, &legacy_date);
        if let Some(date) = effective_date_key(&raw) {
            if !dates.contains(&date) {
                dates.push(date);
            }
        }
    }
    Ok(dates)
}

/// Tag names for a plan: the stored `selected_tag_ids` order when present,
/// otherwise the distinct artist folder tag names ordered by name.
fn plan_tag_names(
    conn: &Connection,
    artist_id: i64,
    folder: &str,
    selected_json: &str,
) -> Result<Vec<String>> {
    let ids: Vec<i64> = serde_json::from_str(selected_json).unwrap_or_default();
    if !ids.is_empty() {
        let mut names = Vec::with_capacity(ids.len());
        let mut stmt = conn.prepare("SELECT name FROM tags WHERE id=? AND artist_id=?")?;
        for id in ids {
            let name: Option<String> = stmt
                .query_row(params![id, artist_id], |row| row.get(0))
                .optional()?;
            if let Some(name) = name {
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
        return Ok(names);
    }
    let artist_path = artist_plan_path(conn, artist_id)?;
    let mut names = BTreeSet::new();
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='item_tags')",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT i.file_path, i.folder_name, t.name FROM items i
         JOIN item_tags it ON it.item_id=i.id
         JOIN tags t ON t.id=it.tag_id
         WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
         ORDER BY t.name",
    )?;
    let rows = stmt.query_map(params![artist_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (file_path, folder_name, name) = row?;
        if source_folder_for_item(&artist_path, &file_path, &folder_name).as_deref() == Some(folder)
        {
            names.insert(name);
        }
    }
    Ok(names.into_iter().collect())
}

fn is_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .and_then(|number| number.parse::<u8>().ok())
            .map(|number| (1..=9).contains(&number))
            .unwrap_or(false)
}

/// Validate a fixed archive target as a portable artist-relative folder path.
fn validate_fixed_target(value: &str) -> Result<String> {
    let raw = value.replace('\\', "/").trim().to_string();
    if raw.is_empty() || raw.starts_with('/') || raw.starts_with("//") {
        bail!("fixed archive target must be an artist-relative folder");
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        bail!("fixed archive target must not use a drive path");
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() {
            bail!("fixed archive target cannot contain empty path segments");
        }
        if component == "." || component == ".." {
            bail!("fixed archive target cannot contain traversal segments");
        }
        if component.chars().count() > 180 {
            bail!("fixed archive target has an overlong path segment");
        }
        if component.ends_with(' ') || component.ends_with('.') {
            bail!("fixed archive target has an unsafe trailing path character");
        }
        if component
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            bail!("fixed archive target contains unsafe filename characters");
        }
        if is_reserved_component(component) {
            bail!("fixed archive target contains a reserved filename");
        }
        components.push(component);
    }
    if components.is_empty() {
        bail!("fixed archive target must not be empty");
    }
    Ok(components.join("/"))
}

/// The item's parent directory relative to the artist root. Older databases
/// only have the final path component in `folder_name`, so use it solely when
/// the stored file path cannot be safely made relative to the artist path.
fn source_folder_for_item(artist_path: &str, file_path: &str, folder_name: &str) -> Option<String> {
    let artist_root = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let file_path = file_path.replace('\\', "/");
    let from_path = if artist_root.is_empty() {
        None
    } else {
        file_path
            .strip_prefix(&format!("{artist_root}/"))
            .and_then(|relative| relative.rsplit_once('/').map(|(folder, _)| folder))
            .and_then(|folder| validate_relative_folder(folder).ok())
    };
    from_path.or_else(|| validate_relative_folder(folder_name).ok())
}

fn artist_plan_path(conn: &Connection, artist_id: i64) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

fn archive_profile_for_artist(conn: &Connection, artist_id: i64) -> Result<(Value, String)> {
    let settings = archive_profiles::load(conn)?;
    archive_format::profile_for_artist(&settings, artist_id)
}

fn suffixed_archive_target(target: &str, number: usize) -> String {
    match target.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/{name} ({number})"),
        None => format!("{target} ({number})"),
    }
}

fn is_suffixed_archive_target(target: &str, requested_target: &str) -> bool {
    target
        .strip_prefix(requested_target)
        .and_then(|suffix| suffix.strip_prefix(" ("))
        .and_then(|suffix| suffix.strip_suffix(')'))
        .and_then(|number| number.parse::<usize>().ok())
        .is_some_and(|number| number >= 2)
}

fn render_archive_target(
    profile: &Value,
    artist: &str,
    date: &str,
    tags: &[String],
    title: &str,
    folder: &str,
    index: usize,
) -> Result<String> {
    Ok(archive_format::render_profile(
        profile,
        &RenderContext {
            artist: artist.to_string(),
            date: date.to_string(),
            tags: tags.to_vec(),
            title: title.to_string(),
            folder: folder.to_string(),
            index,
        },
    )?
    .target_folder)
}

#[derive(Clone, Debug)]
struct DiscoveredFolderItem {
    id: i64,
    file_path: String,
    file_name: String,
    manual_date: Option<String>,
    detected_date: String,
    legacy_date: String,
    tags: Vec<i64>,
}

#[derive(Clone, Debug)]
struct SplitFileMove {
    item_id: i64,
    source_db: String,
    target_db: String,
    source: PathBuf,
    target: PathBuf,
    target_folder: String,
}

fn tag_names_for_ids(conn: &Connection, artist_id: i64, tag_ids: &[i64]) -> Result<Vec<String>> {
    let mut names = Vec::with_capacity(tag_ids.len());
    let mut stmt = conn.prepare("SELECT name FROM tags WHERE id=? AND artist_id=?")?;
    for tag_id in tag_ids {
        if let Some(name) = stmt
            .query_row(params![tag_id, artist_id], |row| row.get::<_, String>(0))
            .optional()?
            .filter(|name| !name.is_empty())
        {
            names.push(name);
        }
    }
    Ok(names)
}

fn build_split_actions(
    conn: &Connection,
    artist_id: i64,
    artist: &str,
    profile: &Value,
    source_folder: &str,
    items: &[DiscoveredFolderItem],
) -> Result<(String, bool)> {
    let mut actions = Vec::new();
    let mut missing_date = false;
    for (index, item) in items
        .iter()
        .filter(|item| !item.tags.is_empty())
        .enumerate()
    {
        let raw_date = item_effective_date(
            item.manual_date.as_deref(),
            &item.detected_date,
            &item.legacy_date,
        );
        let Some(_date) = effective_date_key(&raw_date) else {
            missing_date = true;
            continue;
        };
        let tag_names = tag_names_for_ids(conn, artist_id, &item.tags)?;
        if tag_names.is_empty() {
            continue;
        }
        actions.push(json!({
            "item_id": item.id,
            "source_file_path": item.file_path,
            "source_relative_path": item.file_name,
            "target_folder": render_archive_target(
                profile,
                artist,
                &raw_date,
                &tag_names,
                &item.file_name,
                source_folder,
                index + 1,
            )?,
            "target_relative_path": item.file_name,
            "format_index": index + 1,
        }));
    }
    actions.sort_by(|left, right| {
        left["target_folder"]
            .as_str()
            .cmp(&right["target_folder"].as_str())
            .then_with(|| {
                left["source_relative_path"]
                    .as_str()
                    .cmp(&right["source_relative_path"].as_str())
            })
    });
    Ok((Value::Array(actions).to_string(), missing_date))
}

fn split_actions_complete(
    conn: &Connection,
    artist_id: i64,
    source_folder: &str,
    split_actions: &str,
) -> Result<bool> {
    let artist_path = artist_plan_path(conn, artist_id)?;
    let action_ids = serde_json::from_str::<Value>(split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| action["item_id"].as_i64())
        .collect::<HashSet<_>>();
    if action_ids.is_empty() {
        return Ok(false);
    }
    let tagged_ids = conn
        .prepare(
            "SELECT i.id, i.file_path, i.folder_name FROM items i
             WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
               AND EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id=i.id)",
        )?
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|row| match row {
            Ok((id, file_path, folder_name))
                if source_folder_for_item(&artist_path, &file_path, &folder_name).as_deref()
                    == Some(source_folder) =>
            {
                Some(Ok(id))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(action_ids == tagged_ids)
}

fn split_failure_reason(error: &anyhow::Error) -> &str {
    let message = error.to_string();
    [
        "backup_failed",
        "source_missing",
        "target_exists",
        "bad_folder_path",
        "db_update_failed",
        "rollback_failed",
        "outside_artist",
        "stale_split_plan",
        "missing_split_actions",
    ]
    .into_iter()
    .find(|reason| message.starts_with(reason))
    .unwrap_or("execution_failed")
}

fn prepare_split_file_moves(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    artist_path: &str,
    source_folder: &str,
    split_actions: &str,
) -> Result<Vec<SplitFileMove>> {
    let raw_actions = serde_json::from_str::<Value>(split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if raw_actions.is_empty() {
        bail!("missing_split_actions");
    }
    let artist_root = roots.map_to_real(artist_path)?;
    if !artist_root.is_dir() || !path_under_authorized_roots(&artist_root, roots) {
        bail!("outside_artist");
    }
    let source_folder =
        validate_relative_folder(source_folder).map_err(|_| anyhow!("bad_folder_path"))?;
    let artist: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("stale_split_plan"))?;
    let (profile, _) =
        archive_profile_for_artist(conn, artist_id).map_err(|_| anyhow!("stale_split_plan"))?;
    let mut targets = HashSet::new();
    let mut moves = Vec::with_capacity(raw_actions.len());
    for raw in raw_actions {
        let item_id = raw["item_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("stale_split_plan"))?;
        let target_folder_raw = raw["target_folder"]
            .as_str()
            .ok_or_else(|| anyhow!("bad_folder_path"))?;
        let target_folder =
            validate_fixed_target(target_folder_raw).map_err(|_| anyhow!("bad_folder_path"))?;
        let (source_db, file_name, folder_name, manual_date, detected_date, legacy_date): (
            String,
            String,
            String,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT file_path, file_name, folder_name, manual_date, detected_date, date
                 FROM items
                 WHERE id=? AND artist_id=? AND COALESCE(missing, 0)=0",
                params![item_id, artist_id],
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
            .optional()?
            .ok_or_else(|| anyhow!("source_missing"))?;
        if source_folder_for_item(artist_path, &source_db, &folder_name).as_deref()
            != Some(source_folder.as_str())
        {
            bail!("stale_split_plan");
        }
        if raw["source_file_path"]
            .as_str()
            .is_some_and(|saved| saved != source_db)
        {
            bail!("stale_split_plan");
        }
        let target_relative_raw = raw["target_relative_path"].as_str().unwrap_or(&file_name);
        let target_relative =
            validate_fixed_target(target_relative_raw).map_err(|_| anyhow!("bad_folder_path"))?;
        let raw_date = item_effective_date(manual_date.as_deref(), &detected_date, &legacy_date);
        effective_date_key(&raw_date).ok_or_else(|| anyhow!("stale_split_plan"))?;
        let tag_ids = conn
            .prepare("SELECT tag_id FROM item_tags WHERE item_id=? ORDER BY tag_id")?
            .query_map(params![item_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tag_names = tag_names_for_ids(conn, artist_id, &tag_ids)?;
        let format_index = raw["format_index"].as_u64().unwrap_or(1) as usize;
        if tag_names.is_empty()
            || render_archive_target(
                &profile,
                &artist,
                &raw_date,
                &tag_names,
                &file_name,
                &source_folder,
                format_index,
            )? != target_folder
        {
            bail!("stale_split_plan");
        }
        let source = roots.map_to_real(&source_db)?;
        let target_db = PathBuf::from(artist_path)
            .join(&target_folder)
            .join(&target_relative)
            .to_string_lossy()
            .replace('\\', "/");
        let target = roots.map_to_real(&target_db)?;
        if !source.is_file() {
            bail!("source_missing");
        }
        if !path_under_artist(&source, &artist_root)
            || !target_parent_under_artist(target.parent().unwrap_or(&target), &artist_root)
        {
            bail!("outside_artist");
        }
        if source == target || target.exists() {
            bail!("target_exists");
        }
        let target_real = target.to_string_lossy().replace('\\', "/");
        let occupied: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM items
               WHERE id!=? AND COALESCE(missing, 0)=0 AND file_path IN (?, ?)
             )",
            params![item_id, target_db, target_real],
            |row| row.get(0),
        )?;
        if occupied || !targets.insert(target_real) {
            bail!("target_exists");
        }
        moves.push(SplitFileMove {
            item_id,
            source_db,
            target_db,
            source,
            target,
            target_folder,
        });
    }
    Ok(moves)
}

fn remove_empty_parents(paths: impl IntoIterator<Item = PathBuf>, artist_root: &Path) {
    let mut dirs = paths
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    dirs.dedup();
    for mut dir in dirs {
        while dir != artist_root && dir.starts_with(artist_root) {
            if std::fs::remove_dir(&dir).is_err() {
                break;
            }
            let Some(parent) = dir.parent() else {
                break;
            };
            dir = parent.to_path_buf();
        }
    }
}

fn rollback_split_moves(moved: &[SplitFileMove], roots: &MediaRoots) -> Result<()> {
    let mut failures = Vec::new();
    for file in moved.iter().rev() {
        if let Err(error) =
            move_file_from_authorized_path_no_overwrite(&file.target, &file.source, roots)
        {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("rollback_failed: {}", failures.join("; "))
    }
}

fn execute_split_plan(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
    artist_id: i64,
    artist_path: &str,
    source_folder: &str,
    split_actions: &str,
    backup_path: &str,
) -> Result<Value> {
    let artist_root = roots.map_to_real(artist_path)?;
    let moves = prepare_split_file_moves(
        conn,
        roots,
        artist_id,
        artist_path,
        source_folder,
        split_actions,
    )?;
    let mut moved = Vec::with_capacity(moves.len());
    for file in &moves {
        if let Err(error) =
            move_file_to_authorized_path_no_overwrite(&file.source, &file.target, roots)
        {
            if let Err(rollback_error) = rollback_split_moves(&moved, roots) {
                bail!("rollback_failed: {error}; {rollback_error}");
            }
            remove_empty_parents(moved.iter().map(|file| file.target.clone()), &artist_root);
            bail!("execution_failed: {error}");
        }
        moved.push(file.clone());
    }

    let db_result = (|| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        for file in &moves {
            let folder_name = Path::new(&file.target_folder)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("bad_folder_path"))?;
            let changed = tx.execute(
                "UPDATE items SET file_path=?, folder_name=?
                 WHERE id=? AND artist_id=? AND file_path=? AND COALESCE(missing, 0)=0",
                params![
                    file.target_db,
                    folder_name,
                    file.item_id,
                    artist_id,
                    file.source_db,
                ],
            )?;
            if changed != 1 {
                bail!("stale_state");
            }
        }
        let targets = moves
            .iter()
            .map(|file| file.target_folder.clone())
            .collect::<BTreeSet<_>>();
        let files = moves
            .iter()
            .map(|file| {
                json!({
                    "item_id": file.item_id,
                    "source": file.source_db,
                    "target": file.target_db,
                })
            })
            .collect::<Vec<_>>();
        let log = json!([{
            "at": now(),
            "status": "executed",
            "kind": "split_by_tag",
            "source": source_folder,
            "targets": targets,
            "files": files,
            "backup": backup_path,
            "updated_items": moves.len(),
        }]);
        let changed = tx.execute(
            "UPDATE folder_rename_plans
             SET status='executed', executed_at=?, execution_log=?, updated_at=?
             WHERE id=? AND status='confirmed' AND plan_kind='split_by_tag'
               AND source_folder=? AND split_actions=?",
            params![
                now(),
                log.to_string(),
                now(),
                plan_id,
                source_folder,
                split_actions,
            ],
        )?;
        if changed != 1 {
            bail!("stale_state");
        }
        tx.commit()?;
        Ok(moves.len() as i64)
    })();
    let updated_items = match db_result {
        Ok(updated) => updated,
        Err(error) => {
            rollback_split_moves(&moved, roots)
                .with_context(|| format!("rollback_failed: database update failed: {error}"))?;
            remove_empty_parents(moved.iter().map(|file| file.target.clone()), &artist_root);
            bail!("db_update_failed: {error}");
        }
    };
    remove_empty_parents(moves.iter().map(|file| file.source.clone()), &artist_root);
    Ok(json!({
        "plan_id": plan_id,
        "status": "executed",
        "kind": "split_by_tag",
        "source": source_folder,
        "targets": moves
            .iter()
            .map(|file| file.target_folder.clone())
            .collect::<BTreeSet<_>>(),
        "updated_items": updated_items,
    }))
}

/// Recompute the fixed target and status of every unexecuted plan of one
/// artist from the current effective item dates. Plans without any effective
/// date become `needs_date`; plans whose active items span several dates
/// become `date_conflict`; a folder already at its derived target becomes
/// `aligned`; source-and-target-missing legacy plans become hidden `stale`
/// records; other physical conflicts become `manual_review`. Confirmed,
/// executed and reverted plans are left untouched; date edits already demoted
/// stale confirmations via `invalidate_plans_after_item_date_change`.
pub fn recompute_artist_plan_targets(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    artist_id: i64,
) -> Result<usize> {
    ensure_folder_schema(conn)?;
    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?;
    let artist_path = artist_path.unwrap_or_default();
    let artist_name: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let (archive_profile, profile_source) = archive_profile_for_artist(conn, artist_id)?;
    let format_snapshot =
        archive_format::rule_snapshot(&archive_profile, &profile_source).to_string();
    let artist_root = roots.and_then(|roots| {
        roots
            .map_to_real(&artist_path)
            .ok()
            .filter(|root| path_under_authorized_roots(root, roots))
    });
    let suffix_collisions = archive_profile["collision_strategy"].as_str() == Some("suffix");
    let mut seen_targets = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT id, source_folder, original_title, selected_tag_ids, status, plan_kind, split_actions
         FROM folder_rename_plans
         WHERE artist_id=? AND status NOT IN ('confirmed','executed','reverted')
         ORDER BY id",
    )?;
    let plans = stmt
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut changed = 0usize;
    for (
        plan_id,
        source_folder,
        original_title,
        selected_json,
        _status,
        plan_kind,
        split_actions,
    ) in plans
    {
        if plan_kind == "split_by_tag" {
            let complete = split_actions_complete(conn, artist_id, &source_folder, &split_actions)?;
            let new_status = if !complete {
                "needs_date"
            } else if let Some(roots) = roots {
                match prepare_split_file_moves(
                    conn,
                    roots,
                    artist_id,
                    &artist_path,
                    &source_folder,
                    &split_actions,
                ) {
                    Ok(_) => "ready",
                    Err(_) => "manual_review",
                }
            } else {
                "ready"
            };
            changed += conn.execute(
                "UPDATE folder_rename_plans
                 SET target_folder='', status=?, confirmed_at=NULL, confirmation_source='',
                     format_snapshot=?, updated_at=?
                 WHERE id=? AND status NOT IN ('confirmed','executed','reverted')",
                params![new_status, format_snapshot, now(), plan_id],
            )? as usize;
            continue;
        }
        let dates = effective_item_dates(conn, artist_id, &source_folder)?;
        let mut target = if dates.is_empty() || dates.len() > 1 {
            String::new()
        } else {
            let tags = plan_tag_names(conn, artist_id, &source_folder, &selected_json)?;
            render_archive_target(
                &archive_profile,
                &artist_name,
                &dates[0],
                &tags,
                &original_title,
                &source_folder,
                plan_id as usize,
            )?
        };
        if suffix_collisions && !target.is_empty() && target != source_folder {
            let requested_target = target.clone();
            let mut number = 2usize;
            while seen_targets.contains(&target.to_ascii_lowercase())
                || artist_root
                    .as_ref()
                    .is_some_and(|root| root.join(&target).exists())
            {
                target = suffixed_archive_target(&requested_target, number);
                number += 1;
            }
        }
        if !target.is_empty() {
            seen_targets.insert(target.to_ascii_lowercase());
        }
        let new_status = if dates.is_empty() {
            "needs_date"
        } else if dates.len() > 1 {
            "date_conflict"
        } else if let Some(roots) = roots {
            if target.is_empty() {
                "needs_date"
            } else {
                match evaluate_plan_paths(roots, artist_id, &artist_path, &source_folder, &target) {
                    Ok(check) if source_folder == target && check.source_exists => "aligned",
                    Ok(check)
                        if !check.source_exists
                            && !check.target_exists
                            && roots
                                .map_to_real(&artist_path)
                                .ok()
                                .is_some_and(|root| path_under_authorized_roots(&root, roots)) =>
                    {
                        "stale"
                    }
                    Ok(check) if check.reason.is_some() => "manual_review",
                    Ok(_) => "ready",
                    Err(_) => "manual_review",
                }
            }
        } else if source_folder == target {
            "aligned"
        } else {
            "ready"
        };
        let n = conn.execute(
            "UPDATE folder_rename_plans
             SET target_folder=?, status=?, confirmed_at=NULL, confirmation_source='',
                 format_snapshot=?, updated_at=?
             WHERE id=? AND status NOT IN ('confirmed','executed','reverted')",
            params![target, new_status, format_snapshot, now(), plan_id],
        )?;
        changed += n as usize;
    }
    Ok(changed)
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
    let tag_tables_ready: bool = conn.query_row(
        "SELECT COUNT(*)=2 FROM sqlite_master
         WHERE type='table' AND name IN ('tags','item_tags')",
        [],
        |row| row.get(0),
    )?;
    if !tag_tables_ready {
        return Ok(());
    }
    let artist_path = artist_plan_path(conn, artist_id)?;
    let artist_name: String = conn.query_row(
        "SELECT name FROM artists WHERE id=?",
        params![artist_id],
        |row| row.get(0),
    )?;
    let (archive_profile, _) = archive_profile_for_artist(conn, artist_id)?;
    let mut stmt = conn.prepare(
        "SELECT i.file_path, i.folder_name, i.id, i.file_name, i.manual_date,
                COALESCE(i.detected_date, ''), COALESCE(i.date, ''), (
             SELECT json_group_array(it.tag_id)
             FROM (SELECT it.tag_id FROM item_tags it WHERE it.item_id=i.id ORDER BY it.tag_id) it
         )
         FROM items i
         WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
         ORDER BY i.id",
    )?;
    let discovered_items: Vec<(String, DiscoveredFolderItem)> = stmt
        .query_map(params![artist_id], |row| {
            let raw: Option<String> = row.get(7)?;
            let tags: Vec<i64> = raw
                .and_then(|value| serde_json::from_str::<Vec<i64>>(&value).ok())
                .unwrap_or_default();
            Ok((
                row.get(1)?,
                DiscoveredFolderItem {
                    id: row.get(2)?,
                    file_path: row.get(0)?,
                    file_name: row.get(3)?,
                    manual_date: row.get(4)?,
                    detected_date: row.get(5)?,
                    legacy_date: row.get(6)?,
                    tags,
                },
            ))
        })?
        .filter_map(|r| r.ok())
        .collect();
    let mut folders = BTreeMap::<String, (String, Vec<DiscoveredFolderItem>)>::new();
    for (folder_name, item) in discovered_items {
        let Some(folder) = source_folder_for_item(&artist_path, &item.file_path, &folder_name)
        else {
            continue;
        };
        let entry = folders
            .entry(folder)
            .or_insert_with(|| (item.legacy_date.clone(), Vec::new()));
        if item.legacy_date < entry.0 {
            entry.0 = item.legacy_date.clone();
        }
        entry.1.push(item);
    }

    for folder in folders.keys().filter(|folder| folder.contains('/')) {
        let legacy_folder = folder.rsplit('/').next().unwrap_or(folder);
        conn.execute(
            "DELETE FROM folder_rename_plans
             WHERE artist_id=? AND source_folder=?
               AND status NOT IN ('confirmed', 'executed', 'reverted')",
            params![artist_id, legacy_folder],
        )?;
    }

    for (folder, (min_date, items)) in folders {
        if folder.trim().is_empty() {
            continue;
        }
        let mut parsed_date = crate::media_type::extract_date_from_folder(&folder);
        if parsed_date.is_empty() && min_date.len() >= 10 && !min_date.starts_with("0000") {
            parsed_date = min_date[..10].to_string();
        }

        let total_items = items.len();
        if total_items == 0 {
            continue;
        }

        let has_any_tags = items.iter().any(|item| !item.tags.is_empty());
        if !has_any_tags {
            // Folders with no tags must not appear in the pending organize list.
            let _ = conn.execute(
                "DELETE FROM folder_rename_plans WHERE artist_id=? AND source_folder=? AND status NOT IN ('confirmed', 'executed')",
                params![artist_id, folder],
            );
            continue;
        }

        let mut groups = BTreeSet::new();
        let mut union_set = BTreeSet::new();
        for item in items.iter().filter(|item| !item.tags.is_empty()) {
            let raw_date = item_effective_date(
                item.manual_date.as_deref(),
                &item.detected_date,
                &item.legacy_date,
            );
            groups.insert((effective_date_key(&raw_date), item.tags.clone()));
            for tag_id in &item.tags {
                union_set.insert(*tag_id);
            }
        }
        let split_needed = items.iter().any(|item| item.tags.is_empty()) || groups.len() > 1;
        let plan_kind = if split_needed {
            "split_by_tag"
        } else {
            "rename_folder"
        };
        let selected_tag_ids = union_set.into_iter().collect::<Vec<_>>();
        let selected_tag_ids_json = serde_json::to_string(&selected_tag_ids)?;
        let (split_actions, split_missing_date) = if split_needed {
            build_split_actions(
                conn,
                artist_id,
                &artist_name,
                &archive_profile,
                &folder,
                &items,
            )?
        } else {
            ("[]".to_string(), false)
        };
        let initial_status = if split_missing_date {
            "needs_date"
        } else {
            "draft"
        };

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
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, '', '[]', ?, ?, '', ?)",
                    params![
                        artist_id,
                        folder,
                        folder,
                        folder,
                        parsed_date,
                        selected_tag_ids_json,
                        initial_status,
                        total_items as i64,
                        plan_kind,
                        split_actions,
                        now()
                    ],
                );
            }
            Some((plan_id, status, old_date, _old_tags, _old_count, _old_target)) => {
                if status != "confirmed" && status != "executed" {
                    let final_date = if old_date.is_empty() {
                        parsed_date
                    } else {
                        old_date
                    };
                    let _ = conn.execute(
                        "UPDATE folder_rename_plans
                         SET selected_tag_ids=?, parsed_date=?, file_count=?, status=?, target_folder='',
                             plan_kind=?, split_actions=?, updated_at=?
                         WHERE id=?",
                        params![
                            selected_tag_ids_json,
                            final_date,
                            total_items as i64,
                            initial_status,
                            plan_kind,
                            split_actions,
                            now(),
                            plan_id,
                        ],
                    );
                }
            }
        }
    }
    Ok(())
}

pub fn list_folder_renames(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    artist_id: Option<i64>,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    if let Some(aid) = artist_id {
        let _ = auto_discover_artist_folder_plans(conn, aid);
        let _ = recompute_artist_plan_targets(conn, roots, aid);
    }
    let mut sql = String::from(
        "SELECT id, artist_id, source_folder, target_folder, status, plan_kind, file_count,
                selected_tag_ids, parsed_date, execution_log, confirmed_at, executed_at,
                split_actions
         FROM folder_rename_plans",
    );
    let mut plans = Vec::new();
    if let Some(aid) = artist_id {
        sql.push_str(" WHERE artist_id=? AND status!='stale' ORDER BY id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![aid], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    } else {
        sql.push_str(" WHERE status!='stale' ORDER BY id DESC LIMIT 500");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    }
    Ok(json!({"plans": plans, "total": plans.len()}))
}

fn map_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let split_actions = r.get::<_, String>(12)?;
    let target_folders = serde_json::from_str::<Value>(&split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| action["target_folder"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>();
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
        "target_folders": target_folders,
        "split_action_count": serde_json::from_str::<Value>(&split_actions)
            .ok()
            .and_then(|value| value.as_array().map(Vec::len))
            .unwrap_or(0),
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
        if !matches!(
            status,
            "draft" | "needs_tags" | "ready" | "manual_review" | "inconsistent_tags"
        ) {
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
    let artist_name: String = conn.query_row(
        "SELECT name FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let artist_root = roots.map_to_real(&artist_path)?;
    let plans: Vec<(i64, String, String, String, String)> = conn
        .prepare(
            "SELECT id, source_folder, target_folder, plan_kind, split_actions
             FROM folder_rename_plans
             WHERE artist_id=? AND status='confirmed'
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
        )?
        .query_map(params![artist_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if !path_under_authorized_roots(&artist_root, roots) {
        let results = plans
            .iter()
            .map(|(id, source, target, _, _)| {
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
                for (id, source, target, _, _) in &plans {
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
                    "results": plans.into_iter().map(|(id, source, target, _, _)| json!({
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
    for (id, source_raw, target_raw, plan_kind, split_actions) in plans {
        if plan_kind == "split_by_tag" {
            if dry_run {
                match prepare_split_file_moves(
                    conn,
                    roots,
                    artist_id,
                    &artist_path,
                    &source_raw,
                    &split_actions,
                ) {
                    Ok(files) => executed.push(json!({
                        "plan_id": id,
                        "status": "dry_run",
                        "kind": "split_by_tag",
                        "source": source_raw,
                        "targets": files
                            .iter()
                            .map(|file| file.target_folder.clone())
                            .collect::<BTreeSet<_>>(),
                        "file_count": files.len(),
                    })),
                    Err(error) => executed.push(json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": split_failure_reason(&error),
                        "source": source_raw,
                        "error": error.to_string(),
                    })),
                }
                continue;
            }
            match execute_split_plan(
                conn,
                roots,
                id,
                artist_id,
                &artist_path,
                &source_raw,
                &split_actions,
                &backup_path,
            ) {
                Ok(result) => executed.push(result),
                Err(error) => {
                    let reason = split_failure_reason(&error);
                    let _ = record_plan_execution_failure(
                        conn,
                        id,
                        reason,
                        &source_raw,
                        "",
                        Some(json!({"error": error.to_string()})),
                    );
                    executed.push(json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": reason,
                        "source": source_raw,
                        "error": error.to_string(),
                    }));
                }
            }
            continue;
        }
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

        // Revalidate targets produced by a saved format snapshot. Plans from
        // before custom naming had no snapshot and retain their confirmed path.
        if !dry_run {
            let dates = effective_item_dates(conn, artist_id, &source_raw)?;
            if dates.is_empty() {
                demote_plan_with_log(conn, id, "needs_date", "", "needs_date", &source_raw)?;
                executed.push(json!({
                    "plan_id": id, "status": "error", "reason": "needs_date",
                    "source": source_raw, "target": target_raw,
                }));
                continue;
            }
            if dates.len() > 1 {
                demote_plan_with_log(conn, id, "date_conflict", "", "date_conflict", &source_raw)?;
                executed.push(json!({
                    "plan_id": id, "status": "error", "reason": "date_conflict",
                    "source": source_raw, "target": target_raw,
                }));
                continue;
            }
            let (selected, format_snapshot): (String, String) = conn.query_row(
                "SELECT selected_tag_ids, format_snapshot FROM folder_rename_plans WHERE id=?",
                params![id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let profile = serde_json::from_str::<Value>(&format_snapshot)
                .ok()
                .and_then(|snapshot| snapshot.get("profile").cloned());
            if let Some(profile) = profile {
                let tags = plan_tag_names(conn, artist_id, &source_raw, &selected)?;
                let original_title: String = conn.query_row(
                    "SELECT original_title FROM folder_rename_plans WHERE id=?",
                    params![id],
                    |row| row.get(0),
                )?;
                match render_archive_target(
                    &profile,
                    &artist_name,
                    &dates[0],
                    &tags,
                    &original_title,
                    &source_raw,
                    id as usize,
                ) {
                    Ok(derived)
                        if derived == target
                            || (profile["collision_strategy"].as_str() == Some("suffix")
                                && is_suffixed_archive_target(&target, &derived)) => {}
                    Ok(derived) => {
                        demote_plan_with_log(
                            conn,
                            id,
                            "ready",
                            &derived,
                            "stale_target",
                            &source_raw,
                        )?;
                        executed.push(json!({
                            "plan_id": id, "status": "error", "reason": "stale_target",
                            "source": source_raw, "target": target_raw, "target_folder": derived,
                        }));
                        continue;
                    }
                    Err(error) => {
                        record_plan_execution_failure(
                            conn,
                            id,
                            "bad_target",
                            &source_raw,
                            &target_raw,
                            Some(json!({"error": error.to_string()})),
                        )?;
                        executed.push(json!({
                            "plan_id": id, "status": "error", "reason": "bad_target",
                            "source": source_raw, "target": target_raw, "error": error.to_string(),
                        }));
                        continue;
                    }
                }
            }
        }

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
    let plan: Option<(i64, String, String, String, String, String)> = conn
        .query_row(
            "SELECT artist_id, source_folder, target_folder, status, execution_log, plan_kind
             FROM folder_rename_plans WHERE id=?",
            params![plan_id],
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
    let Some((artist_id, source_raw, target_raw, status, execution_log, plan_kind)) = plan else {
        bail!("plan_not_found");
    };
    if status != "executed" {
        bail!("plan_not_executed");
    }
    if plan_kind == "split_by_tag" {
        bail!("split_undo_not_supported");
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

/// Generate fixed archive targets for the draft plans of one or more artists
/// without requiring a manual "更新整理项" click, so a scan + auto execute
/// becomes fully automatic. The fixed target is derived from the current
/// effective item dates (`YYYY/YYYY-MM <tag>`); plans without dates, with
/// conflicting dates, or with physical conflicts stay manual. Shared by the
/// post-scan auto archive and the per-artist manual auto run.
pub fn auto_name_artist_draft_plans(
    conn: &Connection,
    roots: &MediaRoots,
    artists: &[i64],
) -> Result<usize> {
    let mut recomputed = 0usize;
    for artist_id in artists {
        auto_discover_artist_folder_plans(conn, *artist_id)?;
        recomputed += recompute_artist_plan_targets(conn, Some(roots), *artist_id)?;
    }
    Ok(recomputed)
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
    // (target exists, unsafe paths, etc.) are skipped and stay manual.
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
         WHERE status='ready'
           AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))
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
             WHERE artist_id=? AND status='confirmed'
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
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
                             WHERE artist_id=? AND status='confirmed'
                               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
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

pub(crate) fn evaluate_plan_paths(
    roots: &MediaRoots,
    artist_id: i64,
    artist_path: &str,
    source_raw: &str,
    target_raw: &str,
) -> Result<PlanPathCheck> {
    let source = match validate_relative_folder(source_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source_raw.to_string(),
                target_folder: target_raw.to_string(),
                source_exists: false,
                target_exists: false,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let target = match validate_relative_folder(target_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source,
                target_folder: target_raw.to_string(),
                source_exists: false,
                target_exists: false,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let artist_root = roots.map_to_real(artist_path)?;
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

pub(crate) fn check_plan_paths(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
) -> Result<PlanPathCheck> {
    let (artist_id, artist_path, source_raw, target_raw, plan_kind, split_actions): (
        i64,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT p.artist_id, a.path, p.source_folder, p.target_folder,
                    p.plan_kind, p.split_actions
             FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
             WHERE p.id=?",
            params![plan_id],
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
        .optional()?
        .ok_or_else(|| anyhow!("plan not found"))?;
    if plan_kind == "split_by_tag" {
        let result = prepare_split_file_moves(
            conn,
            roots,
            artist_id,
            &artist_path,
            &source_raw,
            &split_actions,
        );
        let reason = result
            .err()
            .map(|error| split_failure_reason(&error).to_string());
        return Ok(PlanPathCheck {
            artist_id,
            source_folder: source_raw,
            target_folder: String::new(),
            source_exists: reason.as_deref() != Some("source_missing"),
            target_exists: reason.as_deref() == Some("target_exists"),
            reason,
        });
    }
    evaluate_plan_paths(roots, artist_id, &artist_path, &source_raw, &target_raw)
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
                    folder_name TEXT,
                    manual_date TEXT,
                    detected_date TEXT,
                    date TEXT,
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
            "CREATE TABLE IF NOT EXISTS items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, missing INTEGER DEFAULT 0, manual_date TEXT,
                detected_date TEXT, date TEXT
             );
             CREATE TABLE IF NOT EXISTS item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO items VALUES (1, 1, '/one/no_tags/a.jpg', 'a.jpg', 'no_tags', 0, NULL, '2026-01-01', '2026-01-01'),
                                      (2, 1, '/one/no_tags/b.jpg', 'b.jpg', 'no_tags', 0, NULL, '2026-01-01', '2026-01-01');
             INSERT INTO items VALUES (3, 1, '/one/inconsistent/a.jpg', 'a.jpg', 'inconsistent', 0, NULL, '2026-01-01', '2026-01-01'),
                                      (4, 1, '/one/inconsistent/b.jpg', 'b.jpg', 'inconsistent', 0, NULL, '2026-01-01', '2026-01-01');
             INSERT INTO item_tags VALUES (3, 1);
             INSERT INTO items VALUES (5, 1, '/one/consistent/a.jpg', 'a.jpg', 'consistent', 0, NULL, '2026-01-01', '2026-01-01'),
                                      (6, 1, '/one/consistent/b.jpg', 'b.jpg', 'consistent', 0, NULL, '2026-01-01', '2026-01-01');
             INSERT INTO item_tags VALUES (5, 1), (6, 1);"
        ).unwrap();

        auto_discover_artist_folder_plans(&conn, 1).unwrap();

        let no_tags_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM folder_rename_plans WHERE source_folder='no_tags'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(no_tags_count, 0);

        let inconsistent_plan: (String, String, String) = conn
            .query_row(
                "SELECT status, plan_kind, split_actions
                 FROM folder_rename_plans WHERE source_folder='inconsistent'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(inconsistent_plan.0, "draft");
        assert_eq!(inconsistent_plan.1, "split_by_tag");
        let actions = serde_json::from_str::<Value>(&inconsistent_plan.2).unwrap();
        assert_eq!(actions.as_array().unwrap().len(), 1);
        assert_eq!(actions[0]["item_id"], 3);
        assert_eq!(actions[0]["target_folder"], "2026/2026-01-01 first");

        let consistent_status: (String, String) = conn.query_row(
            "SELECT status, selected_tag_ids FROM folder_rename_plans WHERE source_folder='consistent'",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        ).unwrap();
        assert_eq!(consistent_status.0, "draft");
        assert_eq!(
            serde_json::from_str::<Value>(&consistent_status.1).unwrap(),
            json!([1])
        );
    }

    #[test]
    fn recompute_uses_default_and_saved_default_template() {
        let conn = create_plan_db();
        conn.execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, missing INTEGER DEFAULT 0, manual_date TEXT,
                detected_date TEXT, date TEXT
             );
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO items VALUES (1, 1, '/one/source/a.jpg', 'a.jpg', 'source', 0,
                 NULL, '2026-01-08', '2026-01-08');
             INSERT INTO item_tags VALUES (1, 1);
             INSERT INTO folder_rename_plans
                (artist_id, source_folder, original_title, selected_tag_ids, status)
             VALUES (1, 'source', 'source', '[1]', 'draft');",
        )
        .unwrap();

        recompute_artist_plan_targets(&conn, None, 1).unwrap();
        let target: String = conn
            .query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target, "2026/2026-01-08 first");

        archive_profiles::set_folder_rename_format_settings(
            &conn,
            &json!({
                "active_profile_id": "default",
                "profiles": [{
                    "id": "default",
                    "name": "Default",
                    "template": "{artist}/{tags}/{date}",
                    "collision_strategy": "suffix"
                }],
                "artist_profile_ids": {}
            }),
            None,
        )
        .unwrap();
        recompute_artist_plan_targets(&conn, None, 1).unwrap();
        let target: String = conn
            .query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target, "one/first/2026-01-08");
    }

    #[test]
    fn recompute_suffixes_existing_targets_and_keeps_the_suffix() {
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(artist.join("2026/2026-01-08 first")).unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        conn.execute_batch(
            "CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO tags VALUES (1, 1, 'first');
             INSERT INTO items
                (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, 'source/a.jpg', 'a.jpg', 'source', '2026-01-08', '2026-01-08');
             INSERT INTO item_tags VALUES (1, 1);
             INSERT INTO folder_rename_plans
                (artist_id, source_folder, original_title, selected_tag_ids, status)
             VALUES (1, 'source', 'source', '[1]', 'draft');",
        )
        .unwrap();

        let roots = test_roots(dir.path());
        recompute_artist_plan_targets(&conn, Some(&roots), 1).unwrap();
        let first: (String, String) = conn
            .query_row(
                "SELECT target_folder, status FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(first, ("2026/2026-01-08 first (2)".into(), "ready".into()));

        recompute_artist_plan_targets(&conn, Some(&roots), 1).unwrap();
        let second: String = conn
            .query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(second, first.0);
    }

    #[test]
    fn recompute_marks_path_mapping_errors_for_manual_review() {
        let conn = create_plan_db();
        conn.execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, missing INTEGER DEFAULT 0, manual_date TEXT,
                detected_date TEXT, date TEXT
             );
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO items VALUES (1, 1, '/one/source/a.jpg', 'a.jpg', 'source', 0,
                 NULL, '2026-01-08', '2026-01-08');
             INSERT INTO item_tags VALUES (1, 1);
             INSERT INTO folder_rename_plans
                (artist_id, source_folder, original_title, selected_tag_ids, status)
             VALUES (1, 'source', 'source', '[1]', 'draft');
             UPDATE artists SET path='/pictures1/../invalid' WHERE id=1;",
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["root".into()],
            real_paths: vec!["/tmp/gallery-root".into()],
        };

        recompute_artist_plan_targets(&conn, Some(&roots), 1).unwrap();
        let status: String = conn
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(status, "manual_review");
    }

    #[test]
    fn recompute_marks_aligned_plans_and_hides_missing_legacy_plans() {
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(artist.join("2026/2026-01-08 first")).unwrap();
        let conn = create_archive_db(&dir.path().join("gallery.db"), &artist, true);
        conn.execute_batch(
            "CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO tags VALUES (1, 1, 'first'), (2, 1, 'second');
             INSERT INTO items VALUES
               (1, 1, '2026/2026-01-08 first/a.jpg', 'a.jpg', '2026/2026-01-08 first', NULL, '2026-01-08', '2026-01-08', 0),
               (2, 1, 'legacy/a.jpg', 'b.jpg', 'legacy', NULL, '2025-02-03', '2025-02-03', 0);
             INSERT INTO item_tags VALUES (1, 1), (2, 2);
             INSERT INTO folder_rename_plans
               (artist_id, source_folder, original_title, selected_tag_ids, status)
             VALUES
               (1, '2026/2026-01-08 first', '2026-01-08 first', '[1]', 'draft'),
               (1, 'legacy', 'legacy', '[2]', 'manual_review');",
        )
        .unwrap();

        let roots = test_roots(dir.path());
        recompute_artist_plan_targets(&conn, Some(&roots), 1).unwrap();

        let statuses = conn
            .prepare("SELECT source_folder, status FROM folder_rename_plans ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            statuses,
            vec![
                ("2026/2026-01-08 first".into(), "aligned".into()),
                ("legacy".into(), "stale".into()),
            ]
        );

        let listed = list_folder_renames(&conn, Some(&roots), Some(1)).unwrap();
        assert_eq!(listed["total"], 1);
        assert_eq!(listed["plans"][0]["status"], "aligned");
    }

    #[test]
    fn auto_discover_uses_full_relative_path_and_discards_legacy_basename_plan() {
        let conn = create_plan_db();
        conn.execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, missing INTEGER DEFAULT 0, manual_date TEXT,
                detected_date TEXT, date TEXT
             );
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             INSERT INTO items VALUES
                (1, 1, '/one/2022/2022-12-06 罗丝/a.jpg', 'a.jpg', '2022-12-06 罗丝', 0,
                 NULL, '2022-12-06', '2022-12-06');
             INSERT INTO item_tags VALUES (1, 1);",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, status, execution_log)
             VALUES (1, '2022-12-06 罗丝', 'manual_review', '[]')",
            [],
        )
        .unwrap();

        auto_discover_artist_folder_plans(&conn, 1).unwrap();
        recompute_artist_plan_targets(&conn, None, 1).unwrap();

        let plan: (String, String) = conn
            .query_row(
                "SELECT target_folder, status FROM folder_rename_plans
                 WHERE artist_id=1 AND source_folder='2022/2022-12-06 罗丝'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(plan, ("2022/2022-12-06 first".into(), "ready".into()));
        let legacy_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM folder_rename_plans
                 WHERE artist_id=1 AND source_folder='2022-12-06 罗丝'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy_count, 0);
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
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists VALUES (1,'a',?)", params![ap])
            .unwrap();
        let fp = src.join("a.jpg").to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1,1,?, 'a.jpg', 'old', '2026-01-01', '2026-01-01')",
            params![fp],
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1,'old','2026/2026-01 untitled','confirmed')",
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
        assert!(artist
            .join("2026")
            .join("2026-01 untitled")
            .join("a.jpg")
            .is_file());
        let new_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(new_path.contains("/2026/2026-01 untitled/"));
    }

    #[test]
    fn execute_splits_tagged_files_by_date_and_keeps_untagged_files() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("2022").join("mixed");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("jan.jpg"), b"jan").unwrap();
        std::fs::write(source.join("feb.jpg"), b"feb").unwrap();
        std::fs::write(source.join("untagged.jpg"), b"untagged").unwrap();

        let conn = Connection::open(dir.path().join("gallery.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE items (
               id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
               folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT,
               missing INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        let artist_db = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', ?)",
            params![artist_db],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO tags VALUES (1, 1, 'alpha'), (2, 1, 'beta');
             INSERT INTO item_tags VALUES (1, 1), (2, 2);",
        )
        .unwrap();
        for (id, name, date) in [
            (1, "jan.jpg", "2026-01-12"),
            (2, "feb.jpg", "2026-02-02"),
            (3, "untagged.jpg", "2026-01-12"),
        ] {
            let path = source.join(name).to_string_lossy().replace('\\', "/");
            conn.execute(
                "INSERT INTO items
                 (id, artist_id, file_path, file_name, folder_name, detected_date, date)
                 VALUES (?, 1, ?, ?, 'mixed', ?, ?)",
                params![id, path, name, date, date],
            )
            .unwrap();
        }
        ensure_folder_schema(&conn).unwrap();
        let roots = test_roots(dir.path());
        auto_discover_artist_folder_plans(&conn, 1).unwrap();
        let source_folder: String = conn
            .query_row(
                "SELECT source_folder FROM folder_rename_plans WHERE artist_id=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(source_folder, "2022/mixed");
        recompute_artist_plan_targets(&conn, Some(&roots), 1).unwrap();
        conn.execute(
            "UPDATE folder_rename_plans SET status='confirmed' WHERE artist_id=1",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let result = execute_folder_renames(&conn, &roots, 1, false).unwrap();

        assert_eq!(result["results"][0]["status"], "executed");
        assert!(artist
            .join("2026")
            .join("2026-01-12 alpha")
            .join("jan.jpg")
            .is_file());
        assert!(artist
            .join("2026")
            .join("2026-02-02 beta")
            .join("feb.jpg")
            .is_file());
        assert!(source.join("untagged.jpg").is_file());
        let paths = conn
            .prepare("SELECT file_path, folder_name FROM items ORDER BY id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(paths[0].0.ends_with("/2026/2026-01-12 alpha/jan.jpg"));
        assert_eq!(paths[0].1, "2026-01-12 alpha");
        assert!(paths[1].0.ends_with("/2026/2026-02-02 beta/feb.jpg"));
        assert_eq!(paths[1].1, "2026-02-02 beta");
        assert_eq!(paths[2].1, "mixed");
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
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, '/pictures1/artist/old/a.jpg', 'a.jpg', 'old', '2026-01-01', '2026-01-01')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'old', '2026/2026-01 untitled', 'confirmed')",
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
        assert!(artist
            .join("2026")
            .join("2026-01 untitled")
            .join("a.jpg")
            .is_file());
        let moved_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(moved_path, "/pictures1/artist/2026/2026-01 untitled/a.jpg");

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
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, ?, 'a.jpg', 'old', '2026-01-01', '2026-01-01')",
            [format!("{source_path}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, execution_log)
             VALUES (1, 'old', '2026/2026-01 untitled', 'confirmed', '[]')",
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
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);",
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
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, ?, 'a.jpg', 'source', '2026-01-01', '2026-01-01')",
            [artist.join("source").join("a.jpg").to_string_lossy().as_ref()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1, 'source', 'target', 'ready')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_file);
        let roots = test_roots(dir.path());

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
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);",
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
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (7, 1, '测试')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, ?, 'a.jpg', '2026-01-05 测试', '2026-01-05', '2026-01-05'),
                    (2, 1, ?, 'b.jpg', '2026-01-05 测试', '2026-01-05', '2026-01-05')",
            params![
                format!("{artist_path}/2026-01-05 测试/a.jpg"),
                format!("{artist_path}/2026-01-05 测试/b.jpg"),
            ],
        )
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
        let row: (String, String, String) = conn
            .query_row(
                "SELECT target_folder, status, confirmation_source FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(row.0, "2026/2026-01-05 测试");
        assert_eq!(row.1, "executed");
        assert_eq!(row.2, "auto");
        assert!(artist.join("2026").join("2026-01-05 测试").is_dir());
        assert!(!artist.join("2026-01-05 测试").exists());
    }

    #[test]
    fn auto_archive_suffixes_conflicted_draft_plan() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("2026-01-05 测试");
        let taken = artist.join("2026").join("2026-01-05 测试");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&taken).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);",
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
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (7, 1, '测试')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, ?, 'a.jpg', '2026-01-05 测试', '2026-01-05', '2026-01-05'),
                    (2, 1, ?, 'b.jpg', '2026-01-05 测试', '2026-01-05', '2026-01-05')",
            params![
                format!("{artist_path}/2026-01-05 测试/a.jpg"),
                format!("{artist_path}/2026-01-05 测试/b.jpg"),
            ],
        )
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

        assert_eq!(result["executed_count"], 1);
        assert_eq!(result["auto_named"], 1);
        let row: (String, String) = conn
            .query_row(
                "SELECT target_folder, status FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "2026/2026-01-05 测试 (2)");
        assert_eq!(row.1, "executed");
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
