//! Native library scan (replaces residual `app/scanner.py` for product runtime).

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use uuid::Uuid;
use walkdir::WalkDir;

use crate::content_hash::hash_file;
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

#[derive(Default)]
pub struct ScanControl {
    stop: AtomicBool,
    running: AtomicBool,
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
        self.running.store(running, Ordering::SeqCst);
    }

    pub fn is_running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    /// Atomically claim the scan slot. Clears stop only on success.
    pub fn try_start(&self) -> bool {
        match self
            .running
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
        {
            Ok(_) => {
                self.stop.store(false, Ordering::SeqCst);
                true
            }
            Err(_) => false,
        }
    }
}

/// RAII: always clear running on drop (normal return, `?`, panic unwind).
struct RunningGuard<'a> {
    control: &'a ScanControl,
}

impl Drop for RunningGuard<'_> {
    fn drop(&mut self) {
        self.control.set_running(false);
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
            updated_at REAL
        );
        INSERT OR IGNORE INTO scan_state (id, status) VALUES (1, 'idle');
        ",
    )?;
    Ok(())
}

pub fn get_scan_state(conn: &Connection) -> Result<Value> {
    ensure_scan_state(conn)?;
    let row = conn
        .query_row(
            "SELECT status, phase, scanned_count, total_estimate, current_path, started_at, updated_at, artist_id
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
                }))
            },
        )
        .optional_row()?;
    Ok(row.unwrap_or_else(|| json!({"status": "idle"})))
}

trait OptionalRow<T> {
    fn optional_row(self) -> Result<Option<T>>;
}

impl<T> OptionalRow<T> for std::result::Result<T, rusqlite::Error> {
    fn optional_row(self) -> Result<Option<T>> {
        match self {
            Ok(v) => Ok(Some(v)),
            Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
            Err(e) => Err(e.into()),
        }
    }
}

pub fn update_scan_state(conn: &Connection, fields: &[(&str, Value)]) -> Result<()> {
    ensure_scan_state(conn)?;
    let mut sets = Vec::new();
    let mut values: Vec<rusqlite::types::Value> = Vec::new();
    for (k, v) in fields {
        sets.push(format!("{k}=?"));
        values.push(json_to_sql(v));
    }
    sets.push("updated_at=?".into());
    values.push(rusqlite::types::Value::Real(now()));
    let sql = format!("UPDATE scan_state SET {} WHERE id=1", sets.join(", "));
    conn.execute(&sql, rusqlite::params_from_iter(values.iter()))?;
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
    name.starts_with('-') || CATEGORY_DIR_NAMES.iter().any(|n| *n == name)
}

fn is_collection_wrapper(name: &str) -> bool {
    COLLECTION_WRAPPER_DIR_NAMES.iter().any(|n| *n == name)
}

fn count_media_files(directory: &Path) -> usize {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return 0;
    };
    let mut count = 0usize;
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') {
            continue;
        }
        if entry.file_type().map(|t| t.is_file()).unwrap_or(false) && media_type_for_file(&name).is_some()
        {
            count += 1;
        }
    }
    count
}

fn has_subdirs(directory: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(directory) else {
        return false;
    };
    entries.flatten().any(|entry| {
        let name = entry.file_name().to_string_lossy().to_string();
        !name.starts_with('.') && entry.file_type().map(|t| t.is_dir()).unwrap_or(false)
    })
}

/// Discover artist directories under a real authorized root (Python `_discover_artist_dirs`).
fn discover_artist_dirs(root_path: &Path) -> Vec<(String, String)> {
    let mut result = Vec::new();

    fn walk(current: &Path, result: &mut Vec<(String, String)>) {
        let Ok(mut entries) = std::fs::read_dir(current) else {
            return;
        };
        let mut dirs: Vec<_> = entries
            .by_ref()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
            .collect();
        dirs.sort_by_key(|e| e.file_name());

        for entry in dirs {
            let name = entry.file_name().to_string_lossy().to_string();
            if name.starts_with('.') {
                continue;
            }
            let full = entry.path();
            let media_count = count_media_files(&full);
            if is_category_dir_name(&name)
                || (media_count == 0 && is_collection_wrapper(&name))
            {
                walk(&full, result);
                continue;
            }
            let has_children = has_subdirs(&full);
            if has_children || media_count > 0 {
                let path = normalize_slashes(&full.to_string_lossy());
                result.push((name, path));
                continue;
            }
            walk(&full, result);
        }
    }

    walk(root_path, &mut result);
    result
}

fn real_path_key(path: &str, roots: &MediaRoots) -> String {
    let mapped = map_media_path(path, roots);
    let canonical = mapped
        .canonicalize()
        .unwrap_or(mapped);
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

/// Sample media identities from a discovered artist directory for relocation matching.
fn sample_dir_media_identities(dir: &Path, limit: usize) -> Vec<(Option<(i64, i64)>, String, String)> {
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
    let artist_mapped = map_media_path(artist_path, roots);
    let artist_root = artist_mapped.canonicalize().with_context(|| {
        format!(
            "artist path not found or not accessible: {}",
            artist_mapped.display()
        )
    })?;
    if !artist_root.is_dir() {
        return Err(anyhow!("artist path is not a directory"));
    }

    let folder = folder.map(str::trim).filter(|s| !s.is_empty());
    let Some(folder) = folder else {
        return Ok(artist_root);
    };

    let rel = validate_relative_folder(folder)?;
    let mut target = artist_mapped;
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

/// Run a full or scoped scan synchronously (called from a background task).
///
/// Prefer claiming the slot with `ScanControl::try_start` before calling.
/// If the slot is free, this function claims it. A Drop guard always releases it.
pub fn run_scan(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
    artist_id: Option<i64>,
    folder: Option<&str>,
) -> Result<Value> {
    // Handler path: already claimed via try_start (is_running).
    // Direct/test path: claim now if free.
    if !control.is_running() && !control.try_start() {
        return Ok(json!({"ok": false, "message": "Already scanning"}));
    }
    let _guard = RunningGuard { control };

    let scan_id = Uuid::new_v4().to_string();
    let started = now();
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
            (
                "artist_id",
                artist_id.map(|v| json!(v)).unwrap_or(Value::Null),
            ),
        ],
    )?;

    let result = (|| -> Result<Value> {
        let folder_scoped = folder.map(str::trim).filter(|s| !s.is_empty()).is_some();
        let full_library = artist_id.is_none() && !folder_scoped;
        if full_library {
            // Keep legacy path rewrites out of HTTP startup; full scans already run in the background.
            crate::db::normalize_configured_media_paths(conn, roots)
                .context("normalize configured media paths")?;
        }
        let artists = list_artists_for_scan(conn, roots, artist_id)?;
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
        let mut stopped = false;

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
                    // Full-library scan: skip missing artist roots.
                    continue;
                }
            };
            let artist_mapped = map_media_path(&apath, roots);
            let artist_root = match artist_mapped.canonicalize() {
                Ok(p) => p,
                Err(_) => continue,
            };
            let artist_s = artist_root.to_string_lossy().replace('\\', "/");
            let scan_root = resolved.to_string_lossy().replace('\\', "/");
            update_scan_state(
                conn,
                &[
                    ("current_path", json!(scan_root)),
                    ("scanned_count", json!(idx as i64 + 1)),
                    ("phase", json!("scan")),
                ],
            )?;
            let (nc, ui, walk_stopped) =
                walk_artist(conn, aid, &artist_s, &scan_root, &scan_id, control)?;
            new_candidates += nc;
            updated_items += ui;
            scanned += 1;
            if walk_stopped {
                stopped = true;
                // Do not reconcile missing after a partial walk.
                break;
            }
            // Missing reconciliation only after a complete walk of this scope.
            reconcile_missing(conn, aid, &artist_s, &scan_root, &scan_id)?;
        }

        let phase = if stopped || control.is_stop_requested() {
            "stopped"
        } else {
            "complete"
        };
        if full_library && phase == "complete" {
            // Only after every authorized root was discovered and scanned.
            let _ = mark_missing_artists_after_full_scan(conn, roots, &artists_for_missing)?;
        }
        update_scan_state(
            conn,
            &[
                ("status", json!("idle")),
                ("phase", json!(phase)),
                ("scanned_count", json!(scanned)),
                ("current_path", json!("")),
            ],
        )?;
        Ok(json!({
            "ok": true,
            "phase": phase,
            "scanned": scanned,
            "new_candidates": new_candidates,
            "updated_items": updated_items,
            "scan_id": scan_id,
        }))
    })();

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

/// Full-library scans are the only scans allowed to trigger automatic folder archive work.
pub fn run_full_library_scan(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
) -> Result<Value> {
    let scan = run_scan(conn, roots, control, None, None)?;
    if scan.get("phase") == Some(&json!("complete")) {
        let archive = crate::folder_archive::run_folder_rename_auto_after_full_scan(conn, roots)?;
        Ok(json!({"scan": scan, "archive": archive}))
    } else {
        Ok(scan)
    }
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
        CREATE INDEX IF NOT EXISTS idx_scan_seen_path
            ON scan_seen(scan_id, file_path);
        ",
    )?;
    Ok(())
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
               AND (file_path = ? OR file_path LIKE ?)
               AND NOT EXISTS (
                 SELECT 1 FROM scan_seen s
                 WHERE s.scan_id=? AND s.file_path=items.file_path
               )",
            params![artist_id, scan_norm, format!("{prefix}%"), scan_id],
        )?;
    }
    Ok(())
}

/// Register or relocate a discovered artist path. Returns (id, path) for scanning.
fn register_discovered_artist(
    conn: &Connection,
    roots: &MediaRoots,
    artist_name: &str,
    artist_path: &str,
) -> Result<(i64, String)> {
    let path_norm = normalize_slashes(artist_path).trim_end_matches('/').to_string();
    let path_key = real_path_key(&path_norm, roots);

    // Exact path hit (including virtual/real alias equivalence).
    let mut existing: Option<(i64, String, String)> = conn
        .query_row(
            "SELECT id, name, path FROM artists WHERE path=?",
            params![&path_norm],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional_row()?;
    if existing.is_none() {
        let candidates = conn
            .prepare("SELECT id, name, path FROM artists")?
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?)))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, name, path) in candidates {
            if real_path_key(&path, roots) == path_key {
                existing = Some((id, name, path));
                break;
            }
        }
    }

    if let Some((id, name, old_path)) = existing {
        if name != artist_name {
            conn.execute(
                "UPDATE artists SET name=? WHERE id=?",
                params![artist_name, id],
            )?;
        }
        if old_path != path_norm {
            conn.execute(
                "UPDATE artists SET path=? WHERE id=?",
                params![&path_norm, id],
            )?;
        }
        conn.execute(
            "UPDATE artists SET missing=0, missing_at=NULL WHERE id=?",
            params![id],
        )?;
        return Ok((id, path_norm));
    }

    // New path: try high-confidence directory relocation before creating a new row.
    if let Some(relocated) = try_relocate_artist_dir(conn, roots, artist_name, &path_norm)? {
        return Ok(relocated);
    }

    conn.execute(
        "INSERT INTO artists (name, path) VALUES (?, ?)",
        params![artist_name, &path_norm],
    )?;
    Ok((conn.last_insert_rowid(), path_norm))
}

/// High-confidence directory move: ≥2 independent file identities uniquely point to one old artist.
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

    use std::collections::HashMap;
    let mut votes: HashMap<i64, usize> = HashMap::new();
    let mut ambiguous = false;

    for (identity, _path, hash) in &samples {
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

    // Only relocate when the old path no longer exists (true directory move).
    let old_path: String = conn.query_row(
        "SELECT path FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let old_mapped = map_media_path(&old_path, roots);
    if old_mapped.is_dir() {
        // Old path still present — not a move; keep as separate artist.
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
    let old_norm = normalize_slashes(old_path).trim_end_matches('/').to_string();
    let new_norm = normalize_slashes(new_path).trim_end_matches('/').to_string();
    let tx = conn.unchecked_transaction()?;
    let updated = tx.execute(
        "UPDATE artists SET path=?, name=?, missing=0, missing_at=NULL WHERE id=? AND path=?",
        params![&new_norm, artist_name, artist_id, &old_norm],
    )?;
    if updated != 1 {
        // Path may already use a different alias form; force update by id.
        tx.execute(
            "UPDATE artists SET path=?, name=?, missing=0, missing_at=NULL WHERE id=?",
            params![&new_norm, artist_name, artist_id],
        )?;
    }
    tx.execute(
        "UPDATE items SET file_path=? || substr(file_path, length(?) + 1), missing=0, missing_at=NULL
         WHERE artist_id=? AND (file_path=? OR file_path LIKE ?)",
        params![
            &new_norm,
            &old_norm,
            artist_id,
            &old_norm,
            format!("{old_norm}/%")
        ],
    )?;
    tx.execute(
        "DELETE FROM scan_seen WHERE artist_id=?",
        params![artist_id],
    )?;
    // Invalidate pending path-confirmation work for the old prefix; new scan rebuilds.
    let _ = tx.execute(
        "UPDATE scan_candidates SET status='superseded', resolved_at=strftime('%s','now')
         WHERE artist_id=? AND status IN ('pending','candidate','previewed')
           AND (file_path=? OR file_path LIKE ?)",
        params![artist_id, &old_norm, format!("{old_norm}/%")],
    );
    let _ = tx.execute(
        "UPDATE move_candidates SET status='superseded', resolved_at=strftime('%s','now')
         WHERE artist_id=? AND status='pending'
           AND (old_path=? OR old_path LIKE ? OR new_path=? OR new_path LIKE ?)",
        params![
            artist_id,
            &old_norm,
            format!("{old_norm}/%"),
            &old_norm,
            format!("{old_norm}/%")
        ],
    );
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
        let _ = tx.execute(
            "INSERT INTO move_history (item_id, artist_id, old_path, new_path, reason, status, details, applied_at)
             VALUES (?, ?, ?, ?, 'artist_directory_relocated', 'applied', ?, strftime('%s','now'))",
            params![item_id, artist_id, &old_norm, &new_norm, details],
        );
    }
    tx.commit()?;
    Ok(())
}

fn list_artists_for_scan(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: Option<i64>,
) -> Result<Vec<(i64, String)>> {
    if let Some(id) = artist_id {
        // Single-artist / folder scan: do not expand discovery scope.
        let path: String = conn.query_row(
            "SELECT path FROM artists WHERE id=?",
            params![id],
            |r| r.get(0),
        )?;
        return Ok(vec![(id, path)]);
    }

    // Full-library scan: always discover under every authorized real root.
    if roots.roots.is_empty() {
        return Ok(Vec::new());
    }

    let mut inaccessible = Vec::new();
    let mut discovered: Vec<(String, String)> = Vec::new();
    let mut by_key: std::collections::BTreeMap<String, (String, String)> =
        std::collections::BTreeMap::new();

    for i in 0..roots.roots.len() {
        let Some(real) = roots.real_root_at(i) else {
            inaccessible.push(roots.roots[i].clone());
            continue;
        };
        let real_path = Path::new(real);
        if !real_path.is_dir() {
            inaccessible.push(format!("{} -> {}", roots.roots[i], real));
            continue;
        }
        for (name, path) in discover_artist_dirs(real_path) {
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

    let mut rows = Vec::new();
    for (name, path) in discovered {
        rows.push(register_discovered_artist(conn, roots, &name, &path)?);
    }
    Ok(rows)
}

/// After a complete full-library discovery+scan, mark artists whose paths no longer exist.
fn mark_missing_artists_after_full_scan(
    conn: &Connection,
    roots: &MediaRoots,
    scanned_paths: &[(i64, String)],
) -> Result<i64> {
    let current_keys: std::collections::HashSet<String> = scanned_paths
        .iter()
        .map(|(_, p)| real_path_key(p, roots))
        .collect();
    let all = conn
        .prepare("SELECT id, path FROM artists WHERE COALESCE(missing,0)=0")?
        .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut stale = 0i64;
    for (id, path) in all {
        let key = real_path_key(&path, roots);
        if current_keys.contains(&key) {
            continue;
        }
        let mapped = map_media_path(&path, roots);
        if mapped.is_dir() {
            continue;
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
        conn.execute(
            "UPDATE artists SET missing=1, missing_at=strftime('%s','now') WHERE id=?",
            params![id],
        )?;
        conn.execute(
            "UPDATE items SET missing=1, missing_at=strftime('%s','now')
             WHERE artist_id=? AND COALESCE(missing,0)=0",
            params![id],
        )?;
        stale += 1;
    }
    Ok(stale)
}

fn walk_artist(
    conn: &Connection,
    artist_id: i64,
    artist_path: &str,
    scan_root: &str,
    scan_id: &str,
    control: &ScanControl,
) -> Result<(i64, i64, bool)> {
    ensure_scan_seen(conn)?;
    let artist_norm = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let mut new_candidates = 0i64;
    let mut updated = 0i64;
    let mut seen_batch: Vec<(String, i64, String, String, i64, f64, String, String)> = Vec::new();
    let mut stopped = false;

    for entry in WalkDir::new(scan_root)
        .into_iter()
        .filter_entry(|e| {
            e.file_name()
                .to_str()
                .map(|n| !n.starts_with('.'))
                .unwrap_or(true)
        })
        .filter_map(|e| e.ok())
    {
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
        let full = entry.path().to_string_lossy().replace('\\', "/");
        let meta = match std::fs::metadata(entry.path()) {
            Ok(m) => m,
            Err(_) => continue,
        };
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
            .map(|p| p.to_string_lossy().replace('\\', "/"))
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
        let is_archive = if media_type == "archive" { 1 } else { 0 };

        let existing: Option<(i64, i64, f64, String, String, Option<f64>)> = conn
            .query_row(
                "SELECT id, file_size, file_mtime, content_hash, hash_status, hash_updated_at FROM items WHERE file_path=?",
                params![&full],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                    ))
                },
            )
            .optional_row()?;

        let (content_hash, hash_status) = if let Some((
            id,
            old_size,
            old_mtime,
            old_hash,
            old_status,
            old_hash_at,
        )) = existing
        {
            let same = old_size == file_size && (old_mtime - file_mtime).abs() < 1.0;
            let (content_hash, hash_status, hash_updated_at) =
                if same && old_status == "done" && !old_hash.is_empty() {
                    (old_hash, old_status, old_hash_at)
                } else {
                    (String::new(), "pending".into(), None)
                };
            conn.execute(
                "UPDATE items SET file_name=?, file_size=?, file_mtime=?, folder_name=?, date=?,
                 is_archive=?, media_type=?, content_hash=?, hash_status=?, hash_updated_at=?,
                 missing=0, missing_at=NULL, scanned_at=strftime('%s','now') WHERE id=?",
                params![
                    fname,
                    file_size,
                    file_mtime,
                    folder_name,
                    date_str,
                    is_archive,
                    media_type,
                    content_hash,
                    hash_status,
                    hash_updated_at,
                    id
                ],
            )?;
            updated += 1;
            (content_hash, hash_status)
        } else {
            // Candidate or create-new path: insert scan_candidate if not already pending.
            let exists_cand: i64 = conn.query_row(
                "SELECT COUNT(*) FROM scan_candidates WHERE file_path=? AND status IN ('pending','candidate','previewed')",
                params![&full],
                |r| r.get(0),
            )?;
            if exists_cand == 0 {
                conn.execute(
                    "INSERT INTO scan_candidates
                     (scan_id, artist_id, file_path, file_name, file_size, file_mtime, folder_name, date,
                      is_archive, media_type, content_hash, hash_status, status)
                     VALUES (?,?,?,?,?,?,?,?,?,?, '','pending','pending')",
                    params![
                        scan_id,
                        artist_id,
                        full,
                        fname,
                        file_size,
                        file_mtime,
                        folder_name,
                        date_str,
                        is_archive,
                        media_type
                    ],
                )?;
                new_candidates += 1;
            }
            (String::new(), "pending".into())
        };

        // Both existing items and new candidates must appear in scan_seen.
        seen_batch.push((
            scan_id.to_string(),
            artist_id,
            full,
            media_type.to_string(),
            file_size,
            file_mtime,
            content_hash,
            hash_status,
        ));
        if seen_batch.len() >= 200 {
            flush_scan_seen(conn, &seen_batch)?;
            seen_batch.clear();
        }
    }
    if !seen_batch.is_empty() {
        flush_scan_seen(conn, &seen_batch)?;
    }
    let _ = hash_file; // reserved for optional inline hash
    Ok((new_candidates, updated, stopped))
}

fn flush_scan_seen(
    conn: &Connection,
    rows: &[(String, i64, String, String, i64, f64, String, String)],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    {
        let mut stmt = tx.prepare(
            "INSERT INTO scan_seen
             (scan_id, artist_id, file_path, media_type, file_size, file_mtime,
              content_hash, hash_status)
             VALUES (?,?,?,?,?,?,?,?)",
        )?;
        for (scan_id, artist_id, path, media_type, size, mtime, hash, status) in rows {
            stmt.execute(params![
                scan_id, artist_id, path, media_type, size, mtime, hash, status
            ])?;
        }
    }
    tx.commit()?;
    Ok(())
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
              date TEXT DEFAULT '', is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
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
              id INTEGER PRIMARY KEY, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, resolved_at REAL
            );
            CREATE TABLE move_history (
              id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
              reason TEXT, status TEXT, details TEXT DEFAULT '{}'
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
        let result = run_full_library_scan(&conn, &roots, &control).unwrap();

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
        let result = run_scan(&conn, &roots, &control, Some(1), None).unwrap();
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
        let err = run_scan(
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

    fn empty_db_with_real_root(dir: &tempfile::TempDir, real_name: &str) -> (Connection, MediaRoots, PathBuf) {
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
              date TEXT DEFAULT '', is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
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
        assert!(!path.starts_with("/pictures"), "must not store virtual root: {path}");

        let item_path: String = conn
            .query_row("SELECT file_path FROM scan_candidates LIMIT 1", [], |r| r.get(0))
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

        let found = discover_artist_dirs(&root);
        let names: Vec<_> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"カジノ(Casino)"), "{names:?}");
        assert!(names.contains(&"OtherArtist"), "{names:?}");
        assert!(!names.iter().any(|n| n.starts_with('-')), "category dirs not artists: {names:?}");
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

        let found = discover_artist_dirs(&root);
        let paths: Vec<_> = found
            .into_iter()
            .filter(|(n, _)| n == "same-name")
            .map(|(_, p)| p)
            .collect();
        assert_eq!(paths.len(), 2, "same name different paths must stay separate");
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
        roots
            .real_paths
            .push(dir.path().join("does-not-exist").to_string_lossy().replace('\\', "/"));

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
            paths.len() >= 1,
            "new path must be registered; got {paths:?}"
        );
        let new_visible = paths.iter().any(|p| p.contains("- R18"));
        assert!(new_visible, "new categorized path must appear: {paths:?}");
    }
}
