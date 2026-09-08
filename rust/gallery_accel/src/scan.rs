//! Native library scan (replaces residual `app/scanner.py` for product runtime).

use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use tempfile::tempfile_in;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::content_hash::hash_file;
use crate::db_housekeeping::cleanup_scan_seen;
use crate::folder_archive::validate_relative_folder;
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
const MTIME_REUSE_EPSILON: f64 = 1e-6;

pub struct ScanControl {
    stop: AtomicBool,
    running: AtomicBool,
    active_ticket: AtomicU64,
    next_ticket: AtomicU64,
}

impl Default for ScanControl {
    fn default() -> Self {
        Self {
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            active_ticket: AtomicU64::new(0),
            next_ticket: AtomicU64::new(1),
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

    pub fn clear_stop(&self) {
        self.stop.store(false, Ordering::SeqCst);
    }

    pub fn is_stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
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

#[cfg(test)]
thread_local! {
    static INJECTED_READDIR_ERROR: std::cell::RefCell<Option<(String, std::io::ErrorKind)>> = std::cell::RefCell::new(None);
    static INJECTED_METADATA_ERROR: std::cell::RefCell<Option<(String, std::io::ErrorKind)>> = std::cell::RefCell::new(None);
}

fn path_read_dir(path: &Path) -> std::io::Result<std::fs::ReadDir> {
    #[cfg(test)]
    {
        let path_str = path.to_string_lossy();
        let injected = INJECTED_READDIR_ERROR.with(|cell| cell.borrow().clone());
        if let Some((target, kind)) = injected {
            if path_str.contains(&target) {
                return Err(std::io::Error::new(kind, "injected read_dir error for test"));
            }
        }
    }
    std::fs::read_dir(path)
}

fn path_metadata(path: &Path) -> std::io::Result<std::fs::Metadata> {
    #[cfg(test)]
    {
        let path_str = path.to_string_lossy();
        let injected = INJECTED_METADATA_ERROR.with(|cell| cell.borrow().clone());
        if let Some((target, kind)) = injected {
            if path_str.contains(&target) {
                return Err(std::io::Error::new(kind, "injected metadata error for test"));
            }
        }
    }
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
                errors.push(format!("count_media_files error on {}: {}", directory.display(), err));
            }
            return 0;
        }
    };
    let mut count = 0usize;
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!("read_dir entry error in {}: {}", directory.display(), err));
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
                errors.push(format!("file_type error for {}: {}", entry.path().display(), err));
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
                errors.push(format!("has_subdirs error on {}: {}", directory.display(), err));
            }
            return false;
        }
    };
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                errors.push(format!("read_dir entry error in {}: {}", directory.display(), err));
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
                errors.push(format!("file_type error for {}: {}", entry.path().display(), err));
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
                    errors.push(format!("discovery read_dir error on {}: {}", current.display(), err));
                }
                return;
            }
        };
        let mut dirs: Vec<std::fs::DirEntry> = Vec::new();
        for entry in entries {
            match entry {
                Ok(e) => {
                    match e.file_type() {
                        Ok(t) if t.is_dir() => dirs.push(e),
                        Ok(_) => {}
                        Err(err) => {
                            errors.push(format!("discovery file_type error for {}: {}", e.path().display(), err));
                        }
                    }
                }
                Err(err) => {
                    errors.push(format!("discovery read_dir entry error in {}: {}", current.display(), err));
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
    let canonical = mapped.canonicalize().unwrap_or(mapped);
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
/// Returns the canonical scan root. `folder=None` (or empty) scans the artist root.
pub fn resolve_scan_scope(
    artist_path: &str,
    folder: Option<&str>,
    roots: &MediaRoots,
) -> Result<PathBuf> {
    // A scoped scan is a filesystem boundary. Unlike full-library discovery,
    // it must never fall back to a database path that did not map to a media root.
    let artist_mapped = roots.map_to_real(artist_path)?;
    let artist_root = authorized_artist_scan_root(&artist_mapped, artist_path, roots)?;

    let folder = folder.map(str::trim).filter(|s| !s.is_empty());
    let Some(folder) = folder else {
        return Ok(artist_root);
    };

    let rel = validate_relative_folder(folder)?;
    let mut target = artist_root.clone();
    for part in rel.split('/') {
        target.push(part);
    }
    let target = target
        .canonicalize()
        .with_context(|| format!("scan folder not found: {rel}"))?;
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
    let artist_root = mapped
        .canonicalize()
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

fn run_scan_inner(
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
            let outcome = walk_artist(
                conn, aid, &artist_s, &scan_root, &scan_id, control, &spool,
            )?;
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

        if full_library && !stopped && !control.is_stop_requested() && failed_artists == 0 && scan_errors.is_empty() {
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
            scan_errors.sample.join("; ").chars().take(500).collect::<String>()
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

    let result = match result {
        Ok(value) => cleanup_scan_seen(conn, &scan_id).map(|_| value),
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
                let tx = conn
                    .unchecked_transaction()
                    .context("begin chunked delete transaction on scan_seen (scan_id, artist_id)")?;
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
    index
        .by_key
        .insert(path_key, (id, artist_name.to_string(), path_norm.clone(), 0));
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
        if ranking.len() > 1 && ranking[1].1 > 0 {
            return Ok(None);
        }
        if score < 2 {
            return Ok(None);
        }
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
            .query_row("SELECT path FROM artists WHERE id=?", params![artist_id], |r| r.get(0))
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
                return Err(anyhow!("cannot relocate artist {artist_id}: artist not found"));
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

    for ((root_name, &known), &discovered) in
        roots.roots.iter().zip(&known_per_root).zip(&discovered_per_root)
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
                    let cand_identity_changed = match (file.st_dev, file.st_ino, old_cand_dev, old_cand_ino) {
                        (Some(d1), Some(i1), Some(d2), Some(i2)) => d1 != d2 || i1 != i2,
                        _ => false,
                    };
                    let keep_hash = same && !cand_identity_changed && old_hash_status == "done" && !old_hash.is_empty();
                    let next_status = if old_status == "previewed" || !same || cand_identity_changed {
                        "pending"
                    } else {
                        &old_status
                    };
                    if old_status == "previewed" || !same || cand_identity_changed {
                        stmt_supersede_move.execute(params![candidate_id])?;
                    }
                    stmt_update_cand.execute(params![
                        scan_id, artist_id, file.fname, file.file_size, file.file_mtime, file.folder_name, file.date_str,
                        file.is_archive, file.media_type,
                        if keep_hash { old_hash.clone() } else { String::new() },
                        if keep_hash { "done" } else { "pending" },
                        file.st_dev, file.st_ino, next_status, candidate_id,
                    ])?;
                    *new_candidates += i64::from(old_status == "previewed" || !same || cand_identity_changed);
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

    let (db_active, db_missing): (i64, i64) = conn
        .query_row(
            "SELECT COUNT(CASE WHEN COALESCE(missing,0)=0 THEN 1 END),
                    COUNT(CASE WHEN COALESCE(missing,0)=1 THEN 1 END)
             FROM items WHERE artist_id=?",
            params![artist_id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap_or((0, 0));

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

    for entry in WalkDir::new(scan_root).into_iter().filter_entry(|e| {
        e.file_name()
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
                    walk_errors.push(format!("metadata error for {}: {err}", entry.path().display()));
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

const MAX_IN_MEMORY_PRESENCE: usize = 200;
const MAX_SPOOLED_PATH_BYTES: usize = 65536;
/// Name prefix minted by [`PresenceTempFile`]; the trailing `_<pid>` suffix
/// identifies the owning process for stale-file reclamation.
const PRESENCE_SPOOL_PREFIX: &str = "gallery_pres_";

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

/// Spool files are only reclaimed once they are at least this old. A live
/// scan never keeps a spool file open anywhere near this long, so an aged
/// file can only belong to a process that is gone.
pub const PRESENCE_SPOOL_MAX_AGE: Duration = Duration::from_secs(24 * 60 * 60);

/// Reclaim presence spool files left behind by terminated processes.
///
/// Ownership is decided by three facts together, never by file extension:
/// the file lives directly in the spool directory, its name was minted by
/// [`PresenceTempFile`] (`gallery_pres_<random>_<pid>`), and it is older
/// than `max_age`. Unknown files, unrelated `.tmp` files, directories and
/// anything belonging to the current process are left untouched, which
/// matters because the spool directory can be a pre-existing shared
/// directory. The scan is non-recursive and never walks a media root.
pub fn cleanup_stale_presence_spools(max_age: Duration) -> usize {
    cleanup_stale_presence_spools_in(&presence_spool_dir(), max_age)
}

#[cfg(windows)]
fn is_process_running(pid_str: &str) -> bool {
    let Ok(pid) = pid_str.parse::<u32>() else { return false; };
    extern "system" {
        fn OpenProcess(dwDesiredAccess: u32, bInheritHandle: i32, dwProcessId: u32) -> *mut std::ffi::c_void;
        fn CloseHandle(hObject: *mut std::ffi::c_void) -> i32;
        fn GetExitCodeProcess(hProcess: *mut std::ffi::c_void, lpExitCode: *mut u32) -> i32;
        fn GetLastError() -> u32;
    }
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const STILL_ACTIVE: u32 = 259;
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
        if handle.is_null() {
            return GetLastError() == 5; // ERROR_ACCESS_DENIED means process exists
        }
        let mut exit_code: u32 = 0;
        let ok = GetExitCodeProcess(handle, &mut exit_code);
        CloseHandle(handle);
        ok != 0 && exit_code == STILL_ACTIVE
    }
}

#[cfg(unix)]
fn is_process_running(pid_str: &str) -> bool {
    let Ok(pid) = pid_str.parse::<i32>() else { return false; };
    if Path::new("/proc").exists() {
        return Path::new(&format!("/proc/{pid}")).exists();
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        if kill(pid, 0) == 0 {
            true
        } else {
            std::io::Error::last_os_error().raw_os_error() == Some(1)
        }
    }
}

#[cfg(not(any(unix, windows)))]
fn is_process_running(_pid_str: &str) -> bool {
    true
}

pub fn cleanup_stale_presence_spools_in(dir: &Path, max_age: Duration) -> usize {
    let now = SystemTime::now();
    let current_pid = std::process::id().to_string();
    let mut cleaned = 0usize;
    let entries = match std::fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return 0,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
            continue;
        };
        let Some(pid) = presence_spool_owner_pid(name) else {
            continue;
        };
        if pid == current_pid {
            continue;
        }
        if is_process_running(&pid) {
            // Owning process is still alive; never reclaim active spools.
            continue;
        }
        let expired = entry
            .metadata()
            .ok()
            .and_then(|meta| meta.modified().ok())
            .and_then(|modified| now.duration_since(modified).ok())
            .map(|age| age >= max_age)
            .unwrap_or(false);
        if !expired {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            cleaned += 1;
        }
    }
    cleaned
}

/// Returns the owner pid encoded in a spool file name minted by
/// [`PresenceTempFile`], or `None` for anything this process did not create.
fn presence_spool_owner_pid(name: &str) -> Option<String> {
    let rest = name.strip_prefix(PRESENCE_SPOOL_PREFIX)?;
    let pid = rest.rsplit_once('_').map(|(_, pid)| pid)?;
    if pid.is_empty() || !pid.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some(pid.to_string())
}

fn max_presence_spool_bytes() -> u64 {
    std::env::var("GALLERY_MAX_PRESENCE_SPOOL_BYTES")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(64 * 1024 * 1024) // 64 MB default per artist
}

/// Immutable spool settings resolved once, so a scan cannot observe a
/// half-changed environment and tests never have to mutate process state.
#[derive(Clone, Debug)]
pub struct PresenceSpoolConfig {
    pub dir: PathBuf,
    pub max_bytes: u64,
}

impl PresenceSpoolConfig {
    pub fn from_env() -> Self {
        Self {
            dir: presence_spool_dir(),
            max_bytes: max_presence_spool_bytes(),
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
            format!("failed to create presence spool directory {}", dir.display())
        })?;
        let file = tempfile_in(dir).with_context(|| {
            format!("failed to create anonymous presence temp file in {}", dir.display())
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
            buffer: Vec::with_capacity(MAX_IN_MEMORY_PRESENCE),
            spooled_file: None,
            spooled_count: 0,
            spooled_bytes: 0,
        }
    }

    fn push(&mut self, path: String) -> Result<()> {
        self.buffer.push(path);
        if self.buffer.len() >= MAX_IN_MEMORY_PRESENCE {
            self.spool()?;
        }
        Ok(())
    }

    fn spool(&mut self) -> Result<()> {
        if self.buffer.is_empty() {
            return Ok(());
        }
        let batch_bytes: u64 = self
            .buffer
            .iter()
            .map(|p| p.as_bytes().len() as u64 + 4)
            .sum();
        let limit = self.spool.max_bytes;
        if self.spooled_bytes + batch_bytes > limit {
            bail!(
                "presence spool byte budget exceeded: {} bytes would exceed limit of {} bytes",
                self.spooled_bytes + batch_bytes,
                limit
            );
        }
        if self.spooled_file.is_none() {
            self.spooled_file = Some(PresenceTempFile::new(&self.spool.dir)?);
        }
        let temp = self.spooled_file.as_mut().unwrap();
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
                let temp = self
                    .spooled_file
                    .as_mut()
                    .context("presence spool state invalid: spooled_count > 0 but no spooled file")?;
                temp.file.flush()?;
                temp.file.seek(SeekFrom::Start(0))?;
                let mut reader = BufReader::new(&mut temp.file);
                let mut len_buf = [0u8; 4];
                let mut chunk = Vec::with_capacity(200);
                let mut read_records = 0usize;

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
                        let tx = conn.unchecked_transaction()?;
                        {
                            let mut stmt = tx.prepare_cached(
                                "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                            )?;
                            for p in &chunk {
                                stmt.execute(params![scan_id, artist_id, p])?;
                            }
                        }
                        tx.commit()?;
                        chunk.clear();
                    }
                }

                if !chunk.is_empty() {
                    let tx = conn.unchecked_transaction()?;
                    {
                        let mut stmt = tx.prepare_cached(
                            "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                        )?;
                        for p in &chunk {
                            stmt.execute(params![scan_id, artist_id, p])?;
                        }
                    }
                    tx.commit()?;
                    chunk.clear();
                }

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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture() -> (tempfile::TempDir, Connection, MediaRoots) {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistA");
        std::fs::create_dir_all(artist.join("sub")).unwrap();
        std::fs::write(artist.join("one.jpg"), b"jpg").unwrap();
        std::fs::write(artist.join("sub").join("two.jpg"), b"jpg2").unwrap();
        // Outside artist root — must never be scanned via traversal.
        std::fs::create_dir_all(media.join("Other")).unwrap();
        std::fs::write(media.join("Other").join("secret.jpg"), b"nope").unwrap();
        let db_path = dir.path().join("t.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (
              id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0, missing_at REAL
            );
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              st_dev INTEGER, st_ino INTEGER,
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER, file_mtime REAL, folder_name TEXT, date TEXT, is_archive INTEGER,
              media_type TEXT, content_hash TEXT, hash_status TEXT, status TEXT, st_dev INTEGER, st_ino INTEGER,
              resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              media_type TEXT, file_size INTEGER, file_mtime REAL, content_hash TEXT, hash_status TEXT
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, resolved_at REAL
            );
            CREATE TABLE move_history (
              id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}', applied_at REAL
            );
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            ",
        )
        .unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'ArtistA', ?)",
            params![path],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        (dir, conn, roots)
    }

    #[test]
    fn read_only_scan_state_query_does_not_create_missing_table() {
        let conn = Connection::open_in_memory().unwrap();
        conn.pragma_update(None, "query_only", "ON").unwrap();

        let state = get_scan_state(&conn).unwrap();

        assert_eq!(state["status"], "idle");
        assert_eq!(state["phase"], "");
        let table_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='scan_state'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(table_count, 0);
    }

    #[test]
    fn legacy_scan_state_table_is_migrated_with_empty_scan_id() {
        let conn = Connection::open_in_memory().unwrap();
        // Pre-scan_id production schema: every column except scan_id.
        conn.execute_batch(
            "CREATE TABLE scan_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                artist_id INTEGER,
                status TEXT NOT NULL DEFAULT 'idle',
                phase TEXT NOT NULL DEFAULT '',
                scanned_count INTEGER NOT NULL DEFAULT 0,
                total_estimate INTEGER NOT NULL DEFAULT 0,
                current_path TEXT NOT NULL DEFAULT '',
                started_at REAL,
                updated_at REAL
             );
             INSERT INTO scan_state (id, status) VALUES (1, 'idle');",
        )
        .unwrap();

        ensure_scan_state(&conn).unwrap();

        let state = get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["phase"], "");
        assert_eq!(state["scan_id"], "");
        let row: String = conn
            .query_row("SELECT scan_id FROM scan_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(row, "");
    }

    #[test]
    fn scan_run_exposes_one_stable_scan_id_until_the_next_run() {
        let (_dir, conn, roots) = fixture();
        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let first: String = conn
            .query_row("SELECT scan_id FROM scan_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !first.is_empty(),
            "scan run must publish an immutable scan_id"
        );
        let terminal = get_scan_state(&conn).unwrap();
        assert_eq!(terminal["status"], "idle");
        assert_eq!(
            terminal["scan_id"], first,
            "terminal state keeps the run key"
        );
        assert!(
            terminal["started_at"].as_f64().is_some(),
            "started_at stays available as a fallback run key"
        );

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let second: String = conn
            .query_row("SELECT scan_id FROM scan_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_ne!(first, second, "next run must replace the scan-run key");
    }

    #[test]
    fn folder_scan_publishes_one_stable_scan_id_per_run() {
        let (_dir, conn, roots) = fixture();
        run_scan(&conn, &roots, &ScanControl::new(), Some(1), Some("sub")).unwrap();
        let first: String = conn
            .query_row("SELECT scan_id FROM scan_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !first.is_empty(),
            "folder scan must publish an immutable scan_id"
        );
        let terminal = get_scan_state(&conn).unwrap();
        assert_eq!(
            terminal["scan_id"], first,
            "folder terminal state keeps the run key"
        );

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), Some("sub")).unwrap();
        let second: String = conn
            .query_row("SELECT scan_id FROM scan_state WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_ne!(
            first, second,
            "next folder run must replace the scan-run key"
        );
    }

    #[test]
    fn scan_discovers_new_candidate() {
        let (_dir, conn, roots) = fixture();
        let control = ScanControl::new();
        let result = run_scan(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(result["ok"], true);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 2);
        let state = get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["phase"], "complete");
    }

    #[test]
    fn rescan_refreshes_previewed_candidate_and_requeues_changed_file() {
        let (dir, conn, roots) = fixture();
        let file = dir.path().join("pictures").join("ArtistA").join("one.jpg");
        let path = normalize_slashes(&file.to_string_lossy());
        conn.execute(
            "INSERT INTO scan_candidates
             (id, scan_id, artist_id, file_path, file_name, file_size, file_mtime, folder_name,
              date, is_archive, media_type, content_hash, hash_status, status, resolved_at)
             VALUES (9, 'old', 1, ?, 'one.jpg', 999, 1, '', '', 0, 'image', 'stale', 'done', 'previewed', 1)",
            [&path],
        )
        .unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();

        let row: (String, String, String, Option<f64>, i64) = conn
            .query_row(
                "SELECT status, hash_status, content_hash, resolved_at, file_size
                 FROM scan_candidates WHERE id=9",
                [],
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
            .unwrap();
        assert_eq!(
            (row.0.as_str(), row.1.as_str(), row.2.as_str(), row.3),
            ("pending", "pending", "", None)
        );
        assert_eq!(row.4, 3);
    }

    #[test]
    fn scoped_missing_reconciliation_treats_underscore_as_literal() {
        let (_dir, conn, _roots) = fixture();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES
             (10, 1, '/pictures/ArtistA/A_B/missing.jpg', 'missing.jpg'),
             (11, 1, '/pictures/ArtistA/AXB/keep.jpg', 'keep.jpg')",
            [],
        )
        .unwrap();
        reconcile_missing(
            &conn,
            1,
            "/pictures/ArtistA",
            "/pictures/ArtistA/A_B",
            "scan-x",
        )
        .unwrap();

        let states: (i64, i64) = conn
            .query_row(
                "SELECT (SELECT missing FROM items WHERE id=10), (SELECT missing FROM items WHERE id=11)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(states, (1, 0));
    }

    #[test]
    fn full_library_scan_runs_the_auto_archive_hook_only_after_completion() {
        let (_dir, conn, roots) = fixture();
        let control = ScanControl::new();
        let result = run_full_library_scan(&conn, &roots, &control).unwrap();
        assert_eq!(result["scan"]["phase"], "complete");
        assert_eq!(result["archive"]["status"], "disabled");
        assert_eq!(result["archive"]["skipped_count"], 0);
    }

    #[test]
    fn stopped_full_library_scan_does_not_run_auto_archive() {
        let (_dir, conn, mut roots) = fixture();
        roots.roots.clear();
        roots.labels.clear();
        roots.real_paths.clear();
        conn.execute("DELETE FROM artists", []).unwrap();
        conn.execute(
            "INSERT OR REPLACE INTO app_settings(key, value, updated_at) VALUES('folder_rename_auto', '1', 0)",
            [],
        )
        .unwrap();

        let control = ScanControl::new();
        assert!(control.try_start());
        control.request_stop();
        let result = run_full_library_scan_claimed(&conn, &roots, &control).unwrap();

        assert_eq!(result["phase"], "stopped");
        let legacy: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM app_settings WHERE key='folder_rename_auto'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let canonical: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM app_settings WHERE key='folder_rename_auto_enabled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(legacy, 1);
        assert_eq!(canonical, 0);
    }

    #[test]
    fn resolve_scan_scope_accepts_root_and_subdir() {
        let (dir, _conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let ap = artist.to_string_lossy().replace('\\', "/");
        let root = resolve_scan_scope(&ap, None, &roots).unwrap();
        assert_eq!(root, artist.canonicalize().unwrap());
        let sub = resolve_scan_scope(&ap, Some("sub"), &roots).unwrap();
        assert_eq!(sub, artist.join("sub").canonicalize().unwrap());
    }

    #[test]
    fn resolve_scan_scope_rejects_traversal_and_absolute() {
        let (dir, _conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let ap = artist.to_string_lossy().replace('\\', "/");
        for bad in [
            "../Other",
            "a/../../Other",
            "a/./b",
            "/absolute",
            r"C:\absolute",
            r"\\unc\share",
            "//unc/share",
        ] {
            assert!(
                resolve_scan_scope(&ap, Some(bad), &roots).is_err(),
                "should reject {bad}"
            );
        }
    }

    #[test]
    fn resolve_scan_scope_maps_virtual_artist_path() {
        let (dir, _conn, _roots) = fixture();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistA");
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let resolved = resolve_scan_scope("/pictures1/ArtistA", None, &roots).unwrap();
        assert_eq!(resolved, artist.canonicalize().unwrap());
        let sub = resolve_scan_scope("/pictures1/ArtistA", Some("sub"), &roots).unwrap();
        assert_eq!(sub, artist.join("sub").canonicalize().unwrap());
    }

    #[test]
    fn resolve_scan_scope_rejects_outside_root_artist_path() {
        let (dir, _conn, roots) = fixture();
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        let outside_s = outside.to_string_lossy().replace('\\', "/");
        let err = resolve_scan_scope(&outside_s, None, &roots).unwrap_err();
        assert!(err.to_string().contains("outside"), "unexpected err: {err}");
    }

    #[test]
    fn scoped_scan_rejects_outside_root_artist_path() {
        let (dir, conn, roots) = fixture();
        let outside = dir.path().join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.jpg"), b"nope").unwrap();
        conn.execute(
            "UPDATE artists SET path=? WHERE id=1",
            params![outside.to_string_lossy().replace('\\', "/")],
        )
        .unwrap();
        let control = ScanControl::new();
        let err = run_scan(&conn, &roots, &control, Some(1), None).unwrap_err();
        assert!(err.to_string().contains("outside"), "unexpected err: {err}");
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(!control.is_running());
    }

    #[test]
    fn malicious_folder_scan_writes_no_candidates() {
        let (_dir, conn, roots) = fixture();
        let control = ScanControl::new();
        let err = run_scan(&conn, &roots, &control, Some(1), Some("../Other")).unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("folder")
                || err.to_string().to_lowercase().contains("path")
                || err.to_string().to_lowercase().contains("outside"),
            "unexpected err: {err}"
        );
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0);
        assert!(!control.is_running());
        let state = get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
    }

    #[test]
    fn scoped_subdir_scan_only_sees_subdir_files() {
        let (_dir, conn, roots) = fixture();
        let control = ScanControl::new();
        let result = run_scan(&conn, &roots, &control, Some(1), Some("sub")).unwrap();
        assert_eq!(result["ok"], true);
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 1);
        let name: String = conn
            .query_row("SELECT file_name FROM scan_candidates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(name, "two.jpg");
    }

    #[cfg(unix)]
    #[test]
    fn resolve_scan_scope_rejects_symlink_escape() {
        let (dir, _conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let outside = dir.path().join("pictures").join("Other");
        let link = artist.join("escape");
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        let err = resolve_scan_scope(&ap, Some("escape"), &roots).unwrap_err();
        assert!(
            err.to_string().contains("outside") || err.to_string().contains("not found"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn try_start_is_exclusive() {
        let control = ScanControl::new();
        assert!(control.try_start());
        assert!(!control.try_start());
        assert!(control.is_running());
        control.set_running(false);
        assert!(control.try_start());
    }

    #[test]
    fn full_scan_marks_deleted_file_missing_and_keeps_present() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let one = artist.join("one.jpg");
        let two = artist.join("sub").join("two.jpg");
        // Seed as existing items (not candidates).
        let p1 = one.to_string_lossy().replace('\\', "/");
        let p2 = two.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, media_type, missing)
             VALUES (10,1,?,?,3,1.0,'image',0)",
            params![p1, "one.jpg"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, media_type, missing)
             VALUES (11,1,?,?,4,1.0,'image',0)",
            params![p2, "two.jpg"],
        )
        .unwrap();
        std::fs::remove_file(&one).unwrap();

        let control = ScanControl::new();
        let result = run_scan(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(result["phase"], "complete");
        assert!(!control.is_running());

        let m10: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        let m11: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=11", [], |r| r.get(0))
            .unwrap();
        assert_eq!(m10, 1, "deleted file must be missing");
        assert_eq!(m11, 0, "present file stays active");

        // Revive same id when file returns.
        std::fs::write(&one, b"jpg").unwrap();
        let control2 = ScanControl::new();
        run_scan(&conn, &roots, &control2, Some(1), None).unwrap();
        let m10b: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        assert_eq!(m10b, 0, "same item id must revive");
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE file_path=?",
                params![p1],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "no duplicate item");
    }

    #[test]
    fn scoped_scan_does_not_mark_outside_folder_missing() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let one = artist.join("one.jpg");
        let two = artist.join("sub").join("two.jpg");
        let p1 = one.to_string_lossy().replace('\\', "/");
        let p2 = two.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (20,1,?,?, 'image',0)",
            params![p1, "one.jpg"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (21,1,?,?, 'image',0)",
            params![p2, "two.jpg"],
        )
        .unwrap();
        // Delete root file; scoped sub scan must not touch it.
        std::fs::remove_file(&one).unwrap();
        let control = ScanControl::new();
        run_scan(&conn, &roots, &control, Some(1), Some("sub")).unwrap();
        let m20: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=20", [], |r| r.get(0))
            .unwrap();
        let m21: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=21", [], |r| r.get(0))
            .unwrap();
        assert_eq!(m20, 0, "out-of-scope item must not be marked missing");
        assert_eq!(m21, 0, "scoped present item stays active");
    }

    #[test]
    fn stopped_scan_does_not_reconcile_missing() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        // Many files so stop can interrupt mid-walk.
        for i in 0..50 {
            std::fs::write(artist.join(format!("f{i}.jpg")), b"x").unwrap();
        }
        let gone = artist.join("gone.jpg");
        std::fs::write(&gone, b"g").unwrap();
        let pg = gone.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (30,1,?,?, 'image',0)",
            params![pg, "gone.jpg"],
        )
        .unwrap();
        std::fs::remove_file(&gone).unwrap();

        let control = ScanControl::new();
        assert!(control.try_start());
        control.request_stop();
        // run_scan sees stop before walk completes (or immediately).
        let result = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert!(
            result["phase"] == "stopped" || result["phase"] == "complete",
            "{result}"
        );
        // Even if walk finished instantly after stop, stop path must not force missing.
        // If phase is complete, missing may apply — only assert when stopped.
        if result["phase"] == "stopped" {
            let m: i64 = conn
                .query_row("SELECT missing FROM items WHERE id=30", [], |r| r.get(0))
                .unwrap();
            assert_eq!(m, 0, "stopped scan must not mark unscanned files missing");
        }
        assert!(!control.is_running());
        // Can start again after stop.
        assert!(control.try_start());
        control.set_running(false);
    }

    #[test]
    fn sqlite_error_resets_running_flag() {
        let control = ScanControl::new();
        assert!(control.try_start());
        // Closed connection forces error inside run_scan.
        let conn = Connection::open_in_memory().unwrap();
        // No artists table → list_artists fails.
        let err = run_scan_claimed(
            &conn,
            &MediaRoots {
                roots: vec![],
                labels: vec![],
                real_paths: vec![],
            },
            &control,
            Some(1),
            None,
        )
        .unwrap_err();
        assert!(!err.to_string().is_empty());
        assert!(!control.is_running(), "running must reset after error");
    }

    fn empty_db_with_real_root(
        dir: &tempfile::TempDir,
        real_name: &str,
    ) -> (Connection, MediaRoots, PathBuf) {
        let real = dir.path().join(real_name);
        std::fs::create_dir_all(&real).unwrap();
        let db_path = dir.path().join("t.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (
              id INTEGER PRIMARY KEY, name TEXT, path TEXT UNIQUE, missing INTEGER DEFAULT 0, missing_at REAL
            );
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT UNIQUE, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              st_dev INTEGER, st_ino INTEGER,
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER, file_mtime REAL, folder_name TEXT, date TEXT, is_archive INTEGER,
              media_type TEXT, content_hash TEXT, hash_status TEXT, status TEXT, st_dev INTEGER, st_ino INTEGER,
              resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              media_type TEXT, file_size INTEGER, file_mtime REAL, content_hash TEXT, hash_status TEXT
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER, artist_id INTEGER,
              old_path TEXT, new_path TEXT, reason TEXT, content_hash TEXT, st_dev INTEGER, st_ino INTEGER,
              status TEXT, resolved_at REAL
            );
            CREATE TABLE move_history (
              id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}', created_at REAL, applied_at REAL, reverted_at REAL
            );
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            ",
        )
        .unwrap();
        let real_s = real.to_string_lossy().replace('\\', "/");
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec![real_s.clone()],
            real_paths: vec![real_s],
        };
        (conn, roots, real)
    }

    #[test]
    fn empty_db_full_scan_stores_real_paths_not_virtual() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "其他目录名");
        let artist = real.join("ArtistX");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("a.jpg"), b"a").unwrap();

        let control = ScanControl::new();
        let result = run_scan(&conn, &roots, &control, None, None).unwrap();
        assert_eq!(result["phase"], "complete");

        let (path,): (String,) = conn
            .query_row("SELECT path FROM artists", [], |r| Ok((r.get(0)?,)))
            .unwrap();
        assert!(
            path.contains("其他目录名") || path.replace('\\', "/").contains("其他目录名"),
            "artist path must use real root: {path}"
        );
        assert!(
            !path.starts_with("/pictures"),
            "must not store virtual root: {path}"
        );

        let item_path: String = conn
            .query_row("SELECT file_path FROM scan_candidates LIMIT 1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(
            !item_path.starts_with("/pictures"),
            "item path must be real: {item_path}"
        );
    }

    #[test]
    fn full_scan_migrates_legacy_virtual_paths_after_startup_deferral() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let artist = real.join("LegacyArtist");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("a.jpg"), b"a").unwrap();
        let real_s = real.to_string_lossy().replace('\\', "/");

        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'LegacyArtist', '/pictures1/LegacyArtist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type)
             VALUES (1, 1, '/pictures1/LegacyArtist/a.jpg', 'a.jpg', 'image')",
            [],
        )
        .unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap();

        let artist_path: String = conn
            .query_row("SELECT path FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(artist_path, format!("{real_s}/LegacyArtist"));
        let item_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(item_path, format!("{real_s}/LegacyArtist/a.jpg"));
    }

    #[test]
    fn full_scan_discovers_new_artist_when_db_already_has_artists() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let existing = real.join("Existing");
        std::fs::create_dir_all(&existing).unwrap();
        std::fs::write(existing.join("e.jpg"), b"e").unwrap();
        let existing_path = existing.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Existing', ?)",
            params![existing_path],
        )
        .unwrap();

        let newbie = real.join("Newbie");
        std::fs::create_dir_all(&newbie).unwrap();
        std::fs::write(newbie.join("n.jpg"), b"n").unwrap();

        let control = ScanControl::new();
        run_scan(&conn, &roots, &control, None, None).unwrap();

        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
            .unwrap();
        assert!(count >= 2, "must discover Newbie while Existing remains");
        let newbie_rows: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artists WHERE name='Newbie'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(newbie_rows, 1);
    }

    #[test]
    fn discover_drills_category_and_collection_wrappers() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        // 涩图/- R18/- 有码/Casino
        let nested = root
            .join("涩图")
            .join("- R18")
            .join("- 有码")
            .join("カジノ(Casino)");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(nested.join("x.jpg"), b"x").unwrap();
        // also bare - R18/- 有码/OtherArtist
        let other = root.join("- R18").join("- 有码").join("OtherArtist");
        std::fs::create_dir_all(&other).unwrap();
        std::fs::write(other.join("y.jpg"), b"y").unwrap();

        let found = discover_artist_dirs(&root).artists;
        let names: Vec<_> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"カジノ(Casino)"), "{names:?}");
        assert!(names.contains(&"OtherArtist"), "{names:?}");
        assert!(
            !names.iter().any(|n| n.starts_with('-')),
            "category dirs not artists: {names:?}"
        );
        assert!(!names.contains(&"涩图"));
        assert!(!names.contains(&"有码"));
    }

    #[test]
    fn discover_keeps_same_name_under_coded_and_uncoded() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        let a = root.join("- R18").join("- 有码").join("same-name");
        let b = root.join("- R18").join("- 无码").join("same-name");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("a.jpg"), b"a").unwrap();
        std::fs::write(b.join("b.jpg"), b"b").unwrap();

        let found = discover_artist_dirs(&root).artists;
        let paths: Vec<_> = found
            .into_iter()
            .filter(|(n, _)| n == "same-name")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(
            paths.len(),
            2,
            "same name different paths must stay separate"
        );
    }

    #[test]
    fn full_scan_registers_same_name_as_two_artists() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let a = real.join("- R18").join("- 有码").join("same-name");
        let b = real.join("- R18").join("- 无码").join("same-name");
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        std::fs::write(a.join("a.jpg"), b"a").unwrap();
        std::fs::write(b.join("b.jpg"), b"b").unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artists WHERE name='same-name'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 2);
    }

    #[test]
    fn virtual_artist_path_maps_for_scan_scope() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("real-media");
        let artist = real.join("ArtistA");
        std::fs::create_dir_all(artist.join("sub")).unwrap();
        std::fs::write(artist.join("one.jpg"), b"1").unwrap();
        let real_s = real.to_string_lossy().replace('\\', "/");
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec![real_s.clone()],
            real_paths: vec![real_s],
        };
        let scope = resolve_scan_scope("/pictures1/ArtistA", None, &roots).unwrap();
        assert_eq!(scope, artist.canonicalize().unwrap());
    }

    #[test]
    fn inaccessible_root_errors_and_does_not_mark_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, mut roots, real) = empty_db_with_real_root(&dir, "ok");
        let artist = real.join("Keep");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("k.jpg"), b"k").unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'Keep', ?, 0)",
            params![path],
        )
        .unwrap();
        // Add a second authorized root that does not exist.
        roots.roots.push("/pictures2".into());
        roots.labels.push("missing-root".into());
        roots.real_paths.push(
            dir.path()
                .join("does-not-exist")
                .to_string_lossy()
                .replace('\\', "/"),
        );

        let err = run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap_err();
        assert!(
            err.to_string().contains("not accessible") || err.to_string().contains("authorized"),
            "{err}"
        );
        let missing: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0, "must not bulk-mark missing when a root is down");
    }

    #[test]
    fn empty_known_root_fails_closed_and_does_not_mark_missing() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let artist = real.join("Gone");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("a.jpg"), b"a").unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'Gone', ?, 0)",
            params![path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (1, 1, ?, 'a.jpg', 'image', 0)",
            params![format!("{path}/a.jpg")],
        )
        .unwrap();
        // The mount/share went empty: root exists, all artist dirs are gone.
        std::fs::remove_dir_all(&artist).unwrap();

        let err = run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap_err();
        assert!(
            err.to_string().contains("none were discovered"),
            "unexpected err: {err}"
        );
        let (artist_missing, item_missing): (i64, i64) = conn
            .query_row(
                "SELECT (SELECT missing FROM artists WHERE id=1), (SELECT missing FROM items WHERE id=1)",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(artist_missing, 0, "empty known root must not mark artists missing");
        assert_eq!(item_missing, 0, "empty known root must not mark items missing");

        // Content returns: the same scan succeeds again.
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::write(artist.join("a.jpg"), b"a").unwrap();
        let result = run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap();
        assert_eq!(result["phase"], "complete");
    }

    #[test]
    fn zero_scan_complete_does_not_trigger_auto_archive() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, _real) = empty_db_with_real_root(&dir, "media");
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at)
             VALUES ('folder_rename_auto_enabled', '1', 0)",
            [],
        )
        .unwrap();

        let result = run_full_library_scan(&conn, &roots, &ScanControl::new()).unwrap();

        assert_eq!(result["phase"], "complete");
        assert_eq!(result["scanned"], 0);
        assert!(
            result.get("archive").is_none(),
            "zero-coverage complete scan must not run auto archive: {result}"
        );
    }

    #[test]
    fn scoped_scan_of_emptied_artist_dir_marks_its_items_missing() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let one = artist.join("one.jpg");
        let two = artist.join("sub").join("two.jpg");
        let p1 = one.to_string_lossy().replace('\\', "/");
        let p2 = two.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (40,1,?,?, 'image',0)",
            params![p1, "one.jpg"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (41,1,?,?, 'image',0)",
            params![p2, "two.jpg"],
        )
        .unwrap();
        std::fs::remove_file(&one).unwrap();
        std::fs::remove_dir_all(artist.join("sub")).unwrap();

        let result = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(result["phase"], "complete");
        let m40: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=40", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            m40, 1,
            "an explicitly scanned artist scope reports vanished files as missing"
        );
    }

    #[test]
    fn path_escape_rejected_by_map_to_real() {
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["/vol1/ok".into()],
            real_paths: vec!["/vol1/ok".into()],
        };
        assert!(roots.map_to_real("/pictures1/../etc/passwd").is_err());
        assert!(roots.map_to_real("/pictures1/foo/../../etc").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn high_confidence_inode_relocation_keeps_artist_id() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let old = real.join("Casino");
        std::fs::create_dir_all(&old).unwrap();
        let f1 = old.join("a.jpg");
        let f2 = old.join("b.jpg");
        std::fs::write(&f1, b"aa").unwrap();
        std::fs::write(&f2, b"bb").unwrap();
        let old_s = old.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (431, 'Casino', ?)",
            params![&old_s],
        )
        .unwrap();
        for (id, file) in [(1i64, &f1), (2i64, &f2)] {
            let meta = std::fs::metadata(file).unwrap();
            use std::os::unix::fs::MetadataExt;
            let p = file.to_string_lossy().replace('\\', "/");
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, st_dev, st_ino, media_type, missing)
                 VALUES (?, 431, ?, ?, ?, ?, 'image', 0)",
                params![id, p, file.file_name().unwrap().to_string_lossy(), meta.dev() as i64, meta.ino() as i64],
            )
            .unwrap();
        }
        // Move directory to categorized location (same inodes).
        let new_dir = real.join("- R18").join("- 有码").join("Casino");
        std::fs::create_dir_all(new_dir.parent().unwrap()).unwrap();
        std::fs::rename(&old, &new_dir).unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap();

        let (id, path, missing): (i64, String, i64) = conn
            .query_row(
                "SELECT id, path, missing FROM artists WHERE id=431",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(id, 431);
        assert_eq!(missing, 0);
        assert!(path.contains("- R18") || path.contains("有码"), "{path}");
        let item_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE artist_id=431 AND missing=0",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(item_count, 2);
    }

    #[test]
    fn ambiguous_move_creates_new_path_without_merging() {
        let dir = tempfile::tempdir().unwrap();
        let (conn, roots, real) = empty_db_with_real_root(&dir, "media");
        let old = real.join("OldArtist");
        std::fs::create_dir_all(&old).unwrap();
        // Old path gone from disk but still in DB; new path has only one file (insufficient evidence).
        let old_s = old.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (10, 'OldArtist', ?, 0)",
            params![&old_s],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, content_hash, hash_status, media_type, missing)
             VALUES (1, 10, ?, 'only.jpg', 'hash1', 'done', 'image', 1)",
            params![format!("{old_s}/only.jpg")],
        )
        .unwrap();
        std::fs::remove_dir_all(&old).unwrap();

        let new_dir = real.join("- R18").join("OldArtist");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::fs::write(new_dir.join("only.jpg"), b"only").unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), None, None).unwrap();

        // New path must be visible as its own row (or relocated only with strong evidence).
        // Single-file dirs never auto-merge → expect 2 rows or missing old + new.
        let paths: Vec<String> = conn
            .prepare("SELECT path FROM artists WHERE name='OldArtist' ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert!(
            !paths.is_empty(),
            "new path must be registered; got {paths:?}"
        );
        let new_visible = paths.iter().any(|p| p.contains("- R18"));
        assert!(new_visible, "new categorized path must appear: {paths:?}");
    }

    #[test]
    fn scan_refreshes_detected_date_but_never_overwrites_manual_date() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let month_folder = artist.join("2026").join("202607 works");
        std::fs::create_dir_all(&month_folder).unwrap();
        let file = month_folder.join("pic.jpg");
        std::fs::write(&file, b"pic").unwrap();
        let file_s = file.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, media_type, missing)
             VALUES (10,1,?,?,3,1.0,'image',0)",
            params![file_s, "pic.jpg"],
        )
        .unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let (date, detected, manual): (String, String, Option<String>) = conn
            .query_row(
                "SELECT date, detected_date, manual_date FROM items WHERE id=10",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(date, "2026-07-01");
        assert_eq!(detected, "2026-07");
        assert_eq!(manual, None);

        conn.execute("UPDATE items SET manual_date='2026-08' WHERE id=10", [])
            .unwrap();
        let new_folder = artist.join("2027").join("202701 moved");
        std::fs::create_dir_all(&new_folder).unwrap();
        let moved = new_folder.join("pic.jpg");
        std::fs::rename(&file, &moved).unwrap();
        // The folder-rename executor rewrites item paths in the DB; the next
        // scan then observes the same item id at its new location.
        let moved_s = moved.to_string_lossy().replace('\\', "/");
        conn.execute(
            "UPDATE items SET file_path=?, folder_name='202701 moved' WHERE id=10",
            params![moved_s],
        )
        .unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let (date, detected, manual): (String, String, Option<String>) = conn
            .query_row(
                "SELECT date, detected_date, manual_date FROM items WHERE id=10",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            date, "2026-07-01",
            "manual override must keep the effective canonical date"
        );
        assert_eq!(
            detected, "2027-01",
            "detected_date must refresh from the new folder"
        );
        assert_eq!(manual, Some("2026-08".to_string()));

        conn.execute("UPDATE items SET manual_date=NULL WHERE id=10", [])
            .unwrap();
        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let (date, detected, manual): (String, String, Option<String>) = conn
            .query_row(
                "SELECT date, detected_date, manual_date FROM items WHERE id=10",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            date, "2027-01-01",
            "clearing the override restores the latest detected date"
        );
        assert_eq!(detected, "2027-01");
        assert_eq!(manual, None);
    }

    #[test]
    fn scan_recognizes_compact_year_month_folder_full_date() {
        let (dir, conn, roots) = fixture();
        let artist = dir.path().join("pictures").join("ArtistA");
        let compact_folder = artist.join("202508").join("01_1536_title");
        std::fs::create_dir_all(&compact_folder).unwrap();
        let file = compact_folder.join("pic.jpg");
        std::fs::write(&file, b"pic").unwrap();
        let file_s = file.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, media_type, missing)
             VALUES (20,1,?,?,3,1.0,'image',0)",
            params![file_s, "pic.jpg"],
        )
        .unwrap();

        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        let (date, detected, manual): (String, String, Option<String>) = conn
            .query_row(
                "SELECT date, detected_date, manual_date FROM items WHERE id=20",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(date, "2025-08-01");
        assert_eq!(detected, "2025-08-01");
        assert_eq!(manual, None);
    }

    /// The 1.0.320 write-amplification skip only fires when file size, mtime,
    /// hash state, the missing flag and the file identity are all stable. Every
    /// case below plants a sentinel in `folder_name` -- a column only the item
    /// UPDATE writes -- so a surviving sentinel proves the row was left alone
    /// without depending on `scanned_at`'s one-second resolution.
    fn seed_pending_item(conn: &Connection, file: &std::path::Path) {
        let path = normalize_slashes(&file.to_string_lossy());
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (10, 1, ?, ?, 'image', 0)",
            params![path, file.file_name().unwrap().to_string_lossy()],
        )
        .unwrap();
    }

    /// Setup for the skip cases: seed the item, scan once so the row carries the
    /// real size/mtime/identity, then simulate a finished hash and leave a
    /// sentinel behind for the next scan to (not) overwrite.
    fn stable_item_fixture() -> (tempfile::TempDir, Connection, MediaRoots, std::path::PathBuf) {
        let (dir, conn, roots) = fixture();
        let file = dir.path().join("pictures").join("ArtistA").join("one.jpg");
        seed_pending_item(&conn, &file);
        run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        conn.execute(
            "UPDATE items SET content_hash='hash-skip-test', hash_status='done',
             folder_name='sentinel' WHERE id=10",
            [],
        )
        .unwrap();
        (dir, conn, roots, file)
    }

    fn item_folder_and_hash(conn: &Connection) -> (String, String, String) {
        conn.query_row(
            "SELECT folder_name, content_hash, hash_status FROM items WHERE id=10",
            [],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .unwrap()
    }

    #[test]
    fn stable_hashed_item_is_not_rewritten_on_rescan() {
        let (dir, conn, roots) = fixture();
        let file = dir.path().join("pictures").join("ArtistA").join("one.jpg");
        seed_pending_item(&conn, &file);

        let first = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(
            first["updated_items"], 1i64,
            "a row that is not hashed yet is written: {first}"
        );
        let (folder, hash, status) = item_folder_and_hash(&conn);
        assert_eq!(folder, "", "the first scan normalizes folder_name");
        assert_eq!(
            (hash.as_str(), status.as_str()),
            ("", "pending"),
            "scanning alone does not hash"
        );

        conn.execute(
            "UPDATE items SET content_hash='hash-skip-test', hash_status='done' WHERE id=10",
            [],
        )
        .unwrap();
        // Control row: a path that is no longer on disk must be reconciled to
        // missing, proving this second scan really walked the artist instead of
        // silently doing nothing (which would otherwise fake a passing skip).
        let ghost = normalize_slashes(
            &dir.path()
                .join("pictures")
                .join("ArtistA")
                .join("ghost.jpg")
                .to_string_lossy(),
        );
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, media_type, missing)
             VALUES (11, 1, ?, 'ghost.jpg', 'image', 0)",
            params![ghost],
        )
        .unwrap();
        let second = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(
            second["updated_items"], 0i64,
            "an unchanged hashed item must not be rewritten: {second}"
        );
        let (folder, hash, status) = item_folder_and_hash(&conn);
        assert_eq!(
            folder, "",
            "folder_name remains clean"
        );
        assert_eq!(
            (hash.as_str(), status.as_str()),
            ("hash-skip-test", "done"),
            "skipping the write must not reset the hash"
        );
        let ghost_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=11", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            ghost_missing, 1,
            "the second scan really walked the artist and reconciled the ghost row"
        );

        // Derived metadata repair (S5): corrupting folder_name must trigger a repair
        // write while preserving the already computed content_hash.
        conn.execute(
            "UPDATE items SET folder_name='sentinel' WHERE id=10",
            [],
        )
        .unwrap();
        let third = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(
            third["updated_items"], 1i64,
            "divergent derived metadata is repaired on rescan: {third}"
        );
        let (folder, hash, status) = item_folder_and_hash(&conn);
        assert_eq!(
            folder, "",
            "the corrupted folder_name is repaired back to disk reality"
        );
        assert_eq!(
            (hash.as_str(), status.as_str()),
            ("hash-skip-test", "done"),
            "metadata repair must retain the valid content_hash"
        );
    }

    #[test]
    fn changed_file_contents_force_item_rewrite_and_hash_reset() {
        let (_dir, conn, roots, file) = stable_item_fixture();
        std::fs::write(&file, b"jpg-and-more").unwrap();

        let out = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(out["updated_items"], 1i64, "a changed file is rewritten: {out}");

        let (folder, hash, status) = item_folder_and_hash(&conn);
        assert_eq!(folder, "", "the rewrite clears the sentinel");
        assert_eq!(
            (hash.as_str(), status.as_str()),
            ("", "pending"),
            "a size/mtime change invalidates the stored hash"
        );
    }

    #[test]
    fn missing_flag_forces_item_rewrite() {
        let (_dir, conn, roots, _file) = stable_item_fixture();
        conn.execute("UPDATE items SET missing=1 WHERE id=10", [])
            .unwrap();

        let out = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(
            out["updated_items"], 1i64,
            "a missing-flagged item is rewritten even when every other field matches: {out}"
        );

        let (folder, _hash, _status) = item_folder_and_hash(&conn);
        assert_eq!(folder, "", "the rewrite clears the sentinel");
        let missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0, "a file present on disk is healed");
    }

    #[cfg(unix)]
    #[test]
    fn stale_file_identity_forces_item_rewrite() {
        let (_dir, conn, roots, _file) = stable_item_fixture();
        // Same path and metadata, but the recorded dev/ino no longer matches the
        // file on disk (reformatted volume, restored backup): the row must be
        // rewritten even though size and mtime are unchanged.
        conn.execute("UPDATE items SET st_ino=987654321 WHERE id=10", [])
            .unwrap();

        let out = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(
            out["updated_items"], 1i64,
            "a stale file identity must force the write: {out}"
        );

        let (folder, hash, status) = item_folder_and_hash(&conn);
        assert_eq!(folder, "", "the rewrite clears the sentinel");
        assert_eq!(
            (hash.as_str(), status.as_str()),
            ("", "pending"),
            "stale file identity invalidates content_hash and resets hash_status to pending"
        );
    }

    #[cfg(unix)]
    #[test]
    fn stale_candidate_identity_invalidates_hash_and_requeues() {
        let (dir, conn, roots) = fixture();
        let file = dir.path().join("pictures").join("ArtistA").join("new_cand.jpg");
        std::fs::write(&file, b"content").unwrap();
        let file_s = normalize_slashes(&file.to_string_lossy());

        // Scan once to register the candidate (fixture has 2 files + 1 new file = 3)
        let out = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(out["new_candidates"], 3);

        // Mark candidate as previewed with done hash
        conn.execute(
            "UPDATE scan_candidates
             SET content_hash='cand-hash', hash_status='done', status='previewed', st_ino=111222
             WHERE file_path=?",
            params![&file_s],
        )
        .unwrap();

        // Rescan: the file on disk has real ino != 111222
        let out2 = run_scan(&conn, &roots, &ScanControl::new(), Some(1), None).unwrap();
        assert_eq!(out2["new_candidates"], 1, "identity change requeues candidate");

        let (hash, status, cand_status): (String, String, String) = conn
            .query_row(
                "SELECT content_hash, hash_status, status FROM scan_candidates WHERE file_path=?",
                params![&file_s],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(hash, "", "inode change on candidate clears content_hash");
        assert_eq!(status, "pending", "inode change resets hash_status");
        assert_eq!(cand_status, "pending", "inode change resets candidate status");
    }

    #[test]
    fn scan_slot_ticket_guard_prevents_stale_release() {
        let control = Arc::new(ScanControl::new());
        let guard1 = control.try_claim().expect("guard1 claim succeeds");
        assert!(control.is_running());
        assert!(control.try_claim().is_none(), "second claim while held must fail");

        // Release guard1
        drop(guard1);
        assert!(!control.is_running(), "after guard1 drops, slot is free");

        // Guard2 claims ticket 2
        let guard2 = control.try_claim().expect("guard2 claim succeeds");
        assert!(control.is_running());

        // A stale release with ticket 1 (e.g. from an old guard or callback) must NOT clear guard2!
        control.release_ticket(1);
        assert!(control.is_running(), "stale ticket release must not clear active slot");

        // Dropping guard2 releases ticket 2
        drop(guard2);
        assert!(!control.is_running(), "guard2 drop frees slot");
    }

    #[test]
    fn artist_relocation_rolls_back_on_failure() {
        let (_dir, conn, _roots) = fixture();
        // Artist 1 exists with path .../pictures/ArtistA
        let old_path: String = conn
            .query_row("SELECT path FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();
        let new_path = format!("{}_relocated", old_path);

        // Seed an item under artist 1
        let item_path = format!("{}/one.jpg", old_path);
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'one.jpg')",
            params![item_path],
        )
        .unwrap();

        // Inject a trigger failure on move_history insertion
        conn.execute(
            "CREATE TRIGGER fail_move_history BEFORE INSERT ON move_history
             BEGIN SELECT RAISE(ABORT, 'injected failure in move_history'); END;",
            [],
        )
        .unwrap();

        // Attempt relocation
        let err = apply_artist_path_relocation(&conn, 1, "ArtistA", &old_path, &new_path, 3)
            .unwrap_err();
        assert!(err.to_string().contains("injected failure"));

        // Verify transaction rolled back completely:
        let current_path: String = conn
            .query_row("SELECT path FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(current_path, old_path, "artist path must not change when transaction fails");

        let items_path: String = conn
            .query_row(
                "SELECT file_path FROM items WHERE artist_id=1 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(items_path.starts_with(&old_path), "items path must not be modified");
    }

    #[test]
    fn artist_relocation_fails_on_concurrent_path_conflict() {
        let (_dir, conn, _roots) = fixture();
        let old_path: String = conn
            .query_row("SELECT path FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();

        // Concurrently change artist path to something else
        conn.execute(
            "UPDATE artists SET path='/concurrent/other' WHERE id=1",
            [],
        )
        .unwrap();

        let err = apply_artist_path_relocation(&conn, 1, "ArtistA", &old_path, "/new/path", 3)
            .unwrap_err();
        assert!(err.to_string().contains("cannot relocate artist 1"));
    }

    #[test]
    fn walk_artist_captures_io_errors_without_dropping_cause() {
        let (dir, conn, _roots) = fixture();
        let non_existent = dir.path().join("pictures").join("ArtistA").join("ghost_dir");
        let non_existent_s = normalize_slashes(&non_existent.to_string_lossy());
        let control = ScanControl::new();

        let outcome = walk_artist(
            &conn,
            1,
            &non_existent_s,
            &non_existent_s,
            "test-scan-id",
            &control,
            &test_spool(dir.path()),
        )
        .unwrap();

        assert!(!outcome.errors.is_empty(), "walk errors must be captured");
        assert!(
            outcome.errors[0].contains("walk entry error in"),
            "error description must identify location: {}",
            outcome.errors[0]
        );
    }

    #[test]
    fn pre_scan_artist_scope_failure_marks_partial_and_blocks_auto_archive() {
        let (dir, conn, roots) = fixture();
        let control = ScanControl::new();

        // Create ArtistB on disk so discovery finds it
        let media = dir.path().join("pictures");
        let artist_b = media.join("ArtistB");
        std::fs::create_dir_all(&artist_b).unwrap();
        std::fs::write(artist_b.join("pic.jpg"), b"content").unwrap();

        // Register a custom collation that removes ArtistB when artists row is touched
        let artist_b_clone = artist_b.clone();
        conn.create_collation("del_b", move |a, b| {
            let _ = std::fs::remove_dir_all(&artist_b_clone);
            a.cmp(b)
        })
        .unwrap();
        conn.execute(
            "CREATE TABLE trigger_coll (name TEXT COLLATE del_b PRIMARY KEY)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO trigger_coll (name) VALUES ('AAA')",
            [],
        )
        .unwrap();
        conn.execute(
            "CREATE TRIGGER remove_b_after_insert AFTER INSERT ON artists
             WHEN NEW.name = 'ArtistB'
             BEGIN
                 INSERT INTO trigger_coll (name) VALUES (NEW.name);
             END;",
            [],
        )
        .unwrap();

        let outcome = run_full_library_scan_claimed(&conn, &roots, &control).unwrap();
        let scan_val = if outcome.get("scan").is_some() {
            &outcome["scan"]
        } else {
            &outcome
        };
        let phase = scan_val.get("phase").and_then(|v| v.as_str()).unwrap();
        assert_eq!(phase, "partial", "phase must be partial when one artist fails pre-scan");
        assert_eq!(scan_val["failed_artists"], 1);
        assert_eq!(scan_val["completed_artists"], 2);

        let state = get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["phase"], "partial");
        assert!(
            state["current_path"].as_str().unwrap().contains("pre-scan artist error"),
            "scan_state current_path must retain error reason: {}",
            state["current_path"]
        );

        // Auto-archive must not have executed:
        assert!(
            outcome.get("archive").is_none(),
            "auto archive must not run on partial scan"
        );
    }

    #[test]
    fn null_inode_backfilled_without_invalidating_hash_and_rescans_zero_writes() {
        let (dir, conn, roots) = fixture();
        let control = ScanControl::new();

        let artist_dir = dir.path().join("pictures").join("ArtistA");
        let file_path = normalize_slashes(&artist_dir.join("one.jpg").to_string_lossy());
        let meta = std::fs::metadata(artist_dir.join("one.jpg")).unwrap();
        let file_size = meta.len() as i64;
        let file_mtime = meta
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime,
                                folder_name, detected_date, date, manual_date, is_archive, media_type,
                                content_hash, hash_status, missing, st_dev, st_ino)
             VALUES (100, 1, ?, 'one.jpg', ?, ?, '', '', '', NULL, 0, 'image',
                     'sentinel_hash_123', 'done', 0, NULL, NULL)",
            params![file_path, file_size, file_mtime],
        )
        .unwrap();

        // Rescan: the missing identity should trigger an update to backfill st_dev/st_ino,
        // but can_reuse_hash should preserve content_hash and hash_status='done'.
        let res2 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res2["phase"], "complete");

        let (hash, status, dev, ino): (String, String, Option<i64>, Option<i64>) = conn
            .query_row(
                "SELECT content_hash, hash_status, st_dev, st_ino FROM items WHERE id=100",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash, "sentinel_hash_123", "hash must be preserved when backfilling NULL inode");
        assert_eq!(status, "done", "hash_status must remain done");

        #[cfg(unix)]
        {
            assert!(dev.is_some() && ino.is_some(), "st_dev and st_ino must be backfilled on unix");
        }
        #[cfg(not(unix))]
        {
            let _ = (dev, ino);
        }

        // Third scan: now that st_dev/st_ino are synced with disk, unchanged must be true
        let res3 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res3["updated_items"], 0, "third scan must be 0 writes (unchanged fast-path)");
    }

    #[test]
    fn clean_rescan_skips_scan_seen_writes_and_reconcile() {
        let (dir, conn, roots) = fixture();
        let control = ScanControl::new();

        // 1. Initial scan creates candidates
        run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();

        // Convert scan_candidates to items so they are active items in DB
        conn.execute_batch(
            "INSERT INTO items (artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                                date, detected_date, manual_date, is_archive, media_type,
                                content_hash, hash_status, missing, st_dev, st_ino)
             SELECT artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                    date, date, NULL, is_archive, media_type,
                    'h123', 'done', 0, st_dev, st_ino
             FROM scan_candidates;
             DELETE FROM scan_candidates;"
        ).unwrap();

        // Verify active items match disk (2 files: one.jpg and sub/two.jpg)
        let active_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM items WHERE missing=0", [], |r| r.get(0))
            .unwrap();
        assert_eq!(active_count, 2);

        // Install a trigger on scan_seen that aborts if any row is inserted.
        conn.execute_batch(
            "CREATE TRIGGER test_no_scan_seen_insert BEFORE INSERT ON scan_seen
             BEGIN
                 SELECT RAISE(FAIL, 'unexpected scan_seen write on clean artist');
             END;"
        ).unwrap();

        // 2. Second scan: identical directory, 0 changes -> must bypass scan_seen completely!
        let res2 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res2["phase"], "complete");
        assert_eq!(res2["updated_items"], 0);
        assert_eq!(res2["new_candidates"], 0);

        let seen_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(seen_count, 0, "scan_seen must have 0 rows");

        // Drop the blocking trigger
        conn.execute("DROP TRIGGER test_no_scan_seen_insert", []).unwrap();

        // Count inserts into scan_seen using a counter trigger
        conn.execute_batch(
            "CREATE TABLE test_seen_counter (cnt INTEGER);
             INSERT INTO test_seen_counter VALUES (0);
             CREATE TRIGGER test_count_scan_seen_insert AFTER INSERT ON scan_seen
             BEGIN
                 UPDATE test_seen_counter SET cnt = cnt + 1;
             END;"
        ).unwrap();

        // 3. Delete one file from disk (one.jpg)
        let artist_dir = dir.path().join("pictures").join("ArtistA");
        std::fs::remove_file(artist_dir.join("one.jpg")).unwrap();

        // Run rescan: since 1 file was deleted, matched_active (1) != db_active (2),
        // so needs_reconcile is true!
        let res3 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res3["phase"], "complete");

        // scan_seen should have received exactly 1 row (for sub/two.jpg)
        let inserted_seen: i64 = conn
            .query_row("SELECT cnt FROM test_seen_counter", [], |r| r.get(0))
            .unwrap();
        assert_eq!(inserted_seen, 1, "scan_seen must receive remaining file on reconcile");

        // But after artist walk completes, scan_seen must be immediately deleted per-artist
        let remaining_seen: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining_seen, 0, "scan_seen must be cleaned up immediately per-artist");

        // The deleted file must be marked missing=1
        let one_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE file_name='one.jpg'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(one_missing, 1, "deleted file must be marked missing=1");

        let two_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE file_name='two.jpg'", [], |r| r.get(0))
            .unwrap();
        assert_eq!(two_missing, 0, "existing file must remain missing=0");
    }

    #[test]
    fn artist_missing_flag_skips_redundant_updates() {
        let (_dir, conn, roots) = fixture();
        let control = ScanControl::new();

        // Initial scan
        run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();

        // Artist missing is 0
        let missing: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(missing, 0);

        // Attach trigger to detect updates on artists table
        conn.execute_batch(
            "CREATE TABLE test_artist_updates (cnt INTEGER);
             INSERT INTO test_artist_updates VALUES (0);
             CREATE TRIGGER test_trg_artists_update AFTER UPDATE ON artists
             BEGIN
                 UPDATE test_artist_updates SET cnt = cnt + 1;
             END;"
        ).unwrap();

        // Second scan: artist is still missing=0, so register_discovered_artist must NOT update artists
        run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();

        let update_count: i64 = conn
            .query_row("SELECT cnt FROM test_artist_updates", [], |r| r.get(0))
            .unwrap();
        assert_eq!(update_count, 0, "artists table must not be updated when missing is already 0");
    }

    #[test]
    fn batch_processing_handles_large_artist_file_counts() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistBig");
        std::fs::create_dir_all(&artist).unwrap();
        for i in 0..250 {
            std::fs::write(artist.join(format!("file_{:03}.jpg", i)), b"test").unwrap();
        }
        let db_path = dir.path().join("t.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0, missing_at REAL);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              st_dev INTEGER, st_ino INTEGER, missing INTEGER DEFAULT 0, missing_at REAL, scanned_at INTEGER DEFAULT 0);
            CREATE TABLE scan_candidates (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER, file_mtime REAL, folder_name TEXT, date TEXT, is_archive INTEGER,
              media_type TEXT, content_hash TEXT, hash_status TEXT, status TEXT, st_dev INTEGER, st_ino INTEGER, resolved_at REAL);
            CREATE TABLE scan_seen (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              media_type TEXT, file_size INTEGER, file_mtime REAL, content_hash TEXT, hash_status TEXT);
            CREATE TABLE move_candidates (id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT, new_path TEXT, reason TEXT, status TEXT, resolved_at REAL);
            CREATE TABLE move_history (id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}', applied_at REAL);
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            ",
        ).unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists (id, name, path) VALUES (1, 'ArtistBig', ?)", params![path]).unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let control = ScanControl::new();
        let res = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res["phase"], "complete");
        let cand_count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0)).unwrap();
        assert_eq!(cand_count, 250);
    }

    #[test]
    fn presence_tracker_enforces_bounded_memory_budget_and_spools_to_temp_file() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistSpool");
        std::fs::create_dir_all(&artist).unwrap();
        // Create 450 files (exceeds MAX_IN_MEMORY_PRESENCE=200 multiple times)
        for i in 0..450 {
            std::fs::write(artist.join(format!("spool_{:03}.jpg", i)), b"presence_test").unwrap();
        }
        let db_path = dir.path().join("t.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0, missing_at REAL);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              st_dev INTEGER, st_ino INTEGER, missing INTEGER DEFAULT 0, missing_at REAL, scanned_at INTEGER DEFAULT 0);
            CREATE TABLE scan_candidates (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER, file_mtime REAL, folder_name TEXT, date TEXT, is_archive INTEGER,
              media_type TEXT, content_hash TEXT, hash_status TEXT, status TEXT, st_dev INTEGER, st_ino INTEGER, resolved_at REAL);
            CREATE TABLE scan_seen (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              media_type TEXT, file_size INTEGER, file_mtime REAL, content_hash TEXT, hash_status TEXT);
            CREATE TABLE move_candidates (id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT, new_path TEXT, reason TEXT, status TEXT, resolved_at REAL);
            CREATE TABLE move_history (id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}', applied_at REAL);
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            ",
        ).unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists (id, name, path) VALUES (1, 'ArtistSpool', ?)", params![path]).unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let control = ScanControl::new();

        // 1. First scan discovers candidates
        let res1 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res1["phase"], "complete");
        let cand_count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0)).unwrap();
        assert_eq!(cand_count, 450);

        // Convert to active done items
        conn.execute_batch(
            "INSERT INTO items (artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                                date, detected_date, manual_date, is_archive, media_type,
                                content_hash, hash_status, missing, st_dev, st_ino)
             SELECT artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                    date, date, NULL, is_archive, media_type,
                    'hash_spool', 'done', 0, st_dev, st_ino
             FROM scan_candidates;
             DELETE FROM scan_candidates;"
        ).unwrap();

        // 2. Install a blocking trigger on scan_seen: if any row is inserted into persistent scan_seen, fail!
        conn.execute_batch(
            "CREATE TRIGGER test_no_persistent_scan_seen BEFORE INSERT ON scan_seen
             BEGIN
                 SELECT RAISE(FAIL, 'persistent scan_seen must not be written for clean large artist');
             END;"
        ).unwrap();

        // Clean rescan on large directory (450 files > 200 memory budget)
        let res2 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res2["phase"], "complete");
        assert_eq!(res2["updated_items"], 0);
        assert_eq!(res2["new_candidates"], 0);

        let persistent_seen: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
        assert_eq!(persistent_seen, 0, "persistent scan_seen must have 0 rows");

        // 3. Now delete 1 file from the 450 files (leaving 449 files).
        conn.execute("DROP TRIGGER test_no_persistent_scan_seen", []).unwrap();
        conn.execute_batch(
            "CREATE TABLE test_seen_spool_counter (cnt INTEGER);
             INSERT INTO test_seen_spool_counter VALUES (0);
             CREATE TRIGGER test_count_seen_spool AFTER INSERT ON scan_seen
             BEGIN
                 UPDATE test_seen_spool_counter SET cnt = cnt + 1;
             END;"
        ).unwrap();

        std::fs::remove_file(artist.join("spool_000.jpg")).unwrap();

        // Rescan: matched_active (449) != db_active (450) -> needs_reconcile = true!
        // Spooled rows (200 + 200) plus buffer (49) must all be flushed to scan_seen for reconcile
        let res3 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res3["phase"], "complete");

        let total_inserted_seen: i64 = conn.query_row("SELECT cnt FROM test_seen_spool_counter", [], |r| r.get(0)).unwrap();
        assert_eq!(total_inserted_seen, 449, "must flush all 449 remaining paths to scan_seen");

        // Deleted file must be marked missing=1
        let missing_count: i64 = conn.query_row("SELECT COUNT(*) FROM items WHERE missing=1", [], |r| r.get(0)).unwrap();
        assert_eq!(missing_count, 1, "the single deleted file must be marked missing=1");

        let missing_name: String = conn.query_row("SELECT file_name FROM items WHERE missing=1", [], |r| r.get(0)).unwrap();
        assert_eq!(missing_name, "spool_000.jpg");

        // After artist scan completes, scan_seen must be cleared per-artist
        let scan_seen_remaining: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
        assert_eq!(scan_seen_remaining, 0, "scan_seen must be cleaned up per-artist");
    }

    #[test]
    fn failed_presence_flush_leaves_no_contamination_for_subsequent_reconciliation_on_same_connection() {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistContam");
        std::fs::create_dir_all(&artist).unwrap();
        // Create 450 files (more than 2x MAX_IN_MEMORY_PRESENCE)
        for i in 0..450 {
            std::fs::write(artist.join(format!("file_{:03}.jpg", i)), b"content").unwrap();
        }
        let db_path = dir.path().join("t.db");
        let conn = crate::db::open_writable_db(&db_path).unwrap();

        // Check that temp_store is indeed MEMORY (2) as configured by production configure_connection
        let temp_store: i64 = conn.query_row("PRAGMA temp_store", [], |r| r.get(0)).unwrap();
        assert_eq!(temp_store, 2, "PRAGMA temp_store must be MEMORY (2)");

        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0, missing_at REAL);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              st_dev INTEGER, st_ino INTEGER, missing INTEGER DEFAULT 0, missing_at REAL, scanned_at INTEGER DEFAULT 0);
            CREATE TABLE scan_candidates (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER, file_mtime REAL, folder_name TEXT, date TEXT, is_archive INTEGER,
              media_type TEXT, content_hash TEXT, hash_status TEXT, status TEXT, st_dev INTEGER, st_ino INTEGER, resolved_at REAL);
            CREATE TABLE scan_seen (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              media_type TEXT, file_size INTEGER, file_mtime REAL, content_hash TEXT, hash_status TEXT);
            CREATE TABLE move_candidates (id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT, new_path TEXT, reason TEXT, status TEXT, resolved_at REAL);
            CREATE TABLE move_history (id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}', applied_at REAL);
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            ",
        ).unwrap();
        let path = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists (id, name, path) VALUES (1, 'ArtistContam', ?)", params![path]).unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let control = ScanControl::new();

        // 1. First scan discovers candidates
        let res1 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res1["phase"], "complete");
        let cand_count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_candidates", [], |r| r.get(0)).unwrap();
        assert_eq!(cand_count, 450);

        // Convert candidates to active done items
        conn.execute_batch(
            "INSERT INTO items (artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                                date, detected_date, manual_date, is_archive, media_type,
                                content_hash, hash_status, missing, st_dev, st_ino)
             SELECT artist_id, file_path, file_name, file_size, file_mtime, folder_name,
                    date, date, NULL, is_archive, media_type,
                    'hash_val', 'done', 0, st_dev, st_ino
             FROM scan_candidates;
             DELETE FROM scan_candidates;"
        ).unwrap();

        // 2. Delete file_000.jpg on disk so that a reconcile is needed
        std::fs::remove_file(artist.join("file_000.jpg")).unwrap();

        // 3. Inject a failure on scan_seen: simulate SQL error during presence flush
        conn.execute_batch(
            "CREATE TRIGGER test_inject_presence_failure BEFORE INSERT ON scan_seen
             BEGIN
                 SELECT RAISE(ABORT, 'injected presence failure');
             END;"
        ).unwrap();

        // 4. Run scan: must fail due to trigger
        let res2 = run_scan_claimed(&conn, &roots, &control, Some(1), None);
        assert!(res2.is_err(), "scan must fail when presence flush triggers error");
        let err_str = res2.unwrap_err().to_string();
        assert!(err_str.contains("injected presence failure"));

        // 5. Now delete a SECOND file (file_001.jpg) from the 450 files
        std::fs::remove_file(artist.join("file_001.jpg")).unwrap();

        // 6. Remove the failure trigger
        conn.execute("DROP TRIGGER test_inject_presence_failure", []).unwrap();

        // 7. Rescan on the EXACT SAME connection (as pooled connections are reused)
        let res3 = run_scan_claimed(&conn, &roots, &control, Some(1), None).unwrap();
        assert_eq!(res3["phase"], "complete");

        // 8. Verify H1: Both deleted files must be marked missing=1!
        // Under the old bug, file_001.jpg was marked missing=0 due to stale presence rows.
        let missing_000: i64 = conn.query_row(
            "SELECT missing FROM items WHERE file_name='file_000.jpg'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(missing_000, 1, "file_000.jpg must be missing=1");

        let missing_001: i64 = conn.query_row(
            "SELECT missing FROM items WHERE file_name='file_001.jpg'",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(missing_001, 1, "file_001.jpg must be missing=1 (no contamination from failed run!)");

        let total_missing: i64 = conn.query_row(
            "SELECT COUNT(*) FROM items WHERE missing=1",
            [],
            |r| r.get(0),
        ).unwrap();
        assert_eq!(total_missing, 2, "exactly the two deleted files must be marked missing");

        // Scan seen must be empty
        let scan_seen_count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
        assert_eq!(scan_seen_count, 0, "scan_seen must be empty");
    }

    #[test]
    fn presence_tracker_rejects_truncated_or_corrupted_spool_file() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("t.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute(
            "CREATE TABLE scan_seen (id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT)",
            [],
        ).unwrap();

        // 1. 200 items (exact spool threshold) -> success
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            assert_eq!(tracker.spooled_count, 200);
            assert!(tracker.buffer.is_empty());
            tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap();
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen WHERE scan_id='scan1'", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 200);
            conn.execute("DELETE FROM scan_seen", []).unwrap();
        }

        // 2. 201 items (200 spooled + 1 in buffer) -> success
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..201 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            assert_eq!(tracker.spooled_count, 200);
            assert_eq!(tracker.buffer.len(), 1);
            tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap();
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen WHERE scan_id='scan1'", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 201);
            conn.execute("DELETE FROM scan_seen", []).unwrap();
        }

        // 3. 450 items (400 spooled + 50 in buffer) -> success
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..450 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            assert_eq!(tracker.spooled_count, 400);
            assert_eq!(tracker.buffer.len(), 50);
            tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap();
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen WHERE scan_id='scan1'", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 450);
            conn.execute("DELETE FROM scan_seen", []).unwrap();
        }

        // 4. Truncated length header (less than 4 bytes: e.g. 2 bytes total)
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            file.set_len(2).unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("presence spool read error"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 5. Truncated at record boundary (e.g. 400 spooled, but file truncated to 200 records)
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..400 {
                tracker.push(format!("/path/to/file_{:03}.jpg", i)).unwrap();
            }
            assert_eq!(tracker.spooled_count, 400);
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            let original_len = file.metadata().unwrap().len();
            file.set_len(original_len / 2).unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("presence spool read error") || err.to_string().contains("record count mismatch"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 6. Payload truncated (chop off 5 bytes from payload)
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            let original_len = file.metadata().unwrap().len();
            file.set_len(original_len - 5).unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("payload truncated") || err.to_string().contains("read error"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 7. Invalid UTF-8 in payload
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            file.seek(SeekFrom::Start(4)).unwrap(); // skip first 4-byte length
            file.write_all(&[0xFF, 0xFE, 0xFD]).unwrap(); // invalid UTF-8 bytes
            file.flush().unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("invalid UTF-8 path"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 8. Corrupted oversized length (> MAX_SPOOLED_PATH_BYTES)
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&(100_000u32).to_le_bytes()).unwrap();
            file.flush().unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("corrupted length"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 9. Corrupted length of 0
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            file.seek(SeekFrom::Start(0)).unwrap();
            file.write_all(&(0u32).to_le_bytes()).unwrap();
            file.flush().unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("corrupted length"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }

        // 10. Trailing unread garbage after all records
        {
            let mut tracker = PresenceTracker::new(1, test_spool(dir.path()));
            for i in 0..200 {
                tracker.push(format!("/path/to/file_{i}.jpg")).unwrap();
            }
            let file = &mut tracker.spooled_file.as_mut().unwrap().file;
            file.seek(SeekFrom::End(0)).unwrap();
            file.write_all(b"garbage_trailing_bytes").unwrap();
            file.flush().unwrap();

            let err = tracker.flush_to_persistent_scan_seen(&conn, "scan1", 1).unwrap_err();
            assert!(err.to_string().contains("trailing data"));
            let count: i64 = conn.query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0)).unwrap();
            assert_eq!(count, 0, "scan_seen must be empty after failure");
        }
    }

    fn test_spool(dir: &Path) -> PresenceSpoolConfig {
        PresenceSpoolConfig {
            dir: dir.to_path_buf(),
            max_bytes: 64 * 1024 * 1024,
        }
    }

    #[test]
    fn presence_tracker_enforces_byte_budget_and_blocks_overflow() {
        let dir = tempfile::tempdir().unwrap();
        // Budget is passed explicitly: the test never mutates process-wide
        // environment, so a parallel scan keeps its own default budget.
        let mut tracker = PresenceTracker::new(
            1,
            PresenceSpoolConfig {
                dir: dir.path().to_path_buf(),
                max_bytes: 1024,
            },
        );
        let mut err = None;
        for i in 0..250 {
            if let Err(e) = tracker.push(format!("/very/long/path/name/for/media/file_{:04}.jpg", i))
            {
                err = Some(e);
                break;
            }
        }
        let err = err.expect("pushing beyond byte budget must return Err");
        assert!(
            err.to_string().contains("presence spool byte budget exceeded"),
            "error message should cite budget exceeded: {err}"
        );
    }

    #[test]
    fn presence_spool_owner_pid_only_accepts_own_naming() {
        assert_eq!(
            presence_spool_owner_pid("gallery_pres_ab12_4242").as_deref(),
            Some("4242")
        );
        assert_eq!(presence_spool_owner_pid("gallery_pres_ab12.tmp"), None);
        assert_eq!(presence_spool_owner_pid("abandoned_old.tmp"), None);
        assert_eq!(presence_spool_owner_pid("gallery_pres__"), None);
    }

    #[test]
    fn cleanup_stale_presence_spools_only_reclaims_aged_own_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let spool_path = temp_dir.path().to_path_buf();

        let foreign_stale = spool_path.join("gallery_pres_ab12_4242");
        let foreign_fresh = spool_path.join("gallery_pres_cd34_4243");
        let own_file = spool_path.join(format!("gallery_pres_ef56_{}", std::process::id()));
        let unrelated_tmp = spool_path.join("unrelated_export.tmp");
        let unknown_prefix = spool_path.join("other_tool_4242");
        let legitimate = spool_path.join("config.json");
        let subdir = spool_path.join("gallery_pres_gh78_4244");
        std::fs::create_dir(&subdir).unwrap();
        let nested_stale = subdir.join("gallery_pres_ij90_4245");
        for path in [
            &foreign_stale,
            &foreign_fresh,
            &own_file,
            &unrelated_tmp,
            &unknown_prefix,
            &legitimate,
            &nested_stale,
        ] {
            std::fs::write(path, b"old").unwrap();
        }

        // Only the foreign file is both aged and named by this process's
        // convention; everything else must survive untouched.
        let aged = SystemTime::now() - PRESENCE_SPOOL_MAX_AGE - Duration::from_secs(60);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&foreign_stale)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&nested_stale)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        let cleaned = cleanup_stale_presence_spools_in(&spool_path, PRESENCE_SPOOL_MAX_AGE);

        assert_eq!(cleaned, 1, "only the aged foreign spool file is reclaimed");
        assert!(!foreign_stale.exists());
        assert!(foreign_fresh.exists(), "fresh spool file must survive");
        assert!(own_file.exists(), "current process spool file must survive");
        assert!(unrelated_tmp.exists(), "unrelated .tmp must survive");
        assert!(unknown_prefix.exists(), "unknown prefix must survive");
        assert!(legitimate.exists(), "unrelated file must survive");
        assert!(nested_stale.exists(), "cleanup must not recurse");
    }

    #[test]
    fn delete_scan_seen_chunked_deletes_across_multiple_transactions() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scan_seen (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_id TEXT NOT NULL,
                artist_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                created_at REAL DEFAULT 0
            );
            CREATE INDEX idx_scan_seen_artist ON scan_seen(artist_id);",
        )
        .unwrap();

        // 1. Delete 550 items with both scan_id and artist_id
        {
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..550 {
                tx.execute(
                    "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                    params!["scan-1", 10, format!("/path/{i}.jpg")],
                )
                .unwrap();
            }
            tx.commit().unwrap();

            let deleted = delete_scan_seen_chunked(&conn, Some("scan-1"), Some(10)).unwrap();
            assert_eq!(deleted, 550);
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0))
                .unwrap();
            assert_eq!(remaining, 0);
        }

        // 2. Delete with artist_id only
        {
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..450 {
                tx.execute(
                    "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                    params![format!("scan-{}", i % 3), 20, format!("/path/{i}.jpg")],
                )
                .unwrap();
            }
            tx.commit().unwrap();

            let deleted = delete_scan_seen_chunked(&conn, None, Some(20)).unwrap();
            assert_eq!(deleted, 450);
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0))
                .unwrap();
            assert_eq!(remaining, 0);
        }

        // 3. Delete all with (None, None)
        {
            let tx = conn.unchecked_transaction().unwrap();
            for i in 0..350 {
                tx.execute(
                    "INSERT INTO scan_seen (scan_id, artist_id, file_path) VALUES (?, ?, ?)",
                    params![format!("scan-{}", i % 2), i % 5, format!("/path/{i}.jpg")],
                )
                .unwrap();
            }
            tx.commit().unwrap();

            let deleted = delete_scan_seen_chunked(&conn, None, None).unwrap();
            assert_eq!(deleted, 350);
            let remaining: i64 = conn
                .query_row("SELECT COUNT(*) FROM scan_seen", [], |r| r.get(0))
                .unwrap();
            assert_eq!(remaining, 0);
        }

        // 4. Empty table returns 0 without error
        let deleted = delete_scan_seen_chunked(&conn, Some("scan-none"), None).unwrap();
        assert_eq!(deleted, 0);
    }

    // --- L3: sub-second mtime changes must invalidate cached hashes ---

    fn seeded_item(conn: &Connection, keep_hash: bool) {
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime,
                 content_hash, hash_status, hash_updated_at, folder_name, detected_date, date,
                 manual_date, is_archive, media_type, missing, st_dev, st_ino)
             VALUES (1, 1, '/a/one.jpg', 'one.jpg', 3, 1000.0, ?, ?, 1000.0,
                 '', '', '', NULL, 0, 'image', 0, NULL, NULL)",
            params![
                if keep_hash { "deadbeef" } else { "" },
                if keep_hash { "done" } else { "pending" }
            ],
        )
        .unwrap();
    }

    fn discovered_file(mtime: f64) -> DiscoveredFile {
        DiscoveredFile {
            full: "/a/one.jpg".into(),
            fname: "one.jpg".into(),
            media_type: "image",
            file_size: 3,
            file_mtime: mtime,
            st_dev: None,
            st_ino: None,
            folder_name: String::new(),
            date_str: String::new(),
            detected_raw: String::new(),
            is_archive: 0,
        }
    }

    #[test]
    fn process_discovered_batch_keeps_hash_when_mtime_unchanged() {
        let (_dir, conn, _roots) = fixture();
        seeded_item(&conn, true);
        let mut m = 0i64;
        let mut u = 0i64;
        let mut nc = 0i64;
        process_discovered_batch(&conn, &[discovered_file(1000.0)], "s1", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h, st): (String, String) = conn
            .query_row("SELECT content_hash, hash_status FROM items WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(h, "deadbeef");
        assert_eq!(st, "done");
    }

    #[test]
    fn process_discovered_batch_rehashes_after_subsecond_and_one_second_change() {
        let (_dir, conn, _roots) = fixture();
        seeded_item(&conn, true);
        let mut m = 0i64;
        let mut u = 0i64;
        let mut nc = 0i64;

        // 0.5ms (500 microseconds = 0.0005s) mtime bump must invalidate the cached hash.
        process_discovered_batch(&conn, &[discovered_file(1000.0005)], "s2_sub_ms", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h_sub, st_sub): (String, String) = conn
            .query_row("SELECT content_hash, hash_status FROM items WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(h_sub, "", "0.5ms (500us) mtime change must invalidate cached hash");
        assert_eq!(st_sub, "pending");

        // 500ms mtime bump must also invalidate the cached hash.
        process_discovered_batch(&conn, &[discovered_file(1000.5)], "s2", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h, st): (String, String) = conn
            .query_row("SELECT content_hash, hash_status FROM items WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(h, "", "500ms mtime change must invalidate cached hash");
        assert_eq!(st, "pending");

        // 1s mtime bump must also invalidate.
        process_discovered_batch(&conn, &[discovered_file(1001.0)], "s3", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h2, st2): (String, String) = conn
            .query_row("SELECT content_hash, hash_status FROM items WHERE id=1", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(h2, "", "1s mtime change must invalidate cached hash");
        assert_eq!(st2, "pending");
    }

    #[test]
    fn process_discovered_batch_rehashes_candidate_after_subsecond_change() {
        let (_dir, conn, _roots) = fixture();
        conn.execute(
            "INSERT INTO scan_candidates (id, scan_id, artist_id, file_path, file_name, file_size,
                 file_mtime, folder_name, date, is_archive, media_type, content_hash, hash_status, status, st_dev, st_ino)
             VALUES (1, 's0', 1, '/a/one.jpg', 'one.jpg', 3, 1000.0, '', '', 0, 'image', 'deadbeef', 'done', 'candidate', NULL, NULL)",
            [],
        )
        .unwrap();
        let mut m = 0i64;
        let mut u = 0i64;
        let mut nc = 0i64;

        process_discovered_batch(&conn, &[discovered_file(1000.0)], "s1", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h, st): (String, String) = conn
            .query_row(
                "SELECT content_hash, hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(h, "deadbeef");
        assert_eq!(st, "done");

        // 0.5ms (500us = 0.0005s) mtime bump must invalidate candidate hash
        process_discovered_batch(&conn, &[discovered_file(1000.0005)], "s2_sub_ms", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h_sub, st_sub): (String, String) = conn
            .query_row(
                "SELECT content_hash, hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(h_sub, "", "candidate hash must be invalidated after 0.5ms change");
        assert_eq!(st_sub, "pending");

        process_discovered_batch(&conn, &[discovered_file(1000.5)], "s2", 1, &mut m, &mut u, &mut nc)
            .unwrap();
        let (h2, st2): (String, String) = conn
            .query_row(
                "SELECT content_hash, hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(h2, "", "candidate hash must be invalidated after 500ms change");
        assert_eq!(st2, "pending");
    }

    // --- L4: relocation votes count distinct evidence, not duplicate files ---

    #[test]
    fn try_relocate_requires_two_distinct_evidence_identities() {
        let (_dir, conn, _roots) = fixture();
        let new_dir = _dir.path().join("new_artist");
        std::fs::create_dir_all(&new_dir).unwrap();
        let roots = MediaRoots {
            roots: vec![],
            labels: vec![],
            real_paths: vec![],
        };

        // Positive control: two files with different content (distinct hashes)
        // both resolve to the same old artist -> genuine relocation.
        let content_a = "AAAA".repeat(500);
        let content_b = "BBBB".repeat(500);
        std::fs::write(new_dir.join("a.jpg"), &content_a).unwrap();
        std::fs::write(new_dir.join("b.jpg"), &content_b).unwrap();
        let hash_a = crate::content_hash::hash_file(&new_dir.join("a.jpg"), 1024 * 1024).unwrap();
        let hash_b = crate::content_hash::hash_file(&new_dir.join("b.jpg"), 1024 * 1024).unwrap();
        conn.execute("INSERT INTO artists (id, name, path) VALUES (2, 'Old', '/old/path')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, content_hash, hash_status) VALUES (20, 2, '/old/a.jpg', ?, 'done')",
            params![hash_a],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, content_hash, hash_status) VALUES (21, 2, '/old/b.jpg', ?, 'done')",
            params![hash_b],
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta_a = std::fs::metadata(new_dir.join("a.jpg")).unwrap();
            let meta_b = std::fs::metadata(new_dir.join("b.jpg")).unwrap();
            conn.execute(
                "UPDATE items SET st_dev=?, st_ino=? WHERE id=20",
                params![meta_a.dev() as i64, meta_a.ino() as i64],
            )
            .unwrap();
            conn.execute(
                "UPDATE items SET st_dev=?, st_ino=? WHERE id=21",
                params![meta_b.dev() as i64, meta_b.ino() as i64],
            )
            .unwrap();
        }

        let res = try_relocate_artist_dir(
            &conn,
            &roots,
            "Old",
            &new_dir.to_string_lossy().replace('\\', "/"),
        )
        .unwrap();
        assert!(
            res.is_some(),
            "two distinct evidence files should relocate the old artist"
        );
    }

    #[test]
    fn try_relocate_does_not_double_count_identical_evidence() {
        let (_dir, conn, _roots) = fixture();
        let new_dir = _dir.path().join("dup_artist");
        std::fs::create_dir_all(&new_dir).unwrap();
        let roots = MediaRoots {
            roots: vec![],
            labels: vec![],
            real_paths: vec![],
        };

        // Two byte-identical files (hard link on Unix, same-content copy on Windows)
        // must count as ONE piece of evidence, not two votes.
        let content = "ZZZZ".repeat(500);
        std::fs::write(new_dir.join("a.jpg"), &content).unwrap();
        #[cfg(unix)]
        std::fs::hard_link(new_dir.join("a.jpg"), new_dir.join("b.jpg")).unwrap();
        #[cfg(not(unix))]
        std::fs::copy(new_dir.join("a.jpg"), new_dir.join("b.jpg")).unwrap();
        let hash = crate::content_hash::hash_file(&new_dir.join("a.jpg"), 1024 * 1024).unwrap();
        conn.execute("INSERT INTO artists (id, name, path) VALUES (2, 'Old', '/old/path')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, content_hash, hash_status) VALUES (20, 2, '/old/a.jpg', ?, 'done')",
            params![hash],
        )
        .unwrap();

        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            let meta_a = std::fs::metadata(new_dir.join("a.jpg")).unwrap();
            conn.execute(
                "UPDATE items SET st_dev=?, st_ino=? WHERE id=20",
                params![meta_a.dev() as i64, meta_a.ino() as i64],
            )
            .unwrap();
        }

        let res = try_relocate_artist_dir(
            &conn,
            &roots,
            "Old",
            &new_dir.to_string_lossy().replace('\\', "/"),
        )
        .unwrap();
        assert!(
            res.is_none(),
            "two identical files must not count as two relocation votes"
        );
    }

    #[test]
    fn try_relocate_does_not_double_count_duplicate_hashes_without_inode() {
        let (_dir, conn, _roots) = fixture();
        let new_dir = _dir.path().join("hash_dup_artist");
        std::fs::create_dir_all(&new_dir).unwrap();
        let roots = MediaRoots {
            roots: vec![],
            labels: vec![],
            real_paths: vec![],
        };

        let content = "HASH_DUP_TEST".repeat(500);
        std::fs::write(new_dir.join("f1.jpg"), &content).unwrap();
        std::fs::write(new_dir.join("f2.jpg"), &content).unwrap();
        let hash = crate::content_hash::hash_file(&new_dir.join("f1.jpg"), 1024 * 1024).unwrap();

        conn.execute("INSERT INTO artists (id, name, path) VALUES (3, 'OldHash', '/old/hash_path')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, content_hash, hash_status) VALUES (30, 3, '/old/f1.jpg', ?, 'done')",
            params![hash],
        )
        .unwrap();

        let res = try_relocate_artist_dir(
            &conn,
            &roots,
            "OldHash",
            &new_dir.to_string_lossy().replace('\\', "/"),
        )
        .unwrap();
        assert!(
            res.is_none(),
            "two identical hash files must not produce two distinct relocation votes"
        );
    }

    // --- S2: confirmed-missing semantics must not fold unknown into "missing" ---

    #[test]
    fn path_confirmed_missing_distinguishes_notfound_from_present() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope");
        assert!(
            path_confirmed_missing(&missing),
            "NotFound must be confirmed missing"
        );
        let present = dir.path().join("present.txt");
        std::fs::write(&present, b"x").unwrap();
        assert!(
            !path_confirmed_missing(&present),
            "an existing file is not missing"
        );
    }

    fn test_roots(dir: &tempfile::TempDir) -> MediaRoots {
        let root = dir.path().to_string_lossy().replace('\\', "/");
        MediaRoots {
            roots: vec![root.clone()],
            labels: vec!["p1".into()],
            real_paths: vec![root],
        }
    }

    #[test]
    fn mark_missing_never_marks_an_existing_directory() {
        let (_dir, conn, _) = fixture();
        let roots = test_roots(&_dir);
        let existing = format!("{}/existing_artist", _dir.path().to_string_lossy().replace('\\', "/"));
        std::fs::create_dir_all(&existing).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (3, 'Live', ?, 0)",
            params![existing],
        )
        .unwrap();
        let mut errors = ScanErrors::new();
        let scanned: Vec<(i64, String)> = vec![];
        let stale =
            mark_missing_artists_after_full_scan(&conn, &roots, &scanned, &mut errors).unwrap();
        assert_eq!(stale, 0, "existing directory must not be marked missing");
        let am: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=3", [], |r| r.get(0))
            .unwrap();
        assert_eq!(am, 0);
        assert!(errors.is_empty());
    }

    // --- L2: artist + media missing must be atomic and recoverable ---

    #[test]
    fn mark_missing_marks_artist_and_items_together_and_recovers_inconsistent() {
        let (_dir, conn, _) = fixture();
        let roots = test_roots(&_dir);
        let root = _dir.path().to_string_lossy().replace('\\', "/");
        let gone = format!("{root}/gone");
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (2, 'Gone', ?, 0)",
            params![gone],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, missing) VALUES (10, 2, ? , 0)",
            params![format!("{gone}/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, missing) VALUES (11, 2, ? , 0)",
            params![format!("{gone}/b.jpg")],
        )
        .unwrap();

        // 1. Inject failure on items UPDATE to verify atomic rollback of artist update
        conn.execute_batch(
            "CREATE TRIGGER test_fail_item_missing BEFORE UPDATE ON items
             BEGIN SELECT RAISE(ABORT, 'injected missing update failure'); END;",
        )
        .unwrap();

        let mut errors = ScanErrors::new();
        let scanned: Vec<(i64, String)> = vec![];
        let res = mark_missing_artists_after_full_scan(&conn, &roots, &scanned, &mut errors);
        assert!(res.is_err(), "injected trigger must fail mark_missing");

        // Both artist and individual items must roll back together (missing=0)
        let a_missing: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=2", [], |r| r.get(0))
            .unwrap();
        let i10_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        let i11_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=11", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a_missing, 0, "artist update must roll back when item update fails");
        assert_eq!(i10_missing, 0, "item 10 must roll back");
        assert_eq!(i11_missing, 0, "item 11 must roll back");

        // Drop trigger and retry
        conn.execute("DROP TRIGGER test_fail_item_missing", []).unwrap();
        let stale = mark_missing_artists_after_full_scan(&conn, &roots, &scanned, &mut errors).unwrap();
        assert_eq!(stale, 1);
        let a_missing2: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=2", [], |r| r.get(0))
            .unwrap();
        let i10_missing2: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        let i11_missing2: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=11", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a_missing2, 1, "artist must be marked missing");
        assert_eq!(i10_missing2, 1, "item 10 must be marked missing");
        assert_eq!(i11_missing2, 1, "item 11 must be marked missing");

        // 2. Simulate an interrupted run: artist missing=1 but item 11 reset to 0.
        conn.execute("UPDATE items SET missing=0 WHERE id=11", []).unwrap();
        let stale2 = mark_missing_artists_after_full_scan(&conn, &roots, &scanned, &mut errors).unwrap();
        assert_eq!(stale2, 1, "retry must reach the inconsistent artist");
        let i10_missing3: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=10", [], |r| r.get(0))
            .unwrap();
        let i11_missing3: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=11", [], |r| r.get(0))
            .unwrap();
        assert_eq!(i10_missing3, 1);
        assert_eq!(i11_missing3, 1, "inconsistent item 11 must be reconciled on retry");
    }

    #[test]
    fn discovery_error_is_captured_in_scan_errors_and_preserves_discovery_outcome() {
        let (_dir, conn, _) = fixture();
        let roots = test_roots(&_dir);
        let root_s = _dir.path().to_string_lossy().replace('\\', "/");
        let healthy = format!("{root_s}/HealthyArtist");
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::write(format!("{healthy}/one.jpg"), b"jpg1").unwrap();

        // Subdirectory that will trigger fault-injected readdir error
        let restricted = format!("{root_s}/- R18");
        std::fs::create_dir_all(&restricted).unwrap();

        INJECTED_READDIR_ERROR.with(|cell| {
            *cell.borrow_mut() = Some(("- R18".to_string(), std::io::ErrorKind::PermissionDenied));
        });

        let control = ScanControl::new();
        assert!(control.try_start());
        let outcome = run_full_library_scan_claimed(&conn, &roots, &control).unwrap();

        INJECTED_READDIR_ERROR.with(|cell| *cell.borrow_mut() = None);

        let scan = outcome.get("scan").unwrap_or(&outcome);
        let phase = scan.get("phase").and_then(Value::as_str).unwrap();
        assert_eq!(phase, "partial", "discovery error must force phase to partial");
        assert!(
            outcome.get("archive").is_none(),
            "auto-archive must be blocked on partial scan"
        );

        let errors = scan.get("errors").and_then(Value::as_array).unwrap();
        assert!(!errors.is_empty(), "discovery error must be reported in errors");
        assert!(
            errors.iter().any(|e| e.as_str().unwrap().contains("discovery read_dir error")),
            "expected discovery read_dir error, got {errors:?}"
        );

        let scanned = scan.get("scanned").and_then(Value::as_i64).unwrap_or(0);
        assert!(scanned >= 1, "healthy artist should still be scanned: {scanned}");
    }

    #[test]
    fn inaccessible_artist_in_missing_reconciliation_blocks_auto_archive() {
        let (_dir, conn, _) = fixture();
        let roots = test_roots(&_dir);
        let root_s = _dir.path().to_string_lossy().replace('\\', "/");
        let healthy = format!("{root_s}/HealthyArtist");
        std::fs::create_dir_all(&healthy).unwrap();
        std::fs::write(format!("{healthy}/one.jpg"), b"jpg1").unwrap();

        let inacc_path = format!("{root_s}/InaccessibleArtist");

        conn.execute("DELETE FROM items", []).unwrap();
        conn.execute("DELETE FROM artists", []).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (1, 'HealthyArtist', ?, 0)",
            params![healthy],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing) VALUES (10, 1, ?, 'one.jpg', 0)",
            params![format!("{healthy}/one.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing) VALUES (2, 'InaccessibleArtist', ?, 0)",
            params![inacc_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing) VALUES (20, 2, ?, 'two.jpg', 0)",
            params![format!("{inacc_path}/two.jpg")],
        )
        .unwrap();

        // Inject metadata permission error specifically for InaccessibleArtist
        INJECTED_METADATA_ERROR.with(|cell| {
            *cell.borrow_mut() = Some(("InaccessibleArtist".to_string(), std::io::ErrorKind::PermissionDenied));
        });

        let control = ScanControl::new();
        assert!(control.try_start());
        let outcome = run_full_library_scan_claimed(&conn, &roots, &control).unwrap();

        INJECTED_METADATA_ERROR.with(|cell| *cell.borrow_mut() = None);

        let scan = outcome.get("scan").unwrap_or(&outcome);
        let phase = scan.get("phase").and_then(Value::as_str).unwrap();
        assert_eq!(phase, "partial", "reconciliation metadata error must yield partial phase");
        assert!(
            outcome.get("archive").is_none(),
            "auto-archive must be blocked when missing reconciliation encounters inaccessible artists"
        );

        let a2_missing: i64 = conn
            .query_row("SELECT missing FROM artists WHERE id=2", [], |r| r.get(0))
            .unwrap();
        let i20_missing: i64 = conn
            .query_row("SELECT missing FROM items WHERE id=20", [], |r| r.get(0))
            .unwrap();
        assert_eq!(a2_missing, 0, "inaccessible artist must never be marked missing");
        assert_eq!(i20_missing, 0, "items of inaccessible artist must never be marked missing");
    }

    #[test]
    fn scan_errors_bounds_large_error_volume_and_tracks_coverage_flag() {
        let mut errors = ScanErrors::new();
        assert!(errors.is_empty());
        assert!(!errors.has_inaccessible);

        for i in 0..1500 {
            errors.push(format!("permission denied on item {i}"));
        }

        assert_eq!(errors.total_count, 1500);
        assert_eq!(errors.sample.len(), ScanErrors::MAX_SAMPLE);
        assert_eq!(errors.sample[0], "permission denied on item 0");
        assert_eq!(errors.sample[19], "permission denied on item 19");
        assert!(errors.has_inaccessible);
        assert!(!errors.is_empty());
    }

    #[test]
    fn cleanup_stale_presence_spools_keeps_active_process_files() {
        let temp_dir = tempfile::tempdir().unwrap();
        let spool_path = temp_dir.path().to_path_buf();

        let active_pid = std::process::id();
        let active_spool = spool_path.join(format!("gallery_pres_act1_{active_pid}"));
        let dead_spool = spool_path.join("gallery_pres_dead_99999999");
        std::fs::write(&active_spool, b"active").unwrap();
        std::fs::write(&dead_spool, b"dead").unwrap();

        let aged = SystemTime::now() - PRESENCE_SPOOL_MAX_AGE - Duration::from_secs(120);
        std::fs::OpenOptions::new()
            .write(true)
            .open(&active_spool)
            .unwrap()
            .set_modified(aged)
            .unwrap();
        std::fs::OpenOptions::new()
            .write(true)
            .open(&dead_spool)
            .unwrap()
            .set_modified(aged)
            .unwrap();

        let cleaned = cleanup_stale_presence_spools_in(&spool_path, PRESENCE_SPOOL_MAX_AGE);
        assert_eq!(cleaned, 1, "only dead process spool file must be cleaned");
        assert!(active_spool.exists(), "active process spool file must survive cleanup");
        assert!(!dead_spool.exists(), "dead process spool file must be deleted");
    }
}
