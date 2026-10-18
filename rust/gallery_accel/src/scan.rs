//! Native library scan (replaces residual `app/scanner.py` for product runtime).

use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use tempfile::tempfile_in;
use uuid::Uuid;
use walkdir::WalkDir;

use crate::content_hash::hash_file;
use crate::db_housekeeping::cleanup_scan_seen;
use crate::folder_archive::validate_relative_folder;
use crate::fs_util::safe_canonicalize;
use crate::media_roots::{normalize_slashes, path_under_authorized_roots, MediaRoots};
use crate::media_type::{extract_date_from_folder, media_type_for_file};

/// Category directories that must be drilled through (never registered as artists).
const CATEGORY_DIR_NAMES: &[&str] = &[
    "- R18",
    "- 全年龄",
    "- 停更",
    "- 无码",
    "- 有码",
    "- loli",
    "loli",
    "- 已收集未整理",
    "- 已整理",
    "R18",
    "全年龄",
    "无码",
    "有码",
    "已收集未整理",
    "已整理",
];
const COLLECTION_WRAPPER_DIR_NAMES: &[&str] = &["合购", "涩图"];

/// Maximum mtime difference (seconds) still treated as "unchanged" when deciding
/// whether a cached content hash may be reused. This is strictly an epsilon for
/// floating-point/round-trip representation, NOT a grace window for real
/// modifications: any actual sub-millisecond change (e.g. an in-place equal-length
/// rewrite that bumps mtime by 0.5ms) must be detected and re-hashed. The prior
/// `1.0` tolerance wrongly kept stale hashes for sub-second modifications.
pub(crate) const MTIME_REUSE_EPSILON: f64 = 1e-6;

pub struct ScanControl {
    stop: AtomicBool,
    running: AtomicBool,
    active_ticket: AtomicU64,
    next_ticket: AtomicU64,
    /// Set when the process is leaving.
    ///
    /// `stop` is the operator asking one run to end; this is the process
    /// shutting down. The difference matters because fnOS stops the package
    /// with SIGTERM and then SIGKILL a second later, so work that only reacts
    /// to `stop` never writes its terminal state row — the next startup then
    /// sees `status='scanning'` and `reconcile_interrupted_scan` has to correct
    /// it. Workers read this between units of work and finish what they hold.
    shutting_down: AtomicBool,
}

impl ScanControl {
    /// Signal that the process is stopping. Idempotent.
    pub fn begin_shutdown(&self) {
        self.shutting_down.store(true, Ordering::SeqCst);
    }

    /// True once shutdown began. Batch loops check this between units of work.
    pub fn is_shutting_down(&self) -> bool {
        self.shutting_down.load(Ordering::SeqCst)
    }
}

impl Default for ScanControl {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            active_ticket: AtomicU64::new(0),
            next_ticket: AtomicU64::new(1),
            shutting_down: AtomicBool::new(false),
        }
    }
}

/// RAII slot guard returned by `try_claim`. Ensures exactly one owner holds
/// the scan slot. On drop, it only releases if the active ticket matches this guard,
/// preventing an older task's drop from releasing a newer task's claim.
pub struct ScanSlotGuard {
    control: Arc<ScanControl>,
    ticket: u64,
}

impl Drop for ScanSlotGuard {
    fn drop(&mut self) {
        self.control.release_ticket(self.ticket);
    }
}

impl ScanControl {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    pub fn is_stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst) || self.is_shutting_down()
    }

    pub fn set_running(&self, running: bool) {
        if !running {
            self.active_ticket.store(0, Ordering::SeqCst);
            self.running.store(false, Ordering::SeqCst);
        } else {
            let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
            self.active_ticket.store(ticket, Ordering::SeqCst);
            self.running.store(true, Ordering::SeqCst);
        }
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Claim the scan slot with an RAII guard that releases the slot on drop.
    /// Ticket validation ensures an older guard's drop cannot clear a newer claim.
    pub fn try_claim(self: &Arc<Self>) -> Option<ScanSlotGuard> {
        if self.is_shutting_down() {
            return None;
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
        match self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                self.stop.store(false, Ordering::SeqCst);
                self.active_ticket.store(ticket, Ordering::SeqCst);
                Some(ScanSlotGuard {
                    control: Arc::clone(self),
                    ticket,
                })
            }
            Err(_) => None,
        }
    }

    pub fn release_ticket(&self, ticket: u64) {
        if self
            .active_ticket
            .compare_exchange(ticket, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            self.running.store(false, Ordering::SeqCst);
        }
    }

    /// Atomically claim the scan slot. Clears stop only on success.
    pub fn try_start(&self) -> bool {
        if self.is_shutting_down() {
            return false;
        }
        let ticket = self.next_ticket.fetch_add(1, Ordering::SeqCst);
        match self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                self.stop.store(false, Ordering::SeqCst);
                self.active_ticket.store(ticket, Ordering::SeqCst);
                true
            }
            Err(_) => false,
        }
    }
}

/// RAII: clear running on drop (normal return, `?`, panic unwind).
struct RunningGuard<'a> {
    control: &'a ScanControl,
    ticket: u64,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.control.release_ticket(self.ticket);
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

pub fn ensure_scan_state(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS scan_state (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            artist_id INTEGER,
            status TEXT NOT NULL DEFAULT 'idle',
            phase TEXT NOT NULL DEFAULT '',
            scanned_count INTEGER NOT NULL DEFAULT 0,
            total_estimate INTEGER NOT NULL DEFAULT 0,
            current_path TEXT NOT NULL DEFAULT '',
            started_at REAL,
            updated_at REAL,
            scan_id TEXT NOT NULL DEFAULT ''
        );
        INSERT OR IGNORE INTO scan_state (id, status) VALUES (1, 'idle');
        ",
    )?;
    // Older databases lack the immutable per-run scan_id column; add it in
    // place so every scan run still exposes a stable key to the WS client.
    let has_scan_id: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pragma_table_info('scan_state') WHERE name='scan_id'",
        [],
        |row| row.get(0),
    )?;
    if has_scan_id == 0 {
        conn.execute(
            "ALTER TABLE scan_state ADD COLUMN scan_id TEXT NOT NULL DEFAULT ''",
            [],
        )?;
    }
    Ok(())
}

/// Clear a scan that the process did not finish, at startup, before the
/// listener binds.
///
/// `scan_state` is the only record of whether a scan is running, and it is
/// written when the run starts and again when it reaches a terminal phase. A
/// kill in between (fnOS stop/upgrade sends TERM and then KILL) leaves
/// `status='scanning'` behind forever: the panel disables its scan button and
/// the idle character-import tick answers `skipped: scan_active` on every pass,
/// and nothing self-heals before the next scheduled scan — which may never come
/// when `SCAN_INTERVAL=0`. This process cannot be the owner of that marker, so
/// it is stale by definition.
///
/// Returns the number of rows reset (0 or 1), so startup can log it.
pub fn reconcile_interrupted_scan(conn: &Connection) -> Result<usize> {
    let has_table: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='scan_state'",
        [],
        |row| row.get(0),
    )?;
    if has_table == 0 {
        return Ok(0);
    }
    conn.execute(
        "UPDATE scan_state
         SET status='idle', phase='interrupted', current_path='', scan_id='', updated_at=?
         WHERE id=1 AND status='scanning'",
        params![now()],
    )
    .map_err(Into::into)
}

pub fn get_scan_state(conn: &Connection) -> Result<Value> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='scan_state')",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(default_scan_state());
    }
    let row = conn
        .query_row(
            "SELECT status, phase, scanned_count, total_estimate, current_path, started_at, updated_at, artist_id, scan_id
             FROM scan_state WHERE id=1",
            [],
            |r| {
                Ok(json!({
                    "status": r.get::<_, String>(0)?,
                    "phase": r.get::<_, String>(1)?,
                    "scanned_count": r.get::<_, i64>(2)?,
                    "total_estimate": r.get::<_, i64>(3)?,
                    "current_path": r.get::<_, String>(4)?,
                    "started_at": r.get::<_, Option<f64>>(5)?,
                    "updated_at": r.get::<_, Option<f64>>(6)?,
                    "artist_id": r.get::<_, Option<i64>>(7)?,
                    "scan_id": r.get::<_, String>(8)?,
                }))
            },
        )
        .optional()?;
    Ok(row.unwrap_or_else(default_scan_state))
}

fn default_scan_state() -> Value {
    json!({
        "status": "idle",
        "phase": "",
        "scanned_count": 0,
        "total_estimate": 0,
        "current_path": "",
        "started_at": Value::Null,
        "updated_at": Value::Null,
        "artist_id": Value::Null,
        "scan_id": "",
    })
}

pub fn update_scan_state(conn: &Connection, fields: &[(&str, Value)]) -> Result<()> {
    let mut sets = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    for (k, v) in fields {
        sets.push(format!("{k}=?"));
        values.push(json_to_sql(v));
    }
    sets.push("updated_at=?".into());
    values.push(rusqlite::types::Value::Real(now()));
    let sql = format!("UPDATE scan_state SET {} WHERE id=1", sets.join(", "));
    if let Err(_err) = conn.execute(&sql, rusqlite::params_from_iter(values.iter())) {
        ensure_scan_state(conn)?;
        conn.execute(&sql, rusqlite::params_from_iter(values.iter()))?;
    }
    Ok(())
}

fn json_to_sql(v: &Value) -> rusqlite::types::Value {
    match v {
        Value::Null => rusqlite::types::Value::Null,
        Value::Bool(b) => rusqlite::types::Value::Integer(if *b { 1 } else { 0 }),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                rusqlite::types::Value::Integer(i)
            } else {
                rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0))
            }
        }
        Value::String(s) => rusqlite::types::Value::Text(s.clone()),
        other => rusqlite::types::Value::Text(other.to_string()),
    }
}

/// Map legacy virtual media roots (`/picturesN`) to real host paths when configured.
fn map_media_path(path: &str, roots: &MediaRoots) -> PathBuf {
    roots
        .map_to_real(path)
        .unwrap_or_else(|_| PathBuf::from(normalize_slashes(path).trim_end_matches('/')))
}

fn is_category_dir_name(name: &str) -> bool {
    name.starts_with('-') || CATEGORY_DIR_NAMES.contains(&name)
}

fn is_collection_wrapper(name: &str) -> bool {
    COLLECTION_WRAPPER_DIR_NAMES.contains(&name)
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct ScanErrors {
    pub sample: Vec<String>,
    pub total_count: usize,
    pub has_inaccessible: bool,
}

impl ScanErrors {
    pub const MAX_SAMPLE: usize = 20;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, err: impl Into<String>) {
        self.total_count += 1;
        self.has_inaccessible = true;
        if self.sample.len() < Self::MAX_SAMPLE {
            self.sample.push(err.into());
        }
    }

    pub fn extend(&mut self, other: ScanErrors) {
        self.total_count += other.total_count;
        if other.has_inaccessible {
            self.has_inaccessible = true;
        }
        for s in other.sample {
            if self.sample.len() < Self::MAX_SAMPLE {
                self.sample.push(s);
            }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.total_count == 0
    }

    pub fn len(&self) -> usize {
        self.total_count
    }
}


fn path_read_dir(path: &Path) -> std::io::Result<std::fs::ReadDir> {

    std::fs::read_dir(path)
}

fn path_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {

    std::fs::metadata(path)
}

#[derive(Debug, Default, Clone)]
pub struct DiscoveryResult {
    pub artists: Vec<(String, String)>,
    pub errors: ScanErrors,
}

fn count_media_files(directory: &Path, errors: &mut ScanErrors) -> usize {
    let entries = match path_read_dir(directory) {
        Ok(e) => e,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                errors.push(format!(
                    "count_media_files error on {}: {}",
                    directory.display(),
                    err
                ));
            }
            return 0;
        }
    };
    let mut count = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!(
                    "read_dir entry error in {}: {}",
                    directory.display(),
                    err
                ));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_file = match entry.file_type() {
            Ok(t) => t.is_file(),
            Err(err) => {
                errors.push(format!(
                    "file_type error for {}: {}",
                    entry.path().display(),
                    err
                ));
                false
            }
        };
        if is_file && media_type_for_file(&name).is_some() {
            count += 1;
        }
    }
    count
}

fn has_subdirs(directory: &Path, errors: &mut ScanErrors) -> bool {
    let entries = match path_read_dir(directory) {
        Ok(e) => e,
        Err(err) => {
            if err.kind() != std::io::ErrorKind::NotFound {
                errors.push(format!(
                    "has_subdirs error on {}: {}",
                    directory.display(),
                    err
                ));
            }
            return false;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!(
                    "read_dir entry error in {}: {}",
                    directory.display(),
                    err
                ));
                continue;
            }
        };
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        let is_dir = match entry.file_type() {
            Ok(t) => t.is_dir(),
            Err(err) => {
                errors.push(format!(
                    "file_type error for {}: {}",
                    entry.path().display(),
                    err
                ));
                false
            }
        };
        if is_dir {
            return true;
        }
    }
    false
}

/// Discover artist directories under a real authorized root (Python `_discover_artist_dirs`).
pub fn discover_artist_dirs(root_path: &Path) -> DiscoveryResult {
    let mut artists = Vec::new();
    let mut errors = ScanErrors::new();

    fn walk(current: &Path, artists: &mut Vec<(String, String)>, errors: &mut ScanErrors) {
        let entries = match path_read_dir(current) {
            Ok(e) => e,
            Err(err) => {
                if err.kind() != std::io::ErrorKind::NotFound {
                    errors.push(format!(
                        "discovery read_dir error on {}: {}",
                        current.display(),
                        err
                    ));
                }
                return;
            }
        };
        let mut dirs: Vec<std::fs::DirEntry> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => match e.file_type() {
                    Ok(t) if t.is_dir() => dirs.push(e),
                    Ok(_) => {}
                    Err(err) => {
                        errors.push(format!(
                            "discovery file_type error for {}: {}",
                            e.path().display(),
                            err
                        ));
                    }
                },
                Err(err) => {
                    errors.push(format!(
                        "discovery read_dir entry error in {}: {}",
                        current.display(),
                        err
                    ));
                }
            }
        }
        dirs.sort_by_key(|e| e.file_name());

        for entry in dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let full = entry.path();
            let media_count = count_media_files(&full, errors);
            if is_category_dir_name(&name) || (media_count == 0 && is_collection_wrapper(&name)) {
                walk(&full, artists, errors);
                continue;
            }
            let has_children = has_subdirs(&full, errors);
            if has_children || media_count > 0 {
                let path = normalize_slashes(&full.to_string_lossy());
                artists.push((name, path));
                continue;
            }
            walk(&full, artists, errors);
        }
    }

    walk(root_path, &mut artists, &mut errors);
    DiscoveryResult { artists, errors }
}

fn real_path_key(path: &str, roots: &MediaRoots) -> String {
    let mapped = map_media_path(path, roots);
    let canonical = safe_canonicalize(&mapped).unwrap_or(mapped);
    normalize_slashes(&canonical.to_string_lossy())
        .trim_end_matches('/')
        .to_string()
}

#[cfg(unix)]
fn file_identity(meta: &std::fs::Metadata) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    Some((meta.dev() as i64, meta.ino() as i64))
}

#[cfg(not(unix))]
fn file_identity(_meta: &std::fs::Metadata) -> Option<(i64, i64)> {
    None
}

/// One sampled file identity: (dev/ino, normalized path, media category).
type SampledMediaIdentity = (Option<(i64, i64)>, String, String);

/// Sample media identities from a discovered artist directory for relocation matching.
fn sample_dir_media_identities(dir: &Path, limit: usize) -> Vec<SampledMediaIdentity> {
    let mut out = Vec::new();
    for entry in WalkDir::new(dir)
        .into_iter()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
    {
        if out.len() >= limit {
            break;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || media_type_for_file(&name).is_none() {
            continue;
        }
        let Ok(meta) = std::fs::metadata(entry.path()) else {
            continue;
        };
        let identity = file_identity(&meta);
        let path = normalize_slashes(&entry.path().to_string_lossy());
        // Hash only when inode unavailable; used as secondary evidence.
        let hash = if identity.is_none() {
            hash_file(entry.path(), 1024 * 1024).unwrap_or_default()
        } else {
            String::new()
        };
        out.push((identity, path, hash));
    }
    out
}

/// Resolve a scan directory under an artist path. Rejects absolute/traversal/symlink escape.
///
/// Accepts an artist root path (e.g. `/pictures1/ArtistA`) and an optional
/// relative folder within it (e.g. `2024/01` or `subdir`). Rejects traversal
/// segments and paths that escape the artist directory.
///
/// Returns the canonical scan root. `folder=None` (or empty) scans the artist root.
pub fn resolve_scan_scope(
    artist_path: &str,
    folder: Option<&str>,
    roots: &MediaRoots,
) -> Result<PathBuf> {
    let mapped = map_media_path(artist_path, roots);
    let artist_root = authorized_artist_scan_root(&mapped, artist_path, roots)?;
    let Some(folder) = folder else {
        return Ok(artist_root);
    };

    let rel = validate_relative_folder(folder)?;
    let mut target = artist_root.clone();
    for part in rel.split('/') {
        target.push(part);
    }
    let target =
        safe_canonicalize(&target).with_context(|| format!("scan folder not found: {rel}"))?;
    if !target.is_dir() {
        return Err(anyhow!("scan folder is not a directory"));
    }
    // Path::starts_with compares components — `/artist-a` does not contain `/artist-a-evil`.
    if target != artist_root && !target.starts_with(&artist_root) {
        return Err(anyhow!("scan folder outside artist root"));
    }
    Ok(target)
}

/// Canonicalize a mapped artist directory and require it to remain inside an
/// authorized root before any scoped scan walks it. With no configured roots
/// (dev/test layouts) the mapped path is accepted as-is.
fn authorized_artist_scan_root(
    mapped: &Path,
    artist_path: &str,
    roots: &MediaRoots,
) -> Result<PathBuf> {
    let artist_root = safe_canonicalize(mapped)
        .with_context(|| format!("artist path not found or not accessible: {}", artist_path))?;
    if !artist_root.is_dir() {
        return Err(anyhow!("artist path is not a directory"));
    }
    if !roots.roots.is_empty() && !path_under_authorized_roots(&artist_root, roots) {
        return Err(anyhow!(
            "artist path is outside configured media roots: {}",
            artist_path
        ));
    }
    Ok(artist_root)
}

/// Run a scan on a slot the caller already claimed with `ScanControl::try_claim`
/// or `try_start`. The caller retains ownership of the slot.
pub fn run_scan_claimed(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
    artist_id: Option<i64>,
    folder: Option<&str>,
) -> Result<Value> {
    let _guard = RunningGuard {
        control,
        ticket: control.active_ticket.load(Ordering::SeqCst),
    };
    run_scan_inner(conn, roots, control, artist_id, folder)
}

/// Convenience wrapper for direct/test callers: claims the slot atomically,
/// then runs. A busy slot yields the structured "Already scanning" result.
pub fn run_scan(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
    artist_id: Option<i64>,
    folder: Option<&str>,
) -> Result<Value> {
    if !control.try_start() {
        return Ok(json!({"ok": false, "message": "Already scanning"}));
    }
    run_scan_claimed(conn, roots, control, artist_id, folder)
}

/// The artist row that owns `folder`, registering it when the folder exists
/// under an authorized root but the library has not indexed it yet.
///
/// A subscription that was never bound to a library artist still delivers into
/// a folder of its own — the one the user named when adding it. 下载后自动入库 has
/// to hand those files to the library, and every item belongs to an artist, so
/// that folder has to become one. This registers exactly the directory a full
/// scan would register, under the same path string, so it only moves that
/// discovery earlier. A path the library already knows is returned untouched,
/// which is what keeps the two from drifting into duplicates.
///
/// `None` means the folder is missing or outside every authorized root: there is
/// nothing to register, and the caller reports it rather than inventing one.
pub fn ensure_artist_for_folder(
    conn: &Connection,
    roots: &MediaRoots,
    folder: &str,
    fallback_name: &str,
) -> Result<Option<(i64, String)>> {
    let mapped = roots.map_to_real(folder)?;
    if !mapped.is_dir() || !path_under_authorized_roots(&mapped, roots) {
        return Ok(None);
    }
    // The mapped path is the real one, which is the form `artists.path` holds
    // (the scan writes whatever `map_to_real` produced for the root).
    let path = normalize_slashes(&mapped.to_string_lossy())
        .trim_end_matches('/')
        .to_string();
    if path.is_empty() {
        return Ok(None);
    }
    let existing: Option<i64> = conn
        .query_row(
            "SELECT id FROM artists WHERE path = ?1",
            params![&path],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(id) = existing {
        return Ok(Some((id, path)));
    }
    let name = Path::new(&path)
        .file_name()
        .and_then(|value| value.to_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(fallback_name);
    // A concurrent scan can register the same folder between the lookup above
    // and here, so the insert is allowed to lose that race: the read below is
    // the answer either way.
    let _ = conn.execute(
        "INSERT INTO artists (name, path) VALUES (?1, ?2)",
        params![name, &path],
    );
    let id: i64 = conn.query_row(
        "SELECT id FROM artists WHERE path = ?1",
        params![&path],
        |row| row.get(0),
    )?;
    Ok(Some((id, path)))
}

pub(crate) fn run_scan_inner(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
    artist_id: Option<i64>,
    folder: Option<&str>,
) -> Result<Value> {
    let scan_id = Uuid::new_v4().to_string();
    let started = now();
    ensure_scan_state(conn)?;
    ensure_scan_seen(conn)?;
    update_scan_state(
        conn,
        &[
            ("status", json!("scanning")),
            ("phase", json!("discover")),
            ("scanned_count", json!(0)),
            ("total_estimate", json!(0)),
            ("current_path", json!("")),
            ("started_at", json!(started)),
            ("scan_id", json!(scan_id.clone())),
            (
                "artist_id",
                artist_id.map(|v| json!(v)).unwrap_or(Value::Null),
            ),
        ],
    )?;

    // Resolved once per scan so every artist shares one spool directory and
    // byte budget; per-artist trackers receive an immutable copy.
    let spool = PresenceSpoolConfig::from_env();

    let result = (|| -> Result<Value> {
        let folder_scoped = folder.map(str::trim).filter(|s| !s.is_empty()).is_some();
        let full_library = artist_id.is_none() && !folder_scoped;
        if full_library {
            // Keep legacy path rewrites out of HTTP startup; full scans already run in the background.
            crate::db::normalize_configured_media_paths(conn, roots)
                .context("normalize configured media paths")?;
        }
        let mut scan_errors = ScanErrors::new();
        let artists = list_artists_for_scan(conn, roots, artist_id, &mut scan_errors)?;
        let artists_for_missing = artists.clone();
        let total = artists.len() as i64;
        update_scan_state(
            conn,
            &[
                ("phase", json!("scan")),
                ("total_estimate", json!(total.max(1))),
            ],
        )?;

        let mut scanned = 0i64;
        let mut new_candidates = 0i64;
        let mut updated_items = 0i64;
        let mut scanned_artist_ids = Vec::new();
        let mut stopped = false;
        let mut completed_artists = 0i64;
        let mut failed_artists = 0i64;

        for (idx, (aid, apath)) in artists.into_iter().enumerate() {
            if control.is_stop_requested() {
                stopped = true;
                break;
            }
            let resolved = match resolve_scan_scope(&apath, folder, roots) {
                Ok(path) => path,
                Err(err) => {
                    if folder_scoped || artist_id.is_some() {
                        return Err(err);
                    }
                    failed_artists += 1;
                    scan_errors.push(format!("pre-scan artist error for {apath}: {err}"));
                    continue;
                }
            };
            let artist_root = match roots
                .map_to_real(&apath)
                .and_then(|mapped| authorized_artist_scan_root(&mapped, &apath, roots))
            {
                Ok(p) => p,
                Err(err) => {
                    if folder_scoped || artist_id.is_some() {
                        return Err(anyhow!(
                            "artist path is outside configured media roots: {}",
                            apath
                        ));
                    }
                    failed_artists += 1;
                    scan_errors.push(format!("pre-scan artist root error for {apath}: {err}"));
                    continue;
                }
            };
            let artist_s = normalize_slashes(&artist_root.to_string_lossy());
            let scan_root = normalize_slashes(&resolved.to_string_lossy());
            update_scan_state(
                conn,
                &[
                    ("current_path", json!(scan_root)),
                    ("scanned_count", json!(idx as i64 + 1)),
                    ("phase", json!("scan")),
                ],
            )?;
            let outcome = walk_artist(conn, aid, &artist_s, &scan_root, &scan_id, control, &spool)?;
            new_candidates += outcome.new_candidates;
            updated_items += outcome.updated_items;
            scanned += 1;
            if !outcome.errors.is_empty() {
                failed_artists += 1;
                for err in &outcome.errors {
                    scan_errors.push(err.clone());
                }
            } else {
                completed_artists += 1;
            }
            if outcome.stopped {
                stopped = true;
                // Do not reconcile missing after a partial walk.
                break;
            }
            if !outcome.errors.is_empty() {
                // A subtree failed (permission/I/O): unseen files are unknown,
                // not missing. Keep scanning the remaining artists.
                continue;
            }
            // Missing reconciliation only after a complete walk of this scope.
            if outcome.needs_reconcile {
                reconcile_missing(conn, aid, &artist_s, &scan_root, &scan_id)?;
                delete_scan_seen_chunked(conn, Some(&scan_id), Some(aid))
                    .context("clean scan_seen after missing reconciliation")?;
            }
            scanned_artist_ids.push(aid);
        }

        if full_library
            && !stopped
            && !control.is_stop_requested()
            && failed_artists == 0
            && scan_errors.is_empty()
        {
            // Only after every authorized root was discovered and scanned without error.
            let _ = mark_missing_artists_after_full_scan(
                conn,
                roots,
                &artists_for_missing,
                &mut scan_errors,
            )?;
        }

        let phase = if stopped || control.is_stop_requested() {
            "stopped"
        } else if failed_artists > 0 || !scan_errors.is_empty() {
            if completed_artists == 0 && failed_artists > 0 {
                "failed"
            } else {
                "partial"
            }
        } else {
            "complete"
        };

        let links = if phase == "complete" {
            match crate::link_index::reindex_scanned_artist_links(conn, roots, &scanned_artist_ids)
            {
                Ok(value) => value,
                Err(error) => json!({"ok": false, "error": error.to_string()}),
            }
        } else {
            json!({"ok": true, "skipped": "scan_not_complete"})
        };
        let error_summary = if !scan_errors.is_empty() {
            scan_errors
                .sample
                .join("; ")
                .chars()
                .take(500)
                .collect::<String>()
        } else if phase == "failed" {
            "all artist scans failed".to_string()
        } else if phase == "partial" {
            "some artist scans failed".to_string()
        } else {
            String::new()
        };
        update_scan_state(
            conn,
            &[
                ("status", json!("idle")),
                ("phase", json!(phase)),
                ("scanned_count", json!(scanned)),
                (
                    "current_path",
                    json!(if phase == "partial" || phase == "failed" {
                        &error_summary
                    } else {
                        ""
                    }),
                ),
            ],
        )?;
        Ok(json!({
            "ok": phase != "failed",
            "phase": phase,
            "scanned": scanned,
            "completed_artists": completed_artists,
            "failed_artists": failed_artists,
            "errors": scan_errors.sample,
            "total_errors": scan_errors.total_count,
            "new_candidates": new_candidates,
            "updated_items": updated_items,
            "scan_id": scan_id,
            "links": links,
        }))
    })();

    // The trailing `scan_seen` sweep is housekeeping, not part of the scan's
    // result. Promoting its failure overwrote `phase='complete'` with
    // `phase='error'`, marked `worker_scan` degraded and made
    // `run_full_library_scan_claimed` skip the post-scan auto-archive — for a
    // walk that had already reconciled the whole library.
    let result = match result {
        Ok(value) => {
            if let Err(error) = cleanup_scan_seen(conn, &scan_id) {
                log_warn!("scan {scan_id}: scan_seen cleanup failed: {error:#}");
            }
            Ok(value)
        }
        Err(err) => {
            let _ = cleanup_scan_seen(conn, &scan_id);
            Err(err)
        }
    };

    match result {
        Ok(v) => Ok(v),
        Err(err) => {
            let msg = err.to_string();
            let _ = update_scan_state(
                conn,
                &[
                    ("status", json!("idle")),
                    ("phase", json!("error")),
                    (
                        "current_path",
                        json!(msg.chars().take(500).collect::<String>()),
                    ),
                ],
            );
            Err(err)
        }
    }
}

/// Full-library scans are the only scans allowed to trigger automatic folder
/// archive work. Runs on a slot the caller already claimed.
pub fn run_full_library_scan_claimed(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
) -> Result<Value> {
    let _guard = RunningGuard {
        control,
        ticket: control.active_ticket.load(Ordering::SeqCst),
    };
    let scan = run_scan_inner(conn, roots, control, None, None)?;
    let scanned = scan.get("scanned").and_then(Value::as_i64).unwrap_or(0);
    // A scan that covered zero artists or was partial/failed must never start real file moves:
    // only a scan that actually walked content completely may trigger auto archive.
    if scan.get("phase") == Some(&json!("complete")) && scanned > 0 {
        let archive = crate::folder_archive::run_folder_rename_auto_after_full_scan(conn, roots)?;
        Ok(json!({"scan": scan, "archive": archive}))
    } else {
        Ok(scan)
    }
}

/// Convenience wrapper for direct/test callers: claims the slot, then runs the
/// full-library scan and its auto-archive hook.
pub fn run_full_library_scan(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
) -> Result<Value> {
    if !control.try_start() {
        return Ok(json!({"ok": false, "message": "Already scanning"}));
    }
    run_full_library_scan_claimed(conn, roots, control)
}

fn ensure_scan_seen(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS scan_seen (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scan_id TEXT NOT NULL,
            artist_id INTEGER NOT NULL,
            file_path TEXT NOT NULL,
            media_type TEXT NOT NULL DEFAULT 'image',
            file_size INTEGER NOT NULL DEFAULT 0,
            file_mtime REAL NOT NULL DEFAULT 0,
            st_dev INTEGER,
            st_ino INTEGER,
            content_hash TEXT NOT NULL DEFAULT '',
            hash_status TEXT NOT NULL DEFAULT 'pending',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_scan_seen_scan_artist
            ON scan_seen(scan_id, artist_id);
        CREATE INDEX IF NOT EXISTS idx_scan_seen_artist
            ON scan_seen(artist_id);
        CREATE INDEX IF NOT EXISTS idx_scan_seen_path
            ON scan_seen(scan_id, file_path);
        ",
    )?;
    Ok(())
}

pub const SCAN_SEEN_DELETE_CHUNK_LIMIT: usize = 200;

/// Delete matching rows from scan_seen in bounded short-transaction chunks of 200 rows.
/// Never holds a long-lived write lock; each 200-row batch commits immediately.
pub fn delete_scan_seen_chunked(
    conn: &Connection,
    scan_id: Option<&str>,
    artist_id: Option<i64>,
) -> Result<usize> {
    let mut total_deleted = 0usize;
    loop {
        let deleted = match (scan_id, artist_id) {
            (Some(sid), Some(aid)) => {
                let tx = conn.unchecked_transaction().context(
                    "begin chunked delete transaction on scan_seen (scan_id, artist_id)",
                )?;
                let count = tx
                    .execute(
                        "DELETE FROM scan_seen WHERE id IN (
                            SELECT id FROM scan_seen
                            WHERE scan_id = ? AND artist_id = ?
                            LIMIT 200
                        )",
                        params![sid, aid],
                    )
                    .context("execute chunked delete on scan_seen (scan_id, artist_id)")?;
                tx.commit()
                    .context("commit chunked delete on scan_seen (scan_id, artist_id)")?;
                count
            }
            (Some(sid), None) => {
                let tx = conn
                    .unchecked_transaction()
                    .context("begin chunked delete transaction on scan_seen (scan_id)")?;
                let count = tx
                    .execute(
                        "DELETE FROM scan_seen WHERE id IN (
                            SELECT id FROM scan_seen
                            WHERE scan_id = ?
                            LIMIT 200
                        )",
                        params![sid],
                    )
                    .context("execute chunked delete on scan_seen (scan_id)")?;
                tx.commit()
                    .context("commit chunked delete on scan_seen (scan_id)")?;
                count
            }
            (None, Some(aid)) => {
                let tx = conn
                    .unchecked_transaction()
                    .context("begin chunked delete transaction on scan_seen (artist_id)")?;
                let count = tx
                    .execute(
                        "DELETE FROM scan_seen WHERE id IN (
                            SELECT id FROM scan_seen
                            WHERE artist_id = ?
                            LIMIT 200
                        )",
                        params![aid],
                    )
                    .context("execute chunked delete on scan_seen (artist_id)")?;
                tx.commit()
                    .context("commit chunked delete on scan_seen (artist_id)")?;
                count
            }
            (None, None) => {
                let tx = conn
                    .unchecked_transaction()
                    .context("begin chunked delete transaction on scan_seen (all)")?;
                let count = tx
                    .execute(
                        "DELETE FROM scan_seen WHERE id IN (
                            SELECT id FROM scan_seen
                            LIMIT 200
                        )",
                        [],
                    )
                    .context("execute chunked delete on scan_seen (all)")?;
                tx.commit()
                    .context("commit chunked delete on scan_seen (all)")?;
                count
            }
        };

        total_deleted += deleted;
        if deleted < SCAN_SEEN_DELETE_CHUNK_LIMIT {
            break;
        }
    }
    Ok(total_deleted)
}

/// Mark active items missing when not present in this scan_seen set.
/// Full artist root: whole artist. Scoped folder: only under that prefix.
fn reconcile_missing(
    conn: &Connection,
    artist_id: i64,
    artist_path: &str,
    scan_root: &str,
    scan_id: &str,
) -> Result<()> {
    let artist_norm = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let scan_norm = scan_root
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let full_artist = scan_norm == artist_norm;

    // Revive anything seen this scan.
    conn.execute(
        "UPDATE items SET missing=0, missing_at=NULL
         WHERE artist_id=? AND COALESCE(missing,0)=1
           AND EXISTS (
             SELECT 1 FROM scan_seen s
             WHERE s.scan_id=? AND s.file_path=items.file_path
           )",
        params![artist_id, scan_id],
    )?;

    if full_artist {
        conn.execute(
            "UPDATE items SET missing=1, missing_at=strftime('%s','now')
             WHERE artist_id=? AND COALESCE(missing,0)=0
               AND NOT EXISTS (
                 SELECT 1 FROM scan_seen s
                 WHERE s.scan_id=? AND s.file_path=items.file_path
               )",
            params![artist_id, scan_id],
        )?;
    } else {
        // Scoped: only items under the scan root prefix.
        let prefix = format!("{scan_norm}/");
        conn.execute(
            "UPDATE items SET missing=1, missing_at=strftime('%s','now')
             WHERE artist_id=? AND COALESCE(missing,0)=0
               AND (file_path = ? OR instr(file_path, ?) = 1)
               AND NOT EXISTS (
                 SELECT 1 FROM scan_seen s
                 WHERE s.scan_id=? AND s.file_path=items.file_path
               )",
            params![artist_id, scan_norm, prefix, scan_id],
        )?;
    }
    Ok(())
}
/// Per-scan canonicalized artist index: `real_path_key` → artist row.
///
/// Building this once per full-library discovery keeps artist registration
/// O(artists + directories) instead of re-canonicalizing the whole artists
/// table for every discovered directory.
struct ArtistIndex {
    by_key: std::collections::HashMap<String, (i64, String, String, i64)>,
}

impl ArtistIndex {
    fn load(conn: &Connection, roots: &MediaRoots) -> Result<Self> {
        let all = conn
            .prepare("SELECT id, name, path, COALESCE(missing, 0) FROM artists")?
            .query_map([], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, i64>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let mut by_key = std::collections::HashMap::with_capacity(all.len());
        for (id, name, path, missing) in all {
            by_key.insert(real_path_key(&path, roots), (id, name, path, missing));
        }
        Ok(Self { by_key })
    }

    fn reload(&mut self, conn: &Connection, roots: &MediaRoots) -> Result<()> {
        *self = Self::load(conn, roots)?;
        Ok(())
    }
}

/// Register or relocate a discovered artist path. Returns (id, path) for scanning.
fn register_discovered_artist(
    conn: &Connection,
    roots: &MediaRoots,
    index: &mut ArtistIndex,
    artist_name: &str,
    artist_path: &str,
) -> Result<(i64, String)> {
    let path_norm = normalize_slashes(artist_path)
        .trim_end_matches('/')
        .to_string();
    let path_key = real_path_key(&path_norm, roots);

    // Exact path hit (including virtual/real alias equivalence).
    let existing = index.by_key.get(&path_key).cloned();

    if let Some((id, name, old_path, old_missing)) = existing {
        let mut cur_name = name.clone();
        let mut cur_path = old_path.clone();
        let mut cur_missing = old_missing;
        if name != artist_name {
            conn.execute(
                "UPDATE artists SET name=? WHERE id=?",
                params![artist_name, id],
            )?;
            cur_name = artist_name.to_string();
        }
        if old_path != path_norm {
            conn.execute(
                "UPDATE artists SET path=? WHERE id=?",
                params![&path_norm, id],
            )?;
            cur_path = path_norm.clone();
        }
        if old_missing != 0 {
            conn.execute(
                "UPDATE artists SET missing=0, missing_at=NULL WHERE id=?",
                params![id],
            )?;
            cur_missing = 0;
        }
        if cur_name != name || cur_path != old_path || cur_missing != old_missing {
            index
                .by_key
                .insert(path_key, (id, cur_name, cur_path, cur_missing));
        }
        return Ok((id, path_norm));
    }

    // New path: try high-confidence directory relocation before creating a new row.
    if let Some(relocated) = try_relocate_artist_dir(conn, roots, artist_name, &path_norm)? {
        // Relocation rewrote another artist's path; rebuild so later lookups
        // see the new identity mapping.
        index.reload(conn, roots)?;
        return Ok(relocated);
    }

    conn.execute(
        "INSERT INTO artists (name, path) VALUES (?, ?)",
        params![artist_name, &path_norm],
    )?;
    let id = conn.last_insert_rowid();
    index.by_key.insert(
        path_key,
        (id, artist_name.to_string(), path_norm.clone(), 0),
    );
    Ok((id, path_norm))
}

/// High-confidence directory move: ≥2 independent file identities uniquely point to one old artist.
/// True only when the path is confirmed to not exist (`NotFound`). A permission
/// denial or any other I/O error yields `false`: it is "unknown", not "missing",
/// and must never trigger missing marks, auto-relocation, or bulk deletion.
/// `Path::is_dir()` cannot make this distinction (it returns `false` for both a
/// non-existent path and an inaccessible one).
fn path_confirmed_missing(path: &Path) -> bool {
    match path_metadata(path) {
        Ok(_) => false,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

/// Independent evidence identity for one sampled media file, used to deduplicate
/// relocation votes. Two hard links share the same `(dev, ino)`; two
/// byte-identical copies on platforms without inode identity share the same
/// content hash. Counting each independently would let a single physical file
/// cast multiple votes for the same old artist.
#[derive(Clone, PartialEq, Eq, Hash)]
enum EvidenceKey {
    Inode(i64, i64),
    Content(String),
}

fn try_relocate_artist_dir(
    conn: &Connection,
    roots: &MediaRoots,
    artist_name: &str,
    new_path: &str,
) -> Result<Option<(i64, String)>> {
    let new_dir = PathBuf::from(new_path);
    if !new_dir.is_dir() {
        return Ok(None);
    }
    let samples = sample_dir_media_identities(&new_dir, 32);
    if samples.len() < 2 {
        // Single-file directories never auto-merge.
        return Ok(None);
    }

    use std::collections::{HashMap, HashSet};
    let mut votes: HashMap<i64, usize> = HashMap::new();
    let mut ambiguous = false;
    let mut seen_evidence: HashSet<EvidenceKey> = HashSet::new();

    for (identity, _path, hash) in &samples {
        // Count each independent piece of evidence at most once. Two hard links
        // (same dev/ino) or two byte-identical copies (same content hash) must
        // not each cast a separate vote for the same old artist.
        let key = match identity {
            Some((dev, ino)) => EvidenceKey::Inode(*dev, *ino),
            None => {
                if hash.is_empty() {
                    continue;
                }
                EvidenceKey::Content(hash.clone())
            }
        };
        if !seen_evidence.insert(key) {
            continue;
        }

        let mut matched_artists: Vec<i64> = Vec::new();
        if let Some((dev, ino)) = identity {
            let rows = conn
                .prepare(
                    "SELECT DISTINCT artist_id FROM items
                     WHERE st_dev=? AND st_ino=? AND st_dev IS NOT NULL AND st_ino IS NOT NULL",
                )?
                .query_map(params![dev, ino], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            matched_artists.extend(rows);
        }
        if matched_artists.is_empty() && !hash.is_empty() {
            // Secondary: completed content hash must be globally unique.
            let rows = conn
                .prepare(
                    "SELECT DISTINCT artist_id FROM items
                     WHERE content_hash=? AND hash_status='done' AND content_hash <> ''",
                )?
                .query_map(params![hash], |r| r.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            if rows.len() == 1 {
                matched_artists.extend(rows);
            } else if rows.len() > 1 {
                ambiguous = true;
            }
        }
        matched_artists.sort_unstable();
        matched_artists.dedup();
        if matched_artists.len() == 1 {
            *votes.entry(matched_artists[0]).or_default() += 1;
        } else if matched_artists.len() > 1 {
            ambiguous = true;
        }
    }

    if ambiguous || votes.is_empty() {
        return Ok(None);
    }
    let mut ranking: Vec<(i64, usize)> = votes.into_iter().collect();
    ranking.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
    let (artist_id, score) = ranking[0];
    if score < 2 || ranking.len() > 1 {
        // Need unique winner with ≥2 independent file votes.
        return Ok(None);
    }

    // Only relocate when the old path is confirmed gone (true directory move).
    // An inaccessible (permission / I/O error) old path is "unknown", not a move.
    let old_path: String = conn.query_row(
        "SELECT path FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let old_mapped = map_media_path(&old_path, roots);
    if !path_confirmed_missing(&old_mapped) {
        // Old path still present (or inaccessible): not a confirmed move; keep as separate artist.
        return Ok(None);
    }

    apply_artist_path_relocation(conn, artist_id, artist_name, &old_path, new_path, score)?;
    Ok(Some((artist_id, new_path.to_string())))
}

fn apply_artist_path_relocation(
    conn: &Connection,
    artist_id: i64,
    artist_name: &str,
    old_path: &str,
    new_path: &str,
    evidence_count: usize,
) -> Result<()> {
    let old_norm = normalize_slashes(old_path)
        .trim_end_matches('/')
        .to_string();
    let new_norm = normalize_slashes(new_path)
        .trim_end_matches('/')
        .to_string();
    let tx = conn.unchecked_transaction()?;
    let updated = tx.execute(
        "UPDATE artists SET path=?, name=?, missing=0, missing_at=NULL WHERE id=? AND path=?",
        params![&new_norm, artist_name, artist_id, &old_norm],
    )?;
    if updated != 1 {
        let current_path: Option<String> = tx
            .query_row(
                "SELECT path FROM artists WHERE id=?",
                params![artist_id],
                |r| r.get(0),
            )
            .optional()?;
        match current_path {
            Some(ref p) if normalize_slashes(p).trim_end_matches('/') == new_norm => {
                tx.execute(
                    "UPDATE artists SET name=?, missing=0, missing_at=NULL WHERE id=?",
                    params![artist_name, artist_id],
                )?;
            }
            Some(ref p) if normalize_slashes(p).trim_end_matches('/') == old_norm => {
                tx.execute(
                    "UPDATE artists SET path=?, name=?, missing=0, missing_at=NULL WHERE id=?",
                    params![&new_norm, artist_name, artist_id],
                )?;
            }
            Some(other) => {
                return Err(anyhow!(
                    "cannot relocate artist {artist_id}: expected path {old_norm}, but found {other}"
                ));
            }
            None => {
                return Err(anyhow!(
                    "cannot relocate artist {artist_id}: artist not found"
                ));
            }
        }
    }
    tx.execute(
        "UPDATE items SET file_path=? || substr(file_path, length(?) + 1), missing=0, missing_at=NULL
         WHERE artist_id=? AND (file_path=? OR instr(file_path, ?)=1)",
        params![
            &new_norm,
            &old_norm,
            artist_id,
            &old_norm,
            format!("{old_norm}/")
        ],
    )?;
    loop {
        let cnt = tx.execute(
            "DELETE FROM scan_seen WHERE id IN (
                SELECT id FROM scan_seen WHERE artist_id = ? LIMIT 200
            )",
            params![artist_id],
        )?;
        if cnt < 200 {
            break;
        }
    }
    // Invalidate pending path-confirmation work for the old prefix; new scan rebuilds.
    tx.execute(
        "UPDATE scan_candidates SET status='superseded', resolved_at=strftime('%s','now')
         WHERE artist_id=? AND status IN ('pending','candidate','previewed')
           AND (file_path=? OR instr(file_path, ?)=1)",
        params![artist_id, &old_norm, format!("{old_norm}/")],
    )?;
    tx.execute(
        "UPDATE move_candidates SET status='superseded', resolved_at=strftime('%s','now')
         WHERE artist_id=? AND status='pending'
           AND (old_path=? OR instr(old_path, ?)=1 OR new_path=? OR instr(new_path, ?)=1)",
        params![
            artist_id,
            &old_norm,
            format!("{old_norm}/"),
            &old_norm,
            format!("{old_norm}/")
        ],
    )?;
    // Structured operation record (first item if any).
    let details = serde_json::json!({
        "kind": "artist_directory_relocated",
        "old_path": old_norm,
        "new_path": new_norm,
        "evidence_count": evidence_count,
        "artist_id": artist_id,
    })
    .to_string();
    if let Ok(item_id) = tx.query_row(
        "SELECT id FROM items WHERE artist_id=? ORDER BY id LIMIT 1",
        params![artist_id],
        |r| r.get::<_, i64>(0),
    ) {
        tx.execute(
            "INSERT INTO move_history (item_id, artist_id, old_path, new_path, reason, status, details, applied_at)
             VALUES (?, ?, ?, ?, 'artist_directory_relocated', 'applied', ?, strftime('%s','now'))",
            params![item_id, artist_id, &old_norm, &new_norm, details],
        )?;
    }
    tx.commit()?;
    Ok(())
}

fn list_artists_for_scan(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: Option<i64>,
    discovery_errors: &mut ScanErrors,
) -> Result<Vec<(i64, String)>> {
    if let Some(id) = artist_id {
        // Single-artist / folder scan: do not expand discovery scope.
        let path: String =
            conn.query_row("SELECT path FROM artists WHERE id=?", params![id], |r| {
                r.get(0)
            })?;
        return Ok(vec![(id, path)]);
    }

    // Full-library scan: always discover under every authorized real root.
    if roots.roots.is_empty() {
        return Ok(Vec::new());
    }

    // Known active artists per root, captured BEFORE discovery. A root that
    // held active artists but now discovers none is an empty mount or lost
    // share, not an empty library: fail closed instead of marking every
    // artist/item missing (which would eventually cascade tags and favorites
    // away in the 90-day lifecycle cleanup).
    let known_active: Vec<String> = conn
        .prepare("SELECT path FROM artists WHERE COALESCE(missing,0)=0")?
        .query_map([], |r| r.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut known_per_root = vec![0usize; roots.roots.len()];
    for path in &known_active {
        if let Some(root_index) = roots.root_index_for_path(path) {
            known_per_root[root_index] += 1;
        }
    }

    let mut inaccessible = Vec::new();
    let mut discovered: Vec<(String, String)> = Vec::new();
    let mut by_key: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();
    let mut discovered_per_root = vec![0usize; roots.roots.len()];

    for (i, root) in roots.roots.iter().enumerate() {
        let Some(real) = roots.real_root_at(i) else {
            inaccessible.push(root.clone());
            continue;
        };
        let real_path = Path::new(real);
        if !real_path.is_dir() {
            inaccessible.push(format!("{root} -> {real}"));
            continue;
        }
        let outcome = discover_artist_dirs(real_path);
        discovered_per_root[i] = outcome.artists.len();
        discovery_errors.extend(outcome.errors);
        for (name, path) in outcome.artists {
            let key = real_path_key(&path, roots);
            by_key.entry(key).or_insert((name, path));
        }
    }

    if !inaccessible.is_empty() {
        return Err(anyhow!(
            "authorized media root not accessible: {}; refusing full-library discovery and missing marks",
            inaccessible.join("; ")
        ));
    }

    for ((root_name, &known), &discovered) in roots
        .roots
        .iter()
        .zip(&known_per_root)
        .zip(&discovered_per_root)
    {
        if known > 0 && discovered == 0 {
            return Err(anyhow!(
                "authorized media root {root_name} previously contained {known} active artist(s) but none were discovered; refusing full-library missing marks (empty mount or lost share?)"
            ));
        }
    }

    // Merge known artists that still exist under authorized roots.
    let known = conn
        .prepare("SELECT id, name, path FROM artists")?
        .query_map([], |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (_id, name, path) in &known {
        let mapped = map_media_path(path, roots);
        if mapped.is_dir() && path_under_authorized_roots(&mapped, roots) {
            let key = real_path_key(path, roots);
            let display_path = normalize_slashes(&mapped.to_string_lossy());
            by_key
                .entry(key)
                .or_insert_with(|| (name.clone(), display_path));
        }
    }

    discovered.extend(by_key.into_values());
    discovered.sort_by(|a, b| a.1.cmp(&b.1));

    let mut index = ArtistIndex::load(conn, roots)?;
    let mut rows = Vec::new();
    for (name, path) in discovered {
        rows.push(register_discovered_artist(
            conn, roots, &mut index, &name, &path,
        )?);
    }
    Ok(rows)
}

/// After a complete full-library discovery+scan, mark artists whose paths no longer exist.
///
/// Each artist's missing transition plus its media reconciliation is an atomic unit
/// (single transaction): if the media update fails, the artist flag rolls back, so a
/// later retry can converge instead of stranding a half-marked artist. Artists left
/// inconsistent by an interrupted prior run (missing=1 but still holding non-missing
/// items) are re-included so a retried full scan reaches a consistent state.
///
/// A path is only ever treated as removed when it is *confirmed* `NotFound`. A
/// permission denial / I/O error is "unknown", never "missing" (S2): it is surfaced
/// as a scan error and skipped, never bulk-marked.
fn mark_missing_artists_after_full_scan(
    conn: &Connection,
    roots: &MediaRoots,
    scanned_paths: &[(i64, String)],
    errors: &mut ScanErrors,
) -> Result<i64> {
    let current_keys: std::collections::HashSet<String> = scanned_paths
        .iter()
        .map(|(_, p)| real_path_key(p, roots))
        .collect();
    // Re-include artists left inconsistent by an interrupted run (missing=1 but
    // still holding non-missing items) so a retried complete scan converges.
    let all = conn
        .prepare(
            "SELECT id, path FROM artists
             WHERE COALESCE(missing,0)=0
                OR (COALESCE(missing,0)=1 AND id IN (
                    SELECT artist_id FROM items WHERE COALESCE(missing,0)=0))",
        )?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stale = 0i64;
    for (id, path) in all {
        let key = real_path_key(&path, roots);
        if current_keys.contains(&key) {
            // Rediscovered this scan: clear any stale artist-level missing flag.
            conn.execute(
                "UPDATE artists SET missing=0, missing_at=NULL WHERE id=? AND COALESCE(missing,0)=1",
                params![id],
            )?;
            continue;
        }
        let mapped = map_media_path(&path, roots);
        match path_metadata(&mapped) {
            Ok(_) => {
                // Exists (file or dir) but was not discovered this round: recover a
                // stale flag without touching items, which walk_artist reconciles.
                conn.execute(
                    "UPDATE artists SET missing=0, missing_at=NULL WHERE id=? AND COALESCE(missing,0)=1",
                    params![id],
                )?;
                continue;
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Confirmed removed - proceed to check authorized roots and mark missing together.
            }
            Err(e) => {
                // Inaccessible (permission / I/O error), not a confirmed removal:
                // surface and skip — never silently mark missing.
                errors.push(format!(
                    "artist {id} path inaccessible ({e}), not marking missing: {path}"
                ));
                continue;
            }
        }
        if !path_under_authorized_roots(&mapped, roots)
            && !roots.roots.iter().any(|r| {
                let n = normalize_slashes(r).trim_end_matches('/').to_string();
                let p = normalize_slashes(&path);
                p == n || p.starts_with(&(n + "/"))
            })
        {
            // Outside authorized roots: do not bulk-mark.
            continue;
        }
        // Atomic unit: artist and its media go missing together, or neither does.
        let tx = conn.unchecked_transaction()?;
        tx.execute(
            "UPDATE artists SET missing=1, missing_at=strftime('%s','now') WHERE id=? AND COALESCE(missing,0)=0",
            params![id],
        )?;
        tx.execute(
            "UPDATE items SET missing=1, missing_at=strftime('%s','now')
             WHERE artist_id=? AND COALESCE(missing,0)=0",
            params![id],
        )?;
        tx.commit()?;
        stale += 1;
    }
    Ok(stale)
}

#[derive(Debug, Default)]
pub struct WalkArtistOutcome {
    pub new_candidates: i64,
    pub updated_items: i64,
    pub stopped: bool,
    pub errors: Vec<String>,
    pub needs_reconcile: bool,
}

struct DiscoveredFile {
    full: String,
    fname: String,
    media_type: &'static str,
    file_size: i64,
    file_mtime: f64,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
    folder_name: String,
    date_str: String,
    detected_raw: String,
    is_archive: i64,
}

fn process_discovered_batch(
    conn: &Connection,
    batch: &[DiscoveredFile],
    scan_id: &str,
    artist_id: i64,
    matched_active: &mut i64,
    updated: &mut i64,
    new_candidates: &mut i64,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt_find_item = tx.prepare_cached(
            "SELECT id, file_size, file_mtime, content_hash, hash_status, hash_updated_at,
                    missing, st_dev, st_ino, file_name, folder_name, detected_date,
                    date, manual_date, is_archive, media_type
             FROM items WHERE file_path=?",
        )?;
        let mut stmt_update_item = tx.prepare_cached(
            "UPDATE items SET file_name=?, file_size=?, file_mtime=?, folder_name=?,
                 detected_date=?, date=CASE WHEN manual_date IS NULL THEN ? ELSE date END,
                 is_archive=?, media_type=?, content_hash=?, hash_status=?, hash_updated_at=?,
                 st_dev=?, st_ino=?, missing=0, missing_at=NULL, scanned_at=strftime('%s','now') WHERE id=?",
        )?;
        let mut stmt_find_cand = tx.prepare_cached(
            "SELECT id, file_size, file_mtime, content_hash, hash_status, status, st_dev, st_ino
             FROM scan_candidates
             WHERE file_path=? AND status IN ('pending','candidate','previewed')
             ORDER BY id DESC LIMIT 1",
        )?;
        let mut stmt_supersede_move = tx.prepare_cached(
            "UPDATE move_candidates SET status='superseded', resolved_at=strftime('%s','now')
             WHERE scan_candidate_id=? AND status='pending'",
        )?;
        let mut stmt_update_cand = tx.prepare_cached(
            "UPDATE scan_candidates
             SET scan_id=?, artist_id=?, file_name=?, file_size=?, file_mtime=?, folder_name=?, date=?,
                 is_archive=?, media_type=?, content_hash=?, hash_status=?, st_dev=?, st_ino=?,
                 status=?, resolved_at=NULL
             WHERE id=?",
        )?;
        let mut stmt_insert_cand = tx.prepare_cached(
            "INSERT INTO scan_candidates
             (scan_id, artist_id, file_path, file_name, file_size, file_mtime, folder_name, date,
              is_archive, media_type, content_hash, hash_status, st_dev, st_ino, status)
             VALUES (?,?,?,?,?,?,?,?,?,?, '','pending',?,?, 'pending')",
        )?;

        for file in batch {
            let existing = stmt_find_item
                .query_row(params![&file.full], |r| {
                    Ok((
                        r.get::<_, i64>(0)?,
                        r.get::<_, i64>(1)?,
                        r.get::<_, f64>(2)?,
                        r.get::<_, String>(3)?,
                        r.get::<_, String>(4)?,
                        r.get::<_, Option<f64>>(5)?,
                        r.get::<_, i64>(6)?,
                        r.get::<_, Option<i64>>(7)?,
                        r.get::<_, Option<i64>>(8)?,
                        r.get::<_, String>(9)?,
                        r.get::<_, String>(10)?,
                        r.get::<_, String>(11)?,
                        r.get::<_, String>(12)?,
                        r.get::<_, Option<String>>(13)?,
                        r.get::<_, i64>(14)?,
                        r.get::<_, String>(15)?,
                    ))
                })
                .optional()?;

            if let Some((
                id,
                old_size,
                old_mtime,
                old_hash,
                old_status,
                old_hash_at,
                old_missing,
                old_st_dev,
                old_st_ino,
                old_fname,
                old_folder_name,
                old_detected_date,
                old_date,
                old_manual_date,
                old_is_archive,
                old_media_type,
            )) = existing
            {
                if old_missing == 0 {
                    *matched_active += 1;
                }
                let same_size = old_size == file.file_size;
                let same_mtime = (old_mtime - file.file_mtime).abs() < MTIME_REUSE_EPSILON;
                let identity_synced = match (file.st_dev, file.st_ino, old_st_dev, old_st_ino) {
                    (Some(d1), Some(i1), Some(d2), Some(i2)) => d1 == d2 && i1 == i2,
                    (None, None, None, None) => true,
                    _ => false,
                };
                let identity_changed = match (file.st_dev, file.st_ino, old_st_dev, old_st_ino) {
                    (Some(d1), Some(i1), Some(d2), Some(i2)) => d1 != d2 || i1 != i2,
                    _ => false,
                };

                let can_reuse_hash = same_size
                    && same_mtime
                    && !identity_changed
                    && old_status == "done"
                    && !old_hash.is_empty();

                let metadata_matches = old_fname == file.fname
                    && old_folder_name == file.folder_name
                    && old_detected_date == file.detected_raw
                    && (old_manual_date.is_some() || old_date == file.date_str)
                    && old_is_archive == file.is_archive
                    && old_media_type == file.media_type;

                let unchanged = same_size
                    && same_mtime
                    && identity_synced
                    && metadata_matches
                    && old_status == "done"
                    && !old_hash.is_empty()
                    && old_missing == 0;

                let (content_hash, hash_status, hash_updated_at) = if can_reuse_hash {
                    (old_hash, old_status, old_hash_at)
                } else {
                    (String::new(), "pending".into(), None)
                };

                if !unchanged {
                    stmt_update_item.execute(params![
                        file.fname,
                        file.file_size,
                        file.file_mtime,
                        file.folder_name,
                        file.detected_raw,
                        file.date_str,
                        file.is_archive,
                        file.media_type,
                        content_hash,
                        hash_status,
                        hash_updated_at,
                        file.st_dev,
                        file.st_ino,
                        id
                    ])?;
                    *updated += 1;
                }
            } else {
                let existing_candidate = stmt_find_cand
                    .query_row(params![&file.full], |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, i64>(1)?,
                            row.get::<_, f64>(2)?,
                            row.get::<_, String>(3)?,
                            row.get::<_, String>(4)?,
                            row.get::<_, String>(5)?,
                            row.get::<_, Option<i64>>(6)?,
                            row.get::<_, Option<i64>>(7)?,
                        ))
                    })
                    .optional()?;

                if let Some((
                    candidate_id,
                    old_size,
                    old_mtime,
                    old_hash,
                    old_hash_status,
                    old_status,
                    old_cand_dev,
                    old_cand_ino,
                )) = existing_candidate
                {
                    let same = old_size == file.file_size
                        && (old_mtime - file.file_mtime).abs() < MTIME_REUSE_EPSILON;
                    let cand_identity_changed =
                        match (file.st_dev, file.st_ino, old_cand_dev, old_cand_ino) {
                            (Some(d1), Some(i1), Some(d2), Some(i2)) => d1 != d2 || i1 != i2,
                            _ => false,
                        };
                    let keep_hash = same
                        && !cand_identity_changed
                        && old_hash_status == "done"
                        && !old_hash.is_empty();
                    let next_status = if old_status == "previewed" || !same || cand_identity_changed
                    {
                        "pending"
                    } else {
                        &old_status
                    };
                    if old_status == "previewed" || !same || cand_identity_changed {
                        stmt_supersede_move.execute(params![candidate_id])?;
                    }
                    stmt_update_cand.execute(params![
                        scan_id,
                        artist_id,
                        file.fname,
                        file.file_size,
                        file.file_mtime,
                        file.folder_name,
                        file.date_str,
                        file.is_archive,
                        file.media_type,
                        if keep_hash {
                            old_hash.clone()
                        } else {
                            String::new()
                        },
                        if keep_hash { "done" } else { "pending" },
                        file.st_dev,
                        file.st_ino,
                        next_status,
                        candidate_id,
                    ])?;
                    *new_candidates +=
                        i64::from(old_status == "previewed" || !same || cand_identity_changed);
                } else {
                    stmt_insert_cand.execute(params![
                        scan_id,
                        artist_id,
                        file.full,
                        file.fname,
                        file.file_size,
                        file.file_mtime,
                        file.folder_name,
                        file.date_str,
                        file.is_archive,
                        file.media_type,
                        file.st_dev,
                        file.st_ino
                    ])?;
                    *new_candidates += 1;
                }
            }
        }
    }
    tx.commit()?;
    Ok(())
}

fn walk_artist(
    conn: &Connection,
    artist_id: i64,
    artist_path: &str,
    scan_root: &str,
    scan_id: &str,
    control: &ScanControl,
    spool: &PresenceSpoolConfig,
) -> Result<WalkArtistOutcome> {
    let artist_norm = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let scan_norm = scan_root
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let is_scoped = scan_norm != artist_norm;

    // A failed count must not read as "this artist has no rows": that is
    // exactly the shape of an artist whose files were all deleted or renamed
    // away, so the walk would declare itself clean and skip missing
    // reconciliation, leaving dead `missing=0` rows behind.
    let (db_active, db_missing): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(CASE WHEN COALESCE(missing,0)=0 THEN 1 END),
                    COUNT(CASE WHEN COALESCE(missing,0)=1 THEN 1 END)
             FROM items WHERE artist_id=?",
            params![artist_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .context("count artist items")?;

    let mut new_candidates = 0i64;
    let mut updated = 0i64;
    let mut matched_active = 0i64;
    let mut presence = PresenceTracker::new(artist_id, spool.clone());
    let mut pending_batch: Vec<DiscoveredFile> = Vec::with_capacity(100);
    let mut stopped = false;
    let mut walk_errors: Vec<String> = Vec::new();
    const MAX_RECORDED_ERRORS_PER_ARTIST: usize = 10;

    // Ensure clean baseline for this artist via bounded chunked delete
    delete_scan_seen_chunked(conn, None, Some(artist_id))
        .context("clean baseline scan_seen for artist")?;

    // Depth 0 is the scan root itself. `filter_entry` runs for the root too,
    // so filtering it out when its own name starts with a dot skipped every
    // descendant: the walk produced no entries, `matched_active` stayed 0, the
    // run still counted as "clean", and reconciliation marked every item of
    // that artist missing. Only descendants are filtered.
    for entry in WalkDir::new(scan_root).into_iter().filter_entry(|e| {
        e.depth() == 0
            || e.file_name()
                .to_str()
                .map(|n| !n.starts_with('.'))
                .unwrap_or(true)
    }) {
        let entry = match entry {
            Ok(entry) => entry,
            Err(err) => {
                if walk_errors.len() < MAX_RECORDED_ERRORS_PER_ARTIST {
                    walk_errors.push(format!("walk entry error in {scan_root}: {err}"));
                }
                continue;
            }
        };
        if control.is_stop_requested() {
            stopped = true;
            break;
        }
        if !entry.file_type().is_file() {
            continue;
        }
        let fname = entry.file_name().to_string_lossy().to_string();
        let Some(media_type) = media_type_for_file(&fname) else {
            continue;
        };
        let full = normalize_slashes(&entry.path().to_string_lossy());
        let meta = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(err) => {
                if walk_errors.len() < MAX_RECORDED_ERRORS_PER_ARTIST {
                    walk_errors.push(format!(
                        "metadata error for {}: {err}",
                        entry.path().display()
                    ));
                }
                continue;
            }
        };
        let (st_dev, st_ino) = file_identity(&meta)
            .map(|(dev, ino)| (Some(dev), Some(ino)))
            .unwrap_or((None, None));
        let file_size = meta.len() as i64;
        let file_mtime = meta
            .modified()
            .ok()
            .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0);
        let parent = entry
            .path()
            .parent()
            .map(|p| normalize_slashes(&p.to_string_lossy()))
            .unwrap_or_default();
        let mut folder_name = entry
            .path()
            .parent()
            .and_then(|p| p.file_name())
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_default();
        let mut date_folder = folder_name.clone();
        if parent.trim_end_matches('/') == artist_norm {
            folder_name.clear();
            date_folder.clear();
        } else if parent.starts_with(&(artist_norm.clone() + "/")) {
            date_folder = parent[artist_norm.len() + 1..].to_string();
        }
        let date_str = extract_date_from_folder(&date_folder);
        let detected_raw = crate::media_type::extract_date_value_from_folder(&date_folder)
            .map(|value| value.raw)
            .unwrap_or_default();
        let is_archive = if media_type == "archive" { 1 } else { 0 };

        presence.push(full.clone())?;
        pending_batch.push(DiscoveredFile {
            full,
            fname,
            media_type,
            file_size,
            file_mtime,
            st_dev,
            st_ino,
            folder_name,
            date_str,
            detected_raw,
            is_archive,
        });

        if pending_batch.len() >= 100 {
            process_discovered_batch(
                conn,
                &pending_batch,
                scan_id,
                artist_id,
                &mut matched_active,
                &mut updated,
                &mut new_candidates,
            )?;
            pending_batch.clear();
        }
    }

    if !pending_batch.is_empty() && !stopped {
        process_discovered_batch(
            conn,
            &pending_batch,
            scan_id,
            artist_id,
            &mut matched_active,
            &mut updated,
            &mut new_candidates,
        )?;
        pending_batch.clear();
    }

    let is_clean_stable = !is_scoped
        && db_missing == 0
        && new_candidates == 0
        && matched_active == db_active
        && walk_errors.is_empty()
        && !stopped;

    let needs_reconcile = if is_clean_stable {
        presence.discard();
        false
    } else if !stopped && walk_errors.is_empty() {
        presence.flush_to_persistent_scan_seen(conn, scan_id, artist_id)?;
        true
    } else {
        presence.discard();
        false
    };

    Ok(WalkArtistOutcome {
        new_candidates,
        updated_items: updated,
        stopped,
        errors: walk_errors,
        needs_reconcile,
    })
}

/// Paths a presence tracker holds in memory before spilling to its spool file.
///
/// The tracker has to remember every path it walks until the artist is done,
/// because only then does it know whether the walk was clean. Spilling is the
/// escape hatch for an artist whose file list genuinely does not fit in memory:
/// a clean rescan discards the collected paths outright, so spilling a normal
/// artist writes a temp file per artist purely to throw it away (measured at
/// ~50-90KB per 2000-file artist, once per scan).
///
/// The default covers every artist in the reference library (488 artists, 755k
/// items: median 820 files, largest 21,640) with headroom, so peak resident
/// memory is a few MB while the disk spool still bounds a pathological artist.
const DEFAULT_MAX_IN_MEMORY_PRESENCE_PATHS: usize = 50_000;
/// Initial capacity of a tracker's in-memory buffer. Only an allocation hint;
/// what bounds memory is [`PresenceSpoolConfig::max_in_memory_paths`].
const PRESENCE_BUFFER_INITIAL_CAPACITY: usize = 256;
const MAX_SPOOLED_PATH_BYTES: usize = 65536;

/// Dedicated spool directory for presence tracking.
/// Priority: GALLERY_PRESENCE_SPOOL_DIR -> DATA_DIR/spool -> system temp/gallery_presence_spool
pub fn presence_spool_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("GALLERY_PRESENCE_SPOOL_DIR") {
        let p = PathBuf::from(dir);
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    if let Ok(dir) = std::env::var("DATA_DIR") {
        let p = PathBuf::from(dir).join("spool");
        if std::fs::create_dir_all(&p).is_ok() {
            return p;
        }
    }
    let p = std::env::temp_dir().join("gallery_presence_spool");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn max_presence_spool_bytes() -> u64 {
    std::env::var("GALLERY_MAX_PRESENCE_SPOOL_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(64 * 1024 * 1024) // 64 MB default per artist
}

fn max_in_memory_presence_paths() -> usize {
    std::env::var("GALLERY_MAX_IN_MEMORY_PRESENCE_PATHS")
        .ok()
        .and_then(|v| v.parse::<usize>().ok())
        .filter(|paths| *paths > 0)
        .unwrap_or(DEFAULT_MAX_IN_MEMORY_PRESENCE_PATHS)
}

/// Immutable spool settings resolved once, so a scan cannot observe a
/// half-changed environment and tests never have to mutate process state.
#[derive(Clone, Debug)]
pub struct PresenceSpoolConfig {
    pub dir: PathBuf,
    /// Total path bytes a single artist may spool to disk before the walk fails.
    pub max_bytes: u64,
    /// Paths a single artist may hold in memory before spilling to disk.
    pub max_in_memory_paths: usize,
}

impl PresenceSpoolConfig {
    pub fn from_env() -> Self {
        Self {
            dir: presence_spool_dir(),
            max_bytes: max_presence_spool_bytes(),
            max_in_memory_paths: max_in_memory_presence_paths(),
        }
    }
}

struct PresenceTempFile {
    /// Anonymous file auto-deleted on handle close/process exit by OS kernel.
    file: std::fs::File,
}

impl PresenceTempFile {
    fn new(dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(dir).with_context(|| {
            format!(
                "failed to create presence spool directory {}",
                dir.display()
            )
        })?;
        let file = tempfile_in(dir).with_context(|| {
            format!(
                "failed to create anonymous presence temp file in {}",
                dir.display()
            )
        })?;
        Ok(Self { file })
    }
}

struct PresenceTracker {
    artist_id: i64,
    spool: PresenceSpoolConfig,
    buffer: Vec<String>,
    spooled_file: Option<PresenceTempFile>,
    spooled_count: usize,
    spooled_bytes: u64,
}

impl PresenceTracker {
    fn new(artist_id: i64, spool: PresenceSpoolConfig) -> Self {
        Self {
            artist_id,
            spool,
            buffer: Vec::with_capacity(PRESENCE_BUFFER_INITIAL_CAPACITY),
            spooled_file: None,
            spooled_count: 0,
            spooled_bytes: 0,
        }
    }

    fn push(&mut self, path: String) -> Result<()> {
        self.buffer.push(path);
        if self.buffer.len() >= self.spool.max_in_memory_paths {
            self.spool()?;
        }
        Ok(())
    }

    fn spool(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let batch_bytes: u64 = self.buffer.iter().map(|p| p.len() as u64 + 4).sum();
        let limit = self.spool.max_bytes;
        if self.spooled_bytes + batch_bytes > limit {
            bail!(
                "presence spool byte budget exceeded: {} bytes would exceed limit of {} bytes",
                self.spooled_bytes + batch_bytes,
                limit
            );
        }
        let temp = match self.spooled_file.as_mut() {
            Some(t) => t,
            None => self
                .spooled_file
                .insert(PresenceTempFile::new(&self.spool.dir)?),
        };
        temp.file.seek(SeekFrom::End(0))?;
        let mut writer = BufWriter::new(&mut temp.file);
        for p in &self.buffer {
            let bytes = p.as_bytes();
            let len = bytes.len() as u32;
            writer.write_all(&len.to_le_bytes())?;
            writer.write_all(bytes)?;
        }
        writer.flush()?;
        self.spooled_count += self.buffer.len();
        self.spooled_bytes += batch_bytes;
        self.buffer.clear();
        Ok(())
    }

    fn flush_to_persistent_scan_seen(
        &mut self,
        conn: &Connection,
        scan_id: &str,
        artist_id: i64,
    ) -> Result<()> {
        if self.artist_id != artist_id {
            bail!(
                "presence tracker artist_id mismatch: tracker={} caller={}",
                self.artist_id,
                artist_id
            );
        }
        let res = (|| -> Result<()> {
            // 1. If paths were spooled to disk, stream in chunks of 200 via short transactions
            if self.spooled_count > 0 {
                let temp = self.spooled_file.as_mut().context(
                    "presence spool state invalid: spooled_count > 0 but no spooled file",
                )?;
                temp.file.flush()?;
                temp.file.seek(SeekFrom::Start(0))?;
                let mut reader = BufReader::new(&mut temp.file);
                let mut len_buf = [0u8; 4];
                let mut chunk = Vec::with_capacity(200);
                let mut read_records = 0usize;

                let flush_chunk = |chunk: &mut Vec<String>| -> Result<()> {
                    if chunk.is_empty() {
                        return Ok(());
                    }
                    let tx = conn.unchecked_transaction()?;
                    {
                        let mut stmt = tx.prepare_cached(
                            "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                        )?;
                        for p in chunk.iter() {
                            stmt.execute(params![scan_id, artist_id, p])?;
                        }
                    }
                    tx.commit()?;
                    chunk.clear();
                    Ok(())
                };

                for record_idx in 0..self.spooled_count {
                    match reader.read_exact(&mut len_buf) {
                        Ok(()) => {}
                        Err(err) => {
                            bail!(
                                "presence spool read error on record {}/{}: {}",
                                record_idx + 1,
                                self.spooled_count,
                                err
                            );
                        }
                    }

                    let len = u32::from_le_bytes(len_buf) as usize;
                    if len == 0 || len > MAX_SPOOLED_PATH_BYTES {
                        bail!(
                            "presence spool corrupted length {} on record {}/{} (limit {})",
                            len,
                            record_idx + 1,
                            self.spooled_count,
                            MAX_SPOOLED_PATH_BYTES
                        );
                    }

                    let mut bytes = vec![0u8; len];
                    reader.read_exact(&mut bytes).with_context(|| {
                        format!(
                            "presence spool payload truncated on record {}/{} (expected {} bytes)",
                            record_idx + 1,
                            self.spooled_count,
                            len
                        )
                    })?;

                    let path = String::from_utf8(bytes).map_err(|e| {
                        anyhow!(
                            "presence spool invalid UTF-8 path on record {}/{}: {}",
                            record_idx + 1,
                            self.spooled_count,
                            e
                        )
                    })?;

                    chunk.push(path);
                    read_records += 1;

                    if chunk.len() >= 200 {
                        flush_chunk(&mut chunk)?;
                    }
                }

                flush_chunk(&mut chunk)?;

                if read_records != self.spooled_count {
                    bail!(
                        "presence spool record count mismatch: read {} records, expected {}",
                        read_records,
                        self.spooled_count
                    );
                }

                // Verify file is strictly at EOF (no unexpected trailing garbage bytes)
                let mut trail = [0u8; 1];
                let trailing_bytes = reader.read(&mut trail)?;
                if trailing_bytes > 0 {
                    bail!(
                        "presence spool has unread trailing data after reading all {} records",
                        self.spooled_count
                    );
                }

                self.spooled_count = 0;
                self.spooled_bytes = 0;
                self.spooled_file = None;
            }

            // 2. Insert any remaining buffer items in chunks of 200 via short transactions
            for chunk in self.buffer.chunks(200) {
                let tx = conn.unchecked_transaction()?;
                {
                    let mut stmt = tx.prepare_cached(
                        "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                    )?;
                    for p in chunk {
                        stmt.execute(params![scan_id, artist_id, p])?;
                    }
                }
                tx.commit()?;
            }
            self.buffer.clear();
            Ok(())
        })();

        if res.is_err() {
            if let Err(del_err) = delete_scan_seen_chunked(conn, Some(scan_id), Some(artist_id)) {
                eprintln!("warning: failed to clean scan_seen after spool error: {del_err}");
            }
        }
        res
    }

    fn discard(&mut self) {
        self.buffer.clear();
        self.spooled_file = None;
        self.spooled_count = 0;
    }
}
