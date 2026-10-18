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
    let mut referenced: std::collections::HashSet<String> = conn
        .prepare(
            "SELECT private_temp_path FROM download_publish_jobs
             WHERE private_temp_path != ''",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    // The walk below runs on canonical directories while the ledger stores the
    // path the publisher used; on Windows those differ by the verbatim prefix
    // and case. Both spellings have to count as "the ledger names this file",
    // or a live staging copy would be reclaimed as an orphan.
    let canonical_refs = referenced
        .iter()
        .filter_map(|path| crate::fs_util::safe_canonicalize(Path::new(path)).ok())
        .map(|path| path.to_string_lossy().to_string())
        .collect::<Vec<_>>();
    referenced.extend(canonical_refs);

    let cutoff = SystemTime::now()
        .checked_sub(min_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    // The staging file is created *beside its target* (step 4), i.e. under
    // `<root>/<artist>/<work folder>/`, never at the root itself. Reading only
    // the roots' top level therefore found nothing in production — and on a
    // native fnOS install even that read failed, because `roots.roots` holds
    // the `/picturesN` aliases while the files live under the real paths.
    //
    // Walk the authorized roots recursively, directories only: the cost is one
    // `read_dir` per directory (bounded by the folder count), not one stat per
    // media file, so a library with 750k items stays cheap at startup. Hidden
    // directories are skipped — no publish target is hidden, and this keeps the
    // app's own recycle/staging trees out of the walk — and symlinks are not
    // followed (`file_type` does not dereference), so a link cannot pull the
    // sweep outside the authorized roots.
    const MAX_PUBLISH_SWEEP_DEPTH: usize = 32;
    let mut seen_dirs = std::collections::HashSet::new();
    let mut sweep = OrphanSweep::default();
    let mut pending: Vec<(PathBuf, usize)> = roots
        .allowed_roots()
        .into_iter()
        .map(|root| (PathBuf::from(root), 0usize))
        .collect();
    while let Some((candidate, depth)) = pending.pop() {
        let Ok(dir) = crate::fs_util::safe_canonicalize(&candidate) else {
            continue;
        };
        if !seen_dirs.insert(dir.clone()) {
            continue;
        }
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
                continue;
            };
            let Ok(file_type) = entry.file_type() else {
                continue;
            };
            if file_type.is_dir() {
                if depth < MAX_PUBLISH_SWEEP_DEPTH && !name.starts_with('.') {
                    pending.push((path, depth + 1));
                }
                continue;
            }
            if !file_type.is_file()
                || !name.starts_with(".gallery_publish_")
                || !name.ends_with(".part")
            {
                continue;
            }
            if referenced.contains(&path.to_string_lossy().to_string()) {
                continue;
            }
            let Ok(metadata) = entry.metadata() else {
                continue;
            };
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
/// Mark a published job as handed to the library even though the scanner has
/// no `items` row for it, because the file's media type is not indexed.
///
/// The bytes are already on disk at the target and were verified before the
/// publish; only the index entry is impossible. Recording the handoff keeps the
/// task's progress honest instead of retrying the same unindexable file until
/// the retry cap drops the task.
pub fn mark_job_delivered_unindexed(conn: &Connection, job_id: i64) -> Result<()> {
    let updated = conn.execute(
        "UPDATE download_publish_jobs
            SET stage = 'ingested', item_id = NULL, updated_at = strftime('%s','now')
          WHERE id = ?1 AND stage = 'published'",
        params![job_id],
    )?;
    if updated > 0 {
        return Ok(());
    }
    let existing: Option<String> = conn
        .query_row(
            "SELECT stage FROM download_publish_jobs WHERE id = ?1",
            params![job_id],
            |row| row.get(0),
        )
        .optional()?;
    match existing.as_deref() {
        Some("ingested") => Ok(()),
        _ => Err(anyhow!("publish job {job_id} is not published")),
    }
}

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
