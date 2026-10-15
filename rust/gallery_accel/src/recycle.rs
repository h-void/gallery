use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::{normalize_slashes, path_under_authorized_roots, MediaRoots};
use crate::media_serve::{
    candidate_recycle_targets, gallery_recycle_dir, move_file_from_authorized_path_no_overwrite,
    move_file_to_authorized_path_no_overwrite, recycle_name_matches_base,
    recycle_source_is_trusted,
};

const ITEM_COLUMNS: &[&str] = &[
    "id",
    "artist_id",
    "file_path",
    "file_name",
    "file_size",
    "file_mtime",
    "folder_name",
    "date",
    "detected_date",
    "manual_date",
    "auto_role",
    "manual_role",
    "tags",
    "is_archive",
    "media_type",
    "content_hash",
    "hash_status",
    "hash_updated_at",
    "st_dev",
    "st_ino",
    "missing",
    "missing_at",
    "scanned_at",
    "width",
    "height",
];

/// Finalize or drop recycle rows left in the pre-commit `'moving'` state by an
/// interrupted delete (crash or power loss between the filesystem move and the
/// database commit). Called once at writable startup; per-row failures never
/// abort startup. Returns (finalized, dropped, marked_missing).
pub fn reconcile_moving_recycle_entries(conn: &Connection) -> (usize, usize, usize) {
    let rows: Vec<(i64, i64, String, String)> = match conn
        .prepare(
            "SELECT id, original_item_id, original_path, recycled_path
             FROM recycle_entries WHERE status='moving'",
        )
        .and_then(|mut stmt| {
            stmt.query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()
        }) {
        Ok(rows) => rows,
        Err(error) => {
            // A missing recycle table is normal for pre-recycle databases;
            // any other query failure must surface so interrupted 'moving'
            // rows are never silently skipped.
            if !error.to_string().contains("no such table") {
                log_error!("recycle reconciliation: failed to list 'moving' rows: {error}");
            }
            return (0, 0, 0);
        }
    };
    let mut finalized = 0;
    let mut dropped = 0;
    let mut missing = 0;

    // Finish the delete the interrupted transaction would have committed.
    let finalize = |id: i64, item_id: i64, note: &str| -> bool {
        (|| -> Result<()> {
            let tx = conn.unchecked_transaction()?;
            tx.execute(
                "DELETE FROM character_references WHERE item_id=? AND source_type='tag_single'",
                params![item_id],
            )?;
            tx.execute("DELETE FROM items WHERE id=?", params![item_id])?;
            tx.execute(
                "UPDATE recycle_entries SET status='recycled', last_error=?
                 WHERE id=? AND status='moving'",
                params![note, id],
            )?;
            tx.commit()?;
            Ok(())
        })()
        .is_ok()
    };

    for (id, item_id, original, recycled) in rows {
        if !recycled.is_empty()
            && (Path::new(&recycled).is_file()
                || crate::media_serve::is_fnos_system_trash_path(Path::new(&recycled)))
        {
            // The file reached recycle storage before the crash, and the row
            // names it; finish the delete.
            if finalize(id, item_id, "finalized after interrupted delete") {
                finalized += 1;
            }
            continue;
        }
        if Path::new(&original).is_file() {
            // Crash before the file moved: nothing happened on disk, and the
            // item row is still active — drop the stale marker entirely. The
            // delete never reached the move, so a same-named file already
            // sitting in recycle storage belongs to some earlier delete and
            // must not be claimed here.
            if conn
                .execute(
                    "DELETE FROM recycle_entries WHERE id=? AND status='moving'",
                    params![id],
                )
                .is_ok()
            {
                dropped += 1;
            }
            continue;
        }
        // The original is gone and the row names no reachable file: the move
        // returned and the process died before the destination was recorded,
        // or the row predates that early write. Look for the copy the move
        // would have left before declaring the bytes lost.
        match find_recycled_file_for(Path::new(&original)) {
            Some(found) => {
                let found_text = found.to_string_lossy().to_string();
                let recorded = conn
                    .execute(
                        "UPDATE recycle_entries SET recycled_path=? WHERE id=? AND status='moving'",
                        params![found_text, id],
                    )
                    .is_ok();
                if recorded
                    && finalize(
                        id,
                        item_id,
                        "finalized after interrupted delete; destination recovered from recycle storage",
                    )
                {
                    finalized += 1;
                }
            }
            None => {
                if conn
                    .execute(
                        "UPDATE recycle_entries SET status='recycled',
                            last_error='interrupted delete: file missing from original and recycle locations'
                         WHERE id=? AND status='moving'",
                        params![id],
                    )
                    .is_ok()
                {
                    missing += 1;
                }
            }
        }
    }
    (finalized, dropped, missing)
}

/// Find the file an interrupted delete left behind when its row never learned
/// the destination: the move had already returned when the process died, so the
/// bytes are in one of the locations [`crate::media_serve`] could have chosen.
///
/// A name is only accepted if [`recycle_name_matches_base`] says the mover can
/// produce it for this original, and a second candidate makes the answer
/// ambiguous — claiming either could restore the wrong bytes later — so the
/// caller keeps the row marked missing instead of guessing.
fn find_recycled_file_for(original: &Path) -> Option<PathBuf> {
    let base = original.file_name()?.to_string_lossy().to_string();
    let mut matches: Vec<PathBuf> = Vec::new();

    // 1. Check candidate volume/space targets for this original.
    //
    // The media roots come from the same environment the mover read
    // (`PICTURES_ROOT` / `PICTURES_ROOT_REAL_PATHS`, defaulting to `/pictures`),
    // so this scan covers the media-root-local stores `.Recycle_bin`, `#recycle`,
    // `.recycle` and `.gallery_recycle` that `move_into_recycle` prefers. Passing
    // `None` here — which is what this call did — skipped that whole section, so
    // an interrupted delete into one of those stores found no candidate, recorded
    // `recycled_path=''` and reported the bytes as lost while they were still on
    // disk in the store the mover itself chose.
    let roots = crate::media_roots::env_media_roots();
    for (trash, rel) in candidate_recycle_targets(original, Some(&roots)) {
        if !trash.exists() {
            continue;
        }
        let nested = trash.join(&rel);
        let mut dirs = vec![trash.clone()];
        if let Some(parent) = nested.parent() {
            if parent != trash && parent.exists() {
                dirs.push(parent.to_path_buf());
            }
        }
        for dir in dirs {
            let Ok(entries) = std::fs::read_dir(&dir) else {
                continue;
            };
            for entry in entries.flatten() {
                if entry.file_type().is_ok_and(|kind| kind.is_file())
                    && recycle_name_matches_base(&base, &entry.file_name().to_string_lossy())
                    && !matches.contains(&entry.path())
                {
                    matches.push(entry.path());
                }
            }
        }
    }

    // 2. The gallery-owned store DATA_DIR/recycle
    let mut stack = vec![gallery_recycle_dir()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let Ok(kind) = entry.file_type() else {
                continue;
            };
            let path = entry.path();
            if kind.is_dir() {
                stack.push(path);
            } else if kind.is_file()
                && recycle_name_matches_base(&base, &entry.file_name().to_string_lossy())
                && !matches.contains(&path)
            {
                matches.push(path);
            }
        }
    }

    if matches.len() == 1 {
        matches.pop()
    } else {
        None
    }
}

pub fn ensure_recycle_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS recycle_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            original_item_id INTEGER NOT NULL,
            artist_id INTEGER NOT NULL,
            original_path TEXT NOT NULL,
            recycled_path TEXT NOT NULL,
            item_snapshot TEXT NOT NULL,
            tag_ids_snapshot TEXT NOT NULL DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'recycled',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            restored_at REAL,
            restore_path TEXT NOT NULL DEFAULT '',
            last_error TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_recycle_entries_status_created ON recycle_entries(status, created_at DESC, id DESC);",
    )?;
    for (name, definition) in [
        ("tag_single_refs_snapshot", "TEXT NOT NULL DEFAULT '[]'"),
        ("non_tag_single_ref_ids", "TEXT NOT NULL DEFAULT '[]'"),
    ] {
        let present = conn
            .prepare("PRAGMA table_info(recycle_entries)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .iter()
            .any(|column| column == name);
        if !present {
            conn.execute(
                &format!("ALTER TABLE recycle_entries ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );",
    )?;
    Ok(())
}

pub fn get_recycle_target_setting(conn: &Connection) -> String {
    let _ = ensure_recycle_schema(conn);
    conn.query_row(
        "SELECT value FROM app_settings WHERE key = 'recycle_target'",
        [],
        |row| row.get::<_, String>(0),
    )
    .unwrap_or_else(|_| "system".to_string())
}

pub fn set_recycle_target_setting(conn: &Connection, target: &str) -> Result<String> {
    let _ = ensure_recycle_schema(conn);
    let val = if target == "app" { "app" } else { "system" };
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('recycle_target', ?1, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
        [val],
    )?;
    Ok(val.to_string())
}

pub fn capture_item_snapshot(conn: &Connection, item_id: i64) -> Result<(Value, Vec<i64>, bool)> {
    let available = conn
        .prepare("PRAGMA table_info(items)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<std::collections::HashSet<_>>>()?;
    let selected = ITEM_COLUMNS
        .iter()
        .copied()
        .filter(|column| available.contains(*column))
        .collect::<Vec<_>>();
    let sql = format!("SELECT {} FROM items WHERE id=?", selected.join(","));
    let mut stmt = conn.prepare(&sql)?;
    let snapshot = stmt.query_row([item_id], |row| {
        let mut value = serde_json::Map::new();
        for (index, column) in selected.iter().enumerate() {
            let raw: rusqlite::types::Value = row.get(index)?;
            value.insert(
                (*column).to_string(),
                match raw {
                    rusqlite::types::Value::Null => Value::Null,
                    rusqlite::types::Value::Integer(v) => json!(v),
                    rusqlite::types::Value::Real(v) => json!(v),
                    rusqlite::types::Value::Text(v) => json!(v),
                    rusqlite::types::Value::Blob(v) => json!(v),
                },
            );
        }
        for column in ITEM_COLUMNS
            .iter()
            .filter(|column| !available.contains(**column))
        {
            value.insert((*column).to_string(), Value::Null);
        }
        Ok(Value::Object(value))
    })?;
    let tags = conn
        .prepare("SELECT tag_id FROM item_tags WHERE item_id=? ORDER BY tag_id")?
        .query_map([item_id], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<i64>>>()?;
    let favorite: i64 = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM item_favorites WHERE item_id=?)",
            [item_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    Ok((snapshot, tags, favorite != 0))
}

pub fn recycle_entries_response(
    conn: &Connection,
    roots: &MediaRoots,
    status: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Value> {
    let status = status.unwrap_or("recycled");
    if !matches!(status, "recycled" | "restored") {
        return Err(anyhow!("invalid recycle status"));
    }
    let table_exists: bool = conn.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='recycle_entries')", [], |row| row.get(0)).unwrap_or(false);
    if !table_exists {
        return Ok(json!({"entries": [], "total": 0, "next_offset": Value::Null}));
    }
    let columns = conn
        .prepare("PRAGMA table_info(recycle_entries)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !columns
        .iter()
        .any(|column| column == "tag_single_refs_snapshot")
        || !columns
            .iter()
            .any(|column| column == "non_tag_single_ref_ids")
    {
        return Ok(json!({"entries": [], "total": 0, "next_offset": Value::Null}));
    }
    let limit = limit
        .unwrap_or(crate::DEFAULT_PREVIEW_RECYCLE_LIMIT)
        .clamp(1, crate::MAX_PREVIEW_RECYCLE_LIMIT);
    let offset = offset.unwrap_or(0).max(0);
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM recycle_entries WHERE status=?",
        [status],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare("SELECT id, original_path, recycled_path, item_snapshot, status, created_at, last_error FROM recycle_entries WHERE status=? ORDER BY created_at DESC, id DESC LIMIT ? OFFSET ?")?;
    let entries = stmt
        .query_map(params![status, limit, offset], |row| {
            let id: i64 = row.get(0)?;
            let original: String = row.get(1)?;
            let recycled: String = row.get(2)?;
            let snapshot: String = row.get(3)?;
            let status: String = row.get(4)?;
            let created_at: f64 = row.get(5)?;
            let last_error: String = row.get(6)?;
            let target = roots.map_to_real(&original).ok();
            let snapshot_val = serde_json::from_str::<Value>(&snapshot).unwrap_or(Value::Null);
            let file_name = snapshot_val
                .get("file_name")
                .and_then(Value::as_str)
                .map(str::to_owned)
                .unwrap_or_default();
            let file_size = snapshot_val
                .get("file_size")
                .and_then(Value::as_i64)
                .unwrap_or(0);
            // Absent from the snapshot only for legacy rows: leave it null so the
            // frontend falls back to the file extension instead of the backend
            // claiming every unknown entry is an image (videos would then request
            // an image preview and get a 400 placeholder).
            let media_type = snapshot_val
                .get("media_type")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let recycled_p = Path::new(&recycled);
            let is_sys_trash = crate::media_serve::is_fnos_system_trash_path(recycled_p);
            let recycled_exists = if is_sys_trash {
                recycled_p.exists()
            } else {
                recycled_p.is_file()
                    && recycle_source_is_trusted(recycled_p, Path::new(&original), Some(roots))
            };
            Ok(json!({
                "id": id, "status": status, "original_path": original, "recycled_path": recycled,
                "file_name": file_name,
                "file_size": file_size,
                "media_type": media_type,
                "system_trash": is_sys_trash,
                "created_at": created_at,
                "last_error": last_error,
                "recycled_file_exists": recycled_exists,
                "original_file_exists": target.map(|p| p.exists()).unwrap_or(false),
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next = (offset + (entries.len() as i64) < total).then_some(offset + entries.len() as i64);
    Ok(json!({"entries": entries, "total": total, "next_offset": next}))
}

/// Whether the row a restore is about to reference still exists.
///
/// Checked before the insert rather than inferred from its failure: `INSERT OR
/// IGNORE` never reports a foreign-key violation (it ignores the row silently),
/// so a deleted tag would disappear from the restored item without a trace.
fn reference_target_exists(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<bool> {
    Ok(conn
        .query_row(sql, params, |_| Ok(()))
        .optional()?
        .is_some())
}

/// Insert a relationship the snapshot recorded, treating "it is already there"
/// as success and everything else as a failure the caller must see.
///
/// Only the two "this row is already present" codes are accepted. The
/// `rusqlite` portable view collapses every constraint failure into
/// `ConstraintViolation`, and treating that whole class as success is how a
/// discarded error turns into a silently incomplete restore — the exact shape
/// this helper exists to remove — so the extended code decides.
fn insert_restored_reference(
    conn: &Connection,
    sql: &str,
    params: impl rusqlite::Params,
) -> Result<()> {
    match conn.execute(sql, params) {
        Ok(_) => Ok(()),
        Err(rusqlite::Error::SqliteFailure(inner, _))
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY
                || inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            Ok(())
        }
        Err(other) => Err(other.into()),
    }
}

fn restore_tag_single_refs(conn: &Connection, item_id: i64, raw: &str) -> Result<i64> {
    let refs: Vec<Value> = serde_json::from_str(raw).unwrap_or_default();
    let mut restored = 0;
    for reference in refs {
        let character_id = reference
            .get("character_id")
            .and_then(Value::as_i64)
            .unwrap_or(0);
        if character_id <= 0
            || conn.query_row(
                "SELECT EXISTS(SELECT 1 FROM characters WHERE id=?)",
                [character_id],
                |row| row.get::<_, i64>(0),
            )? == 0
        {
            continue;
        }
        let embedding = reference
            .get("embedding")
            .and_then(Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(Value::as_u64)
                    .map(|value| value as u8)
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        if embedding.is_empty() {
            continue;
        }
        conn.execute(
            "INSERT INTO character_references
             (character_id, embedding, embedding_dim, embedding_model_repo_id,
              embedding_model_variant, embedding_model_file, embedding_updated_at,
              source_type, item_id, created_at)
             VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                character_id,
                embedding,
                reference["embedding_dim"].as_i64().unwrap_or(0),
                reference["embedding_model_repo_id"]
                    .as_str()
                    .unwrap_or_default(),
                reference["embedding_model_variant"]
                    .as_str()
                    .unwrap_or_default(),
                reference["embedding_model_file"]
                    .as_str()
                    .unwrap_or_default(),
                reference["embedding_updated_at"].as_f64(),
                "tag_single",
                item_id,
                reference["created_at"].as_f64().unwrap_or(0.0),
            ],
        )?;
        restored += 1;
    }
    Ok(restored)
}

fn reattach_non_tag_refs(conn: &Connection, item_id: i64, raw: &str) -> Result<i64> {
    let ids: Vec<i64> = serde_json::from_str(raw).unwrap_or_default();
    let mut restored = 0;
    for id in ids {
        restored += conn.execute(
            "UPDATE character_references SET item_id=? WHERE id=? AND item_id IS NULL",
            params![item_id, id],
        )? as i64;
    }
    Ok(restored)
}

pub fn restore_recycle_entry(
    conn: &Connection,
    roots: &MediaRoots,
    entry_id: i64,
) -> Result<Value, (StatusCode, Value)> {
    let record = conn.query_row("SELECT original_item_id, artist_id, original_path, recycled_path, item_snapshot, tag_ids_snapshot, tag_single_refs_snapshot, non_tag_single_ref_ids, status FROM recycle_entries WHERE id=?", [entry_id], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?, row.get::<_, String>(2)?, row.get::<_, String>(3)?, row.get::<_, String>(4)?, row.get::<_, String>(5)?, row.get::<_, String>(6)?, row.get::<_, String>(7)?, row.get::<_, String>(8)?))).optional().map_err(internal)?;
    let Some((
        item_id,
        artist_id,
        original,
        recycled,
        snapshot_raw,
        tags_raw,
        tag_refs_raw,
        non_tag_refs_raw,
        status,
    )) = record
    else {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error":"recycle entry not found"}),
        ));
    };
    if status != "recycled" {
        return Err(conflict("recycle entry is no longer recoverable"));
    }
    let recycled_path = PathBuf::from(&recycled);
    if crate::media_serve::is_fnos_system_trash_path(&recycled_path) {
        return Err(conflict(
            "item is in fnOS system trash; please restore from system trash on desktop",
        ));
    }
    let target = roots
        .map_to_real(&original)
        .map_err(|e| conflict(e.to_string()))?;
    if !path_under_authorized_roots(&target, roots) {
        return Err(conflict("restore path is outside configured media roots"));
    }
    if target.exists() {
        return Err(conflict("original path is already occupied"));
    }
    if !recycled_path.is_file()
        || !recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
    {
        return Err(conflict("recycled file is missing or untrusted"));
    }
    let snapshot: Value = serde_json::from_str(&snapshot_raw).map_err(internal)?;
    let snapshot_path = snapshot
        .get("file_path")
        .and_then(Value::as_str)
        .ok_or_else(|| conflict("recycle item snapshot has no file path"))?;
    let snapshot_target = roots
        .map_to_real(snapshot_path)
        .map_err(|error| conflict(error.to_string()))?;
    if snapshot.get("id").and_then(Value::as_i64) != Some(item_id)
        || snapshot.get("artist_id").and_then(Value::as_i64) != Some(artist_id)
        || snapshot_target != target
    {
        return Err(conflict("recycle item snapshot does not match its record"));
    }
    let tags: Vec<i64> = serde_json::from_str(&tags_raw).unwrap_or_default();
    // The restored row stores the resolved real authorized path, never a
    // legacy virtual alias: `original_path` above stays as historical audit
    // data, and the snapshot path mapping was already validated.
    let restored_path = normalize_slashes(&target.to_string_lossy());
    let active_conflict: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items WHERE id=? OR file_path=? OR file_path=?",
            params![item_id, snapshot_path, restored_path],
            |row| row.get(0),
        )
        .map_err(internal)?;
    if active_conflict > 0 {
        return Err(conflict("an active library item already uses this record"));
    }
    move_file_to_authorized_path_no_overwrite(&recycled_path, &target, roots).map_err(internal)?;
    let restored = (|| -> Result<()> {
        let tx = conn.unchecked_transaction()?;
        let columns = ITEM_COLUMNS.join(",");
        let placeholders = std::iter::repeat_n("?", ITEM_COLUMNS.len())
            .collect::<Vec<_>>()
            .join(",");
        let values = ITEM_COLUMNS
            .iter()
            .map(|column| snapshot.get(*column).cloned().unwrap_or(Value::Null))
            .collect::<Vec<_>>();
        let mut sql_values = Vec::with_capacity(values.len());
        for (index, value) in values.into_iter().enumerate() {
            if ITEM_COLUMNS[index] == "file_path" {
                sql_values.push(rusqlite::types::Value::Text(restored_path.clone()));
                continue;
            }
            sql_values.push(match value {
                Value::Null => rusqlite::types::Value::Null,
                Value::Bool(v) => rusqlite::types::Value::Integer(v as i64),
                Value::Number(n) => match n.as_i64() {
                    Some(i) => rusqlite::types::Value::Integer(i),
                    None => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
                },
                Value::String(v) => rusqlite::types::Value::Text(v),
                _ => rusqlite::types::Value::Null,
            });
        }
        tx.execute(
            &format!("INSERT INTO items ({columns}) VALUES ({placeholders})"),
            rusqlite::params_from_iter(sql_values.iter()),
        )?;
        let new_id = tx.last_insert_rowid();
        // Restoring the row is not the same as restoring the item. These two
        // inserts used to be `let _ =`, so a failure was invisible: the
        // transaction still committed, the route still answered `ok:true`, and
        // the item came back without its tags or its favorite.
        //
        // The reference is checked instead of relying on `INSERT OR IGNORE`,
        // because OR IGNORE suppresses foreign-key violations too — a tag
        // deleted while the item sat in the recycle bin would vanish without a
        // trace, which is the same silent loss by another route. A reference
        // whose tag is gone is skipped deliberately, and everything else (a
        // uniqueness conflict from the snapshot, a NOT NULL fault, a full disk)
        // aborts the restore.
        for tag_id in tags {
            if !reference_target_exists(&tx, "SELECT 1 FROM tags WHERE id=?1", params![tag_id])? {
                continue;
            }
            insert_restored_reference(
                &tx,
                "INSERT INTO item_tags (item_id, tag_id) VALUES (?1, ?2)",
                params![new_id, tag_id],
            )?;
        }
        restore_tag_single_refs(&tx, new_id, &tag_refs_raw)?;
        reattach_non_tag_refs(&tx, new_id, &non_tag_refs_raw)?;
        if snapshot
            .get("favorite")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            insert_restored_reference(
                &tx,
                "INSERT INTO item_favorites (item_id) VALUES (?1)",
                [new_id],
            )?;
        }
        if tx.execute("UPDATE recycle_entries SET status='restored', restored_at=strftime('%s','now'), restore_path=?, last_error='' WHERE id=? AND status='recycled'", params![target.to_string_lossy().to_string(), entry_id])? != 1 {
            return Err(anyhow!("restore conflict: recycle entry changed during restore"));
        }
        tx.commit()?;
        Ok(())
    })();
    match restored {
        Ok(()) => Ok(
            json!({"ok":true,"id":entry_id,"item_id":item_id,"restored_to":target.to_string_lossy()}),
        ),
        Err(error) => {
            match move_file_from_authorized_path_no_overwrite(&target, &recycled_path, roots) {
                Ok(()) => {
                    let message =
                        format!("database restore failed; file returned to recycle: {error}");
                    let _ = conn.execute(
                        "UPDATE recycle_entries SET last_error=? WHERE id=? AND status='recycled'",
                        params![message, entry_id],
                    );
                    Err(internal(message))
                }
                Err(rollback_error) => {
                    let message = format!(
                    "database restore failed and file rollback failed: db={error}; rollback={rollback_error}"
                );
                    let recorded = conn
                    .execute(
                        "UPDATE recycle_entries SET last_error=? WHERE id=? AND status='recycled'",
                        params![message, entry_id],
                    )
                    .is_ok();
                    Err((
                        StatusCode::INTERNAL_SERVER_ERROR,
                        json!({
                            "error": message,
                            "needs_reconciliation": true,
                            "recycle_entry_id": entry_id,
                            "reconciliation_recorded": recorded,
                        }),
                    ))
                }
            }
        }
    }
}

/// True when `dir` is a recycle store root: the directory entries are written
/// into, as opposed to a subdirectory created for a nested entry path.
fn is_recycle_store_root(dir: &Path) -> bool {
    if dir == gallery_recycle_dir().as_path() {
        return true;
    }
    if crate::media_serve::is_fnos_trash_namespace(dir) {
        return true;
    }
    dir.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| {
            matches!(
                name,
                ".Recycle_bin" | ".recycle" | ".gallery_recycle" | "#recycle"
            )
        })
}

/// Remove the now-empty parent of a purged recycle file, unless that parent is a
/// recycle store root: emptying the last entry must not delete the store the
/// next delete writes into.
fn remove_empty_recycle_parent(file: &Path) {
    let Some(parent) = file.parent() else {
        return;
    };
    if is_recycle_store_root(parent) {
        return;
    }
    let _ = std::fs::remove_dir(parent);
}

/// Best effort: drop the empty mirrored directories a fnOS system-trash delete
/// leaves behind, walking up from the entry's parent and stopping at the
/// reserved trash root.
///
/// `remove_dir` only succeeds on an empty directory, so a directory that still
/// holds trashed entries — or one the service user cannot write to, which is the
/// common case inside `.@#local/trash` — simply ends the walk. Nothing at or
/// above the trash root is ever touched, and a failure is not an error: the
/// desktop trash owns that namespace and may prune it itself later.
fn prune_empty_system_trash_parents(recycled: &Path) {
    let Some(mut current) = recycled.parent() else {
        return;
    };
    while !crate::media_serve::is_fnos_trash_namespace(current) {
        if std::fs::remove_dir(current).is_err() {
            return;
        }
        match current.parent() {
            Some(parent) => current = parent,
            None => return,
        }
    }
}

/// Remove a recycle record, deleting its file when the record owns one.
///
/// `drop_record_only` is the exit for entries whose file lives in the fnOS
/// system trash: that file is managed by the OS trash daemon, so the record can
/// only be dropped without touching the file. It is refused for an app-managed
/// entry, where dropping the record would orphan a file nothing points at.
pub fn purge_recycle_entry(
    conn: &Connection,
    roots: &MediaRoots,
    entry_id: i64,
    drop_record_only: bool,
) -> Result<Value, (StatusCode, Value)> {
    let record = conn
        .query_row(
            "SELECT original_path, recycled_path, status FROM recycle_entries WHERE id=?",
            [entry_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()
        .map_err(internal)?;

    let Some((original, recycled, status)) = record else {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "recycle entry not found"}),
        ));
    };
    // A 'moving' row belongs to a delete that is still in flight. Removing it
    // makes that delete fail after its file already moved, which leaves the item
    // active in the library while the file has no record left.
    if status == "moving" {
        return Err(conflict("a delete is still in progress for this entry"));
    }

    let recycled_path = PathBuf::from(&recycled);
    let is_sys_trash = crate::media_serve::is_fnos_system_trash_path(&recycled_path);
    if drop_record_only && !is_sys_trash {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "record-only removal applies to fnOS system-trash entries only"}),
        ));
    }

    let mut file_removed = false;
    let mut prune_sys_parents = false;
    if !drop_record_only {
        if is_sys_trash {
            // If the file is still present in fnOS system trash, it is managed by the OS trash daemon.
            // Direct removal would bypass system xattr tracking and fail with permission denied on real NAS.
            if recycled_path.exists() {
                return Err(conflict(
                    "item is in fnOS system trash; please empty from system trash on desktop",
                ));
            }
            // The entry's file is already gone (emptied from the desktop trash),
            // so the mirrored directory chain it needed is usually empty now.
            prune_sys_parents = true;
        } else if recycled_path.is_file()
            && recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
        {
            match std::fs::remove_file(&recycled_path) {
                Ok(()) => {
                    file_removed = true;
                    remove_empty_recycle_parent(&recycled_path);
                }
                // Keeping the record is what keeps the file reachable: dropping
                // it here would hide a real file from the UI for good.
                Err(error) => {
                    return Err(internal(anyhow!(
                        "could not remove '{}': {error}; the recycle record was kept",
                        recycled_path.display()
                    )));
                }
            }
        }
    }

    conn.execute("DELETE FROM recycle_entries WHERE id=?", [entry_id])
        .map_err(internal)?;
    if prune_sys_parents {
        prune_empty_system_trash_parents(&recycled_path);
    }

    Ok(json!({
        "ok": true,
        "id": entry_id,
        "file_removed": file_removed,
        "record_only": drop_record_only,
        "message": if drop_record_only { "record_dropped" } else { "purged_from_recycle" }
    }))
}

pub fn clear_recycle_entries(
    conn: &Connection,
    roots: &MediaRoots,
    status: Option<&str>,
) -> Result<Value, (StatusCode, Value)> {
    let status_filter = status.unwrap_or("recycled");
    if !matches!(status_filter, "recycled" | "all") {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid status filter"}),
        ));
    }

    let mut stmt = if status_filter == "all" {
        // A 'moving' row belongs to a delete that is still in flight; deleting
        // it here would make that delete fail after its file already moved.
        conn.prepare(
            "SELECT id, original_path, recycled_path FROM recycle_entries WHERE status != 'moving'",
        )
        .map_err(internal)?
    } else {
        conn.prepare(
            "SELECT id, original_path, recycled_path FROM recycle_entries WHERE status='recycled'",
        )
        .map_err(internal)?
    };

    let rows: Vec<(i64, String, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .map_err(internal)?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(internal)?;

    let mut cleared_count = 0;
    let mut files_removed = 0;
    let mut system_kept = 0;
    let mut failed = 0;
    let mut failed_paths: Vec<String> = Vec::new();
    for (id, original, recycled) in rows {
        let recycled_path = PathBuf::from(&recycled);
        let is_sys_trash = crate::media_serve::is_fnos_system_trash_path(&recycled_path);
        let mut prune_sys_parents = false;
        if is_sys_trash {
            if recycled_path.exists() {
                // The file is managed by the fnOS trash daemon and only the
                // desktop trash can empty it. The record is what keeps the entry
                // visible (and restorable there), so it is not ours to drop.
                system_kept += 1;
                continue;
            }
            // Its file is already gone, so the mirrored directory chain it
            // needed is usually empty now.
            prune_sys_parents = true;
        } else if recycled_path.is_file()
            && recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
        {
            match std::fs::remove_file(&recycled_path) {
                Ok(()) => {
                    files_removed += 1;
                    remove_empty_recycle_parent(&recycled_path);
                }
                // Dropping the record on a failed removal would hide a real file
                // from the UI for good, so the row stays and the caller sees it.
                Err(error) => {
                    failed += 1;
                    if failed_paths.len() < 5 {
                        failed_paths.push(format!("{}: {error}", recycled_path.display()));
                    }
                    continue;
                }
            }
        }
        if conn
            .execute("DELETE FROM recycle_entries WHERE id=?", [id])
            .is_ok()
        {
            cleared_count += 1;
            if prune_sys_parents {
                prune_empty_system_trash_parents(&recycled_path);
            }
        }
    }

    Ok(json!({
        "ok": true,
        "cleared_count": cleared_count,
        "files_removed": files_removed,
        "system_kept": system_kept,
        "failed": failed,
        "failed_paths": failed_paths,
        "message": "cleared_from_recycle"
    }))
}

pub fn recycle_entry_preview(
    conn: &Connection,
    roots: &MediaRoots,
    entry_id: i64,
    max_edge: Option<u32>,
) -> Result<Vec<u8>, (StatusCode, Value)> {
    let record: Option<(String, String, String)> = conn
        .query_row(
            "SELECT recycled_path, original_path, status FROM recycle_entries WHERE id=?",
            [entry_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(internal)?;
    let Some((path, original, status)) = record else {
        return Err((StatusCode::NOT_FOUND, json!({"error": "entry not found"})));
    };
    // Mirror purge/restore: only a live recycle record is previewable, and its
    // file must be a trusted recycle-storage target rather than an arbitrary
    // path a tampered row could point at.
    if status != "recycled" {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "recycle entry is not previewable"}),
        ));
    }
    let recycled_path = PathBuf::from(&path);
    if !recycled_path.is_file()
        || !recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
    {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "recycled file not found"}),
        ));
    }
    let edge = crate::image_preview::clamp_max_edge(max_edge);
    crate::image_preview::image_preview_bytes(&path, edge)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))
}

fn conflict(message: impl Into<String>) -> (StatusCode, Value) {
    (StatusCode::CONFLICT, json!({"error": message.into()}))
}
fn internal(error: impl std::fmt::Display) -> (StatusCode, Value) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": error.to_string()}),
    )
}
