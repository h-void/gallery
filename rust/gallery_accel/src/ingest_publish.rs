//! Shared file ingestion and atomic publication protocol for download engines
//! (Pawchive subscription and JDownloader netdisk integration).
//!
//! Owns the durable publish ledger, source validation (rejecting symlinks and
//! escaping paths), temporary private staging, streaming BLAKE3 verification,
//! atomic non-overwriting publish, and handoff to library scanning.

use std::fs::{File, Metadata, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::media_roots::{path_under_authorized_roots, MediaRoots};

/// Current processing stage of a published download file.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PublishStage {
    Verified,
    Staged,
    Published,
    Ingested,
    Failed,
}

impl PublishStage {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Staged => "staged",
            Self::Published => "published",
            Self::Ingested => "ingested",
            Self::Failed => "failed",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "verified" => Some(Self::Verified),
            "staged" => Some(Self::Staged),
            "published" => Some(Self::Published),
            "ingested" => Some(Self::Ingested),
            "failed" => Some(Self::Failed),
            _ => None,
        }
    }
}

/// A recorded publish job in the database.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PublishJob {
    pub id: i64,
    pub engine: String,
    pub source_job_id: String,
    pub manifest_version: i64,
    pub source_identity: String,
    pub expected_length: u64,
    pub expected_blake3: String,
    pub target_path: String,
    pub private_temp_path: String,
    pub stage: PublishStage,
    pub item_id: Option<i64>,
    pub error_message: Option<String>,
    pub created_at: f64,
    pub updated_at: f64,
}

/// Request parameters for publishing a completed download artifact into media roots.
pub struct IngestPublishRequest<'a> {
    pub engine: &'a str,
    pub source_job_id: &'a str,
    pub manifest_version: i64,
    pub source_identity: &'a str,
    pub source_path: &'a Path,
    pub expected_length: u64,
    pub expected_blake3: &'a str,
    pub target_path: &'a Path,
}

/// Outcome of a publish attempt.
#[derive(Debug, Clone)]
pub struct PublishResult {
    pub job_id: i64,
    pub published_path: PathBuf,
    pub stage: PublishStage,
    pub blake3_hash: String,
    pub file_length: u64,
}

/// Initialize the durable download publication ledger schema.
pub fn ensure_ingest_publish_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS download_publish_jobs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            engine TEXT NOT NULL,
            source_job_id TEXT NOT NULL,
            manifest_version INTEGER NOT NULL DEFAULT 1,
            source_identity TEXT NOT NULL,
            expected_length INTEGER NOT NULL,
            expected_blake3 TEXT NOT NULL,
            target_path TEXT NOT NULL,
            private_temp_path TEXT NOT NULL DEFAULT '',
            stage TEXT NOT NULL CHECK(stage IN ('verified', 'staged', 'published', 'ingested', 'failed')),
            item_id INTEGER REFERENCES items(id) ON DELETE SET NULL,
            error_message TEXT,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            UNIQUE(engine, source_job_id, source_identity, manifest_version)
        );
        CREATE INDEX IF NOT EXISTS idx_download_publish_jobs_engine_stage
            ON download_publish_jobs(engine, stage);
        "#,
    )?;
    Ok(())
}

#[cfg(unix)]
fn file_identity(metadata: &Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &Metadata) -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn fsync_dir(dir: &Path) -> Result<()> {
    match File::open(dir) {
        Ok(file) => file
            .sync_all()
            .with_context(|| format!("failed to sync directory to disk: {:?}", dir)),
        Err(err) if err.kind() == std::io::ErrorKind::PermissionDenied => {
            // Write-only ACL mounts on fnOS cannot open directory with O_RDONLY for fsync
            Ok(())
        }
        Err(err) => {
            Err(err).with_context(|| format!("failed to open directory for sync: {:?}", dir))
        }
    }
}

#[cfg(not(unix))]
fn fsync_dir(_dir: &Path) -> Result<()> {
    Ok(())
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
pub(crate) struct AuthorizedPublishDir {
    pub dir_fd: std::os::fd::OwnedFd,
    pub dir_path: PathBuf,
}

#[cfg(target_os = "linux")]
impl AuthorizedPublishDir {
    pub fn temp_file_path(&self, temp_name: &str) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from("/proc/self/fd")
            .join(self.dir_fd.as_raw_fd().to_string())
            .join(temp_name)
    }

    pub fn target_file_path(&self, file_name: &std::ffi::OsStr) -> PathBuf {
        use std::os::fd::AsRawFd;
        PathBuf::from("/proc/self/fd")
            .join(self.dir_fd.as_raw_fd().to_string())
            .join(file_name)
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_authorized_publish_dir(
    target_parent: &Path,
    roots: &MediaRoots,
) -> Result<AuthorizedPublishDir> {
    use crate::fs_util::safe_canonicalize;
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    const O_PATH: c_int = 0o10000000;

    if !path_under_authorized_roots(target_parent, roots) {
        return Err(anyhow!(
            "target parent directory is outside authorized media roots: {:?}",
            target_parent
        ));
    }

    let mut ancestor = target_parent.to_path_buf();
    let mut uncreated = Vec::new();
    while !ancestor.exists() {
        if let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) {
            uncreated.push(name.to_os_string());
            ancestor = parent.to_path_buf();
        } else {
            return Err(anyhow!(
                "target parent has no existing root ancestor: {:?}",
                target_parent
            ));
        }
    }

    let ancestor_canon = safe_canonicalize(&ancestor)
        .with_context(|| format!("cannot canonicalize ancestor {:?}", ancestor))?;

    let root = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|r| safe_canonicalize(r).ok())
        .filter(|r| ancestor_canon.starts_with(r))
        .max_by_key(|r| r.components().count())
        .ok_or_else(|| {
            anyhow!(
                "cannot find matching authorized root for {:?}",
                target_parent
            )
        })?;

    let existing_relative = ancestor_canon
        .strip_prefix(&root)
        .map_err(|_| anyhow!("ancestor escaped matching root: {:?}", ancestor_canon))?;

    let slash = CString::new("/").unwrap();
    let root_fd = unsafe {
        open(
            slash.as_ptr(),
            O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error())
            .with_context(|| "failed to open root directory /");
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };

    let root_rel = root.strip_prefix("/").unwrap_or(&root);
    for component in root_rel.components().chain(existing_relative.components()) {
        let std::path::Component::Normal(c) = component else {
            continue;
        };
        let name =
            CString::new(c.as_bytes()).map_err(|_| anyhow!("invalid path component: {:?}", c))?;
        let next = unsafe {
            openat(
                current.as_raw_fd(),
                name.as_ptr(),
                O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            )
        };
        if next < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open existing directory component {:?}", c));
        }
        current = unsafe { OwnedFd::from_raw_fd(next) };
    }

    for comp in uncreated.into_iter().rev() {
        let name = CString::new(comp.as_bytes())
            .map_err(|_| anyhow!("invalid path component: {:?}", comp))?;
        let next = unsafe {
            openat(
                current.as_raw_fd(),
                name.as_ptr(),
                O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            )
        };
        let next = if next >= 0 {
            next
        } else if std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound {
            let mk = unsafe { mkdirat(current.as_raw_fd(), name.as_ptr(), 0o755) };
            if mk != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error())
                    .with_context(|| format!("failed to create directory component {:?}", comp));
            }
            unsafe {
                openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                )
            }
        } else {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open directory component {:?}", comp));
        };
        if next < 0 {
            return Err(std::io::Error::last_os_error())
                .with_context(|| format!("failed to open directory handle for {:?}", comp));
        }
        current = unsafe { OwnedFd::from_raw_fd(next) };
    }

    Ok(AuthorizedPublishDir {
        dir_fd: current,
        dir_path: target_parent.to_path_buf(),
    })
}

#[cfg(not(target_os = "linux"))]
#[derive(Debug)]
pub(crate) struct AuthorizedPublishDir {
    pub dir_path: PathBuf,
}

#[cfg(not(target_os = "linux"))]
impl AuthorizedPublishDir {
    pub fn temp_file_path(&self, temp_name: &str) -> PathBuf {
        self.dir_path.join(temp_name)
    }

    pub fn target_file_path(&self, file_name: &std::ffi::OsStr) -> PathBuf {
        self.dir_path.join(file_name)
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn open_authorized_publish_dir(
    target_parent: &Path,
    roots: &MediaRoots,
) -> Result<AuthorizedPublishDir> {
    use crate::fs_util::safe_canonicalize;

    if !path_under_authorized_roots(target_parent, roots) {
        return Err(anyhow!(
            "target parent directory is outside authorized media roots: {:?}",
            target_parent
        ));
    }

    let mut ancestor = target_parent.to_path_buf();
    let mut to_create = Vec::new();
    while !ancestor.exists() {
        if let (Some(parent), Some(name)) = (ancestor.parent(), ancestor.file_name()) {
            to_create.push(name.to_os_string());
            ancestor = parent.to_path_buf();
        } else {
            return Err(anyhow!(
                "target parent has no existing root ancestor: {:?}",
                target_parent
            ));
        }
    }

    let ancestor_canon = safe_canonicalize(&ancestor)
        .with_context(|| format!("cannot canonicalize ancestor {:?}", ancestor))?;

    let root = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|r| safe_canonicalize(r).ok())
        .filter(|r| ancestor_canon.starts_with(r))
        .max_by_key(|r| r.components().count())
        .ok_or_else(|| {
            anyhow!(
                "cannot find matching authorized root for {:?}",
                target_parent
            )
        })?;

    let anc_meta = std::fs::symlink_metadata(&ancestor)?;
    if anc_meta.file_type().is_symlink() || !anc_meta.is_dir() {
        return Err(anyhow!(
            "ancestor is a symlink or non-directory: {:?}",
            ancestor
        ));
    }

    let mut current = ancestor;
    for comp in to_create.into_iter().rev() {
        current.push(comp);
        std::fs::create_dir(&current)
            .with_context(|| format!("failed to create directory {:?}", current))?;
        let meta = std::fs::symlink_metadata(&current)
            .with_context(|| format!("failed to inspect metadata of {:?}", current))?;
        if meta.file_type().is_symlink() || !meta.is_dir() {
            return Err(anyhow!(
                "directory component is a symlink or non-directory: {:?}",
                current
            ));
        }
    }

    let target_parent_canon = safe_canonicalize(target_parent)?;
    if !target_parent_canon.starts_with(&root) {
        return Err(anyhow!(
            "target parent escaped authorized root: {:?}",
            target_parent
        ));
    }

    Ok(AuthorizedPublishDir {
        dir_path: target_parent.to_path_buf(),
    })
}

/// Whether the file at `path` is exactly the artifact this job describes.
///
/// Used to recognise this job's own earlier publish after an interrupted run.
/// Length alone would accept a different file that happens to be the same size,
/// and this is the path a crash recovery takes, so the content is checked.
fn file_matches_artifact(path: &Path, expected_length: u64, expected_blake3: &str) -> Result<bool> {
    let metadata = std::fs::metadata(path)?;
    if !metadata.is_file() || metadata.len() != expected_length {
        return Ok(false);
    }
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher
        .finalize()
        .to_hex()
        .to_string()
        .eq_ignore_ascii_case(expected_blake3))
}

#[cfg(target_os = "linux")]
fn rename_noreplace_linux(temp: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: c_int = -100;
    const RENAME_NOREPLACE: c_uint = 1;

    unsafe extern "C" {
        fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    let temp_c = CString::new(temp.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let target_c = CString::new(target.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;

    let res = unsafe {
        renameat2(
            AT_FDCWD,
            temp_c.as_ptr(),
            AT_FDCWD,
            target_c.as_ptr(),
            RENAME_NOREPLACE,
        )
    };
    if res == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// Publish a generated file, replacing whatever already occupies the name.
///
/// The `content …txt` / `links …txt` files are derived from the post body, so a
/// corrected render has to be able to supersede the file an earlier build wrote.
/// Media keeps using [`publish_noreplace`]: a published artifact is never
/// replaced, and a routing bug that would have overwritten one still fails.
pub(crate) fn publish_generated_file(temp: &Path, target: &Path) -> Result<()> {
    // Both names are in the same directory, so this is an atomic replace on
    // POSIX and (via MOVEFILE_REPLACE_EXISTING) on Windows.
    std::fs::rename(temp, target)
        .with_context(|| format!("cannot publish {temp:?} to {target:?}"))?;
    if let Some(parent) = target.parent() {
        let _ = fsync_dir(parent);
    }
    Ok(())
}

/// Move `temp` onto `target` atomically without ever replacing an existing file.
///
/// `hard_link` is an atomic create-if-absent across POSIX filesystems.
/// On mounts that refuse hard-link (e.g. exFAT or cross-directory constraints),
/// Linux uses `renameat2(..., RENAME_NOREPLACE)` to atomically publish without
/// creating any empty placeholder file or risking overwriting a concurrent entrant.
/// On Windows, `std::fs::rename` natively refuses to overwrite without
/// `MOVEFILE_REPLACE_EXISTING`.
pub(crate) fn publish_noreplace(temp: &Path, target: &Path) -> Result<()> {
    match std::fs::hard_link(temp, target) {
        Ok(()) => {
            let _ = std::fs::remove_file(temp);
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
            "target path already exists: refuse to overwrite {target:?}"
        )),
        Err(link_error) => {
            #[cfg(target_os = "linux")]
            {
                match rename_noreplace_linux(temp, target) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                        Err(anyhow!(
                            "target path already exists: refuse to overwrite {target:?}"
                        ))
                    }
                    Err(rename_error) => {
                        Err(anyhow!(
                            "cannot publish {temp:?} to {target:?} without overwrite: \
                             hard link failed ({link_error}); rename_noreplace failed ({rename_error})"
                        ))
                    }
                }
            }
            #[cfg(windows)]
            {
                if target.exists() {
                    return Err(anyhow!(
                        "target path already exists: refuse to overwrite {target:?}"
                    ));
                }
                match std::fs::rename(temp, target) {
                    Ok(()) => Ok(()),
                    Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => Err(anyhow!(
                        "target path already exists: refuse to overwrite {target:?}"
                    )),
                    Err(rename_error) => Err(anyhow!(
                        "cannot publish {temp:?} to {target:?} without overwrite: \
                             hard link failed ({link_error}); rename failed ({rename_error})"
                    )),
                }
            }
            #[cfg(all(not(target_os = "linux"), not(windows)))]
            {
                Err(anyhow!(
                    "cannot publish {temp:?} to {target:?} without overwrite: \
                     hard link failed ({link_error}); atomic non-replacing publish unsupported"
                ))
            }
        }
    }
}

/// How long an unreferenced `.gallery_publish_*.part` must sit before startup
/// reclaims it. Long enough that a copy in progress — or one interrupted by a
/// restart that is still being retried — is never taken away.
pub const ORPHANED_PART_MIN_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// What an orphan sweep reclaimed, for the startup log.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct OrphanSweep {
    pub files: usize,
    pub bytes: u64,
}

/// Reclaim publish staging files no job references any more.
///
/// The copy in step 4 creates `.gallery_publish_<uuid>.part` beside the target
/// and only *afterwards* records it in `download_publish_jobs.private_temp_path`.
/// A kill in that window — fnOS stop/upgrade sends TERM and then KILL a second
/// later — leaves a file the size of the finished download that nothing knows
/// about: the scanner skips dot-files, `reconcile_published_files` is
/// ledger-driven, and `pawchive::has_transient_suffix` only *ignores* such
/// names. Every interrupted attempt adds another one.
///
/// Two guards keep this from deleting live work: the ledger's recorded paths are
/// never touched, and a file younger than `ORPHANED_PART_MIN_AGE` is left alone
/// even when it is unreferenced (a job row is written one step after the file
/// appears, and a copy can legitimately run for hours).
pub fn sweep_orphaned_publish_parts(
    conn: &Connection,
    roots: &MediaRoots,
    min_age: Duration,
) -> Result<OrphanSweep> {
    let referenced: std::collections::HashSet<String> = conn
        .prepare(
            "SELECT private_temp_path FROM download_publish_jobs
             WHERE private_temp_path != ''",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;

    let cutoff = SystemTime::now()
        .checked_sub(min_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut sweep = OrphanSweep::default();
    for root in &roots.roots {
        let Ok(entries) = std::fs::read_dir(root) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            if !name.starts_with(".gallery_publish_") || !name.ends_with(".part") {
                continue;
            }
            if referenced.contains(&path.to_string_lossy().to_string()) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
            if !metadata.is_file() {
                continue;
            }
            if metadata.modified().map(|at| at > cutoff).unwrap_or(true) {
                // Unreadable or recent mtime: leave it rather than guess.
                continue;
            }
            let size = metadata.len();
            if std::fs::remove_file(&path).is_ok() {
                sweep.files += 1;
                sweep.bytes = sweep.bytes.saturating_add(size);
            }
        }
    }
    Ok(sweep)
}

/// Releases the §6.3 publish reservation when the publish leaves this scope.
///
/// Drop rather than an explicit call at each return: `publish_completed_file`
/// has many of them, and one missed release would leave 整理 blocked on a
/// publish that finished long ago.
struct PublishReservation<'a> {
    conn: &'a Connection,
    group_id: Option<String>,
    owner: String,
}

impl Drop for PublishReservation<'_> {
    fn drop(&mut self) {
        if let Some(group_id) = self.group_id.as_deref() {
            crate::pawchive_groups::release_publish_reservation(self.conn, group_id, &self.owner);
        }
    }
}

/// Fault injection for the two publish windows V2 names.
///
/// Compiled into the test binary only; the shipped binary does not carry it.
/// It exists because neither window can be hit from outside: the staging copy
/// runs no SQL, so there is nothing for an external kill to wait on, and a full
/// filesystem is not something a test can arrange on demand. `GALLERY_PUBLISH_`
/// variables are read here and nowhere else.
#[cfg(test)]
pub(crate) mod publish_faults {
    /// Every fault is scoped to one job id, so a test that sets these
    /// process-wide variables cannot trip a publish running on another thread.
    fn in_scope(job: &str) -> bool {
        std::env::var("GALLERY_PUBLISH_FAULT_JOB")
            .ok()
            .as_deref()
            .is_some_and(|scope| scope == job)
    }

    /// Abort with no destructors and no cleanup — the state a kill leaves.
    pub(crate) fn maybe_abort(point: &str, job: &str) {
        if in_scope(job) && std::env::var("GALLERY_PUBLISH_CRASH_AT").as_deref() == Ok(point) {
            std::process::abort();
        }
    }

    /// Fail the staging write once `written` bytes have gone out, the way a
    /// full device fails: part way through, after the temporary file exists.
    pub(crate) fn maybe_write_error(job: &str, written: u64) -> Option<std::io::Error> {
        if !in_scope(job) {
            return None;
        }
        let limit = std::env::var("GALLERY_PUBLISH_FAIL_WRITE_AFTER")
            .ok()
            .and_then(|value| value.parse::<u64>().ok())?;
        (written >= limit).then(|| {
            std::io::Error::new(
                std::io::ErrorKind::StorageFull,
                "injected failure: no space left on device",
            )
        })
    }

    /// Refuse the directory sync that makes the published name durable.
    pub(crate) fn maybe_fsync_error(job: &str) -> Option<std::io::Error> {
        if !in_scope(job) || std::env::var_os("GALLERY_PUBLISH_FAIL_FSYNC").is_none() {
            return None;
        }
        Some(std::io::Error::other(
            "injected failure: directory sync refused",
        ))
    }
}

/// Atomically publish a completed download file to an authorized media destination.
///
/// Implements Section 7.1 of the download specifications:
/// 1. Verifies source is a regular file and strictly not a symlink.
/// 2. Verifies destination is within authorized media roots and does not already exist.
/// 3. Registers job in SQLite ledger as `verified`.
/// 4. Copies to a private temporary file in target parent dir (`.gallery_publish_*.part`),
///    streaming BLAKE3 digest and checking exact byte length and hash.
/// 5. Validates that source and target temp files do not share an inode.
/// 6. Performs atomic rename without overwrite to final media path and fsyncs parent dir.
/// 7. Updates ledger to `published`.
pub fn publish_completed_file(
    conn: &Connection,
    req: &IngestPublishRequest,
    roots: &MediaRoots,
) -> Result<PublishResult> {
    // Acquire before resolving/creating the target directory. The guard stays
    // held through every return, even when a copy outlives its ledger lease.
    let _operation_lock = crate::pawchive_groups::lock_group_operations(conn, false)?;
    // 1. Source verification (strictly reject symlinks).
    let src_symlink_meta = std::fs::symlink_metadata(req.source_path)
        .with_context(|| format!("failed to read metadata for source {:?}", req.source_path))?;
    if src_symlink_meta.file_type().is_symlink() {
        return Err(anyhow!(
            "source file cannot be a symlink: {:?}",
            req.source_path
        ));
    }
    if !src_symlink_meta.is_file() {
        return Err(anyhow!(
            "source path is not a regular file: {:?}",
            req.source_path
        ));
    }
    if src_symlink_meta.len() != req.expected_length {
        return Err(anyhow!(
            "source file length {} does not match expected length {}",
            src_symlink_meta.len(),
            req.expected_length
        ));
    }

    // 2. Target authorization. Whether an existing target is a collision or
    // this job's own earlier publish is decided by the ledger, so the lookup
    // below runs first: refusing on `exists()` alone turned a legitimate retry
    // into a permanent failure.
    if !path_under_authorized_roots(req.target_path, roots) {
        return Err(anyhow!(
            "target path is outside authorized media roots: {:?}",
            req.target_path
        ));
    }
    let target_parent = req
        .target_path
        .parent()
        .ok_or_else(|| anyhow!("target path has no parent: {:?}", req.target_path))?;

    let bound_dir = open_authorized_publish_dir(target_parent, roots)?;

    // 2b. The §6.3 fence. Both halves matter and neither alone is enough: the
    // reservation stops a publish that starts while 整理 is mid-move, and the
    // bound directory handle above is what catches a move that happens after
    // this check, because the rename it performs targets the handle rather than
    // re-resolving the path.
    //
    // Released on every exit path, including the error returns below, so a
    // failed publish cannot hold 整理 off for longer than the reservation's TTL.
    let owned_group =
        crate::pawchive_groups::owning_group_for_path(conn, &target_parent.to_string_lossy())?;
    let publish_owner = format!(
        "{}:{}:{}",
        req.engine, req.source_job_id, req.source_identity
    );
    if let Some(group) = owned_group.as_ref() {
        crate::pawchive_groups::claim_publish_reservation(conn, &group.group_id, &publish_owner)?;
    }
    let _reservation = PublishReservation {
        conn,
        group_id: owned_group.as_ref().map(|group| group.group_id.clone()),
        owner: publish_owner,
    };

    // 3. Ledger registration or lookup.
    ensure_ingest_publish_schema(conn)?;
    let target_str = req.target_path.to_string_lossy().to_string();

    let existing: Option<(i64, String, Option<i64>, String)> = conn
        .query_row(
            "SELECT id, stage, item_id, private_temp_path FROM download_publish_jobs
             WHERE engine = ?1 AND source_job_id = ?2 AND source_identity = ?3 AND manifest_version = ?4",
            params![
                req.engine,
                req.source_job_id,
                req.source_identity,
                req.manifest_version
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;

    let job_id = match existing {
        Some((id, stage_str, _, private_temp_path)) => {
            let stage = PublishStage::parse(&stage_str).unwrap_or(PublishStage::Failed);
            if matches!(stage, PublishStage::Published | PublishStage::Ingested) {
                // Already published. Repeat the call idempotently when the file
                // is still the one this job wrote, and refuse to touch anything
                // else that now sits at the target.
                if req.target_path.is_file() {
                    if file_matches_artifact(
                        req.target_path,
                        req.expected_length,
                        req.expected_blake3,
                    )? {
                        return Ok(PublishResult {
                            job_id: id,
                            published_path: req.target_path.to_path_buf(),
                            stage,
                            blake3_hash: req.expected_blake3.to_string(),
                            file_length: req.expected_length,
                        });
                    }
                    return Err(anyhow!(
                        "target path holds a different file: refuse to overwrite {:?}",
                        req.target_path
                    ));
                }
                // The file is gone: this job may publish again.
            } else if req.target_path.is_file() {
                // A crash between the final link and the ledger write leaves the
                // target in place while the row still says `verified` or
                // `staged`. The artifact is identified by its content, not by
                // its name, so this job's own publish is recognised and the
                // ledger is completed — while a file that is not this artifact
                // is still refused rather than replaced.
                if file_matches_artifact(req.target_path, req.expected_length, req.expected_blake3)
                    .unwrap_or(false)
                {
                    conn.execute(
                        "UPDATE download_publish_jobs
                         SET stage = 'published', private_temp_path = '', error_message = NULL,
                             updated_at = strftime('%s','now')
                         WHERE id = ?1",
                        params![id],
                    )?;
                    return Ok(PublishResult {
                        job_id: id,
                        published_path: req.target_path.to_path_buf(),
                        stage: PublishStage::Published,
                        blake3_hash: req.expected_blake3.to_string(),
                        file_length: req.expected_length,
                    });
                }
                return Err(anyhow!(
                    "target path already exists: refuse to overwrite {:?}",
                    req.target_path
                ));
            }
            // The staging file of the interrupted attempt is this job's own and
            // is replaced below; leaving it would accumulate one part file per
            // retry.
            if !private_temp_path.trim().is_empty() {
                let _ = std::fs::remove_file(&private_temp_path);
            }
            conn.execute(
                "UPDATE download_publish_jobs SET stage = 'verified', private_temp_path = '', error_message = NULL, updated_at = strftime('%s','now') WHERE id = ?1",
                params![id],
            )?;
            id
        }
        None => {
            // A file this ledger never wrote is somebody else's, and is never
            // overwritten.
            if req.target_path.exists() {
                return Err(anyhow!(
                    "target path already exists: refuse to overwrite {:?}",
                    req.target_path
                ));
            }
            conn.execute(
                "INSERT INTO download_publish_jobs
                 (engine, source_job_id, manifest_version, source_identity, expected_length, expected_blake3, target_path, stage)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'verified')",
                params![
                    req.engine,
                    req.source_job_id,
                    req.manifest_version,
                    req.source_identity,
                    req.expected_length as i64,
                    req.expected_blake3,
                    target_str,
                ],
            )?;
            conn.last_insert_rowid()
        }
    };

    // 4. Staging copy with streaming BLAKE3 hash computation.
    let pre_stage_meta = std::fs::symlink_metadata(target_parent).with_context(|| {
        format!(
            "parent directory missing before staging {:?}",
            target_parent
        )
    })?;
    if pre_stage_meta.file_type().is_symlink() || !pre_stage_meta.is_dir() {
        return Err(anyhow!(
            "target parent directory cannot be a symlink or non-directory: {:?}",
            target_parent
        ));
    }

    // Private temp filename starts with '.' and ends with '.part' so scanner ignores it.
    let temp_name = format!(".gallery_publish_{}.part", uuid::Uuid::new_v4().simple());
    let temp_path = bound_dir.temp_file_path(&temp_name);
    let temp_path_str = bound_dir
        .dir_path
        .join(&temp_name)
        .to_string_lossy()
        .to_string();

    let copy_result: Result<(String, u64)> = (|| {
        let mut src_file = File::open(req.source_path)
            .with_context(|| format!("cannot open source file {:?}", req.source_path))?;
        let src_opened_meta = src_file.metadata().with_context(|| {
            format!(
                "cannot query metadata of opened source {:?}",
                req.source_path
            )
        })?;
        if !src_opened_meta.is_file() {
            return Err(anyhow!(
                "opened source is not a regular file: {:?}",
                req.source_path
            ));
        }
        if src_opened_meta.len() != req.expected_length {
            return Err(anyhow!(
                "opened source file length {} does not match expected length {}",
                src_opened_meta.len(),
                req.expected_length
            ));
        }
        if let (Some(sym_id), Some(open_id)) = (
            file_identity(&src_symlink_meta),
            file_identity(&src_opened_meta),
        ) {
            if sym_id != open_id {
                return Err(anyhow!(
                    "source file identity changed between path check and open: {:?}",
                    req.source_path
                ));
            }
        }

        // Name the staging file in the ledger *before* creating it.
        //
        // The other order leaves a gap the size of one syscall: a kill between
        // `create_new` and the update leaves a part the size of the finished
        // download that no row names, so the retry cannot tell it from a live
        // attempt's and the startup sweep skips it as unattributed. Recording
        // first inverts the gap — a kill then leaves a row naming a file that
        // does not exist, which the retry clears harmlessly.
        conn.execute(
            "UPDATE download_publish_jobs SET private_temp_path = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
            params![temp_path_str, job_id],
        )?;

        let mut dst_file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temp_path)
            .with_context(|| format!("cannot create temporary staging file {:?}", temp_path))?;

        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        let mut total_bytes: u64 = 0;

        loop {
            let read_bytes = src_file.read(&mut buffer)?;
            if read_bytes == 0 {
                break;
            }
            hasher.update(&buffer[..read_bytes]);
            dst_file.write_all(&buffer[..read_bytes])?;
            total_bytes += read_bytes as u64;
            #[cfg(test)]
            {
                // V2: the copy is the one stage with no SQL to hang an external
                // kill on, and a full device is not something a test can
                // arrange. Both leave the same question behind — is the final
                // name ever a half file, and does the next run only claim the
                // artifact this job can account for?
                if let Some(error) =
                    publish_faults::maybe_write_error(req.source_job_id, total_bytes)
                {
                    return Err(error.into());
                }
                publish_faults::maybe_abort("copy", req.source_job_id);
            }
        }

        dst_file.sync_all()?;
        let computed_hash = hasher.finalize().to_hex().to_string();

        if total_bytes != req.expected_length {
            return Err(anyhow!(
                "copied byte count {} does not match expected length {}",
                total_bytes,
                req.expected_length
            ));
        }

        if !computed_hash.eq_ignore_ascii_case(req.expected_blake3) {
            return Err(anyhow!(
                "blake3 hash mismatch: computed {}, expected {}",
                computed_hash,
                req.expected_blake3
            ));
        }

        // Post-copy verification on the opened file handle:
        // Ensure source length, inode identity, and mtime did not change during copy.
        let src_post_copy_meta = src_file.metadata().with_context(|| {
            format!(
                "cannot query metadata of source after copy {:?}",
                req.source_path
            )
        })?;
        if src_post_copy_meta.len() != req.expected_length {
            return Err(anyhow!(
                "source file length changed during copy from {} to {}",
                req.expected_length,
                src_post_copy_meta.len()
            ));
        }
        if let (Some(open_id), Some(post_id)) = (
            file_identity(&src_opened_meta),
            file_identity(&src_post_copy_meta),
        ) {
            if open_id != post_id {
                return Err(anyhow!(
                    "source file identity changed during copy: {:?}",
                    req.source_path
                ));
            }
        }
        if let (Ok(m1), Ok(m2)) = (src_opened_meta.modified(), src_post_copy_meta.modified()) {
            if m1 != m2 {
                return Err(anyhow!(
                    "source file modification time changed during copy: {:?}",
                    req.source_path
                ));
            }
        }

        // Verify source and target do not share an inode (avoid hard link sharing).
        let dst_meta = dst_file.metadata()?;
        if let (Some(src_id), Some(dst_id)) =
            (file_identity(&src_opened_meta), file_identity(&dst_meta))
        {
            if src_id == dst_id {
                return Err(anyhow!(
                    "source and destination must not share the same inode"
                ));
            }
        }

        Ok((computed_hash, total_bytes))
    })();

    let (computed_hash, total_bytes) = match copy_result {
        Ok(ok) => ok,
        Err(err) => {
            let _ = std::fs::remove_file(&temp_path);
            let err_msg = err.to_string();
            let _ = conn.execute(
                "UPDATE download_publish_jobs SET stage = 'failed', error_message = ?1, private_temp_path = '', updated_at = strftime('%s','now') WHERE id = ?2",
                params![err_msg, job_id],
            );
            return Err(err);
        }
    };

    // Update job stage to staged.
    conn.execute(
        "UPDATE download_publish_jobs SET stage = 'staged', private_temp_path = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
        params![temp_path_str, job_id],
    )?;

    // 5. Atomic publish. `link` refuses an occupied target atomically, so a
    // file created between the ledger lookup and here is never replaced: it
    // fails the publish instead.
    let pre_pub_meta = std::fs::symlink_metadata(target_parent).with_context(|| {
        format!(
            "parent directory missing before publish {:?}",
            target_parent
        )
    })?;
    if pre_pub_meta.file_type().is_symlink() || !pre_pub_meta.is_dir() {
        let _ = std::fs::remove_file(&temp_path);
        return Err(anyhow!(
            "target parent directory cannot be a symlink or non-directory: {:?}",
            target_parent
        ));
    }

    let target_file_name = req
        .target_path
        .file_name()
        .ok_or_else(|| anyhow!("target path has no file name: {:?}", req.target_path))?;
    let bound_target_path = bound_dir.target_file_path(target_file_name);

    if let Err(err) = publish_noreplace(&temp_path, &bound_target_path) {
        let _ = std::fs::remove_file(&temp_path);
        let err_msg = err.to_string();
        let _ = conn.execute(
            "UPDATE download_publish_jobs SET stage = 'failed', error_message = ?1, private_temp_path = '', updated_at = strftime('%s','now') WHERE id = ?2",
            params![err_msg, job_id],
        );
        return Err(err);
    }

    #[cfg(test)]
    if let Some(error) = publish_faults::maybe_fsync_error(req.source_job_id) {
        // V2: the sync is what makes the published name durable, and refusing
        // it is not the same as refusing the publish — the bytes are already
        // under their final name. On Windows `fsync_dir` is a no-op, so this is
        // injected rather than arranged.
        let message = format!("failed to sync directory to disk: {error:#}");
        let _ = conn.execute(
            "UPDATE download_publish_jobs SET error_message = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
            params![message, job_id],
        );
        return Err(anyhow!(message));
    }

    if let Err(err) = fsync_dir(target_parent) {
        let err_msg = format!("failed to sync directory to disk: {err:#}");
        let _ = conn.execute(
            "UPDATE download_publish_jobs SET error_message = ?1, updated_at = strftime('%s','now') WHERE id = ?2",
            params![err_msg, job_id],
        );
        return Err(err);
    }

    // 6. Update stage to published.
    conn.execute(
        "UPDATE download_publish_jobs SET stage = 'published', private_temp_path = '', error_message = NULL, updated_at = strftime('%s','now') WHERE id = ?1",
        params![job_id],
    )?;

    Ok(PublishResult {
        job_id,
        published_path: req.target_path.to_path_buf(),
        stage: PublishStage::Published,
        blake3_hash: computed_hash,
        file_length: total_bytes,
    })
}

/// Mark a published job as ingested into the media library with its resulting `item_id`.
///
/// The ingest handoff runs from more than one place (the scan's link pass is one
/// of them), so the same fact can arrive twice. A repeat that names the same item
/// is that same fact and succeeds; only a job that is neither published nor
/// already ingested under this item is an error. Without that, a second pass
/// would abort on a row the first one had already advanced and leave the rest of
/// its batch unrecorded.
pub fn mark_job_ingested(conn: &Connection, job_id: i64, item_id: i64) -> Result<()> {
    let has_error_col = conn
        .prepare("PRAGMA table_info(download_publish_jobs)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .any(|col| col.as_deref() == Ok("error_message"));

    let update_sql = if has_error_col {
        "UPDATE download_publish_jobs SET stage = 'ingested', item_id = ?1, error_message = '', updated_at = strftime('%s','now')
         WHERE id = ?2 AND stage = 'published'"
    } else {
        "UPDATE download_publish_jobs SET stage = 'ingested', item_id = ?1, updated_at = strftime('%s','now')
         WHERE id = ?2 AND stage = 'published'"
    };
    let updated = conn.execute(update_sql, params![item_id, job_id])?;
    if updated > 0 {
        return Ok(());
    }
    let existing: Option<(String, Option<i64>)> = conn
        .query_row(
            "SELECT stage, item_id FROM download_publish_jobs WHERE id = ?1",
            params![job_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((stage, item)) if stage == "ingested" && item == Some(item_id) => {
            if has_error_col {
                conn.execute(
                    "UPDATE download_publish_jobs SET error_message = '', updated_at = strftime('%s','now')
                     WHERE id = ?1 AND error_message != ''",
                    params![job_id],
                )?;
            }
            Ok(())
        }
        Some((stage, _)) => Err(anyhow!(
            "cannot mark job {job_id} ingested: job is in stage {stage}"
        )),
        None => Err(anyhow!("cannot mark job {job_id} ingested: no such job")),
    }
}

/// Fetch a recorded publish job by ID.
pub fn get_publish_job(conn: &Connection, job_id: i64) -> Result<Option<PublishJob>> {
    conn.query_row(
        "SELECT id, engine, source_job_id, manifest_version, source_identity, expected_length,
                expected_blake3, target_path, private_temp_path, stage, item_id, error_message,
                created_at, updated_at
         FROM download_publish_jobs WHERE id = ?1",
        params![job_id],
        |row| {
            let stage_str: String = row.get(9)?;
            let stage = PublishStage::parse(&stage_str).unwrap_or(PublishStage::Failed);
            Ok(PublishJob {
                id: row.get(0)?,
                engine: row.get(1)?,
                source_job_id: row.get(2)?,
                manifest_version: row.get(3)?,
                source_identity: row.get(4)?,
                expected_length: row.get::<_, i64>(5)? as u64,
                expected_blake3: row.get(6)?,
                target_path: row.get(7)?,
                private_temp_path: row.get(8)?,
                stage,
                item_id: row.get(10)?,
                error_message: row.get(11)?,
                created_at: row.get(12)?,
                updated_at: row.get(13)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

/// Query jobs by engine with limit.
pub fn list_publish_jobs_by_engine(
    conn: &Connection,
    engine: &str,
    limit: i64,
) -> Result<Vec<PublishJob>> {
    let mut stmt = conn.prepare(
        "SELECT id, engine, source_job_id, manifest_version, source_identity, expected_length,
                expected_blake3, target_path, private_temp_path, stage, item_id, error_message,
                created_at, updated_at
         FROM download_publish_jobs WHERE engine = ?1 ORDER BY id DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![engine, limit], |row| {
        let stage_str: String = row.get(9)?;
        let stage = PublishStage::parse(&stage_str).unwrap_or(PublishStage::Failed);
        Ok(PublishJob {
            id: row.get(0)?,
            engine: row.get(1)?,
            source_job_id: row.get(2)?,
            manifest_version: row.get(3)?,
            source_identity: row.get(4)?,
            expected_length: row.get::<_, i64>(5)? as u64,
            expected_blake3: row.get(6)?,
            target_path: row.get(7)?,
            private_temp_path: row.get(8)?,
            stage,
            item_id: row.get(10)?,
            error_message: row.get(11)?,
            created_at: row.get(12)?,
            updated_at: row.get(13)?,
        })
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup_test_db() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);
             INSERT INTO items (id) VALUES (42);",
        )
        .unwrap();
        ensure_ingest_publish_schema(&conn).unwrap();
        conn
    }

    /// Age a file so the sweep sees it as old enough to reclaim.
    fn age_file(path: &Path, age: Duration) {
        let old = SystemTime::now().checked_sub(age).unwrap();
        let handle = OpenOptions::new().write(true).open(path).unwrap();
        handle.set_modified(old).unwrap();
    }

    /// The staging file a killed publish leaves behind is reclaimed, and
    /// nothing else is.
    ///
    /// The copy creates `.gallery_publish_<uuid>.part` beside the target and
    /// records it in the ledger only afterwards, so a kill in that window leaves
    /// a file the size of the finished download that no other code path can
    /// find: the scanner skips dot-files and reconciliation is ledger-driven.
    #[test]
    fn orphaned_publish_staging_is_reclaimed_but_live_work_is_not() {
        let conn = setup_test_db();
        let media_dir = tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let day = ORPHANED_PART_MIN_AGE;

        let orphan = media_dir.path().join(".gallery_publish_aaaa.part");
        std::fs::write(&orphan, b"orphaned").unwrap();
        age_file(&orphan, day + Duration::from_secs(60));

        // Young and unreferenced: a copy can be running right now, and the job
        // row is written one step after the file appears.
        let fresh = media_dir.path().join(".gallery_publish_bbbb.part");
        std::fs::write(&fresh, b"in flight").unwrap();

        // Old and referenced: the ledger knows this one, so it is live work.
        let referenced = media_dir.path().join(".gallery_publish_cccc.part");
        std::fs::write(&referenced, b"recorded").unwrap();
        age_file(&referenced, day + Duration::from_secs(60));
        conn.execute(
            "INSERT INTO download_publish_jobs
                 (engine, source_job_id, source_identity, manifest_version, stage,
                  target_path, private_temp_path, expected_length, expected_blake3)
             VALUES ('pawchive', 'job-1', 'res-1', 1, 'staged', '/media/x.jpg', ?1, 8, '')",
            params![referenced.to_string_lossy().to_string()],
        )
        .unwrap();

        // Not a staging file at all.
        let other = media_dir.path().join("keep.part");
        std::fs::write(&other, b"unrelated").unwrap();
        age_file(&other, day + Duration::from_secs(60));

        let sweep = sweep_orphaned_publish_parts(&conn, &roots, day).unwrap();

        assert_eq!(sweep.files, 1, "{sweep:?}");
        assert_eq!(sweep.bytes, b"orphaned".len() as u64);
        assert!(!orphan.exists(), "the orphaned staging file is reclaimed");
        assert!(fresh.exists(), "a recent staging file may be live");
        assert!(
            referenced.exists(),
            "a staging file the ledger names is live work"
        );
        assert!(other.exists(), "only staging files are considered");
    }

    #[test]
    fn publish_completed_file_succeeds_and_creates_independent_media() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();

        let source_path = staging_dir.path().join("downloaded.bin");
        let content = b"Pawchive download payload verification test 1234567890";
        std::fs::write(&source_path, content).unwrap();

        let expected_blake3 = blake3::hash(content).to_hex().to_string();
        let expected_length = content.len() as u64;

        let target_dir = media_dir.path().join("artist_one");
        let target_path = target_dir.join("photo.bin");

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-101",
            manifest_version: 1,
            source_identity: "file-001",
            source_path: &source_path,
            expected_length,
            expected_blake3: &expected_blake3,
            target_path: &target_path,
        };

        let result = publish_completed_file(&conn, &req, &roots).unwrap();
        assert_eq!(result.stage, PublishStage::Published);
        assert_eq!(result.file_length, expected_length);
        assert_eq!(result.blake3_hash, expected_blake3);
        assert!(target_path.is_file());
        assert_eq!(std::fs::read(&target_path).unwrap(), content);

        // Verify DB record
        let job = get_publish_job(&conn, result.job_id).unwrap().unwrap();
        assert_eq!(job.stage, PublishStage::Published);
        assert_eq!(job.engine, "pawchive");

        // Mark as ingested
        mark_job_ingested(&conn, result.job_id, 42).unwrap();
        let ingested_job = get_publish_job(&conn, result.job_id).unwrap().unwrap();
        assert_eq!(ingested_job.stage, PublishStage::Ingested);
        assert_eq!(ingested_job.item_id, Some(42));
    }

    /// §6.3: the publish takes a reservation on the group that owns its target
    /// directory, and gives it back whatever happens. A publish that started
    /// while 整理 was mid-move is refused instead of landing a file in a
    /// directory that is about to be renamed away.
    #[test]
    fn publish_respects_the_group_move_fence_and_always_releases_it() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"fenced publish payload";
        let source_path = staging_dir.path().join("downloaded.bin");
        std::fs::write(&source_path, content).unwrap();
        let expected_blake3 = blake3::hash(content).to_hex().to_string();

        let target_dir = media_dir.path().join("artist_one").join("2026-09-14 A");
        std::fs::create_dir_all(&target_dir).unwrap();
        let target_path = target_dir.join("photo.bin");
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        // Register the group that owns the target directory, the same way the
        // grouping does, then put a move intent on it.
        crate::pawchive::ensure_pawchive_schema(&conn).unwrap();
        crate::pawchive_groups::ensure_content_group_schema(&conn).unwrap();
        let artist_root = media_dir.path().to_string_lossy().to_string();
        crate::pawchive_groups::apply_grouping(
            &conn,
            "artist-scope:A",
            &artist_root,
            &crate::pawchive_groups::group_index(&[crate::pawchive_groups::IndexEntry::as_file(
                "artist_one/2026-09-14 A/1.png",
                &artist_root,
                10,
            )]),
        )
        .unwrap();
        let group_id = crate::pawchive_groups::list_content_groups(&conn, None, false)
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
            .group_id;
        crate::pawchive_groups::begin_group_move_intent(&conn, &group_id, "folder_rename").unwrap();

        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-fence",
            manifest_version: 1,
            source_identity: "file-001",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &expected_blake3,
            target_path: &target_path,
        };
        let refused = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(
            refused.to_string().contains("正在整理移动中"),
            "the publish must name the reason: {refused}"
        );
        assert!(
            !target_path.exists(),
            "nothing is written while 整理 owns the directory"
        );
        // 整理 still owns it: the failed publish left no reservation behind.
        assert!(
            crate::pawchive_groups::begin_group_move_intent(&conn, &group_id, "folder_rename")
                .is_err(),
            "the intent must still be pending after a refused publish"
        );

        // Once the move finishes, the same publish goes through — and releases
        // its reservation on the way out, so 整理 is not blocked afterwards.
        crate::pawchive_groups::finish_group_move_intent(
            &conn,
            &group_id,
            crate::pawchive_groups::GROUP_MOVE_INTENT_APPLIED,
            "",
        )
        .unwrap();
        let result = publish_completed_file(&conn, &req, &roots).unwrap();
        assert_eq!(result.stage, PublishStage::Published);
        assert!(target_path.is_file());

        // A publish that fails after taking the reservation must not leave it
        // behind either: a wrong length is refused before any file is written.
        let bad = IngestPublishRequest {
            source_identity: "file-002",
            expected_length: content.len() as u64 + 1,
            ..req
        };
        let _ = publish_completed_file(&conn, &bad, &roots).unwrap_err();
        crate::pawchive_groups::begin_group_move_intent(&conn, &group_id, "folder_rename").unwrap();
    }

    /// The ingest handoff can report the same fact twice. The repeat succeeds
    /// rather than aborting the batch it arrives in; a different item, or a job
    /// that never reached `published`, is still an error.
    #[test]
    fn mark_job_ingested_is_idempotent_for_the_same_item() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"ingest payload";
        let expected_length = content.len() as u64;
        let expected_blake3 = blake3::hash(content).to_hex().to_string();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();
        let target_path = media_dir.path().join("payload.bin");
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "7:1",
            manifest_version: 1,
            source_identity: "/data/payload.bin",
            source_path: &source_path,
            expected_length,
            expected_blake3: &expected_blake3,
            target_path: &target_path,
        };
        let result = publish_completed_file(&conn, &req, &roots).unwrap();

        mark_job_ingested(&conn, result.job_id, 42).unwrap();
        mark_job_ingested(&conn, result.job_id, 42).unwrap();
        let job = get_publish_job(&conn, result.job_id).unwrap().unwrap();
        assert_eq!(job.stage, PublishStage::Ingested);
        assert_eq!(job.item_id, Some(42));

        let mismatch = mark_job_ingested(&conn, result.job_id, 43).unwrap_err();
        assert!(
            mismatch.to_string().contains("stage ingested"),
            "{mismatch}"
        );
        let missing = mark_job_ingested(&conn, result.job_id + 999, 42).unwrap_err();
        assert!(missing.to_string().contains("no such job"), "{missing}");
    }

    #[test]
    fn publish_rejects_unauthorized_target_root() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let unauthorized_dir = tempdir().unwrap();
        let allowed_dir = tempdir().unwrap();

        let source_path = staging_dir.path().join("file.bin");
        std::fs::write(&source_path, b"test data").unwrap();

        let roots = MediaRoots::identical(
            vec![allowed_dir.path().to_string_lossy().to_string()],
            vec!["Allowed".to_string()],
        );

        let target_path = unauthorized_dir.path().join("photo.bin");

        let req = IngestPublishRequest {
            engine: "jdownloader",
            source_job_id: "jd-1",
            manifest_version: 1,
            source_identity: "file-1",
            source_path: &source_path,
            expected_length: 9,
            expected_blake3: &blake3::hash(b"test data").to_hex().to_string(),
            target_path: &target_path,
        };

        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(err.to_string().contains("outside authorized media roots"));
    }

    #[test]
    fn publish_rejects_existing_target_without_overwrite() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();

        let source_path = staging_dir.path().join("source.bin");
        std::fs::write(&source_path, b"new content").unwrap();

        let target_path = media_dir.path().join("existing.bin");
        std::fs::write(&target_path, b"already exists").unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-conflict",
            manifest_version: 1,
            source_identity: "file-conflict",
            source_path: &source_path,
            expected_length: 11,
            expected_blake3: &blake3::hash(b"new content").to_hex().to_string(),
            target_path: &target_path,
        };

        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(err.to_string().contains("already exists"));
        // Existing content preserved
        assert_eq!(std::fs::read(&target_path).unwrap(), b"already exists");
    }

    #[test]
    fn publish_detects_hash_mismatch_and_rolls_back() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();

        let source_path = staging_dir.path().join("corrupted.bin");
        std::fs::write(&source_path, b"actual content").unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        let target_path = media_dir.path().join("corrupted.bin");

        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-mismatch",
            manifest_version: 1,
            source_identity: "file-mismatch",
            source_path: &source_path,
            expected_length: 14,
            expected_blake3: "0000000000000000000000000000000000000000000000000000000000000000",
            target_path: &target_path,
        };

        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(err.to_string().contains("blake3 hash mismatch"));
        // Final file must not exist
        assert!(!target_path.exists());

        // Staged private files must be cleaned up
        let temp_files: Vec<_> = std::fs::read_dir(media_dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".gallery_publish_")
            })
            .collect();
        assert!(
            temp_files.is_empty(),
            "temporary files must be cleaned up on failure"
        );

        // Job status marked failed
        let jobs = list_publish_jobs_by_engine(&conn, "pawchive", 10).unwrap();
        assert_eq!(jobs.len(), 1);
        assert_eq!(jobs[0].stage, PublishStage::Failed);
    }

    /// Republishing the same job is how a crash between the publish and the
    /// ledger write is recovered. It used to be impossible: the target-exists
    /// check ran before the ledger lookup and rejected the retry outright.
    #[test]
    fn republishing_the_same_job_is_idempotent() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"idempotent publish payload";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_path = media_dir.path().join("artist/photo.bin");
        let expected_blake3 = blake3::hash(content).to_hex().to_string();
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-retry",
            manifest_version: 1,
            source_identity: "asset-1",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &expected_blake3,
            target_path: &target_path,
        };

        let first = publish_completed_file(&conn, &req, &roots).unwrap();
        let second = publish_completed_file(&conn, &req, &roots).unwrap();
        assert_eq!(second.job_id, first.job_id, "one ledger row, one job");
        assert_eq!(second.stage, PublishStage::Published);
        assert_eq!(std::fs::read(&target_path).unwrap(), content);
        assert_eq!(
            list_publish_jobs_by_engine(&conn, "pawchive", 10)
                .unwrap()
                .len(),
            1
        );
    }

    /// A file that appeared after the ledger lookup is never replaced. `link`
    /// refuses it atomically, where `exists()` + `rename` had a window.
    #[test]
    fn publish_never_replaces_a_file_that_appeared_at_the_target() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"our payload";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_path = media_dir.path().join("artist/photo.bin");
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-race",
            manifest_version: 1,
            source_identity: "asset-race",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &blake3::hash(content).to_hex().to_string(),
            target_path: &target_path,
        };
        // Register the job without publishing it, then have a stranger take the
        // target between the lookup and the link.
        let staged = media_dir.path().join("artist/.gallery_publish_race.part");
        std::fs::create_dir_all(staged.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"stranger").unwrap();
        std::fs::write(&target_path, b"stranger").unwrap();

        let err = publish_noreplace(&staged, &target_path).unwrap_err();
        assert!(err.to_string().contains("refuse to overwrite"), "{err}");
        assert_eq!(std::fs::read(&target_path).unwrap(), b"stranger");
        assert!(
            staged.exists(),
            "the staging file stays for the caller to clean"
        );

        // Through the public entry point the stranger's file is refused too.
        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(err.to_string().contains("refuse to overwrite"), "{err}");
        assert_eq!(std::fs::read(&target_path).unwrap(), b"stranger");
    }

    /// Atomic publish via `publish_noreplace` moves bytes and refuses an occupied name.
    /// It never creates or leaves an empty reservation placeholder.
    #[test]
    fn publish_noreplace_places_the_bytes_and_refuses_an_occupied_name() {
        let dir = tempdir().unwrap();
        let staged = dir.path().join(".gallery_publish_x.part");
        let target = dir.path().join("artist/photo.bin");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();
        std::fs::write(&staged, b"payload").unwrap();

        publish_noreplace(&staged, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        assert!(
            !staged.exists(),
            "the staging name must not survive a completed publish"
        );

        // A second publisher loses the race and leaves the winner alone.
        let second = dir.path().join(".gallery_publish_y.part");
        std::fs::write(&second, b"intruder").unwrap();
        let error = publish_noreplace(&second, &target).unwrap_err();
        assert!(error.to_string().contains("refuse to overwrite"), "{error}");
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        assert!(
            second.exists(),
            "the staging file stays for the caller to clean"
        );
        assert!(!target.metadata().unwrap().is_dir());
    }

    /// A failed publish must never leave an empty placeholder file at target.
    #[test]
    fn publish_noreplace_never_leaves_empty_file_on_failure() {
        let dir = tempdir().unwrap();
        let staged = dir.path().join("missing.part");
        let target = dir.path().join("artist/photo.bin");
        std::fs::create_dir_all(target.parent().unwrap()).unwrap();

        let error = publish_noreplace(&staged, &target).unwrap_err();
        let message = error.to_string();
        assert!(
            !target.exists(),
            "a failed publish must never leave an empty placeholder file at target, got error: {message}"
        );
    }

    #[test]
    fn authorized_publish_dir_refuses_path_outside_media_roots() {
        let media_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let err = open_authorized_publish_dir(outside_dir.path(), &roots).unwrap_err();
        assert!(
            err.to_string().contains("outside authorized media roots")
                || err.to_string().contains("escaped"),
            "{err}"
        );
    }

    #[test]
    fn authorized_publish_dir_creates_intermediate_directories_safely() {
        let media_dir = tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_parent = media_dir.path().join("sub1/sub2/sub3");
        let bound = open_authorized_publish_dir(&target_parent, &roots).unwrap();
        assert_eq!(bound.dir_path, target_parent);
        assert!(target_parent.is_dir());
    }

    /// A crash between the final link and the `published` ledger write leaves
    /// the target in place while the row still says `staged`. Retrying used to
    /// fail on the now-existing target, so the delivery could never be recorded
    /// — and the resource stayed unproven for good.
    #[test]
    fn a_retry_recovers_a_publish_that_crashed_after_the_final_link() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"linked but unrecorded payload";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_path = media_dir.path().join("artist/photo.bin");
        let expected_blake3 = blake3::hash(content).to_hex().to_string();
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-crash",
            manifest_version: 1,
            source_identity: "asset-crash",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &expected_blake3,
            target_path: &target_path,
        };
        let first = publish_completed_file(&conn, &req, &roots).unwrap();

        // The crash window: the link happened, the ledger write did not.
        conn.execute(
            "UPDATE download_publish_jobs SET stage = 'staged' WHERE id = ?1",
            params![first.job_id],
        )
        .unwrap();
        assert!(target_path.is_file());

        let recovered = publish_completed_file(&conn, &req, &roots).unwrap();
        assert_eq!(recovered.job_id, first.job_id, "the same ledger row");
        assert_eq!(recovered.stage, PublishStage::Published);
        assert_eq!(std::fs::read(&target_path).unwrap(), content);
        assert_eq!(
            get_publish_job(&conn, first.job_id).unwrap().unwrap().stage,
            PublishStage::Published
        );

        // The same recovery must not adopt somebody else's file: only content
        // that is exactly this job's artifact is claimed.
        std::fs::write(&target_path, vec![b'x'; content.len()]).unwrap();
        conn.execute(
            "UPDATE download_publish_jobs SET stage = 'staged' WHERE id = ?1",
            params![first.job_id],
        )
        .unwrap();
        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(err.to_string().contains("refuse to overwrite"), "{err}");
        assert_eq!(
            get_publish_job(&conn, first.job_id).unwrap().unwrap().stage,
            PublishStage::Staged,
            "the row is not advanced for a file this job did not write"
        );
    }

    /// A retry after an interrupted copy replaces its own part file instead of
    /// leaving one behind per attempt.
    #[test]
    fn a_retry_clears_the_staging_file_of_the_interrupted_attempt() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"payload with an abandoned part";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_path = media_dir.path().join("artist/photo.bin");
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-part",
            manifest_version: 1,
            source_identity: "asset-part",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &blake3::hash(content).to_hex().to_string(),
            target_path: &target_path,
        };
        let first = publish_completed_file(&conn, &req, &roots).unwrap();

        // Rewind to the state an interrupted copy leaves: staged, part on disk,
        // final name not yet linked.
        let orphan = media_dir.path().join("artist/.gallery_publish_orphan.part");
        std::fs::write(&orphan, b"half a copy").unwrap();
        std::fs::remove_file(&target_path).unwrap();
        conn.execute(
            "UPDATE download_publish_jobs SET stage = 'staged', private_temp_path = ?1 WHERE id = ?2",
            params![orphan.to_string_lossy(), first.job_id],
        )
        .unwrap();

        publish_completed_file(&conn, &req, &roots).unwrap();
        assert!(!orphan.exists(), "the abandoned part is cleaned up");
        assert_eq!(std::fs::read(&target_path).unwrap(), content);
    }

    #[test]
    fn parent_directory_outside_authorized_roots_is_rejected() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"payload for unauthorized parent check";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let outside_parent = tempdir().unwrap();
        let target_path = outside_parent.path().join("sub/photo.bin");
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-outside-parent",
            manifest_version: 1,
            source_identity: "asset-outside-parent",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &blake3::hash(content).to_hex().to_string(),
            target_path: &target_path,
        };
        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(
            err.to_string().contains("outside authorized media roots"),
            "{err}"
        );
    }

    /// The publish must follow the directory *handle*, not the path it was
    /// authorised through.
    ///
    /// This is the fault injection RV1's acceptance names and Windows cannot
    /// run: after `open_authorized_publish_dir` returns, the parent directory is
    /// moved away and its name is taken by a symlink pointing outside the
    /// authorized root. Every later step resolves through the handle
    /// (`/proc/self/fd/<n>/<name>`), so the bytes land in the moved-aside
    /// directory — still inside the root — and the escape target stays empty.
    ///
    /// The test is load-bearing: it also asserts that the *path* was really
    /// redirected (`target_parent` now canonicalises to the outside directory),
    /// so a regression that goes back to `dir_path.join(name)` puts the payload
    /// in the outside directory and fails.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_replaced_parent_path_cannot_redirect_a_publish_outside_the_authorized_root() {
        let media_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        // `day` does not exist yet: it is created by the bound walk itself, which
        // is what the plan describes as "创建目录相对同一受验证目录执行".
        let target_parent = media_dir.path().join("artist/day");
        std::fs::create_dir_all(media_dir.path().join("artist")).unwrap();
        let bound = open_authorized_publish_dir(&target_parent, &roots).unwrap();
        assert!(target_parent.is_dir());

        // The race window: the checked directory is moved away and its name is
        // handed to a link that points outside the authorized root.
        let moved = media_dir.path().join("artist/day.moved");
        std::fs::rename(&target_parent, &moved).unwrap();
        std::os::unix::fs::symlink(outside_dir.path(), &target_parent).unwrap();
        let redirected = crate::fs_util::safe_canonicalize(&target_parent).unwrap();
        assert_eq!(
            redirected,
            crate::fs_util::safe_canonicalize(outside_dir.path()).unwrap(),
            "the path really now resolves outside the authorized root"
        );

        // Staging and the final publish both go through the handle.
        let payload = b"handle-bound publish payload";
        let temp_name = ".gallery_publish_race.part";
        let temp_path = bound.temp_file_path(temp_name);
        std::fs::write(&temp_path, payload).unwrap();
        assert!(
            moved.join(temp_name).is_file(),
            "staging must land in the directory the handle names"
        );
        assert!(
            !outside_dir.path().join(temp_name).exists(),
            "staging must not be written through the replaced path"
        );

        let target_name = std::ffi::OsStr::new("photo.bin");
        publish_noreplace(&temp_path, &bound.target_file_path(target_name)).unwrap();
        assert_eq!(std::fs::read(moved.join("photo.bin")).unwrap(), payload);
        assert!(
            !outside_dir.path().join("photo.bin").exists(),
            "no file may be published outside the authorized root"
        );
        assert_eq!(
            std::fs::read_dir(outside_dir.path()).unwrap().count(),
            0,
            "the escape target stays empty"
        );
        assert!(
            !target_parent.join("photo.bin").exists(),
            "nothing is visible through the replaced path either"
        );
    }

    /// `renameat2(RENAME_NOREPLACE)` is the Linux half of the atomic publish: it
    /// must move the bytes when the name is free and refuse the name when it is
    /// taken, without ever replacing the occupant.
    #[cfg(target_os = "linux")]
    #[test]
    fn rename_noreplace_linux_moves_once_and_refuses_an_occupied_name() {
        let dir = tempdir().unwrap();
        let temp = dir.path().join("staged.part");
        let target = dir.path().join("final.bin");
        std::fs::write(&temp, b"payload").unwrap();

        rename_noreplace_linux(&temp, &target).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"payload");
        assert!(!temp.exists(), "the staged name is consumed by the rename");

        let second = dir.path().join("staged2.part");
        std::fs::write(&second, b"intruder").unwrap();
        let error = rename_noreplace_linux(&second, &target).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists, "{error}");
        assert_eq!(
            std::fs::read(&target).unwrap(),
            b"payload",
            "the occupant is never replaced"
        );
        assert!(second.exists(), "the loser keeps its staging file");
    }

    /// A symlink standing in for an ancestor is refused before anything is
    /// written, rather than being walked through.
    #[cfg(target_os = "linux")]
    #[test]
    fn a_symlinked_ancestor_is_refused_before_any_write() {
        let media_dir = tempdir().unwrap();
        let outside_dir = tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        let linked = media_dir.path().join("artist/day");
        std::fs::create_dir_all(media_dir.path().join("artist")).unwrap();
        std::os::unix::fs::symlink(outside_dir.path(), &linked).unwrap();

        let error = open_authorized_publish_dir(&linked.join("deeper"), &roots).unwrap_err();
        assert!(
            error.to_string().contains("outside authorized media roots")
                || error.to_string().contains("escaped")
                || error
                    .to_string()
                    .contains("failed to open existing directory component"),
            "{error}"
        );
        assert_eq!(
            std::fs::read_dir(outside_dir.path()).unwrap().count(),
            0,
            "a refused publish writes nothing through the link"
        );
        assert!(
            !linked.join("deeper").exists(),
            "no directory is created behind the link"
        );
    }

    #[test]
    fn source_file_metadata_length_mismatch_before_copy_is_rejected() {
        let conn = setup_test_db();
        let staging_dir = tempdir().unwrap();
        let media_dir = tempdir().unwrap();
        let content = b"short";
        let source_path = staging_dir.path().join("payload.bin");
        std::fs::write(&source_path, content).unwrap();

        let roots = MediaRoots::identical(
            vec![media_dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let target_path = media_dir.path().join("artist/photo.bin");
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-len-mismatch",
            manifest_version: 1,
            source_identity: "asset-len-mismatch",
            source_path: &source_path,
            expected_length: 100, // expected 100, actual 5
            expected_blake3: &blake3::hash(content).to_hex().to_string(),
            target_path: &target_path,
        };
        let err = publish_completed_file(&conn, &req, &roots).unwrap_err();
        assert!(
            err.to_string().contains("does not match expected length"),
            "{err}"
        );
    }
}

/// V2 — the publish windows: killed during the staging copy, killed after the
/// final link but before the ledger write, a full device, and a refused
/// directory sync.
///
/// What all four have to prove is the same thing: the final name is never a
/// half file, and a restart only ever claims the artifact this job can account
/// for. The two kills are two real processes — the victim is this test binary
/// run again — because the state that matters is the one no destructor gets to
/// clean up. The two failures are injected rather than arranged: a full device
/// is not something a test can produce, and `fsync_dir` is a no-op on Windows
/// entirely.
#[cfg(test)]
mod publish_crash_tests {
    use super::*;
    use crate::test_support::{EnvVar, ENV_LOCK};
    use rusqlite::hooks::Authorization;
    use std::time::{Duration, Instant};

    const VICTIM_DB_ENV: &str = "GALLERY_PUBLISH_VICTIM_DB";
    const VICTIM_ROOT_ENV: &str = "GALLERY_PUBLISH_VICTIM_ROOT";
    const VICTIM_SOURCE_ENV: &str = "GALLERY_PUBLISH_VICTIM_SOURCE";
    const VICTIM_TARGET_ENV: &str = "GALLERY_PUBLISH_VICTIM_TARGET";
    const VICTIM_MARKER_ENV: &str = "GALLERY_PUBLISH_VICTIM_MARKER";
    const VICTIM_MODE_ENV: &str = "GALLERY_PUBLISH_VICTIM_MODE";

    struct Fixture {
        _dir: tempfile::TempDir,
        db_path: PathBuf,
        root: String,
        source: PathBuf,
        target: PathBuf,
        content: Vec<u8>,
        digest: String,
    }

    /// A media root with one authorized directory, a staging source outside it,
    /// and a file-backed ledger — a file and not memory, because the victim is
    /// another process and has to open the same database.
    fn fixture(job: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let target_parent = media.join("artist").join("2026-01-01 day");
        std::fs::create_dir_all(&target_parent).unwrap();
        let staging = dir.path().join("staging");
        std::fs::create_dir_all(&staging).unwrap();
        let content = b"publish payload for the crash windows".to_vec();
        let source = staging.join("payload.bin");
        std::fs::write(&source, &content).unwrap();
        let target = target_parent.join("photo.bin");
        let db_path = dir.path().join("gallery.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            // `items` is what the publish ledger's schema hangs off — the
            // in-memory fixture every other publish test uses creates it too.
            conn.execute_batch("CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);")
                .unwrap();
            ensure_ingest_publish_schema(&conn).unwrap();
        }
        let digest = blake3::hash(&content).to_hex().to_string();
        // An unrelated file the recovery must leave alone.
        std::fs::write(target_parent.join("someone-elses.bin"), b"not ours").unwrap();
        let _ = job;
        Fixture {
            _dir: dir,
            db_path,
            root: media.to_string_lossy().replace('\\', "/"),
            source,
            target,
            content,
            digest,
        }
    }

    impl Fixture {
        fn roots(&self) -> MediaRoots {
            MediaRoots::identical(vec![self.root.clone()], vec!["Media".to_string()])
        }

        fn request<'a>(&'a self, job: &'a str) -> IngestPublishRequest<'a> {
            IngestPublishRequest {
                engine: "pawchive",
                source_job_id: job,
                manifest_version: 1,
                source_identity: "asset-1",
                source_path: &self.source,
                expected_length: self.content.len() as u64,
                expected_blake3: &self.digest,
                target_path: &self.target,
            }
        }

        fn stage(&self) -> String {
            let conn = Connection::open(&self.db_path).unwrap();
            conn.query_row(
                "SELECT stage FROM download_publish_jobs WHERE source_job_id = 'job-v2'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        }
    }

    /// The victim half. `mode` selects the window:
    ///
    /// - `after_link`: an authorizer aborts on the first statement prepared
    ///   once the final name exists, which is the ledger write that follows the
    ///   atomic link. Deterministic where an external kill at a microsecond
    ///   window is not, and it leaves the same state.
    /// - `copy`: the parent sets `GALLERY_PUBLISH_CRASH_AT=copy` and the
    ///   injection aborts inside the staging copy, which runs no SQL at all.
    fn run_publish_until_killed(
        db: &str,
        root: &str,
        source: &str,
        target: &str,
        marker: &str,
        mode: &str,
    ) {
        let conn = Connection::open(db).unwrap();
        let target_path = PathBuf::from(target);
        let marker_path = PathBuf::from(marker);
        if mode == "after_link" {
            conn.authorizer(Some(move |_: rusqlite::hooks::AuthContext<'_>| {
                if target_path.is_file() {
                    std::fs::write(&marker_path, b"killed").ok();
                    std::process::abort();
                }
                Authorization::Allow
            }));
        } else {
            std::fs::write(&marker_path, b"started").ok();
        }
        let source_path = PathBuf::from(source);
        let content = std::fs::read(&source_path).unwrap();
        let digest = blake3::hash(&content).to_hex().to_string();
        let target_path = PathBuf::from(target);
        let req = IngestPublishRequest {
            engine: "pawchive",
            source_job_id: "job-v2",
            manifest_version: 1,
            source_identity: "asset-1",
            source_path: &source_path,
            expected_length: content.len() as u64,
            expected_blake3: &digest,
            target_path: &target_path,
        };
        let roots = MediaRoots::identical(vec![root.to_string()], vec!["Media".to_string()]);
        let _ = publish_completed_file(&conn, &req, &roots);
        std::process::exit(17);
    }

    fn spawn_victim(
        test_path: &str,
        fixture: &Fixture,
        marker: &Path,
        mode: &str,
    ) -> std::process::ExitStatus {
        let mut command = std::process::Command::new(std::env::current_exe().unwrap());
        command
            .args([test_path, "--exact", "--nocapture"])
            .env(VICTIM_DB_ENV, &fixture.db_path)
            .env(VICTIM_ROOT_ENV, &fixture.root)
            .env(VICTIM_SOURCE_ENV, &fixture.source)
            .env(VICTIM_TARGET_ENV, &fixture.target)
            .env(VICTIM_MARKER_ENV, marker)
            .env(VICTIM_MODE_ENV, mode)
            .env("GALLERY_PUBLISH_FAULT_JOB", "job-v2");
        if mode == "copy" {
            command.env("GALLERY_PUBLISH_CRASH_AT", "copy");
        }
        let mut child = command.spawn().expect("spawn the victim process");
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if let Some(status) = child.try_wait().unwrap() {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "the victim never reached its crash window"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    fn victim_env() -> Option<(String, String, String, String, String, String)> {
        let db = std::env::var(VICTIM_DB_ENV).ok()?;
        let root = std::env::var(VICTIM_ROOT_ENV).ok()?;
        let source = std::env::var(VICTIM_SOURCE_ENV).ok()?;
        let target = std::env::var(VICTIM_TARGET_ENV).ok()?;
        let marker = std::env::var(VICTIM_MARKER_ENV).ok()?;
        let mode = std::env::var(VICTIM_MODE_ENV).ok()?;
        Some((db, root, source, target, marker, mode))
    }

    fn part_files(dir: &Path) -> Vec<PathBuf> {
        std::fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.path())
            .filter(|path| {
                path.file_name()
                    .and_then(|name| name.to_str())
                    .is_some_and(|name| name.starts_with(".gallery_publish_"))
            })
            .collect()
    }

    /// Killed after the atomic link, before the ledger write.
    ///
    /// The bytes are under their final name and the row still says `staged`, so
    /// the next run has to recognise its own artifact by content and finish the
    /// ledger — and must not recognise anything else that happens to be there.
    #[test]
    fn a_publish_killed_after_the_final_link_is_completed_by_the_next_run() {
        const TEST_PATH: &str =
            "ingest_publish::publish_crash_tests::a_publish_killed_after_the_final_link_is_completed_by_the_next_run";
        if let Some((db, root, source, target, marker, mode)) = victim_env() {
            run_publish_until_killed(&db, &root, &source, &target, &marker, &mode);
            return;
        }

        let _env_lock = ENV_LOCK.lock().unwrap();
        let fixture = fixture("job-v2");
        let marker = fixture._dir.path().join("killed.marker");
        let status = spawn_victim(TEST_PATH, &fixture, &marker, "after_link");
        assert!(!status.success(), "the victim must not publish: {status}");
        assert!(
            marker.is_file(),
            "the victim must die in the window, not before it: {status}"
        );

        // The window: whole bytes under the final name, ledger not told.
        assert!(fixture.target.is_file());
        assert_eq!(std::fs::read(&fixture.target).unwrap(), fixture.content);
        assert_eq!(fixture.stage(), "staged");

        let conn = Connection::open(&fixture.db_path).unwrap();
        let result = publish_completed_file(&conn, &fixture.request("job-v2"), &fixture.roots())
            .expect("the same job completes the ledger for the file it already published");
        assert_eq!(result.stage, PublishStage::Published);
        assert_eq!(fixture.stage(), "published");
        assert_eq!(
            std::fs::read(&fixture.target).unwrap(),
            fixture.content,
            "the recovery must not rewrite the published file"
        );
        let copies = std::fs::read_dir(fixture.target.parent().unwrap())
            .unwrap()
            .flatten()
            .count();
        assert_eq!(
            copies, 2,
            "one published file, one unrelated file: {copies}"
        );
        assert!(
            fixture
                .target
                .parent()
                .unwrap()
                .join("someone-elses.bin")
                .is_file(),
            "a file this job cannot account for is left alone"
        );

        // Idempotent: a third call changes nothing.
        let again = publish_completed_file(&conn, &fixture.request("job-v2"), &fixture.roots())
            .expect("repeating the completed publish is idempotent");
        assert_eq!(again.job_id, result.job_id);
    }

    /// Killed inside the staging copy.
    ///
    /// The final name must not exist — a half file under it is exactly what
    /// this window is about — and the staging part the kill left is reclaimed by
    /// the retry rather than accumulating one per attempt.
    #[test]
    fn a_publish_killed_in_the_staging_copy_leaves_no_file_under_the_final_name() {
        const TEST_PATH: &str =
            "ingest_publish::publish_crash_tests::a_publish_killed_in_the_staging_copy_leaves_no_file_under_the_final_name";
        if let Some((db, root, source, target, marker, mode)) = victim_env() {
            run_publish_until_killed(&db, &root, &source, &target, &marker, &mode);
            return;
        }

        let _env_lock = ENV_LOCK.lock().unwrap();
        let fixture = fixture("job-v2");
        let marker = fixture._dir.path().join("killed.marker");
        let status = spawn_victim(TEST_PATH, &fixture, &marker, "copy");
        assert!(!status.success(), "the victim must not publish: {status}");

        let target_parent = fixture.target.parent().unwrap().to_path_buf();
        assert!(
            !fixture.target.exists(),
            "a kill during the copy must not leave anything under the final name"
        );
        assert!(
            !part_files(&target_parent).is_empty(),
            "the kill has to land while the staging file exists, otherwise this proves nothing"
        );
        assert_eq!(fixture.stage(), "verified");

        let conn = Connection::open(&fixture.db_path).unwrap();
        let result = publish_completed_file(&conn, &fixture.request("job-v2"), &fixture.roots())
            .expect("the retry publishes the whole file");
        assert_eq!(result.stage, PublishStage::Published);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), fixture.content);
        assert!(target_parent.join("someone-elses.bin").is_file());

        // The staging file the kill left is this job's own and is removed, so
        // an interrupted attempt does not add a part file per retry. It is only
        // attributable because the ledger names it before the file is created;
        // the sweep below, which can only delete parts it can attribute, has
        // nothing left to do.
        assert_eq!(
            part_files(&target_parent).len(),
            0,
            "the staging file of the interrupted attempt is cleared by the retry"
        );
        let swept =
            sweep_orphaned_publish_parts(&conn, &fixture.roots(), Duration::from_secs(0)).unwrap();
        assert_eq!(
            swept.files, 0,
            "nothing unattributed is left behind: {swept:?}"
        );
    }

    /// A device that fills up midway: the write fails, the publish fails, and
    /// nothing is left under the final name.
    #[test]
    fn a_full_device_mid_copy_fails_the_publish_without_a_partial_target() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvVar::set("GALLERY_PUBLISH_FAULT_JOB", "job-enospc");
        let _limit = EnvVar::set("GALLERY_PUBLISH_FAIL_WRITE_AFTER", "1");
        let fixture = fixture("job-enospc");
        let conn = Connection::open(&fixture.db_path).unwrap();

        let error = publish_completed_file(&conn, &fixture.request("job-enospc"), &fixture.roots())
            .expect_err("a failed write must not report a successful publish");
        assert!(
            error.to_string().contains("no space left on device"),
            "{error}"
        );
        assert!(
            !fixture.target.exists(),
            "no half file under the final name"
        );
        assert_eq!(
            part_files(fixture.target.parent().unwrap()).len(),
            0,
            "the staging file of a failed attempt is removed"
        );
        let stage: String = conn
            .query_row(
                "SELECT stage FROM download_publish_jobs WHERE source_job_id='job-enospc'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(stage, "failed");

        // With the device healthy again the same job publishes normally.
        drop(_limit);
        let result =
            publish_completed_file(&conn, &fixture.request("job-enospc"), &fixture.roots())
                .expect("a retry after the failure publishes");
        assert_eq!(result.stage, PublishStage::Published);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), fixture.content);
    }

    /// A refused directory sync is reported, not swallowed, and it is not
    /// confused with the publish having failed: the bytes are already under
    /// their final name, so the retry completes the ledger instead of copying
    /// again or replacing anything.
    #[test]
    fn a_refused_directory_sync_is_reported_and_completed_by_the_retry() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let _scope = EnvVar::set("GALLERY_PUBLISH_FAULT_JOB", "job-fsync");
        let _fail = EnvVar::set("GALLERY_PUBLISH_FAIL_FSYNC", "1");
        let fixture = fixture("job-fsync");
        let conn = Connection::open(&fixture.db_path).unwrap();

        let error = publish_completed_file(&conn, &fixture.request("job-fsync"), &fixture.roots())
            .expect_err("a refused sync must not be reported as a completed publish");
        assert!(
            error.to_string().contains("failed to sync directory"),
            "{error}"
        );
        // The file is whole, not a half file: the link already happened.
        assert_eq!(std::fs::read(&fixture.target).unwrap(), fixture.content);
        let (stage, message): (String, String) = conn
            .query_row(
                "SELECT stage, COALESCE(error_message,'') FROM download_publish_jobs
                  WHERE source_job_id='job-fsync'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(stage, "staged", "the ledger must not claim published");
        assert!(message.contains("failed to sync directory"), "{message}");

        drop(_fail);
        let result = publish_completed_file(&conn, &fixture.request("job-fsync"), &fixture.roots())
            .expect("the retry completes the ledger for the file that is already there");
        assert_eq!(result.stage, PublishStage::Published);
        assert_eq!(std::fs::read(&fixture.target).unwrap(), fixture.content);
    }

    /// The other half of "only claims the artifact this job can account for":
    /// after a crash the final name is taken by a different file of exactly the
    /// same length. Same size is not the same artifact, so it is refused rather
    /// than adopted and the job stays unpublished.
    #[test]
    fn a_same_size_different_file_at_the_target_is_not_claimed_after_a_crash() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let fixture = fixture("job-v2");
        std::fs::create_dir_all(fixture.target.parent().unwrap()).unwrap();
        let same_size_other = b"publish payload for the crash windowz".to_vec();
        assert_eq!(same_size_other.len(), fixture.content.len());
        std::fs::write(&fixture.target, &same_size_other).unwrap();
        let conn = Connection::open(&fixture.db_path).unwrap();
        conn.execute(
            "INSERT INTO download_publish_jobs
                 (engine, source_job_id, manifest_version, source_identity, expected_length,
                  expected_blake3, target_path, stage)
             VALUES ('pawchive','job-v2',1,'asset-1',?1,?2,?3,'staged')",
            params![
                fixture.content.len() as i64,
                fixture.digest,
                fixture.target.to_string_lossy().to_string()
            ],
        )
        .unwrap();

        let error = publish_completed_file(&conn, &fixture.request("job-v2"), &fixture.roots())
            .expect_err("same length is not the same artifact");
        assert!(error.to_string().contains("refuse to overwrite"), "{error}");
        assert_eq!(
            std::fs::read(&fixture.target).unwrap(),
            same_size_other,
            "the occupant is left exactly as it was"
        );
        let stage: String = conn
            .query_row(
                "SELECT stage FROM download_publish_jobs WHERE source_job_id='job-v2'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_ne!(stage, "published", "the job is not marked published");
    }
}
