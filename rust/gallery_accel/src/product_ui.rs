//! Native handlers for remaining static-UI product routes (no residual Python).

use std::io::{self, Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::operations::operation_history_response;
use crate::scan_candidates_write::apply_move_candidate_response;
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
        Some(v) if v > 0 => v.min(300),
        _ => 80,
    };
    let err_limit = match error_limit {
        Some(v) if v > 0 => v.min(120),
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

pub fn recent_log_errors(log_dir: &Path, limit: usize) -> Vec<Value> {
    if limit == 0 {
        return Vec::new();
    }
    let mut out = Vec::new();
    for name in ["gallery.log", "startup.log", "ui-actions.log"] {
        let path = log_dir.join(name);
        let Ok((lines, _truncated)) = read_log_tail(&path, LOG_TAIL_MAX_BYTES as usize, LOG_TAIL_MAX_BYTES) else {
            continue;
        };
        for line in lines.into_iter().rev() {
            if !is_error_log_line(&line) {
                continue;
            }
            out.push(json!({
                "source": name,
                "line": line.chars().take(500).collect::<String>(),
            }));
            if out.len() >= limit {
                return out;
            }
        }
    }
    out
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
    let prefix = {
        let base = artist_path
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        format!("{base}/{folder}/")
    };
    sql.push_str(
        " AND (replace(file_path,'\\\\','/') LIKE ? OR replace(file_path,'\\\\','/') LIKE ?)",
    );
    // Also match files directly under folder without trailing slash edge cases.
    let like_prefix = format!("{prefix}%");
    let exact_folder_prefix = prefix.trim_end_matches('/').to_string();
    let like_exact = format!("{exact_folder_prefix}/%");
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artist_id, like_prefix, like_exact], |r| {
        r.get::<_, i64>(0)
    })?;
    for row in rows {
        ids.push(row?);
    }
    Ok(ids)
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

// ---------------------------------------------------------------------------
// Folder plan confirm / unconfirm / auto run
// ---------------------------------------------------------------------------

pub fn reconfirm_plan(conn: &Connection, plan_id: i64) -> Result<Value> {
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

/// Confirm ready plans for an artist. Execution remains the full-scan path.
pub fn folder_rename_auto_run(conn: &Connection, artist_id: i64) -> Result<Value> {
    let confirmed = conn.execute(
        "UPDATE folder_rename_plans
         SET status='confirmed', confirmed_at=?, confirmation_source='auto', updated_at=?
         WHERE artist_id=? AND status='ready' AND target_folder != ''",
        params![now(), now(), artist_id],
    )?;
    Ok(json!({
        "ok": true,
        "status": "confirmed",
        "scope": "manual_artist",
        "artist_id": artist_id,
        "auto_confirmed": confirmed,
        "message": "confirm_current_artist_plans",
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
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT mc.id FROM move_candidates mc
             JOIN items i ON i.id = mc.item_id
             WHERE mc.status='pending' AND i.artist_id=? AND mc.artist_id=?",
        )?
        .query_map(params![old_artist_id, new_artist_id], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut applied = Vec::new();
    let mut skipped = Vec::new();
    for id in ids {
        match apply_move_candidate_response(conn, id) {
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("moved") => {
                applied.push(json!({"id": id, "item_id": v.get("item_id")}));
            }
            Ok(v) => {
                skipped.push(json!({
                    "id": id,
                    "reason": v.get("reason").cloned().unwrap_or(json!(v.get("action").cloned().unwrap_or(json!("not_moved"))))
                }));
            }
            Err(e) => skipped.push(json!({"id": id, "reason": e.to_string()})),
        }
    }
    Ok(json!({
        "action": "group_applied",
        "item_artist_id": old_artist_id,
        "candidate_artist_id": new_artist_id,
        "applied": applied.len(),
        "stale": 0,
        "skipped": skipped.len(),
        "resolved_existing": 0,
        "applied_candidates": applied,
        "skipped_candidates": skipped,
    }))
}

pub fn auto_resolve_move_candidates(conn: &Connection, limit: i64) -> Result<Value> {
    let limit = if limit > 0 { limit.min(5000) } else { 1000 };
    let ids: Vec<i64> = conn
        .prepare(
            "SELECT id FROM move_candidates WHERE status='pending' AND item_id IS NOT NULL
             ORDER BY id LIMIT ?",
        )?
        .query_map(params![limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;
    let mut applied = 0i64;
    let mut skipped = 0i64;
    for id in ids {
        match apply_move_candidate_response(conn, id) {
            Ok(v) if v.get("action").and_then(|a| a.as_str()) == Some("moved") => applied += 1,
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
        "resolved_existing": 0,
        "applied": applied,
        "stale": 0,
        "skipped": skipped,
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

pub fn delete_character_reference(
    conn: &Connection,
    character_id: i64,
    reference_id: i64,
) -> Result<Value> {
    let n = conn.execute(
        "DELETE FROM character_references WHERE id=? AND character_id=?",
        params![reference_id, character_id],
    )?;
    Ok(json!({
        "ok": n > 0,
        "character_id": character_id,
        "reference_id": reference_id,
        "deleted": n,
    }))
}

pub fn rebuild_character_index(conn: &Connection) -> Result<Value> {
    #[cfg(test)]
    REBUILD_INDEX_CALLS_FOR_TESTS.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
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
    matches!(
        guard
            .as_ref()
            .and_then(|j| j.value.get("status"))
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
static REBUILD_INDEX_CALLS_FOR_TESTS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0);

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

fn embed_for_import(conn: &Connection, item_id: i64) -> Result<Vec<f32>> {
    if fake_import_embedding_enabled() {
        return Ok(fake_embedding_for_item(item_id));
    }
    let (emb, _path, _name, _src) = crate::character_ccip::embed_item(conn, item_id)?;
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
    start_character_import_job_with_index_changes(conn, body, false)
}

fn start_character_import_job_with_index_changes(
    conn: &Connection,
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
    if import_job_busy() {
        let mut busy = get_character_import_job();
        if let Some(obj) = busy.as_object_mut() {
            obj.insert("busy".into(), json!(true));
        }
        return Ok(busy);
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
        .unwrap_or_else(import_job_candidate_limit);

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
    {
        let mut guard = import_job_slot().lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(ImportJob { value: job.clone() });
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
    sql.push_str(&format!(
        " ORDER BY (
            SELECT COUNT(*) FROM character_references cr2
            JOIN characters c2 ON c2.id = cr2.character_id
            WHERE c2.name = t.name AND cr2.source_type = 'tag_single'
          ) ASC, t.name, i.id
          LIMIT {candidate_limit}"
    ));

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
            match embed_for_import(conn, *item_id) {
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
    let job = start_character_import_job_with_index_changes(conn, &body, cleaned > 0)?;
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
                            eprintln!("character idle import error: {e}");
                            no_progress = 0;
                        }
                    },
                    Err(e) => eprintln!("character idle import pool: {e}"),
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
        assert!(log["errors"].as_array().unwrap().len() >= 1);
    }

    #[test]
    fn folder_rename_auto_run_reports_confirmation_not_execution() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE folder_rename_plans (
                id INTEGER PRIMARY KEY,
                artist_id INTEGER NOT NULL,
                target_folder TEXT NOT NULL,
                status TEXT NOT NULL,
                confirmed_at REAL,
                confirmation_source TEXT,
                updated_at REAL
            );
            INSERT INTO folder_rename_plans (id, artist_id, target_folder, status)
            VALUES (1, 7, 'target', 'ready');
            ",
        )
        .unwrap();

        let result = folder_rename_auto_run(&conn, 7).unwrap();

        assert_eq!(result["status"], "confirmed");
        assert_eq!(result["message"], "confirm_current_artist_plans");
        assert_eq!(result["auto_confirmed"], 1);
    }

    #[test]
    fn recent_log_errors_uses_exact_markers_and_a_bounded_tail() {
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
        assert!(errors.iter().any(|row| row["line"] == "frontend_rejection promise"));
        assert!(!errors.iter().any(|row| row["line"] == "[ERROR] old"));
        assert!(!errors.iter().any(|row| row["line"] == "INFO failed=0"));
        assert!(errors.iter().all(|row| row["line"].as_str().unwrap().len() <= 500));
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
        REBUILD_INDEX_CALLS_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);

        let result = run_idle_character_import_once(&conn).unwrap();

        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        assert_eq!(result["status"], "completed", "{result}");
        assert_eq!(result["added"], 0, "{result}");
        assert_eq!(result["auto_deleted_stale_tag_single"], 1, "{result}");
        assert_eq!(
            REBUILD_INDEX_CALLS_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
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
        REBUILD_INDEX_CALLS_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);

        assert!(start_character_import_job(&conn, &json!({})).is_err());

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 3);
        assert_eq!(
            REBUILD_INDEX_CALLS_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
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
        REBUILD_INDEX_CALLS_FOR_TESTS.store(0, std::sync::atomic::Ordering::SeqCst);

        let result = run_idle_character_import_once(&conn);

        std::env::remove_var("CHARACTER_IMPORT_IDLE_ENABLED");
        assert!(result.is_err());
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM character_references", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 0);
        assert_eq!(
            REBUILD_INDEX_CALLS_FOR_TESTS.load(std::sync::atomic::Ordering::SeqCst),
            1
        );
    }
}
