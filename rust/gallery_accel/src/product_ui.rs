//! Native handlers for remaining static-UI product routes (no residual Python).

use std::collections::HashSet;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::operations::operation_history_response;
use crate::scan_candidates_write::{
    apply_move_candidate_group_item_response_with_roots, apply_move_candidate_response_with_roots,
    mark_move_candidate_new_response, resolve_ambiguous_cluster_as_new_response,
};
use crate::tags_write::{update_item_tags_by_name_response, update_item_tags_response};

pub const LOG_TAIL_MAX_BYTES: u64 = 256 * 1024;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// ---------------------------------------------------------------------------
// Operation log (UI: GET /api/operation-log)
// ---------------------------------------------------------------------------

pub fn operation_log_response(
    conn: &Connection,
    roots: &MediaRoots,
    limit: Option<i64>,
    error_limit: Option<i64>,
) -> Result<Value> {
    let history_limit = match limit {
        Some(v) if v > 0 => v.min(crate::MAX_OPERATION_LOG_LIMIT),
        _ => crate::DEFAULT_PREVIEW_RECYCLE_LIMIT,
    };
    let err_limit = match error_limit {
        Some(v) if v > 0 => v.min(crate::MAX_RECENT_ERRORS_LIMIT),
        _ => 40,
    };
    let mut hist = operation_history_response(conn, roots, Some(history_limit))?;
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let log_dir = Path::new(&data_dir).join("logs");
    let errors = recent_log_errors(&log_dir, err_limit as usize);
    if let Some(obj) = hist.as_object_mut() {
        obj.insert("errors".into(), json!(errors));
        obj.insert("error_limit".into(), json!(err_limit));
        obj.insert(
            "sources".into(),
            json!({
                "moves": "move_history",
                "folder_renames": "folder_rename_plans.execution_log",
                "errors": log_dir.display().to_string(),
            }),
        );
    }
    Ok(hist)
}

pub fn read_log_tail(
    path: &Path,
    line_limit: usize,
    max_bytes: u64,
) -> io::Result<(Vec<String>, bool)> {
    let max_bytes = max_bytes.clamp(1, LOG_TAIL_MAX_BYTES);
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let start = size.saturating_sub(max_bytes);
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = Vec::new();
    file.take(max_bytes).read_to_end(&mut bytes)?;
    let truncated = start > 0;
    if truncated {
        if let Some(newline) = bytes.iter().position(|byte| *byte == b'\n') {
            bytes.drain(..=newline);
        } else {
            bytes.clear();
        }
    }
    let lines = String::from_utf8_lossy(&bytes)
        .lines()
        .rev()
        .take(line_limit)
        .map(str::to_owned)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    Ok((lines, truncated))
}

fn is_error_log_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.contains("[error]")
        || lower.contains("traceback")
        || lower.contains("frontend_error")
        || lower.contains("frontend_rejection")
}

/// Parse a persisted log stamp into epoch millis.
///
/// Every writer (`logging::log_timestamp`, the Python-era logger, the fnOS
/// launcher) serializes LOCAL wall-clock time without an offset, so the text
/// must be interpreted in the configured local timezone. Interpreting it as
/// UTC shifted every line by the host offset and mis-dated the
/// `/api/operation-log` and health recent-errors windows.
fn log_line_timestamp_millis(line: &str) -> Option<i64> {
    use chrono::TimeZone;
    let timestamp = line.get(line.find("20")?..)?.get(..23)?;
    ["%Y-%m-%d %H:%M:%S,%f", "%Y-%m-%d %H:%M:%S.%f"]
        .iter()
        .find_map(|format| chrono::NaiveDateTime::parse_from_str(timestamp, format).ok())
        .and_then(|value| {
            chrono::Local
                .from_local_datetime(&value)
                .earliest()
                .map(|local| local.timestamp_millis())
        })
}

/// Health-panel errors are only "recent" inside this window. Log files left
/// behind by the pre-Rust stack are never rewritten, so without a window their
/// historical errors resurface in `/api/health` forever.
/// **0 disables the window** and keeps every error.
const LOG_ERROR_WINDOW_HOURS_DEFAULT: i64 = 168;

fn log_error_window_millis() -> i64 {
    let hours = env_i64(
        "GALLERY_LOG_ERROR_WINDOW_HOURS",
        LOG_ERROR_WINDOW_HOURS_DEFAULT,
    );
    if hours <= 0 {
        return i64::MAX;
    }
    hours.saturating_mul(3_600_000)
}

fn now_millis() -> i64 {
    (now() * 1000.0) as i64
}

pub fn recent_log_errors(log_dir: &Path, limit: usize) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let cutoff = now_millis().saturating_sub(log_error_window_millis());
    let mut out = Vec::new();
    let mut sequence = 0usize;
    for name in ["gallery.log", "startup.log", "ui-actions.log"] {
        let path = log_dir.join(name);
        let Ok((lines, _truncated)) =
            read_log_tail(&path, LOG_TAIL_MAX_BYTES as usize, LOG_TAIL_MAX_BYTES)
        else {
            continue;
        };
        let mut timestamp = path
            .metadata()
            .and_then(|metadata| metadata.modified())
            .ok()
            .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
            .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
            .unwrap_or(0);
        for line in lines {
            if let Some(value) = log_line_timestamp_millis(&line) {
                timestamp = value;
            }
            if !is_error_log_line(&line) {
                continue;
            }
            if timestamp < cutoff {
                continue;
            }
            out.push((
                timestamp,
                sequence,
                json!({
                    "source": name,
                    "line": line.chars().take(500).collect::<String>(),
                }),
            ));
            sequence += 1;
        }
    }
    out.sort_by(|left, right| right.0.cmp(&left.0).then_with(|| right.1.cmp(&left.1)));
    out.truncate(limit);
    out.into_iter().map(|(_, _, value)| value).collect()
}

// ---------------------------------------------------------------------------
// Folder tags
// ---------------------------------------------------------------------------

fn folder_item_ids(conn: &Connection, artist_id: i64, folder: &str) -> Result<Vec<i64>> {
    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |r| r.get(0),
        )
        .optional()?;
    let Some(artist_path) = artist_path else {
        return Ok(vec![]);
    };
    let folder = folder.trim().trim_matches('/').replace('\\', "/");
    let mut sql = String::from(
        "SELECT id FROM items WHERE artist_id=? AND missing=0
         AND (media_type IN ('image','video','source','archive','text') OR is_archive=1)",
    );
    let mut ids = Vec::new();
    if folder.is_empty() {
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![artist_id], |r| r.get::<_, i64>(0))?;
        for row in rows {
            ids.push(row?);
        }
        return Ok(ids);
    }
    // Byte-exact prefix comparison instead of LIKE: SQLite's built-in LIKE is
    // ASCII case-insensitive and would conflate distinct `Foo`/`foo` folders
    // (case-sensitive media filesystems keep both), so a tag write for one
    // would silently rewrite the sibling's metadata. substr()+`=` with an
    // explicit BINARY collation matches the requested folder's exact bytes and
    // needs no wildcard escaping.
    let prefix = {
        let base = artist_path
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        format!("{base}/{folder}/")
    };
    // NOTE: the Rust string `'\\'` is the one-character SQL literal `'\'`
    // (SQLite does not process backslash escapes). The previous `'\\\\'`
    // reached SQLite as a *two*-backslash needle, so single-backslash Windows
    // paths were never normalized and the prefix comparison missed every row.
    sql.push_str(" AND substr(replace(file_path,'\\','/'), 1, length(?)) = ? COLLATE BINARY");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artist_id, prefix, prefix], |r| r.get::<_, i64>(0))?;
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
}

/// Escape SQL LIKE wildcards so folder names (and artist paths) containing
/// `%` or `_` match literally instead of widening the pattern to unrelated
/// items — folder tag writes must never touch the wrong item set.
pub(crate) fn escape_like(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

pub fn update_folder_tags_response(
    conn: &Connection,
    artist_id: i64,
    folder: &str,
    tag_ids: &[i64],
    mode: &str,
) -> Result<Value> {
    let item_ids = folder_item_ids(conn, artist_id, folder)?;
    if item_ids.is_empty() {
        return Ok(json!({"updated": 0, "item_ids": [], "changed_item_ids": []}));
    }
    let mut result = update_item_tags_response(conn, artist_id, &item_ids, tag_ids, mode)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("item_ids".into(), json!(item_ids));
    }
    Ok(result)
}

pub fn update_folder_tags_by_name_response(
    conn: &Connection,
    artist_id: i64,
    folder: &str,
    tag_names: &[String],
    mode: &str,
) -> Result<Value> {
    let item_ids = folder_item_ids(conn, artist_id, folder)?;
    if item_ids.is_empty() {
        return Ok(json!({
            "updated": 0, "artists": 0, "tags": 0, "propagated": 0,
            "tag_names": tag_names, "item_ids": [], "changed_item_ids": []
        }));
    }
    let mut result = update_item_tags_by_name_response(conn, &item_ids, tag_names, mode)?;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("item_ids".into(), json!(item_ids));
        obj.insert("tag_names".into(), json!(tag_names));
    }
    Ok(result)
}

#[derive(Debug, serde::Deserialize)]
pub struct FolderAnnotatePayload {
    pub artist_id: i64,
    pub folder: String,
    #[serde(default)]
    pub tag_names: Vec<String>,
    #[serde(default)]
    pub tag_ids: Vec<i64>,
    #[serde(default = "default_mode_add")]
    pub mode: String,
    pub manual_date: Option<String>,
    pub keep_together: Option<bool>,
}

fn default_mode_add() -> String {
    "add".into()
}

pub fn annotate_folder_response(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    payload: FolderAnnotatePayload,
) -> Result<Value> {
    let artist_id = payload.artist_id;
    let folder = payload.folder.trim().trim_matches('/').replace('\\', "/");
    if artist_id <= 0 {
        return Err(anyhow!("artist_id must be positive"));
    }
    if folder.is_empty() {
        return Err(anyhow!("folder must not be empty"));
    }
    let item_ids = folder_item_ids(conn, artist_id, &folder)?;
    if item_ids.is_empty() {
        return Err(anyhow!("folder has no items or does not exist"));
    }

    // 1. Update tags if provided
    if !payload.tag_names.is_empty() {
        update_folder_tags_by_name_response(
            conn,
            artist_id,
            &folder,
            &payload.tag_names,
            &payload.mode,
        )?;
    } else if !payload.tag_ids.is_empty() {
        update_folder_tags_response(conn, artist_id, &folder, &payload.tag_ids, &payload.mode)?;
    }

    // 2. Update manual date if provided
    if let Some(ref date_str) = payload.manual_date {
        let trimmed = date_str.trim();
        let date_arg = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        crate::item_dates::update_item_dates_response(conn, artist_id, &item_ids, date_arg)?;
    }

    // 3. Update keep_together if provided
    if let Some(keep) = payload.keep_together {
        crate::folder_archive::ensure_folder_schema(conn)?;
        let existing_snapshot: Option<String> = conn
            .query_row(
                "SELECT format_snapshot FROM folder_rename_plans WHERE artist_id=? AND source_folder=?",
                params![artist_id, folder],
                |r| r.get(0),
            )
            .optional()?;

        let mut snapshot_obj = existing_snapshot
            .as_deref()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .and_then(|v| v.as_object().cloned())
            .unwrap_or_default();
        snapshot_obj.insert("keep_together".to_string(), json!(keep));
        let new_snapshot_str = serde_json::to_string(&snapshot_obj)?;

        let n = conn.execute(
            "UPDATE folder_rename_plans
             SET format_snapshot=?, plan_kind=CASE WHEN ?=1 THEN 'rename_folder' ELSE plan_kind END,
                 updated_at=?
             WHERE artist_id=? AND source_folder=? AND status NOT IN ('confirmed', 'executed')",
            params![
                new_snapshot_str,
                if keep { 1 } else { 0 },
                now(),
                artist_id,
                folder
            ],
        )?;
        if n == 0 {
            // Insert-or-update under the same lock rule as the UPDATE above:
            // a confirmed or executed plan has already committed to its
            // shape, so the upsert must not rewrite its snapshot either.
            let _ = conn.execute(
                "INSERT INTO folder_rename_plans
                 (artist_id, source_folder, original_folder_name, original_title, format_snapshot, plan_kind, updated_at)
                 VALUES (?, ?, ?, ?, ?, 'rename_folder', ?)
                 ON CONFLICT(artist_id, source_folder) DO UPDATE SET
                     format_snapshot=excluded.format_snapshot, updated_at=excluded.updated_at
                 WHERE folder_rename_plans.status NOT IN ('confirmed', 'executed')",
                params![artist_id, folder, folder, folder, new_snapshot_str, now()],
            );
        }
    }

    // 4. Discover and recompute plans
    crate::folder_archive::auto_discover_artist_folder_plans(conn, artist_id)?;
    crate::folder_archive::recompute_artist_plan_targets(conn, roots, artist_id)?;

    // 5. Query updated plan
    let plan = conn
        .query_row(
            "SELECT id, artist_id, source_folder, target_folder, status, parsed_date,
                    selected_tag_ids, plan_kind, file_count, format_snapshot
             FROM folder_rename_plans
             WHERE artist_id=? AND source_folder=?",
            params![artist_id, folder],
            |row| {
                Ok(json!({
                    "id": row.get::<_, i64>(0)?,
                    "artist_id": row.get::<_, i64>(1)?,
                    "source_folder": row.get::<_, String>(2)?,
                    "target_folder": row.get::<_, String>(3)?,
                    "status": row.get::<_, String>(4)?,
                    "parsed_date": row.get::<_, String>(5)?,
                    "selected_tag_ids": serde_json::from_str::<Value>(&row.get::<_, String>(6)?).unwrap_or(json!([])),
                    "plan_kind": row.get::<_, String>(7)?,
                    "file_count": row.get::<_, i64>(8)?,
                    "format_snapshot": serde_json::from_str::<Value>(&row.get::<_, String>(9)?).unwrap_or(json!({})),
                }))
            },
        )
        .optional()?;

    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "folder": folder,
        "updated_items": item_ids.len(),
        "plan": plan,
    }))
}

#[derive(Debug, serde::Deserialize)]
pub struct BundleItemsPayload {
    pub artist_id: i64,
    pub item_ids: Vec<i64>,
    pub target_folder: String,
}

/// One file move applied by `bundle_items_response`, recorded the moment it
/// happens so a later failure can put every file back where it started.
struct BundleMove {
    source: PathBuf,
    dest: PathBuf,
}

fn rollback_bundle_moves(moves: &[BundleMove]) {
    for applied in moves.iter().rev() {
        if applied.dest == applied.source {
            continue;
        }
        if let Err(error) = std::fs::rename(&applied.dest, &applied.source) {
            // A cross-device move fails backwards the same way it failed
            // forwards; copy back and drop the copy at the destination.
            match std::fs::copy(&applied.dest, &applied.source) {
                Ok(_) => {
                    let _ = std::fs::remove_file(&applied.dest);
                }
                Err(copy_error) => {
                    log_warn!(
                        "bundle rollback: cannot restore {}: {error}; copy: {copy_error}",
                        applied.source.display()
                    );
                }
            }
        }
    }
}

fn remove_created_bundle_dirs(created: &[PathBuf]) {
    for path in created.iter().rev() {
        if let Err(error) = std::fs::remove_dir(path) {
            if error.kind() != std::io::ErrorKind::NotFound {
                log_warn!(
                    "bundle: cannot remove newly created directory {}: {error}",
                    path.display()
                );
            }
        }
    }
}

/// Create a bundle destination without following a pre-existing symlink.
/// Every existing component is canonicalized before the next component is
/// created, and only directories created by this call are eligible for cleanup.
fn prepare_bundle_target_dir(artist_root: &Path, target_folder: &str) -> Result<PathBuf> {
    let canonical_root = std::fs::canonicalize(artist_root)
        .map_err(|error| anyhow!("cannot resolve artist directory: {error}"))?;
    let mut current = artist_root.to_path_buf();
    let mut created = Vec::new();
    for component in Path::new(target_folder).components() {
        let std::path::Component::Normal(name) = component else {
            return Err(anyhow!("invalid target_folder path"));
        };
        current.push(name);
        match std::fs::symlink_metadata(&current) {
            Ok(_) => {
                let canonical = match std::fs::canonicalize(&current) {
                    Ok(path) => path,
                    Err(error) => {
                        remove_created_bundle_dirs(&created);
                        return Err(error.into());
                    }
                };
                if !canonical.starts_with(&canonical_root) || !canonical.is_dir() {
                    remove_created_bundle_dirs(&created);
                    return Err(anyhow!("target folder must be within artist directory"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if let Err(create_error) = std::fs::create_dir(&current) {
                    remove_created_bundle_dirs(&created);
                    return Err(create_error.into());
                }
                created.push(current.clone());
                let canonical = match std::fs::canonicalize(&current) {
                    Ok(path) => path,
                    Err(error) => {
                        remove_created_bundle_dirs(&created);
                        return Err(error.into());
                    }
                };
                if !canonical.starts_with(&canonical_root) || !canonical.is_dir() {
                    remove_created_bundle_dirs(&created);
                    return Err(anyhow!("target folder must be within artist directory"));
                }
            }
            Err(error) => {
                remove_created_bundle_dirs(&created);
                return Err(error.into());
            }
        }
    }
    Ok(current)
}

pub fn bundle_items_response(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    payload: BundleItemsPayload,
) -> Result<Value> {
    let artist_id = payload.artist_id;
    if artist_id <= 0 {
        return Err(anyhow!("artist_id must be positive"));
    }
    if payload.item_ids.is_empty() {
        return Err(anyhow!("item_ids must not be empty"));
    }
    if payload.item_ids.len() as i64 > crate::MAX_BATCH_ITEM_LIMIT {
        return Err(anyhow!("too many item_ids"));
    }
    if payload.item_ids.iter().any(|id| *id <= 0) {
        return Err(anyhow!("item_ids must be positive"));
    }
    let mut sorted_ids = payload.item_ids.clone();
    sorted_ids.sort_unstable();
    sorted_ids.dedup();
    if sorted_ids.len() != payload.item_ids.len() {
        return Err(anyhow!("item_ids must not contain duplicates"));
    }
    // Reuse the shared folder validator: it rejects absolute paths, drive
    // letters, `.` / `..` segments and empty segments. The strict comparison
    // additionally refuses non-canonical spellings (`a//b`) outright instead of
    // silently re-spelling them into a different folder than the caller named.
    let trimmed = payload.target_folder.trim();
    let target_folder = crate::folder_archive::validate_relative_folder(trimmed)
        .map_err(|_| anyhow!("invalid target_folder path"))?;
    if target_folder != trimmed.replace('\\', "/").trim_matches('/') {
        return Err(anyhow!("invalid target_folder path"));
    }

    let artist_path: String = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |r| r.get(0),
        )
        .map_err(|_| anyhow!("artist not found"))?;

    let artist_real_root = if let Some(r) = roots {
        r.map_to_real(&artist_path)
            .unwrap_or_else(|_| PathBuf::from(&artist_path))
    } else {
        PathBuf::from(&artist_path)
    };

    let target_dir = prepare_bundle_target_dir(&artist_real_root, &target_folder);
    let target_dir = match target_dir {
        Ok(path) => path,
        Err(error) => return Err(error),
    };

    let mut moved_count = 0usize;
    let mut skipped = Vec::new();
    let mut applied: Vec<BundleMove> = Vec::new();

    // Files and their rows move together: one transaction holds every row
    // update while each file move is recorded as it happens, and any failure
    // puts every moved file back where it started. A half-moved batch would
    // leave rows pointing at paths that no longer exist (next scan: mass
    // missing plus duplicate entries in the target folder).
    let outcome = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        for item_id in &payload.item_ids {
            let item_opt: Option<(String, String)> = tx.query_row(
                "SELECT file_path, file_name FROM items WHERE id=? AND artist_id=? AND COALESCE(missing, 0)=0",
                params![item_id, artist_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            ).optional()?;

            let Some((file_path, file_name)) = item_opt else {
                continue;
            };

            // A stored file name must be a plain name: separators or dot
            // segments could otherwise steer the destination outside
            // target_dir no matter how the folder itself was validated.
            if file_name.is_empty()
                || file_name == "."
                || file_name == ".."
                || file_name.contains('/')
                || file_name.contains('\\')
            {
                log_warn!("bundle: skipping item {item_id}: unsafe file_name {file_name:?}");
                skipped.push(json!({"item_id": item_id, "reason": "unsafe_file_name"}));
                continue;
            }

            let src_real = if let Some(r) = roots {
                r.map_to_real(&file_path)
                    .unwrap_or_else(|_| PathBuf::from(&file_path))
            } else {
                PathBuf::from(&file_path)
            };

            if !src_real.is_file() {
                skipped.push(json!({"item_id": item_id, "reason": "source_missing"}));
                continue;
            }

            // Avoid filename collision; never overwrite an existing file.
            let (dest_file, final_file_name) = {
                let direct = target_dir.join(&file_name);
                if !direct.exists() || direct == src_real {
                    (direct, file_name.clone())
                } else {
                    let stem = Path::new(&file_name)
                        .file_stem()
                        .and_then(|s| s.to_str())
                        .unwrap_or("file");
                    let ext = Path::new(&file_name)
                        .extension()
                        .and_then(|s| s.to_str())
                        .map(|e| format!(".{}", e))
                        .unwrap_or_default();
                    let mut chosen = None;
                    for idx in 1..1000 {
                        let candidate_name = format!("{} ({}){}", stem, idx, ext);
                        let candidate = target_dir.join(&candidate_name);
                        if !candidate.exists() {
                            chosen = Some((candidate, candidate_name));
                            break;
                        }
                    }
                    match chosen {
                        Some(pair) => pair,
                        None => {
                            // Every candidate name is taken: skip the item and
                            // say so. Silently writing onto an existing file
                            // would destroy data the caller never selected.
                            skipped.push(
                                json!({"item_id": item_id, "reason": "target_name_exhausted"}),
                            );
                            continue;
                        }
                    }
                }
            };

            if dest_file != src_real {
                if let Err(error) = std::fs::rename(&src_real, &dest_file) {
                    if let Err(copy_error) = std::fs::copy(&src_real, &dest_file) {
                        if let Err(cleanup_error) = std::fs::remove_file(&dest_file) {
                            if cleanup_error.kind() != std::io::ErrorKind::NotFound {
                                log_warn!(
                                    "bundle: cannot remove incomplete target {}: {cleanup_error}",
                                    dest_file.display()
                                );
                            }
                        }
                        return Err(anyhow!(
                            "bundle move failed for item {item_id}: {error}; copy: {copy_error}"
                        ));
                    }
                    if let Err(remove_error) = std::fs::remove_file(&src_real) {
                        let cleanup = std::fs::remove_file(&dest_file).err();
                        if let Some(cleanup_error) = cleanup {
                            log_warn!(
                                "bundle: copied {} but could not remove source {}: {}; also could not remove target {}: {}",
                                src_real.display(),
                                src_real.display(),
                                remove_error,
                                dest_file.display(),
                                cleanup_error
                            );
                        }
                        return Err(anyhow!(
                            "bundle move failed for item {item_id}: copied target but could not remove source {}: {remove_error}",
                            src_real.display()
                        ));
                    }
                }
                applied.push(BundleMove {
                    source: src_real,
                    dest: dest_file.clone(),
                });
            }

            let new_file_path = format!(
                "{}/{}/{}",
                artist_path.trim_end_matches('/'),
                target_folder,
                final_file_name
            );
            tx.execute(
                "UPDATE items SET file_path=?, file_name=?, folder_name=? WHERE id=? AND artist_id=?",
                params![new_file_path, final_file_name, target_folder, item_id, artist_id],
            )?;
            moved_count += 1;
        }
        tx.commit()?;
        Ok(())
    })();
    if let Err(error) = outcome {
        rollback_bundle_moves(&applied);
        return Err(error);
    }

    crate::folder_archive::auto_discover_artist_folder_plans(conn, artist_id)?;
    crate::folder_archive::recompute_artist_plan_targets(conn, roots, artist_id)?;

    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "target_folder": target_folder,
        "moved_count": moved_count,
        "skipped": skipped,
    }))
}

// ---------------------------------------------------------------------------
// Folder plan confirm / unconfirm / auto run
// ---------------------------------------------------------------------------

pub fn reconfirm_plan(conn: &Connection, roots: &MediaRoots, plan_id: i64) -> Result<Value> {
    let check = crate::folder_archive::check_plan_paths(conn, roots, plan_id)?;
    if let Some(reason) = check.reason {
        let permission_path = check
            .permission_path
            .as_deref()
            .map(|path| format!(": {path}"))
            .unwrap_or_default();
        return Err(anyhow!(
            "plan is not physically ready for reconfirmation: {}{}",
            crate::folder_archive::archive_failure_message(&reason),
            permission_path,
        ));
    }
    let n = conn.execute(
        "UPDATE folder_rename_plans
         SET status='confirmed', confirmed_at=?, confirmation_source='manual', updated_at=?
         WHERE id=? AND status IN ('ready','needs_tags','manual_review','confirmed','draft')",
        params![now(), now(), plan_id],
    )?;
    if n == 0 {
        return Err(anyhow!("plan not found or not confirmable"));
    }
    Ok(json!({"ok": true, "id": plan_id, "status": "confirmed"}))
}

pub fn unconfirm_plan(conn: &Connection, plan_id: i64) -> Result<Value> {
    let n = conn.execute(
        "UPDATE folder_rename_plans
         SET status='ready', confirmed_at=NULL, confirmation_source='', updated_at=?
         WHERE id=? AND status='confirmed'",
        params![now(), plan_id],
    )?;
    if n == 0 {
        return Err(anyhow!("plan not found or not confirmed"));
    }
    Ok(json!({"ok": true, "id": plan_id, "status": "ready"}))
}

pub fn confirm_all_artist_plans(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
) -> Result<Value> {
    crate::folder_archive::ensure_folder_schema(conn)?;
    let plan_ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM folder_rename_plans
             WHERE artist_id=? AND status IN ('ready', 'needs_tags', 'manual_review', 'draft')
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
        )?
        .query_map(params![artist_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut confirmed = 0i64;
    let mut failed = 0i64;
    for plan_id in plan_ids {
        match reconfirm_plan(conn, roots, plan_id) {
            Ok(_) => confirmed += 1,
            Err(_) => failed += 1,
        }
    }
    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "confirmed": confirmed,
        "failed": failed,
    }))
}

pub fn folder_rename_auto_run(
    conn: &Connection,
    roots: &crate::media_roots::MediaRoots,
    artist_id: i64,
) -> Result<Value> {
    let auto_named =
        crate::folder_archive::auto_name_artist_draft_plans(conn, roots, &[artist_id])?;
    let confirmed = conn.execute(
        "UPDATE folder_rename_plans
         SET status='confirmed', confirmed_at=?, confirmation_source='auto', updated_at=?
         WHERE artist_id=? AND status='ready'
           AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
        params![now(), now(), artist_id],
    )?;
    Ok(json!({
        "ok": true,
        "status": "confirmed",
        "scope": "manual_artist",
        "artist_id": artist_id,
        "auto_named": auto_named,
        "auto_confirmed": confirmed,
        "message": "confirm_current_artist_plans",
    }))
}

pub fn unconfirm_all_artist_plans(conn: &Connection, artist_id: i64) -> Result<Value> {
    crate::folder_archive::ensure_folder_schema(conn)?;
    let updated = conn.execute(
        "UPDATE folder_rename_plans
         SET status='ready', confirmed_at=NULL, confirmation_source='', updated_at=?
         WHERE artist_id=? AND status='confirmed'",
        params![now(), artist_id],
    )?;
    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "unconfirmed": updated,
    }))
}

// ---------------------------------------------------------------------------
// Move group merge + auto-resolve (simplified native paths)
// ---------------------------------------------------------------------------

pub fn merge_move_candidate_group(
    conn: &Connection,
    old_artist_id: i64,
    new_artist_id: i64,
) -> Result<Value> {
    merge_move_candidate_group_with_roots(
        conn,
        &crate::media_roots::MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new(),
        },
        old_artist_id,
        new_artist_id,
    )
}

pub fn merge_move_candidate_group_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    old_artist_id: i64,
    new_artist_id: i64,
) -> Result<Value> {
    if old_artist_id == new_artist_id {
        return Ok(json!({
            "action": "group_applied",
            "item_artist_id": old_artist_id,
            "candidate_artist_id": new_artist_id,
            "applied": 0,
            "stale": 0,
            "skipped": 0,
            "resolved_existing": 0,
            "applied_candidates": [],
            "skipped_candidates": [],
        }));
    }
    let moves: Vec<(i64, Option<i64>, String)> = conn
        .prepare(
            "SELECT mc.id, mc.scan_candidate_id, mc.new_path FROM move_candidates mc
             JOIN items i ON i.id = mc.item_id
             WHERE mc.status='pending' AND mc.reason='manual_needed'
               AND i.artist_id=? AND mc.artist_id=?",
        )?
        .query_map(params![old_artist_id, new_artist_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    let mut handled_scan_candidates = HashSet::new();
    let mut stale = 0i64;
    for (id, scan_candidate_id, _new_path) in moves {
        if let Some(scan_candidate_id) = scan_candidate_id {
            if !handled_scan_candidates.insert(scan_candidate_id) {
                continue;
            }
        }
        match apply_move_candidate_group_item_response_with_roots(
            conn,
            roots,
            id,
            old_artist_id,
            new_artist_id,
        ) {
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("moved") => {
                applied.push(json!({"id": id, "item_id": v.get("item_id")}));
            }
            Ok(v) => {
                let reason = v.get("reason").cloned().unwrap_or(json!(v
                    .get("action")
                    .cloned()
                    .unwrap_or(json!("not_moved"))));
                if reason == json!("candidate_stale") {
                    stale += 1;
                }
                skipped.push(json!({
                    "id": id,
                    "reason": reason
                }));
            }
            Err(e) => {
                if e.to_string() == "candidate_stale" {
                    stale += 1;
                }
                skipped.push(json!({"id": id, "reason": e.to_string()}));
            }
        }
    }
    Ok(json!({
        "action": "group_applied",
        "item_artist_id": old_artist_id,
        "candidate_artist_id": new_artist_id,
        "applied": applied.len(),
        "stale": stale,
        "skipped": skipped.len(),
        "resolved_existing": 0,
        "applied_candidates": applied,
        "skipped_candidates": skipped,
    }))
}

/// One pending move-candidate row joined with its item and duplicate counts:
/// (id, item_artist, candidate_artist, reason, scan_candidate_id, new_path,
/// item_hash, candidate_hash, missing, same_scan_candidate_count,
/// same_target_count, tag_count, active_hash_duplicates).
type AutoResolveMoveRow = (
    i64,
    i64,
    i64,
    String,
    Option<i64>,
    String,
    String,
    String,
    i64,
    i64,
    i64,
    i64,
    i64,
);

/// Reasons the resolver itself proves unique before it writes a row:
/// identical inode (`inode`) or a category-only rename (`category_rename`).
/// Every other reason in `move_candidates` exists because the resolver could
/// not decide (multiple missing old records, an active copy, an occupied
/// target, multiple inode or category matches), so automatic processing must
/// not confirm them: applying the first row would silently pick an identity
/// and inherit that record's tags.
const AUTO_APPLY_MOVE_REASONS: &[&str] = &["inode", "category_rename"];

pub fn auto_resolve_move_candidates(conn: &Connection, limit: i64) -> Result<Value> {
    auto_resolve_move_candidates_with_roots(
        conn,
        &crate::media_roots::MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new(),
        },
        limit,
    )
}

pub fn auto_resolve_move_candidates_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
) -> Result<Value> {
    let limit = if limit > 0 {
        limit.min(crate::MAX_BATCH_ITEM_LIMIT)
    } else {
        crate::DEFAULT_BATCH_ITEM_LIMIT
    };
    let moves: Vec<AutoResolveMoveRow> = conn
        .prepare(
            // An auto-eligible row is one the loop below will actually apply
            // (cross-artist proven, ambiguous cluster, mark-as-new, or an
            // auto-apply reason). This must mirror the proof in the loop exactly;
            // if the proof changes, update both. Ordering auto-eligible rows
            // first means a backlog of manual-only (`left_for_manual`) rows can
            // never permanently starve background-appliable moves behind them.
            "SELECT *,
                (CASE
                   WHEN item_artist_id <> candidate_artist_id AND reason = 'manual_needed' AND i_missing = 1
                        AND item_hash <> '' AND item_hash = candidate_hash
                        AND active_hash_duplicates = 0 AND group_count >= 2 THEN 1
                   WHEN item_artist_id <> candidate_artist_id AND reason = 'manual_needed' AND i_missing = 1
                        AND item_hash <> '' AND item_hash = candidate_hash
                        AND active_hash_duplicates = 0 AND group_count = 1 AND target_count = 1 THEN 1
                   WHEN reason = 'manual_needed' AND i_missing = 0 AND tag_count = 0
                        AND group_count = 1 AND target_count = 1
                        AND item_hash <> '' AND item_hash = candidate_hash THEN 1
                   WHEN reason IN ('inode', 'category_rename')
                        AND group_count = 1 AND target_count = 1 AND i_missing = 1
                        AND active_hash_duplicates = 0
                        AND (item_hash = '' OR candidate_hash = '' OR item_hash = candidate_hash) THEN 1
                   ELSE 0
                 END) AS auto_eligible
             FROM (
               SELECT mc.id AS id, i.artist_id AS item_artist_id, mc.artist_id AS candidate_artist_id,
                      mc.reason AS reason, mc.scan_candidate_id AS scan_candidate_id, mc.new_path AS new_path,
                      i.content_hash AS item_hash, COALESCE(sc.content_hash, '') AS candidate_hash,
                      i.missing AS i_missing,
                      (SELECT COUNT(*) FROM move_candidates sibling
                       WHERE sibling.status='pending' AND sibling.scan_candidate_id=mc.scan_candidate_id) AS group_count,
                      (SELECT COUNT(*) FROM move_candidates sibling
                       WHERE sibling.status='pending' AND sibling.new_path=mc.new_path) AS target_count,
                      (SELECT COUNT(*) FROM item_tags it WHERE it.item_id=i.id) AS tag_count,
                      (SELECT COUNT(*) FROM items dup
                        WHERE dup.missing=0 AND dup.id<>i.id
                          AND dup.content_hash<>'' AND dup.content_hash=i.content_hash) AS active_hash_duplicates
                 FROM move_candidates mc
                 JOIN items i ON i.id=mc.item_id
                 LEFT JOIN scan_candidates sc ON sc.id=mc.scan_candidate_id
                 WHERE mc.status='pending'
             ) AS pending
             ORDER BY auto_eligible DESC, id LIMIT ?",
        )?
        .query_map(params![limit], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
                row.get(7)?,
                row.get(8)?,
                row.get(9)?,
                row.get(10)?,
                row.get(11)?,
                row.get(12)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut applied = 0i64;
    let mut added_as_new = 0i64;
    let mut skipped = 0i64;
    let mut left_for_manual = 0i64;
    let mut resolved_existing = 0i64;
    let mut stale = 0i64;
    let mut handled_scan_candidates = HashSet::new();
    for (
        id,
        item_artist_id,
        candidate_artist_id,
        reason,
        scan_candidate_id,
        _new_path,
        item_hash,
        candidate_hash,
        missing,
        group_count,
        target_count,
        tag_count,
        active_hash_duplicates,
    ) in moves
    {
        // Re-prove the match is unambiguous before applying it automatically:
        // one pending candidate for the scan candidate, one for the new path,
        // a missing old record, no live copy sharing the hash, and hash
        // agreement whenever both sides know it.
        let hash_agrees =
            item_hash.is_empty() || candidate_hash.is_empty() || item_hash == candidate_hash;
        let auto_appliable = AUTO_APPLY_MOVE_REASONS.contains(&reason.as_str())
            && group_count == 1
            && target_count == 1
            && missing == 1
            && active_hash_duplicates == 0
            && hash_agrees;
        // Cross-artist automatic handling needs the same proof as the shared
        // write: a completed hash on both sides that actually agrees, a still
        // missing source, no live copy of the content and no competing edge.
        // Without it the background worker would merge the first row of an
        // ambiguous cluster and inherit that record's tags.
        let cross_artist_proven = item_artist_id != candidate_artist_id
            && reason == "manual_needed"
            && missing == 1
            && !item_hash.is_empty()
            && item_hash == candidate_hash
            && group_count == 1
            && target_count == 1
            && active_hash_duplicates == 0;
        // An ambiguous cross-artist cluster (several missing old records all
        // claim the same new file) must not be rewritten onto one old record,
        // because that would silently adopt that record's dates and history.
        // Give the new file its own record and inherit the union of the
        // cluster's tags; sources that disagree on tags (or that also wait on
        // another target) are not blocked, because this path rewrites nothing
        // and every pending target of a source shares its content hash.
        let ambiguous_cluster = item_artist_id != candidate_artist_id
            && reason == "manual_needed"
            && group_count >= 2
            && missing == 1
            && !item_hash.is_empty()
            && item_hash == candidate_hash
            && active_hash_duplicates == 0;
        if ambiguous_cluster {
            if let Some(scan_candidate_id) = scan_candidate_id {
                if !handled_scan_candidates.insert(scan_candidate_id) {
                    continue;
                }
            }
            match resolve_ambiguous_cluster_as_new_response(conn, roots, id) {
                Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("new") => {
                    added_as_new += 1;
                }
                _ => {
                    left_for_manual += 1;
                }
            }
            continue;
        }
        let result = if cross_artist_proven {
            if let Some(scan_candidate_id) = scan_candidate_id {
                if !handled_scan_candidates.insert(scan_candidate_id) {
                    continue;
                }
            }
            apply_move_candidate_group_item_response_with_roots(
                conn,
                roots,
                id,
                item_artist_id,
                candidate_artist_id,
            )
        } else if reason == "manual_needed"
            && missing == 0
            && tag_count == 0
            && group_count == 1
            && target_count == 1
            && !item_hash.is_empty()
            && item_hash == candidate_hash
        {
            mark_move_candidate_new_response(conn, id)
        } else if auto_appliable {
            apply_move_candidate_response_with_roots(conn, roots, id)
        } else {
            // Ambiguous or blocked: keep it pending for the 待判断 list instead
            // of picking one old record on the user's behalf.
            left_for_manual += 1;
            continue;
        };
        match result {
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("moved") => applied += 1,
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("new") => added_as_new += 1,
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("existing") => {
                resolved_existing += 1
            }
            Ok(v)
                if v.get("action").and_then(|a| a.as_str()) == Some("no_match")
                    && v.get("reason").and_then(|r| r.as_str()) == Some("candidate_stale") =>
            {
                stale += 1
            }
            Err(e) if e.to_string() == "candidate_stale" => stale += 1,
            _ => skipped += 1,
        }
    }
    let remaining: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM move_candidates WHERE status='pending'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    Ok(json!({
        "action": "auto_processed",
        "resolved_existing": resolved_existing,
        "applied": applied,
        "added_as_new": added_as_new,
        "stale": stale,
        "skipped": skipped,
        "left_for_manual": left_for_manual,
        "remaining": remaining,
    }))
}

// ---------------------------------------------------------------------------
// Artist suggestion confirm
// ---------------------------------------------------------------------------

pub fn confirm_artist_suggestion(conn: &Connection, item_id: i64, artist_id: i64) -> Result<Value> {
    let artist_name: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id=?",
            params![artist_id],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or_default();
    // Best-effort schema: create suggestion table row if present.
    let _ = conn.execute(
        "CREATE TABLE IF NOT EXISTS artist_suggestions (
            item_id INTEGER NOT NULL,
            artist_id INTEGER NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            reason TEXT NOT NULL DEFAULT '',
            fused_score REAL,
            confirmed_at REAL,
            PRIMARY KEY (item_id, artist_id)
        )",
        [],
    );
    conn.execute(
        "INSERT INTO artist_suggestions (item_id, artist_id, status, reason, confirmed_at)
         VALUES (?, ?, 'confirmed', 'manual', ?)
         ON CONFLICT(item_id, artist_id) DO UPDATE SET
           status='confirmed', reason='manual', confirmed_at=excluded.confirmed_at",
        params![item_id, artist_id, now()],
    )?;
    Ok(json!({
        "ok": true,
        "item_id": item_id,
        "artist_id": artist_id,
        "artist_name": artist_name,
        "status": "confirmed",
    }))
}

// ---------------------------------------------------------------------------
// Character reference delete + rebuild index + import jobs
// ---------------------------------------------------------------------------

/// Where manually uploaded reference photos live. Kept under `DATA_DIR`, which
/// `media_roots` deliberately excludes from the authorized roots, so the scanner
/// and the folder organizer never see these files and `/api/file/preview`
/// policy is unchanged (uploads are served by their own id-based route).
pub fn character_references_dir() -> PathBuf {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    Path::new(&data_dir).join("character-references")
}

/// Upload cap for one reference photo.
pub const REFERENCE_IMAGE_MAX_BYTES: usize = 10 * 1024 * 1024;

/// Sniff the image type from the leading bytes. Neither the client's file name
/// nor its `Content-Type` is trusted: a `.jpg` label on arbitrary bytes must not
/// be accepted, and the stored extension has to describe what we actually got.
/// Returns the canonical extension, or `None` for anything that is not one of
/// the supported photo formats.
pub fn reference_image_extension_for_bytes(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some("jpg");
    }
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some("png");
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some("gif");
    }
    if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
        return Some("webp");
    }
    if bytes.starts_with(b"BM") {
        return Some("bmp");
    }
    None
}

/// Store an uploaded photo under `DATA_DIR/character-references/<id>/`.
/// The name is generated here and the extension is the sniffed one, so nothing
/// the client sends can reach the stored path.
pub fn store_manual_reference_image(
    character_id: i64,
    extension: &str,
    bytes: &[u8],
) -> Result<PathBuf> {
    let dir = character_references_dir().join(character_id.to_string());
    std::fs::create_dir_all(&dir)?;
    let target = dir.join(format!("{}.{extension}", uuid::Uuid::new_v4().simple()));
    // Write beside the target then rename, so a failed write never leaves a
    // half-file that a preview request could serve.
    let staging = dir.join(format!(".{}.upload", uuid::Uuid::new_v4().simple()));
    std::fs::write(&staging, bytes)?;
    std::fs::rename(&staging, &target)?;
    Ok(target)
}

/// Best-effort removal of an uploaded reference file. A missing file is fine;
/// a path outside our own upload directory is refused, so a tampered or stale
/// `image_path` value cannot turn a reference delete into an arbitrary unlink.
pub fn remove_reference_image_file(image_path: &str) {
    if image_path.trim().is_empty() {
        return;
    }
    let path = Path::new(image_path);
    if !path.starts_with(character_references_dir()) {
        log_warn!("character reference: refusing to delete outside upload dir: {image_path}");
        return;
    }
    let _ = std::fs::remove_file(path);
}

/// Drop every uploaded file owned by a character's manual references, then the
/// per-character directory itself when it is empty.
pub fn remove_character_reference_images(conn: &Connection, character_id: i64) -> Result<()> {
    let mut stmt = conn.prepare(
        "SELECT image_path FROM character_references
         WHERE character_id=? AND image_path IS NOT NULL",
    )?;
    let paths = stmt
        .query_map(params![character_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for path in paths {
        remove_reference_image_file(&path);
    }
    let _ = std::fs::remove_dir(character_references_dir().join(character_id.to_string()));
    Ok(())
}

/// Insert a manually added reference. Mirrors `insert_tag_single_reference`
/// (same model metadata) but stores the uploaded file instead of an item id.
pub fn insert_manual_reference(
    conn: &Connection,
    character_id: i64,
    image_path: &Path,
    embedding: &[f32],
) -> Result<i64> {
    let blob = crate::character_ccip::pack_embedding_blob(embedding)?;
    let (repo, variant, file) = crate::character_ccip::embedding_model_meta();
    let dim = crate::character_ccip::CCIP_EMBEDDING_DIM as i64;
    conn.execute(
        "INSERT INTO character_references
         (character_id, embedding, embedding_dim, source_type, item_id, created_at,
          embedding_model_repo_id, embedding_model_variant, embedding_model_file,
          embedding_updated_at, image_path)
         VALUES (?, ?, ?, 'manual', NULL, ?, ?, ?, ?, ?, ?)",
        params![
            character_id,
            blob,
            dim,
            now(),
            repo,
            variant,
            file,
            now(),
            image_path.to_string_lossy()
        ],
    )?;
    Ok(conn.last_insert_rowid())
}

/// Embed a stored reference photo. Tests bypass the ONNX model the same way the
/// tag import does, so the upload path stays testable without OpenVINO.
pub fn embed_manual_reference_image(image_path: &Path) -> Result<Vec<f32>> {
    if fake_import_embedding_enabled() {
        return Ok(fake_embedding_for_item(1));
    }
    crate::character_ccip::embed_image_path(image_path)
}

/// Cheap pre-check before accepting an upload. A *cold* session is deliberately
/// not a refusal: the session loads on demand and is idle-unloaded after 10
/// minutes, so demanding `session_loaded` would reject the first upload after
/// every idle period. Only a structural problem — recognizer disabled, model
/// file missing, or a cached load failure — is worth refusing up front.
pub fn manual_reference_embedding_blocker() -> Option<&'static str> {
    if fake_import_embedding_enabled() {
        return None;
    }
    match crate::character_ccip::session_status()
        .get("reason")
        .and_then(|value| value.as_str())
    {
        Some("disabled") => Some("识别功能已关闭"),
        Some("ccip_model_not_found") => Some("识别模型未就绪，请先在「模型与推理」中准备"),
        Some("session_load_failed") => Some("识别模型加载失败，请先在「模型与推理」中重试准备"),
        _ => None,
    }
}

pub fn delete_character_reference(
    conn: &Connection,
    character_id: i64,
    reference_id: i64,
) -> Result<Value> {
    // `RETURNING` keeps "which file belongs to this row" and "the row is gone"
    // in one statement, so two concurrent deletes of the same reference cannot
    // both decide they own the file. A NULL image_path is a tag_single row: the
    // row is still deleted, it just owns no uploaded file.
    let deleted_row: Option<(i64, Option<String>)> = conn
        .query_row(
            "DELETE FROM character_references WHERE id=? AND character_id=?
             RETURNING id, image_path",
            params![reference_id, character_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((_, Some(path))) = deleted_row.as_ref() {
        remove_reference_image_file(path);
    }
    let deleted = deleted_row.is_some() as i64;
    Ok(json!({
        "ok": deleted > 0,
        "character_id": character_id,
        "reference_id": reference_id,
        "deleted": deleted,
    }))
}

pub fn rebuild_character_index(conn: &Connection) -> Result<Value> {
    #[cfg(test)]
    REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.set(c.get() + 1));
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
            r.get(0)
        })
        .unwrap_or(0);
    // Full embedding rebuild requires the ML model; report inventory for UI.
    Ok(json!({
        "ok": true,
        "status": "ready",
        "reference_count": count,
        "rebuilt": 0,
        "message": "index_metadata_ok_embeddings_on_demand",
    }))
}

// ---------------------------------------------------------------------------
// Character import (manual jobs + optional idle worker)
// ---------------------------------------------------------------------------

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(default)
}

/// Optional per-character cap for auto `tag_single` refs. **0 = unlimited** (default).
/// Multi-artist libraries need many refs per character for different styles — do not
/// force a low seed like 3.
fn import_max_refs_per_character() -> i64 {
    env_i64("CHARACTER_IMPORT_MAX_REFERENCES_PER_CHARACTER", 0)
}

/// Optional per-tag *per job* pacing. **0 = unlimited** (default).
/// Idle ticks may pass a positive value only to bound one background slice.
fn import_limit_per_tag_default() -> i64 {
    env_i64("CHARACTER_IMPORT_LIMIT_PER_TAG", 0)
}

/// Max candidates considered in one job (I/O safety), not a lifetime character cap.
fn import_job_candidate_limit() -> i64 {
    env_i64("CHARACTER_IMPORT_JOB_CANDIDATE_LIMIT", 2000).clamp(50, 20_000)
}

fn character_recognition_enabled() -> bool {
    env_bool("CHARACTER_RECOGNITION_ENABLED", true)
}

fn character_import_idle_enabled() -> bool {
    env_bool("CHARACTER_IMPORT_IDLE_ENABLED", false)
}

#[derive(Clone)]
struct ImportJob {
    value: Value,
}

struct ImportJobRunGuard<'a> {
    conn: &'a Connection,
    job_id: String,
    changed: bool,
}

impl Drop for ImportJobRunGuard<'_> {
    fn drop(&mut self) {
        {
            let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
            if let Some(job) = guard.as_mut().filter(|job| {
                job.value.get("job_id").and_then(Value::as_str) == Some(self.job_id.as_str())
            }) {
                if let Some(obj) = job.value.as_object_mut() {
                    if matches!(
                        obj.get("status").and_then(Value::as_str),
                        Some("pending" | "running")
                    ) {
                        let error = "character import failed before completion";
                        obj.insert("status".into(), json!("failed"));
                        obj.insert("failed".into(), json!(1));
                        obj.insert("failures".into(), json!([{ "error": error }]));
                        obj.insert("first_failure_reason".into(), json!(error));
                        obj.insert("finished_at".into(), json!(now()));
                        obj.insert("busy".into(), json!(false));
                        obj.insert("ok".into(), json!(false));
                    }
                }
            }
        }
        if self.changed {
            let _ = rebuild_character_index(self.conn);
        }
    }
}

fn import_job_slot() -> &'static Mutex<Option<ImportJob>> {
    static SLOT: OnceLock<Mutex<Option<ImportJob>>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(None))
}

fn idle_import_job() -> Value {
    json!({
        "job_id": "",
        "status": "idle",
        "scope": "",
        "total": 0,
        "processed": 0,
        "added": 0,
        "added_references": 0,
        "skipped_existing": 0,
        "skipped_low_similarity": 0,
        "skipped_duplicate": 0,
        "skipped_max_references": 0,
        "failed": 0,
        "current_tag": "",
        "failures": [],
        "first_failure_reason": "",
        "characters": {},
        "references": [],
        "imported_character_ids": [],
        "created_at": null,
        "started_at": null,
        "finished_at": null,
        "cancel_requested": false,
        "busy": false,
    })
}

pub fn get_character_import_job() -> Value {
    let guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(job) => job.value.clone(),
        None => idle_import_job(),
    }
}

fn import_job_busy() -> bool {
    let guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
    import_job_status_is_active(guard.as_ref())
}

/// Atomically claim the single import-job slot. Returns the busy payload when a
/// job is already pending/running, so check-then-start can never race into two
/// concurrent imports.
fn claim_import_job_or_busy(job: Value) -> Option<Value> {
    let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
    if import_job_status_is_active(guard.as_ref()) {
        let mut busy = guard
            .as_ref()
            .map(|j| j.value.clone())
            .unwrap_or_else(|| json!({}));
        if let Some(obj) = busy.as_object_mut() {
            obj.insert("busy".into(), json!(true));
        }
        return Some(busy);
    }
    *guard = Some(ImportJob { value: job });
    None
}

fn import_job_status_is_active(job: Option<&ImportJob>) -> bool {
    matches!(
        job.and_then(|j| j.value.get("status"))
            .and_then(|v| v.as_str()),
        Some("pending" | "running")
    )
}

pub fn cancel_character_import_job(job_id: &str) -> Value {
    let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
    if let Some(job) = guard.as_mut() {
        if job.value.get("job_id").and_then(|v| v.as_str()) == Some(job_id) {
            if let Some(obj) = job.value.as_object_mut() {
                obj.insert("cancel_requested".into(), json!(true));
                let status = obj
                    .get("status")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if status == "pending" || status == "running" {
                    obj.insert("status".into(), json!("cancelled"));
                    obj.insert("finished_at".into(), json!(now()));
                    obj.insert("busy".into(), json!(false));
                }
            }
            return job.value.clone();
        }
    }
    json!({"ok": false, "error": "job_not_found", "job_id": job_id})
}

fn cancel_requested(job_id: &str) -> bool {
    let guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|j| j.value.get("job_id").and_then(|v| v.as_str()) == Some(job_id))
        .and_then(|j| j.value.get("cancel_requested").and_then(|v| v.as_bool()))
        .unwrap_or(false)
}

/// Remove historical fake tag_single rows (dim=1, 4-byte all-zero blob).
/// Manual/confirmed references are never touched.
pub fn purge_pseudo_tag_single_references(conn: &Connection) -> Result<i64> {
    let n = conn.execute(
        "DELETE FROM character_references
         WHERE source_type='tag_single'
           AND embedding_dim=1
           AND length(embedding)=4
           AND embedding = x'00000000'",
        [],
    )?;
    Ok(n as i64)
}

/// Validate + insert a tag_single reference with real embedding metadata.
pub(crate) fn insert_tag_single_reference(
    conn: &Connection,
    character_id: i64,
    item_id: i64,
    embedding: &[f32],
) -> Result<()> {
    let blob = crate::character_ccip::pack_embedding_blob(embedding)?;
    let (repo, variant, file) = crate::character_ccip::embedding_model_meta();
    let dim = crate::character_ccip::CCIP_EMBEDDING_DIM as i64;
    conn.execute(
        "INSERT INTO character_references
         (character_id, embedding, embedding_dim, source_type, item_id, created_at,
          embedding_model_repo_id, embedding_model_variant, embedding_model_file, embedding_updated_at)
         VALUES (?, ?, ?, 'tag_single', ?, ?, ?, ?, ?, ?)",
        params![
            character_id,
            blob,
            dim,
            item_id,
            now(),
            repo,
            variant,
            file,
            now()
        ],
    )?;
    Ok(())
}

fn fake_import_embedding_enabled() -> bool {
    // Test-only escape hatch when ONNX model is not available.
    // Atomic flag avoids cross-test races on process env vars.
    if FAKE_EMBEDDING_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst) {
        return true;
    }
    env_bool("CHARACTER_IMPORT_FAKE_EMBEDDING", false)
}

static FAKE_EMBEDDING_FOR_TESTS: std::sync::atomic::AtomicBool =
    std::sync::atomic::AtomicBool::new(false);

#[cfg(test)]
thread_local! {
    static REBUILD_INDEX_CALLS_FOR_TESTS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Serializes import tests that toggle fake embedding (avoids parallel env races).
#[cfg(test)]
fn import_test_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

fn fake_embedding_for_item(item_id: i64) -> Vec<f32> {
    let dim = crate::character_ccip::CCIP_EMBEDDING_DIM;
    let mut v = vec![0.0f32; dim];
    // Deterministic non-zero unit-ish vector from item id.
    let idx = (item_id.unsigned_abs() as usize) % dim;
    v[idx] = 1.0;
    if idx + 1 < dim {
        v[idx + 1] = 0.25;
    }
    v
}

fn embed_for_import_with_roots(
    conn: &Connection,
    roots: &crate::media_roots::MediaRoots,
    item_id: i64,
) -> Result<Vec<f32>> {
    if fake_import_embedding_enabled() {
        return Ok(fake_embedding_for_item(item_id));
    }
    let (emb, _path, _name, _src) =
        crate::character_ccip::embed_item_with_roots(conn, roots, item_id)?;
    Ok(emb)
}

fn character_tag_single_count(conn: &Connection, character_id: i64) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM character_references
         WHERE character_id=? AND source_type='tag_single'",
        params![character_id],
        |r| r.get(0),
    )
    .unwrap_or(0)
}

/// Remove auto tag_single refs whose character name is no longer a tag on the item.
pub fn cleanup_stale_tag_single_references(conn: &Connection, limit: i64) -> Result<i64> {
    let limit = if limit > 0 { limit.min(200) } else { 50 };
    // SQLite: character name must appear among item's current tags.
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT cr.id
             FROM character_references cr
             JOIN characters c ON c.id = cr.character_id
             LEFT JOIN items i ON i.id = cr.item_id
             WHERE cr.source_type = 'tag_single'
               AND (
                 cr.item_id IS NULL
                 OR i.id IS NULL
                 OR i.missing = 1
                 OR NOT EXISTS (
                   SELECT 1 FROM item_tags it
                   JOIN tags t ON t.id = it.tag_id
                   WHERE it.item_id = cr.item_id AND t.name = c.name
                 )
               )
             LIMIT ?",
        )?
        .query_map(params![limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut deleted = 0i64;
    for id in ids {
        deleted += conn.execute("DELETE FROM character_references WHERE id=?", params![id])? as i64;
    }
    Ok(deleted)
}

/// Import single-tag items into the character library.
///
/// Policy (product):
/// - **No default lifetime cap** on refs per character — different artists/styles need many.
/// - `unreferenced_only` (default true): skip items already linked to any character.
/// - Optional `max_references_per_character` / `limit_per_tag` only when explicitly set > 0.
/// - `source_type=tag_single` so cleanup can distinguish auto vs manual refs.
/// - One job still has a candidate scan limit (I/O safety), not a character library size limit.
pub fn start_character_import_job(conn: &Connection, body: &Value) -> Result<Value> {
    start_character_import_job_with_roots(conn, &crate::media_roots::env_media_roots(), body)
}

pub fn start_character_import_job_with_roots(
    conn: &Connection,
    roots: &crate::media_roots::MediaRoots,
    body: &Value,
) -> Result<Value> {
    start_character_import_job_with_index_changes(conn, roots, body, false)
}

fn start_character_import_job_with_index_changes(
    conn: &Connection,
    roots: &crate::media_roots::MediaRoots,
    body: &Value,
    prior_index_changes: bool,
) -> Result<Value> {
    if !character_recognition_enabled() {
        return Ok(json!({
            "job_id": "",
            "status": "skipped",
            "reason": "character_recognition_disabled",
            "busy": false,
            "added": 0,
            "added_references": 0,
        }));
    }

    let job_id = format!("{:x}", (now() * 1000.0) as u64);
    let artist_id = body.get("artist_id").and_then(|v| v.as_i64());
    let tag_ids: Vec<i64> = body
        .get("tag_ids")
        .and_then(|v| v.as_array())
        .map(|a| a.iter().filter_map(|x| x.as_i64()).collect())
        .unwrap_or_default();
    let tag_names: Vec<String> = body
        .get("tag_names")
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(|s| s.trim().to_string()))
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();
    // 0 = no per-tag-per-job throttle (default). Positive = pacing only.
    let limit_per_tag = body
        .get("limit_per_tag")
        .and_then(|v| v.as_i64())
        .unwrap_or_else(import_limit_per_tag_default)
        .max(0);
    // Explicit UI import without filter: still prefer unreferenced unless false.
    let unreferenced_only = body
        .get("unreferenced_only")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // 0 = unlimited refs per character (default). Never force a low seed.
    let hard_max = body
        .get("max_references_per_character")
        .and_then(|v| v.as_i64())
        .or_else(|| {
            body.get("seed_references_per_character")
                .and_then(|v| v.as_i64())
        })
        .unwrap_or_else(import_max_refs_per_character)
        .max(0);
    let candidate_limit = body
        .get("candidate_limit")
        .and_then(|v| v.as_i64())
        .filter(|v| *v > 0)
        .unwrap_or_else(import_job_candidate_limit)
        .clamp(50, 20_000);

    let scope = if !tag_ids.is_empty() || !tag_names.is_empty() {
        "tag"
    } else if artist_id.is_some() {
        "artist"
    } else {
        "all"
    };

    let mut job = json!({
        "job_id": job_id,
        "status": "running",
        "scope": scope,
        "total": 0,
        "processed": 0,
        "added": 0,
        "added_references": 0,
        "skipped_existing": 0,
        "skipped_low_similarity": 0,
        "skipped_duplicate": 0,
        "skipped_max_references": 0,
        "failed": 0,
        "current_tag": "",
        "failures": [],
        "first_failure_reason": "",
        "characters": {},
        "references": [],
        "imported_character_ids": [],
        "created_at": now(),
        "started_at": now(),
        "finished_at": null,
        "cancel_requested": false,
        "busy": false,
        "limit_per_tag": limit_per_tag,
        "unreferenced_only": unreferenced_only,
        "max_references_per_character": hard_max,
        "candidate_limit": candidate_limit,
    });
    if let Some(busy) = claim_import_job_or_busy(job.clone()) {
        return Ok(busy);
    }
    let mut run_guard = ImportJobRunGuard {
        conn,
        job_id: job_id.clone(),
        changed: prior_index_changes,
    };

    // Purge historical pseudo tag_single rows so those items can re-enter candidates.
    let purged_pseudo = purge_pseudo_tag_single_references(conn).unwrap_or(0);
    run_guard.changed |= purged_pseudo > 0;
    let cleanup = match crate::character_cleanup::cleanup_character_references(conn) {
        Ok(cleanup) => cleanup,
        Err(err) => {
            if let Some(obj) = job.as_object_mut() {
                obj.insert("status".into(), json!("failed"));
                obj.insert("failed".into(), json!(1));
                obj.insert("failures".into(), json!([{"error": err.to_string()}]));
                obj.insert("first_failure_reason".into(), json!(err.to_string()));
                obj.insert("finished_at".into(), json!(now()));
                obj.insert("busy".into(), json!(false));
                obj.insert("ok".into(), json!(false));
            }
            let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
            *guard = Some(ImportJob { value: job });
            return Err(err);
        }
    };
    let cleanup_deleted = cleanup
        .get("cleanup_deleted_reference_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    run_guard.changed |= cleanup_deleted > 0;

    // Single-tag image candidates; prefer items with no character_reference yet.
    let mut sql = String::from(
        "SELECT i.id, i.file_path, i.artist_id, t.id AS tag_id, t.name AS tag_name
         FROM items i
         JOIN item_tags it ON it.item_id = i.id
         JOIN tags t ON t.id = it.tag_id
         WHERE i.missing = 0
           AND (i.media_type IN ('image', 'video') OR i.media_type IS NULL OR i.media_type = '')
           AND i.id IN (
             SELECT item_id FROM item_tags GROUP BY item_id HAVING COUNT(*) = 1
           )",
    );
    let mut bind: Vec<Value> = Vec::new();
    if unreferenced_only {
        sql.push_str(
            " AND NOT EXISTS (
                SELECT 1 FROM character_references cr WHERE cr.item_id = i.id
              )",
        );
    }
    if let Some(aid) = artist_id {
        sql.push_str(" AND i.artist_id = ?");
        bind.push(json!(aid));
    }
    if !tag_ids.is_empty() {
        let ph = tag_ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND t.id IN ({ph})"));
        for id in &tag_ids {
            bind.push(json!(id));
        }
    }
    if !tag_names.is_empty() {
        let ph = tag_names.iter().map(|_| "?").collect::<Vec<_>>().join(",");
        sql.push_str(&format!(" AND t.name IN ({ph})"));
        for n in &tag_names {
            bind.push(json!(n));
        }
    }
    // Prefer characters that currently have fewer auto-refs (spread growth), then by tag/id.
    sql.push_str(
        " ORDER BY (
            SELECT COUNT(*) FROM character_references cr2
            JOIN characters c2 ON c2.id = cr2.character_id
            WHERE c2.name = t.name AND cr2.source_type = 'tag_single'
          ) ASC, t.name, i.id
          LIMIT ?",
    );
    bind.push(json!(candidate_limit));

    let mut stmt = conn.prepare(&sql)?;
    let params_vec: Vec<Box<dyn rusqlite::types::ToSql>> = bind
        .iter()
        .map(|v| -> Box<dyn rusqlite::types::ToSql> {
            if let Some(i) = v.as_i64() {
                Box::new(i)
            } else {
                Box::new(v.as_str().unwrap_or("").to_string())
            }
        })
        .collect();
    let param_refs: Vec<&dyn rusqlite::types::ToSql> =
        params_vec.iter().map(|b| b.as_ref()).collect();

    let rows: Vec<(i64, String, Option<i64>, i64, String)> = stmt
        .query_map(param_refs.as_slice(), |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Option<i64>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, String>(4)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut added = 0i64;
    let mut skipped_existing = 0i64;
    let mut skipped_low_similarity = 0i64;
    let mut skipped_duplicate = 0i64;
    let mut skipped_max = 0i64;
    let mut failed = 0i64;
    let mut processed = 0i64;
    let mut imported_character_ids = Vec::new();
    let mut failures: Vec<Value> = Vec::new();
    let mut cancelled = false;

    let mut row_index = 0usize;
    'tags: while row_index < rows.len() {
        if cancel_requested(&job_id) {
            cancelled = true;
            break;
        }
        let tag_name = rows[row_index].4.clone();
        let group_start = row_index;
        while row_index < rows.len() && rows[row_index].4 == tag_name {
            row_index += 1;
        }

        let char_id: i64 = match conn.query_row(
            "SELECT id FROM characters WHERE name=?",
            params![tag_name],
            |r| r.get(0),
        ) {
            Ok(id) => id,
            Err(_) => {
                conn.execute(
                    "INSERT INTO characters (name) VALUES (?)",
                    params![tag_name],
                )?;
                conn.last_insert_rowid()
            }
        };
        let mut reference_records = crate::character_cleanup::load_character_refs(conn, char_id)?;
        let mut voting_core = crate::character_cleanup::stable_core_records(&reference_records);
        let seed_size = crate::character_cleanup::seed_core_size();
        let group_end = if reference_records.len() < seed_size {
            row_index.min(group_start + 10)
        } else {
            row_index
        };
        let mut candidates: Vec<(i64, crate::character_cleanup::RefRec)> = Vec::new();

        for (item_id, path, candidate_artist_id, _tag_id, _) in &rows[group_start..group_end] {
            if cancel_requested(&job_id) {
                cancelled = true;
                break 'tags;
            }
            processed += 1;
            let exists = conn
                .query_row(
                    "SELECT 1 FROM character_references WHERE item_id=? LIMIT 1",
                    params![item_id],
                    |_| Ok(true),
                )
                .optional()?
                .unwrap_or(false);
            if exists {
                skipped_existing += 1;
                continue;
            }
            if hard_max > 0 && character_tag_single_count(conn, char_id) >= hard_max {
                skipped_max += 1;
                continue;
            }
            match embed_for_import_with_roots(conn, roots, *item_id) {
                Ok(vector) => candidates.push((
                    *item_id,
                    crate::character_cleanup::RefRec::candidate(
                        *item_id,
                        vector,
                        *candidate_artist_id,
                        path,
                    ),
                )),
                Err(err) => {
                    failed += 1;
                    if failures.len() < 20 {
                        failures.push(json!({
                            "item_id": item_id,
                            "tag": tag_name,
                            "error": err.to_string(),
                        }));
                    }
                }
            }
        }

        let mut added_for_tag = 0i64;
        let seed_needed = seed_size
            .saturating_sub(reference_records.len())
            .min(candidates.len());
        let seed_target = if limit_per_tag > 0 {
            seed_needed.min(limit_per_tag as usize)
        } else {
            seed_needed
        };
        let seed_order = if seed_target > 0 {
            let candidate_refs: Vec<_> = candidates
                .iter()
                .map(|(_, reference)| reference.clone())
                .collect();
            crate::character_cleanup::select_diverse_indices(&candidate_refs, candidate_refs.len())
        } else {
            Vec::new()
        };
        let mut seed_considered = std::collections::HashSet::new();
        let mut seed_added = 0usize;
        for (position, &candidate_index) in seed_order.iter().enumerate() {
            if seed_added >= seed_target {
                break;
            }
            seed_considered.insert(candidate_index);
            let candidate = &candidates[candidate_index];
            let vote =
                crate::character_cleanup::core_vote_records(&candidate.1.vector, &voting_core);
            if vote.duplicate {
                let remaining_needed = seed_target - seed_added;
                let replacements = seed_order[position + 1..]
                    .iter()
                    .filter(|&&index| {
                        !crate::character_cleanup::core_vote_records(
                            &candidates[index].1.vector,
                            &voting_core,
                        )
                        .duplicate
                    })
                    .count();
                if replacements >= remaining_needed {
                    skipped_duplicate += 1;
                    continue;
                }
            }
            if hard_max > 0 && character_tag_single_count(conn, char_id) >= hard_max {
                skipped_max += 1;
                continue;
            }
            match insert_tag_single_reference(conn, char_id, candidate.0, &candidate.1.vector) {
                Ok(()) => {
                    added += 1;
                    added_for_tag += 1;
                    seed_added += 1;
                    run_guard.changed = true;
                    reference_records.push(candidate.1.clone());
                    voting_core = crate::character_cleanup::stable_core_records(&reference_records);
                    if !imported_character_ids.contains(&char_id) {
                        imported_character_ids.push(char_id);
                    }
                }
                Err(err) => {
                    failed += 1;
                    if failures.len() < 20 {
                        failures.push(json!({
                                "item_id": candidate.0,
                            "tag": tag_name,
                            "error": err.to_string(),
                        }));
                    }
                }
            }
        }

        for (candidate_index, candidate) in candidates.iter().enumerate() {
            if seed_considered.contains(&candidate_index) {
                continue;
            }
            if limit_per_tag > 0 && added_for_tag >= limit_per_tag {
                break;
            }
            let vote =
                crate::character_cleanup::core_vote_records(&candidate.1.vector, &voting_core);
            if vote.duplicate {
                skipped_duplicate += 1;
                continue;
            }
            if !vote.supported {
                skipped_low_similarity += 1;
                continue;
            }
            if hard_max > 0 && character_tag_single_count(conn, char_id) >= hard_max {
                skipped_max += 1;
                continue;
            }
            match insert_tag_single_reference(conn, char_id, candidate.0, &candidate.1.vector) {
                Ok(()) => {
                    added += 1;
                    added_for_tag += 1;
                    run_guard.changed = true;
                    reference_records.push(candidate.1.clone());
                    voting_core = crate::character_cleanup::stable_core_records(&reference_records);
                    if !imported_character_ids.contains(&char_id) {
                        imported_character_ids.push(char_id);
                    }
                }
                Err(err) => {
                    failed += 1;
                    if failures.len() < 20 {
                        failures.push(json!({
                                "item_id": candidate.0,
                            "tag": tag_name,
                            "error": err.to_string(),
                        }));
                    }
                }
            }
        }
    }

    let status = if cancelled { "cancelled" } else { "completed" };
    if run_guard.changed {
        rebuild_character_index(conn)?;
        run_guard.changed = false;
    }
    if let Some(obj) = job.as_object_mut() {
        obj.insert("status".into(), json!(status));
        obj.insert("total".into(), json!(processed));
        obj.insert("processed".into(), json!(processed));
        obj.insert("added".into(), json!(added));
        obj.insert("added_references".into(), json!(added));
        obj.insert("skipped_existing".into(), json!(skipped_existing));
        obj.insert(
            "skipped_low_similarity".into(),
            json!(skipped_low_similarity),
        );
        obj.insert("skipped_duplicate".into(), json!(skipped_duplicate));
        obj.insert("skipped_max_references".into(), json!(skipped_max));
        obj.insert("failed".into(), json!(failed));
        obj.insert("failures".into(), json!(failures));
        if let Some(first) = failures.first() {
            obj.insert(
                "first_failure_reason".into(),
                first.get("error").cloned().unwrap_or_else(|| json!("")),
            );
        }
        obj.insert(
            "imported_character_ids".into(),
            json!(imported_character_ids),
        );
        obj.insert("purged_pseudo_tag_single".into(), json!(purged_pseudo));
        obj.insert("finished_at".into(), json!(now()));
        obj.insert("busy".into(), json!(false));
        obj.insert("ok".into(), json!(status == "completed"));
        obj.insert("cleanup".into(), cleanup);
    }
    {
        let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(ImportJob { value: job.clone() });
    }
    Ok(job)
}

/// One idle tick: yield to scan/hash, clean stale tag_single, import a small
/// unreferenced batch. Default **disabled** (`CHARACTER_IMPORT_IDLE_ENABLED=0`).
pub fn run_idle_character_import_once(conn: &Connection) -> Result<Value> {
    if !character_import_idle_enabled() {
        return Ok(json!({"status": "skipped", "reason": "idle_disabled"}));
    }
    if !character_recognition_enabled() {
        return Ok(json!({"status": "skipped", "reason": "character_recognition_disabled"}));
    }
    if import_job_busy() {
        return Ok(json!({
            "status": "skipped",
            "reason": "import_job_active",
            "job": get_character_import_job(),
        }));
    }

    // Yield while scan is active.
    if let Ok(scan) = crate::scan::get_scan_state(conn) {
        if scan.get("status").and_then(|v| v.as_str()) == Some("scanning") {
            return Ok(json!({"status": "skipped", "reason": "scan_active"}));
        }
    }
    // Yield while hash backlog remains.
    if let Ok(hash) = crate::hash_status::hash_status_response(conn) {
        let remaining = hash
            .pointer("/items/remaining")
            .and_then(|v| v.as_i64())
            .unwrap_or(0)
            + hash
                .pointer("/scan_candidates/remaining")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
        if remaining > 0 {
            return Ok(
                json!({"status": "skipped", "reason": "hash_active", "hash_remaining": remaining}),
            );
        }
    }

    let cleaned = cleanup_stale_tag_single_references(
        conn,
        env_i64("CHARACTER_IMPORT_STALE_TAG_SINGLE_REPAIR_BATCH_SIZE", 10),
    )
    .unwrap_or(0);

    // Idle: no per-character cap; small candidate slice only so one tick stays light.
    let body = json!({
        "unreferenced_only": true,
        "limit_per_tag": 0,
        "max_references_per_character": 0,
        "candidate_limit": env_i64("CHARACTER_IMPORT_IDLE_BATCH", 80).clamp(10, 500),
    });
    let job = start_character_import_job_with_index_changes(
        conn,
        &crate::media_roots::env_media_roots(),
        &body,
        cleaned > 0,
    )?;
    let added = job
        .get("added")
        .or_else(|| job.get("added_references"))
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cleanup_deleted = job
        .pointer("/cleanup/cleanup_deleted_reference_ids")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if job.get("status").and_then(|v| v.as_str()) == Some("skipped") {
        return Ok(job);
    }
    if added == 0 && cleaned == 0 && cleanup_deleted == 0 {
        return Ok(json!({
            "status": "skipped",
            "reason": "no_candidates",
            "auto_deleted_stale_tag_single": cleaned,
            "job": job,
        }));
    }
    Ok(json!({
        "status": "completed",
        "auto_deleted_stale_tag_single": cleaned,
        "job": job,
        "added": added,
    }))
}

/// Background idle loop for primary mode. Safe to call once; no-op if disabled.
pub fn spawn_character_import_idle_worker(pool: std::sync::Arc<crate::db::DbPool>) {
    if !character_import_idle_enabled() {
        return;
    }
    static STARTED: OnceLock<()> = OnceLock::new();
    if STARTED.set(()).is_err() {
        return;
    }
    let start_delay = env_i64("CHARACTER_IMPORT_IDLE_START_DELAY", 120).max(0) as u64;
    let interval_i = env_i64("CHARACTER_IMPORT_IDLE_INTERVAL", 60).max(5);
    let interval = interval_i as u64;
    let backoff = env_i64("CHARACTER_IMPORT_IDLE_BACKOFF_INTERVAL", 600).max(interval_i) as u64;
    std::thread::Builder::new()
        .name("character-import-idle".into())
        .spawn(move || {
            std::thread::sleep(std::time::Duration::from_secs(start_delay));
            let mut no_progress = 0u32;
            loop {
                let sleep_s = if no_progress <= 1 { interval } else { backoff };
                match pool.get() {
                    Ok(conn) => match run_idle_character_import_once(&conn) {
                        Ok(result) => {
                            let status =
                                result.get("status").and_then(|v| v.as_str()).unwrap_or("");
                            let added = result.get("added").and_then(|v| v.as_i64()).unwrap_or(0);
                            if status == "completed" && added > 0 {
                                no_progress = 0;
                            } else if status == "skipped" {
                                let reason =
                                    result.get("reason").and_then(|v| v.as_str()).unwrap_or("");
                                if reason == "no_candidates" {
                                    no_progress = no_progress.saturating_add(1);
                                } else {
                                    // scan/hash busy: do not ramp backoff aggressively
                                    no_progress = 0;
                                }
                            } else {
                                no_progress = no_progress.saturating_add(1);
                            }
                        }
                        Err(e) => {
                            log_error!("character idle import error: {e}");
                            no_progress = 0;
                        }
                    },
                    Err(e) => log_error!("character idle import pool: {e}"),
                }
                std::thread::sleep(std::time::Duration::from_secs(sleep_s));
            }
        })
        .ok();
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn folder_item_ids_escapes_like_wildcards_in_folder_names() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE items (
               id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT,
               media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
               missing INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, path) VALUES (1, '/pictures/a')",
            [],
        )
        .unwrap();
        for (id, path) in [
            (1, "/pictures/a/100%/f1.jpg"),
            (2, "/pictures/a/100x/f2.jpg"),
            (3, "/pictures/a/sub_dir/f3.jpg"),
            (4, "/pictures/a/subxdir/f4.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path) VALUES (?, 1, ?)",
                rusqlite::params![id, path],
            )
            .unwrap();
        }
        // `%` and `_` in the requested folder must match literally; unescaped
        // they widen the pattern and the tag write would touch other folders.
        let ids = folder_item_ids(&conn, 1, "100%").unwrap();
        assert_eq!(ids, vec![1], "% must not act as a wildcard");
        let ids = folder_item_ids(&conn, 1, "sub_dir").unwrap();
        assert_eq!(ids, vec![3], "_ must not act as a wildcard");
        let ids = folder_item_ids(&conn, 1, "100x").unwrap();
        assert_eq!(ids, vec![2], "plain folders still match exactly");
    }

    #[test]
    fn folder_item_ids_matches_folder_case_exactly() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE items (
               id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT,
               media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
               missing INTEGER DEFAULT 0
             );",
        )
        .unwrap();
        conn.execute("INSERT INTO artists (id, path) VALUES (1, '/root')", [])
            .unwrap();
        for (id, path) in [
            (1, "/root/Foo/a.jpg"),
            (2, "/root/foo/b.jpg"),
            (3, "/root/foox/c.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path) VALUES (?, 1, ?)",
                rusqlite::params![id, path],
            )
            .unwrap();
        }
        // SQLite's LIKE is ASCII case-insensitive, so a `Foo` tag write used to
        // also rewrite every file under the distinct `foo` folder.
        let ids = folder_item_ids(&conn, 1, "Foo").unwrap();
        assert_eq!(ids, vec![1], "case-only sibling folders must stay distinct");
        let ids = folder_item_ids(&conn, 1, "foo").unwrap();
        assert_eq!(ids, vec![2], "case-only sibling folders must stay distinct");
        let ids = folder_item_ids(&conn, 1, "Foox").unwrap();
        assert!(ids.is_empty(), "no prefix bleeding into longer names");
    }

    #[test]
    fn log_line_timestamp_parses_local_wall_clock_not_utc() {
        use chrono::TimeZone;
        let naive = chrono::NaiveDateTime::parse_from_str(
            "2026-07-01 12:00:00,000",
            "%Y-%m-%d %H:%M:%S,%3f",
        )
        .unwrap();
        let expected_local = chrono::Local
            .from_local_datetime(&naive)
            .earliest()
            .expect("wall-clock time resolves in the local timezone")
            .timestamp_millis();
        let parsed = log_line_timestamp_millis("2026-07-01 12:00:00,000 [ERROR] boom")
            .expect("log stamp must parse");
        assert_eq!(
            parsed, expected_local,
            "log stamps are local wall-clock; UTC interpretation shifts by the host offset"
        );
    }

    #[test]
    fn operation_log_includes_errors_array() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let db = dir.path().join("g.db");
        let conn = Connection::open(&db).unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE move_history (
               id INTEGER PRIMARY KEY, item_id INTEGER, artist_id INTEGER,
               old_path TEXT, new_path TEXT, reason TEXT, status TEXT,
               details TEXT, created_at REAL, applied_at REAL, reverted_at REAL
             );
             CREATE TABLE folder_rename_plans (
               id INTEGER PRIMARY KEY, artist_id INTEGER, source_folder TEXT,
               target_folder TEXT, status TEXT, plan_kind TEXT, file_count INTEGER,
               selected_tag_ids TEXT, parsed_date TEXT, execution_log TEXT DEFAULT '[]',
               confirmed_at REAL, executed_at REAL, created_at REAL, updated_at REAL
             );",
        )
        .unwrap();
        let logs = dir.path().join("data/logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("gallery.log"),
            "INFO ok\n[ERROR] something failed\n",
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec!["/pictures".into()],
            labels: vec!["p".into()],
            real_paths: vec!["/pictures".into()],
        };
        let log = operation_log_response(&conn, &roots, Some(10), Some(10)).unwrap();
        assert!(log.get("errors").is_some());
        assert!(!log["errors"].as_array().unwrap().is_empty());
    }

    #[test]
    fn folder_rename_auto_run_reports_confirmation_not_execution() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("source");
        std::fs::create_dir_all(&source).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
        let artist_path = artist.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (7, 'Artist', ?)",
            [&artist_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 7, ?, 'a.jpg', 'source', '2026-05-01', '2026-05-01')",
            [format!("{artist_path}/source/a.jpg")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status)
             VALUES (1, 7, 'source', '2026-05/2026-05 untitled', 'ready')",
            [],
        )
        .unwrap();

        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };
        let result = folder_rename_auto_run(&conn, &roots, 7).unwrap();

        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["message"], "confirm_current_artist_plans");
        assert_eq!(result["auto_confirmed"], 1);
    }

    #[test]
    fn folder_rename_auto_run_names_draft_plan_then_confirms() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let source = artist.join("2026-01-05 测试");
        std::fs::create_dir_all(&source).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0);",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
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

        let result = folder_rename_auto_run(&conn, &roots, 1).unwrap();

        assert_eq!(result["auto_named"], 1);
        assert_eq!(result["auto_confirmed"], 1);
        let row: (String, String) = conn
            .query_row(
                "SELECT target_folder, status FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "2026/2026-01-05 测试");
        assert_eq!(row.1, "confirmed");
        assert!(artist.join("2026-01-05 测试").exists());
    }

    /// Builds a log line whose timestamp sits `offset_hours` away from now so
    /// the entry stays inside the "recent" window whenever the suite runs.
    fn log_line_at(offset_hours: i64, fractional: &str, text: &str) -> String {
        let millis = super::now_millis() + offset_hours * 3_600_000;
        let stamp = chrono::DateTime::from_timestamp_millis(millis)
            .map(|value| value.format("%Y-%m-%d %H:%M:%S").to_string())
            .unwrap_or_else(|| "1970-01-01 00:00:00".to_string());
        format!("{}{} {}\n", stamp, fractional, text)
    }

    #[test]
    fn recent_log_errors_uses_exact_markers_and_a_bounded_tail() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _window = crate::test_support::EnvVar::set("GALLERY_LOG_ERROR_WINDOW_HOURS", "168");
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let mut content = String::from("[ERROR] old\n");
        content.push_str(&"INFO ok\n".repeat(300_000));
        content.push_str("INFO failed=0\n[ERROR] real\nTraceback: broken\nfrontend_error ui\nfrontend_rejection promise\n");
        content.push_str(&format!("[ERROR] {}\n", "x".repeat(600)));
        std::fs::write(logs.join("gallery.log"), content).unwrap();

        let errors = recent_log_errors(&logs, 10);

        assert!(errors.iter().any(|row| row["line"] == "[ERROR] real"));
        assert!(errors.iter().any(|row| row["line"] == "Traceback: broken"));
        assert!(errors.iter().any(|row| row["line"] == "frontend_error ui"));
        assert!(errors
            .iter()
            .any(|row| row["line"] == "frontend_rejection promise"));
        assert!(!errors.iter().any(|row| row["line"] == "[ERROR] old"));
        assert!(!errors.iter().any(|row| row["line"] == "INFO failed=0"));
        assert!(errors
            .iter()
            .all(|row| row["line"].as_str().unwrap().len() <= 500));
    }

    #[test]
    fn recent_log_errors_sorts_newest_across_log_files() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _window = crate::test_support::EnvVar::set("GALLERY_LOG_ERROR_WINDOW_HOURS", "168");
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("gallery.log"),
            log_line_at(-2, ",000", "[ERROR] oldest"),
        )
        .unwrap();
        std::fs::write(
            logs.join("startup.log"),
            log_line_at(-1, ",000", "[ERROR] newest"),
        )
        .unwrap();

        let errors = recent_log_errors(&logs, 10);

        assert_eq!(errors.len(), 2, "both entries are inside the window");
        assert!(errors[0]["line"]
            .as_str()
            .unwrap()
            .ends_with("[ERROR] newest"));
        assert!(errors[1]["line"]
            .as_str()
            .unwrap()
            .ends_with("[ERROR] oldest"));
    }

    #[test]
    fn recent_log_errors_orders_colored_dotted_timestamps_and_tracebacks() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _window = crate::test_support::EnvVar::set("GALLERY_LOG_ERROR_WINDOW_HOURS", "168");
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("gallery.log"),
            log_line_at(-1, ",000", "[ERROR] newest"),
        )
        .unwrap();
        std::fs::write(
            logs.join("startup.log"),
            format!(
                "\x1b[1;31m{}Traceback (most recent call last):\n",
                log_line_at(-2, ".031040940", "[ERROR] older")
            ),
        )
        .unwrap();

        let errors = recent_log_errors(&logs, 10);

        assert_eq!(errors.len(), 3, "all entries are inside the window");
        assert!(errors[0]["line"]
            .as_str()
            .unwrap()
            .ends_with("[ERROR] newest"));
        assert_eq!(errors[1]["line"], "Traceback (most recent call last):");
        assert!(errors[2]["line"].as_str().unwrap().contains("older"));
    }

    #[test]
    fn recent_log_errors_drops_entries_older_than_the_window() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _window = crate::test_support::EnvVar::set("GALLERY_LOG_ERROR_WINDOW_HOURS", "168");
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        // Stale file left behind by the pre-Rust stack: must stop polluting health.
        std::fs::write(
            logs.join("gallery.log"),
            log_line_at(-24 * 30, ",000", "[ERROR] fossil"),
        )
        .unwrap();
        std::fs::write(
            logs.join("startup.log"),
            log_line_at(-1, ",000", "[ERROR] current"),
        )
        .unwrap();

        let errors = recent_log_errors(&logs, 10);

        assert_eq!(errors.len(), 1, "only the fresh entry survives");
        assert!(errors[0]["line"]
            .as_str()
            .unwrap()
            .ends_with("[ERROR] current"));
    }

    #[test]
    fn recent_log_errors_window_can_be_disabled() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _window = crate::test_support::EnvVar::set("GALLERY_LOG_ERROR_WINDOW_HOURS", "0");
        let dir = tempdir().unwrap();
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("gallery.log"),
            log_line_at(-24 * 30, ",000", "[ERROR] fossil"),
        )
        .unwrap();

        let errors = recent_log_errors(&logs, 10);

        assert_eq!(errors.len(), 1, "window=0 keeps every error");
        assert!(errors[0]["line"]
            .as_str()
            .unwrap()
            .ends_with("[ERROR] fossil"));
    }

    fn fixture_conn() -> (tempfile::TempDir, Connection) {
        let dir = tempdir().unwrap();
        let conn = Connection::open(dir.path().join("g.db")).unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT,
               file_name TEXT, missing INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER, PRIMARY KEY(item_id, tag_id));
             CREATE TABLE characters (id INTEGER PRIMARY KEY, name TEXT UNIQUE);
             CREATE TABLE character_references (
               id INTEGER PRIMARY KEY, character_id INTEGER, embedding BLOB, embedding_dim INTEGER,
               source_type TEXT, item_id INTEGER, created_at REAL,
               embedding_model_repo_id TEXT DEFAULT '', embedding_model_variant TEXT DEFAULT '',
               embedding_model_file TEXT DEFAULT '', embedding_updated_at REAL
             );
             CREATE TABLE scan_state (id INTEGER PRIMARY KEY, status TEXT, updated_at REAL);
             INSERT INTO artists VALUES (1,'a','/p');
             INSERT INTO items VALUES (1,1,'/p/a.jpg','a.jpg',0,'image',0);
             INSERT INTO items VALUES (2,1,'/p/b.jpg','b.jpg',0,'image',0);
             INSERT INTO items VALUES (3,1,'/p/c.jpg','c.jpg',0,'image',0);
             INSERT INTO items VALUES (4,1,'/p/d.jpg','d.jpg',0,'image',0);
             INSERT INTO tags VALUES (1,1,'hero',1);
             INSERT INTO item_tags VALUES (1,1);
             INSERT INTO item_tags VALUES (2,1);
             INSERT INTO item_tags VALUES (3,1);
             INSERT INTO item_tags VALUES (4,1);",
        )
        .unwrap();
        {
            let mut g = import_job_slot().lock().unwrap();
            *g = None;
        }
        std::env::set_var("CHARACTER_RECOGNITION_ENABLED", "1");
        FAKE_EMBEDDING_FOR_TESTS.store(true, std::sync::atomic::Ordering::SeqCst);
        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        (dir, conn)
    }

    fn clustered_embedding(side_index: usize) -> Vec<f32> {
        let mut embedding = vec![0.0; crate::character_ccip::CCIP_EMBEDDING_DIM];
        embedding[10] = 0.8;
        embedding[side_index] = 0.6;
        embedding
    }

    fn seed_reference(conn: &Connection, character_id: i64, item_id: i64, side_index: usize) {
        insert_tag_single_reference(
            conn,
            character_id,
            item_id,
            &clustered_embedding(side_index),
        )
        .unwrap();
    }

    #[test]
    fn character_import_job_idle_and_run() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        let idle = get_character_import_job();
        assert_eq!(idle["status"], "idle");
        let job = start_character_import_job(&conn, &json!({})).unwrap();
        assert_eq!(job["status"], "completed");
        assert!(job["added"].as_i64().unwrap() >= 1);
        let (source, dim, blob): (String, i64, Vec<u8>) = conn
            .query_row(
                "SELECT source_type, embedding_dim, embedding FROM character_references LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(source, "tag_single");
        assert_eq!(dim, crate::character_ccip::CCIP_EMBEDDING_DIM as i64);
        assert_eq!(blob.len(), crate::character_ccip::CCIP_EMBEDDING_DIM * 4);
        assert!(blob.iter().any(|b| *b != 0));
    }

    #[test]
    fn character_import_default_has_no_per_character_cap() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        let mut core_a = vec![0.0; crate::character_ccip::CCIP_EMBEDDING_DIM];
        core_a[4] = 0.5;
        core_a[20] = 0.8660254;
        let mut core_b = vec![0.0; crate::character_ccip::CCIP_EMBEDDING_DIM];
        core_b[4] = 0.4;
        core_b[21] = 0.9165151;
        insert_tag_single_reference(&conn, 1, 1, &core_a).unwrap();
        insert_tag_single_reference(&conn, 1, 2, &core_b).unwrap();
        seed_reference(&conn, 1, 3, 13);

        let job = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();
        assert_eq!(job["status"], "completed");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(job["added"], 1, "{job}");
        assert_eq!(
            n, 4,
            "default unlimited should grow past the stable core: {job}"
        );
    }

    #[test]
    fn character_import_seeds_from_ten_candidates_with_artist_spread() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute_batch(
            "INSERT INTO artists VALUES (2,'b','/q'),(3,'c','/r');
             INSERT INTO tags VALUES (2,2,'hero',1),(3,3,'hero',1);
             INSERT INTO items VALUES (5,2,'/q/e.jpg','e.jpg',0,'image',0);
             INSERT INTO items VALUES (6,2,'/q/f.jpg','f.jpg',0,'image',0);
             INSERT INTO items VALUES (7,2,'/q/g.jpg','g.jpg',0,'image',0);
             INSERT INTO items VALUES (8,3,'/r/h.jpg','h.jpg',0,'image',0);
             INSERT INTO items VALUES (9,3,'/r/i.jpg','i.jpg',0,'image',0);
             INSERT INTO items VALUES (10,3,'/r/j.jpg','j.jpg',0,'image',0);
             INSERT INTO item_tags VALUES (5,2),(6,2),(7,2),(8,3),(9,3),(10,3);",
        )
        .unwrap();

        let job = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 3}),
        )
        .unwrap();

        let artists: Vec<i64> = conn
            .prepare(
                "SELECT DISTINCT i.artist_id
                 FROM character_references cr JOIN items i ON i.id=cr.item_id
                 ORDER BY i.artist_id",
            )
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(job["total"], 10, "{job}");
        assert_eq!(job["added"], 3, "{job}");
        assert_eq!(artists, vec![1, 2, 3], "{job}");
    }

    #[test]
    fn character_import_requires_two_core_votes() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        let mut one_vote = vec![0.0; crate::character_ccip::CCIP_EMBEDDING_DIM];
        one_vote[4] = 0.5;
        one_vote[20] = 0.8660254;
        insert_tag_single_reference(&conn, 1, 1, &one_vote).unwrap();
        seed_reference(&conn, 1, 2, 12);
        seed_reference(&conn, 1, 3, 13);

        let job = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();

        assert_eq!(job["added"], 0, "{job}");
        assert_eq!(job["skipped_low_similarity"], 1, "{job}");
        let item_four_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_references WHERE item_id=4",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(item_four_refs, 0);
    }

    #[test]
    fn character_import_skips_near_duplicate_candidate() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        insert_tag_single_reference(&conn, 1, 1, &fake_embedding_for_item(4)).unwrap();
        seed_reference(&conn, 1, 2, 12);
        seed_reference(&conn, 1, 3, 13);

        let job = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();

        assert_eq!(job["added"], 0, "{job}");
        assert_eq!(job["skipped_duplicate"], 1, "{job}");
    }

    #[test]
    fn cleanup_before_import_does_not_restore_deleted_outlier() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        seed_reference(&conn, 1, 1, 11);
        seed_reference(&conn, 1, 2, 12);
        seed_reference(&conn, 1, 3, 13);
        insert_tag_single_reference(&conn, 1, 4, &fake_embedding_for_item(4)).unwrap();

        let first = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();
        let second = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();

        assert_eq!(first["added"], 0, "{first}");
        assert_eq!(
            first["cleanup"]["auto_deleted_low_similarity"], 1,
            "{first}"
        );
        assert_eq!(first["skipped_low_similarity"], 1, "{first}");
        assert_eq!(second["added"], 0, "{second}");
        assert_eq!(
            second["cleanup"]["auto_deleted_low_similarity"], 0,
            "{second}"
        );
        assert_eq!(second["skipped_low_similarity"], 1, "{second}");
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 3);
    }

    #[test]
    fn character_import_optional_max_only_when_explicit() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        let job = start_character_import_job(
            &conn,
            &json!({
                "unreferenced_only": true,
                "max_references_per_character": 2,
            }),
        )
        .unwrap();
        assert_eq!(job["status"], "completed");
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 2, "explicit max still honored: {job}");
    }

    #[test]
    fn idle_import_disabled_by_default() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        let r = run_idle_character_import_once(&conn).unwrap();
        assert_eq!(r["status"], "skipped");
        assert_eq!(r["reason"], "idle_disabled");
    }

    #[test]
    fn idle_import_when_enabled_imports_unreferenced_only() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        std::env::set_var("CHARACTER_IMPORT_IDLE_ENABLED", "1");
        std::env::set_var("CHARACTER_IMPORT_SEED_REFERENCES_PER_CHARACTER", "3");
        // seed one existing ref so unreferenced_only skips item 1 if linked
        conn.execute("INSERT INTO characters(name) VALUES('hero')", [])
            .unwrap();
        // no refs yet — idle should add up to seed
        let r = run_idle_character_import_once(&conn).unwrap();
        assert!(
            r["status"] == "completed" || r["status"] == "skipped",
            "{r}"
        );
        if r["status"] == "completed" {
            assert!(r["added"].as_i64().unwrap() > 0);
        }
        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        std::env::remove_var("CHARACTER_IMPORT_SEED_REFERENCES_PER_CHARACTER");
    }

    #[test]
    fn idle_import_uses_job_owned_cleanup_once() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        std::env::set_var("CHARACTER_IMPORT_IDLE_ENABLED", "1");
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        seed_reference(&conn, 1, 1, 11);
        seed_reference(&conn, 1, 2, 12);
        seed_reference(&conn, 1, 3, 13);
        insert_tag_single_reference(&conn, 1, 4, &fake_embedding_for_item(4)).unwrap();

        let result = run_idle_character_import_once(&conn).unwrap();

        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(
            result["job"]["cleanup"]["auto_deleted_low_similarity"], 1,
            "{result}"
        );
        assert!(
            result["job"].get("pre_import_cleanup").is_none(),
            "{result}"
        );
    }

    #[test]
    fn idle_stale_only_cleanup_rebuilds_index_once() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        std::env::set_var("CHARACTER_IMPORT_IDLE_ENABLED", "1");
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'gone')", [])
            .unwrap();
        seed_reference(&conn, 1, 1, 11);
        conn.execute("UPDATE items SET missing=1", []).unwrap();
        REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.set(0));

        let result = run_idle_character_import_once(&conn).unwrap();

        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["added"], 0, "{result}");
        assert_eq!(result["auto_deleted_stale_tag_single"], 1, "{result}");
        assert_eq!(REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.get()), 1);
    }

    #[test]
    fn cleanup_stale_tag_single_removes_orphans() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(9,'gone')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO character_references(character_id,embedding,embedding_dim,source_type,item_id,created_at)
             VALUES(9,x'00',1,'tag_single',1,0)",
            [],
        )
        .unwrap();
        let deleted = cleanup_stale_tag_single_references(&conn, 10).unwrap();
        assert!(deleted >= 1);
    }

    #[test]
    fn purge_pseudo_tag_single_keeps_manual_and_allows_reimport() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        // Historical pseudo row.
        conn.execute(
            "INSERT INTO character_references(character_id,embedding,embedding_dim,source_type,item_id,created_at)
             VALUES(1, x'00000000', 1, 'tag_single', 1, 0)",
            [],
        )
        .unwrap();
        // Manual row must survive.
        conn.execute(
            "INSERT INTO character_references(character_id,embedding,embedding_dim,source_type,item_id,created_at)
             VALUES(1, x'01000000', 1, 'manual', 2, 0)",
            [],
        )
        .unwrap();
        let purged = purge_pseudo_tag_single_references(&conn).unwrap();
        assert_eq!(purged, 1);
        let manual: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_references WHERE source_type='manual'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(manual, 1);
        // Item 1 is free again for import (item 2 still has manual ref).
        let job = start_character_import_job(
            &conn,
            &json!({"unreferenced_only": true, "limit_per_tag": 0}),
        )
        .unwrap();
        assert_eq!(job["status"], "completed");
        assert!(job["added"].as_i64().unwrap() >= 1);
        let dim: i64 = conn
            .query_row(
                "SELECT embedding_dim FROM character_references
                 WHERE item_id=1 AND source_type='tag_single'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(dim, crate::character_ccip::CCIP_EMBEDDING_DIM as i64);
    }

    #[test]
    fn insert_tag_single_rejects_zero_and_wrong_dim() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        let bad = insert_tag_single_reference(&conn, 1, 1, &[0.0f32; 4]);
        assert!(bad.is_err());
        let zero = insert_tag_single_reference(
            &conn,
            1,
            1,
            &vec![0.0f32; crate::character_ccip::CCIP_EMBEDDING_DIM],
        );
        assert!(zero.is_err());
        let mut good = vec![0.0f32; crate::character_ccip::CCIP_EMBEDDING_DIM];
        good[3] = 0.5;
        insert_tag_single_reference(&conn, 1, 1, &good).unwrap();
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn embedding_failure_does_not_add_reference() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        // Force real embed path so missing files fail without inserting.
        FAKE_EMBEDDING_FOR_TESTS.store(false, std::sync::atomic::Ordering::SeqCst);
        let job = start_character_import_job(&conn, &json!({"unreferenced_only": true})).unwrap();
        FAKE_EMBEDDING_FOR_TESTS.store(true, std::sync::atomic::Ordering::SeqCst);
        assert_eq!(job["status"], "completed");
        assert_eq!(job["added"].as_i64().unwrap_or(-1), 0);
        assert!(job["failed"].as_i64().unwrap() >= 1);
        let n: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(n, 0);
    }

    #[test]
    fn cleanup_failure_does_not_leave_import_job_busy() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("DROP TABLE character_references", []).unwrap();

        assert!(start_character_import_job(&conn, &json!({})).is_err());

        let job = get_character_import_job();
        assert_eq!(job["status"], "failed", "{job}");
        assert_eq!(job["busy"], false, "{job}");
        assert!(!import_job_busy(), "{job}");
        assert!(job["finished_at"].as_f64().is_some(), "{job}");
        assert!(
            !job["first_failure_reason"]
                .as_str()
                .unwrap_or_default()
                .is_empty(),
            "{job}"
        );
    }

    #[test]
    fn post_cleanup_failure_rebuilds_changed_index_once() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'hero')", [])
            .unwrap();
        seed_reference(&conn, 1, 1, 11);
        seed_reference(&conn, 1, 2, 12);
        seed_reference(&conn, 1, 3, 13);
        insert_tag_single_reference(&conn, 1, 4, &fake_embedding_for_item(4)).unwrap();
        conn.execute("DROP TABLE item_tags", []).unwrap();
        REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.set(0));

        assert!(start_character_import_job(&conn, &json!({})).is_err());

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 3);
        assert_eq!(REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.get()), 1);
        assert_eq!(get_character_import_job()["status"], "failed");
    }

    #[test]
    fn idle_import_failure_rebuilds_stale_cleanup_once() {
        let _lock = import_test_lock();
        let (_dir, conn) = fixture_conn();
        std::env::set_var("CHARACTER_IMPORT_IDLE_ENABLED", "1");
        conn.execute("INSERT INTO characters(id,name) VALUES(1,'gone')", [])
            .unwrap();
        seed_reference(&conn, 1, 1, 11);
        conn.execute_batch(
            "CREATE TRIGGER fail_hero_character_insert
             BEFORE INSERT ON characters WHEN NEW.name='hero'
             BEGIN SELECT RAISE(ABORT, 'forced character insert failure'); END;",
        )
        .unwrap();
        REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.set(0));

        let result = run_idle_character_import_once(&conn);

        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        assert!(result.is_err());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(REBUILD_INDEX_CALLS_FOR_TESTS.with(|c| c.get()), 1);
    }

    #[test]
    fn reference_image_type_is_sniffed_from_the_bytes() {
        assert_eq!(
            reference_image_extension_for_bytes(&[0xFF, 0xD8, 0xFF, 0xE0]),
            Some("jpg")
        );
        assert_eq!(
            reference_image_extension_for_bytes(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]),
            Some("png")
        );
        assert_eq!(
            reference_image_extension_for_bytes(b"GIF89a...."),
            Some("gif")
        );
        assert_eq!(
            reference_image_extension_for_bytes(b"RIFF\x00\x00\x00\x00WEBPVP8 "),
            Some("webp")
        );
        assert_eq!(reference_image_extension_for_bytes(b"BM...."), Some("bmp"));
        // A script wearing a .jpg label must not be accepted.
        assert_eq!(
            reference_image_extension_for_bytes(b"<?php echo 1; ?>"),
            None
        );
        // RIFF alone is not WebP (an AVI would otherwise pass).
        assert_eq!(
            reference_image_extension_for_bytes(b"RIFF\x00\x00\x00\x00AVI "),
            None
        );
        assert_eq!(reference_image_extension_for_bytes(b""), None);
    }

    #[test]
    fn reference_files_are_stored_by_bytes_and_delete_refuses_foreign_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));

        let stored = store_manual_reference_image(7, "png", &[0x89, b'P', b'N', b'G']).unwrap();
        assert!(stored.is_file());
        assert_eq!(
            stored.extension().and_then(|value| value.to_str()),
            Some("png")
        );
        assert!(stored.starts_with(character_references_dir().join("7")));

        remove_reference_image_file(&stored.to_string_lossy());
        assert!(!stored.exists());

        // A path outside the upload directory must survive: a stale or tampered
        // image_path cannot turn a reference delete into an arbitrary unlink.
        let foreign = dir.path().join("keep.txt");
        std::fs::write(&foreign, b"keep").unwrap();
        remove_reference_image_file(&foreign.to_string_lossy());
        assert!(foreign.is_file());
    }

    #[test]
    fn annotate_folder_updates_tags_date_and_recomputes_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER DEFAULT 0);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
                missing INTEGER DEFAULT 0, manual_date TEXT, detected_date TEXT, date TEXT,
                content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT ''
             );",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/media/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (1, 1, '/media/artist/2024-05 pack/a.jpg', 'a.jpg', '2024-05 pack', '2024-05-01', '2024-05-01')",
            [],
        )
        .unwrap();

        let payload = FolderAnnotatePayload {
            artist_id: 1,
            folder: "2024-05 pack".into(),
            tag_names: vec!["Frieren".into()],
            tag_ids: vec![],
            mode: "add".into(),
            manual_date: Some("2024-05".into()),
            keep_together: Some(true),
        };

        let result = annotate_folder_response(&conn, None, payload).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["updated_items"], 1);

        let plan = &result["plan"];
        assert_eq!(plan["source_folder"], "2024-05 pack");
        assert_eq!(plan["plan_kind"], "rename_folder");
        assert_eq!(plan["status"], "ready");
    }

    #[test]
    fn bundle_target_keeps_preexisting_empty_directories() {
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let existing = artist.join("already");
        std::fs::create_dir_all(&existing).unwrap();
        let target = prepare_bundle_target_dir(&artist, "already").unwrap();
        assert_eq!(target, existing);
        assert!(
            existing.is_dir(),
            "pre-existing empty directory must remain"
        );
    }

    #[test]
    fn bundle_target_rejects_file_components_without_removing_existing_paths() {
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(&artist).unwrap();
        let blocker = artist.join("blocker");
        std::fs::write(&blocker, b"not a directory").unwrap();
        let error = prepare_bundle_target_dir(&artist, "blocker/grandchild").unwrap_err();
        assert!(error.to_string().contains("directory"));
        assert!(
            !artist.join("new").exists(),
            "newly created parents must be rolled back"
        );
        assert!(blocker.is_file(), "pre-existing path must not be removed");
    }

    #[test]
    fn bundle_items_moves_files_and_updates_database() {
        let dir = tempfile::tempdir().unwrap();
        let artist_dir = dir.path().join("Artist");
        std::fs::create_dir_all(&artist_dir).unwrap();
        let file1 = artist_dir.join("loose1.jpg");
        let file2 = artist_dir.join("loose2.jpg");
        std::fs::write(&file1, b"pic1").unwrap();
        std::fs::write(&file2, b"pic2").unwrap();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER DEFAULT 0);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
                missing INTEGER DEFAULT 0, manual_date TEXT DEFAULT '', detected_date TEXT DEFAULT '', date TEXT DEFAULT '',
                content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT ''
             );",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', ?)",
            params![artist_dir.to_string_lossy()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
             VALUES (10, 1, ?, 'loose1.jpg', '', '2024-05-01', '2024-05-01'),
                    (11, 1, ?, 'loose2.jpg', '', '2024-05-01', '2024-05-01')",
            params![file1.to_string_lossy(), file2.to_string_lossy()],
        )
        .unwrap();

        let payload = BundleItemsPayload {
            artist_id: 1,
            item_ids: vec![10, 11],
            target_folder: "2024-05 [TestPack]".into(),
        };

        let result = bundle_items_response(&conn, None, payload).unwrap();
        assert_eq!(result["ok"], true);
        assert_eq!(result["moved_count"], 2);

        let target_dir = artist_dir.join("2024-05 [TestPack]");
        assert!(target_dir.join("loose1.jpg").is_file());
        assert!(target_dir.join("loose2.jpg").is_file());
        assert!(!file1.exists());
        assert!(!file2.exists());

        let folder: String = conn
            .query_row("SELECT folder_name FROM items WHERE id=10", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(folder, "2024-05 [TestPack]");
    }

    fn bundle_test_conn(artist_dir: &Path, items: &[(i64, &str)]) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER DEFAULT 0);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
                missing INTEGER DEFAULT 0, manual_date TEXT DEFAULT '', detected_date TEXT DEFAULT '', date TEXT DEFAULT '',
                content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT ''
             );",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', ?)",
            params![artist_dir.to_string_lossy()],
        )
        .unwrap();
        for (id, file_name) in items {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
                 VALUES (?, 1, ?, ?, '', '2024-05-01', '2024-05-01')",
                params![
                    id,
                    artist_dir.join(file_name).to_string_lossy(),
                    file_name
                ],
            )
            .unwrap();
        }
        conn
    }

    /// SQLite treats string literals verbatim, so the path normalization has to
    /// target a single backslash. With the old two-backslash needle Windows
    /// paths were never normalized and folder tag writes matched no rows.
    #[test]
    fn folder_item_ids_normalizes_windows_backslash_paths() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE items (
               id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT,
               media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
               missing INTEGER DEFAULT 0
             );
             INSERT INTO artists (id, path) VALUES (1, 'C:/root');
             INSERT INTO items (id, artist_id, file_path) VALUES (1, 1, 'C:/root/sub/a.jpg');
             INSERT INTO items (id, artist_id, file_path) VALUES (2, 1, 'C:\\root\\sub\\b.jpg');",
        )
        .unwrap();
        let ids = folder_item_ids(&conn, 1, "sub").unwrap();
        assert_eq!(ids, vec![1, 2], "single-backslash paths must normalize too");
    }

    #[test]
    fn bundle_rejects_invalid_targets_and_batch_shapes() {
        let dir = tempfile::tempdir().unwrap();
        let artist_dir = dir.path().join("Artist");
        std::fs::create_dir_all(&artist_dir).unwrap();
        std::fs::write(artist_dir.join("a.jpg"), b"pic").unwrap();
        let conn = bundle_test_conn(&artist_dir, &[(10, "a.jpg")]);

        for bad in [".", "a/./b", "a//b", "C:/x", "/abs", "..", ""] {
            let err = bundle_items_response(
                &conn,
                None,
                BundleItemsPayload {
                    artist_id: 1,
                    item_ids: vec![10],
                    target_folder: bad.into(),
                },
            )
            .expect_err("invalid target must be refused");
            assert!(
                err.to_string().contains("invalid target_folder path"),
                "{bad}: {err}"
            );
        }
        assert_eq!(
            std::fs::read_dir(&artist_dir).unwrap().count(),
            1,
            "a refused target must not create directories"
        );

        let cases: [(Vec<i64>, &str); 4] = [
            (vec![], "item_ids must not be empty"),
            (vec![0, -1], "item_ids must be positive"),
            (vec![10, 10], "item_ids must not contain duplicates"),
            (
                (1..=crate::MAX_BATCH_ITEM_LIMIT + 1).collect(),
                "too many item_ids",
            ),
        ];
        for (item_ids, expected) in cases {
            let err = bundle_items_response(
                &conn,
                None,
                BundleItemsPayload {
                    artist_id: 1,
                    item_ids,
                    target_folder: "pack".into(),
                },
            )
            .expect_err("invalid batch must be refused");
            assert!(err.to_string().contains(expected), "{expected}: {err}");
        }
    }

    /// A failure halfway through the batch must leave the filesystem and the
    /// rows in agreement: every file moved so far goes back where it started
    /// and the row transaction rolls back with it.
    #[test]
    fn bundle_rolls_back_moved_files_when_a_later_row_fails() {
        let dir = tempfile::tempdir().unwrap();
        let artist_dir = dir.path().join("Artist");
        std::fs::create_dir_all(&artist_dir).unwrap();
        let file1 = artist_dir.join("loose1.jpg");
        let file2 = artist_dir.join("loose2.jpg");
        std::fs::write(&file1, b"pic1").unwrap();
        std::fs::write(&file2, b"pic2").unwrap();
        let conn = bundle_test_conn(&artist_dir, &[(10, "loose1.jpg"), (11, "loose2.jpg")]);
        conn.execute_batch(
            "CREATE TRIGGER fail_item11 BEFORE UPDATE ON items WHEN NEW.id = 11
             BEGIN SELECT RAISE(ABORT, 'forced failure'); END;",
        )
        .unwrap();

        let err = bundle_items_response(
            &conn,
            None,
            BundleItemsPayload {
                artist_id: 1,
                item_ids: vec![10, 11],
                target_folder: "pack".into(),
            },
        )
        .expect_err("row failure must fail the batch");
        assert!(err.to_string().contains("forced failure"), "{err}");

        assert!(file1.is_file(), "first move must be rolled back");
        assert!(file2.is_file(), "second move must be rolled back");
        let target_dir = artist_dir.join("pack");
        assert!(
            !target_dir.join("loose1.jpg").exists() && !target_dir.join("loose2.jpg").exists(),
            "no file may stay in the target after a rollback"
        );
        assert_eq!(&std::fs::read(&file1).unwrap()[..], b"pic1");
        assert_eq!(&std::fs::read(&file2).unwrap()[..], b"pic2");
        for id in [10, 11] {
            let name: String = conn
                .query_row("SELECT file_name FROM items WHERE id=?", params![id], |r| {
                    r.get(0)
                })
                .unwrap();
            assert!(name.starts_with("loose"), "rows must be rolled back too");
        }
    }

    /// When every collision candidate name is taken the item is skipped and
    /// reported. Writing onto an existing file is never an option.
    #[test]
    fn bundle_skips_instead_of_overwriting_when_target_names_are_exhausted() {
        let dir = tempfile::tempdir().unwrap();
        let artist_dir = dir.path().join("Artist");
        std::fs::create_dir_all(&artist_dir).unwrap();
        let source = artist_dir.join("a.jpg");
        std::fs::write(&source, b"new-content").unwrap();
        let conn = bundle_test_conn(&artist_dir, &[(10, "a.jpg")]);

        let target_dir = artist_dir.join("pack");
        std::fs::create_dir_all(&target_dir).unwrap();
        std::fs::write(target_dir.join("a.jpg"), b"existing").unwrap();
        for idx in 1..1000 {
            std::fs::write(target_dir.join(format!("a ({idx}).jpg")), b"existing").unwrap();
        }

        let result = bundle_items_response(
            &conn,
            None,
            BundleItemsPayload {
                artist_id: 1,
                item_ids: vec![10],
                target_folder: "pack".into(),
            },
        )
        .unwrap();
        assert_eq!(result["moved_count"], 0);
        assert_eq!(result["skipped"][0]["item_id"], 10);
        assert_eq!(result["skipped"][0]["reason"], "target_name_exhausted");
        assert!(source.is_file(), "the source file stays put");
        assert_eq!(
            &std::fs::read(target_dir.join("a.jpg")).unwrap()[..],
            b"existing",
            "an occupied name must never be overwritten"
        );
    }

    /// Confirmed and executed plans have committed to their shape: the
    /// keep_together lock may not rewrite their snapshot through the upsert
    /// fallback either.
    #[test]
    fn annotate_keep_together_never_rewrites_locked_plans() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER DEFAULT 0);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                folder_name TEXT, media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0,
                missing INTEGER DEFAULT 0, manual_date TEXT, detected_date TEXT, date TEXT,
                content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT ''
             );",
        )
        .unwrap();
        crate::folder_archive::ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/media/artist')",
            [],
        )
        .unwrap();
        for (id, folder, status) in [
            (1, "c1", "confirmed"),
            (2, "e1", "executed"),
            (3, "d1", "draft"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, detected_date, date)
                 VALUES (?, 1, ?, 'a.jpg', ?, '2024-05-01', '2024-05-01')",
                params![id, format!("/media/artist/{folder}/a.jpg"), folder],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO folder_rename_plans (artist_id, source_folder, status, format_snapshot)
                 VALUES (1, ?, ?, '{}')",
                params![folder, status],
            )
            .unwrap();
        }

        for folder in ["c1", "e1", "d1"] {
            annotate_folder_response(
                &conn,
                None,
                FolderAnnotatePayload {
                    artist_id: 1,
                    folder: folder.into(),
                    tag_names: vec![],
                    tag_ids: vec![],
                    mode: "add".into(),
                    manual_date: None,
                    keep_together: Some(true),
                },
            )
            .unwrap();
        }

        for folder in ["c1", "e1"] {
            let snapshot: String = conn
                .query_row(
                    "SELECT format_snapshot FROM folder_rename_plans WHERE source_folder=?",
                    params![folder],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(snapshot, "{}", "{folder} must stay untouched");
        }
        let snapshot: String = conn
            .query_row(
                "SELECT format_snapshot FROM folder_rename_plans WHERE source_folder='d1'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let parsed = serde_json::from_str::<Value>(&snapshot).unwrap();
        assert_eq!(
            parsed["keep_together"],
            json!(true),
            "the draft plan is locked"
        );
    }
}
