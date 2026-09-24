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
        if !recycled.is_empty() && Path::new(&recycled).is_file() {
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

    // 1. Check candidate volume/space targets for this original
    for (trash, rel) in candidate_recycle_targets(original, None) {
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
                {
                    if !matches.contains(&entry.path()) {
                        matches.push(entry.path());
                    }
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
            {
                if !matches.contains(&path) {
                    matches.push(path);
                }
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
    Ok(())
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
    let entries = stmt.query_map(params![status, limit, offset], |row| {
        let id: i64 = row.get(0)?;
        let original: String = row.get(1)?;
        let recycled: String = row.get(2)?;
        let snapshot: String = row.get(3)?;
        let status: String = row.get(4)?;
        let created_at: f64 = row.get(5)?;
        let last_error: String = row.get(6)?;
        let target = roots.map_to_real(&original).ok();
        Ok(json!({
            "id": id, "status": status, "original_path": original, "recycled_path": recycled,
            "file_name": serde_json::from_str::<Value>(&snapshot).ok().and_then(|v| v.get("file_name").and_then(Value::as_str).map(str::to_owned)).unwrap_or_default(),
            "created_at": created_at,
            "last_error": last_error,
            "recycled_file_exists": Path::new(&recycled).is_file() && recycle_source_is_trusted(Path::new(&recycled), Path::new(&original), Some(roots)),
            "original_file_exists": target.map(|p| p.exists()).unwrap_or(false),
        }))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
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
    let target = roots
        .map_to_real(&original)
        .map_err(|e| conflict(e.to_string()))?;
    if !path_under_authorized_roots(&target, roots) {
        return Err(conflict("restore path is outside configured media roots"));
    }
    if target.exists() {
        return Err(conflict("original path is already occupied"));
    }
    let recycled_path = PathBuf::from(&recycled);
    if !recycled_path.is_file() || !recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
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
                Value::Number(n) if n.is_i64() => {
                    rusqlite::types::Value::Integer(n.as_i64().unwrap())
                }
                Value::Number(n) => rusqlite::types::Value::Real(n.as_f64().unwrap_or(0.0)),
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

pub fn purge_recycle_entry(
    conn: &Connection,
    roots: &MediaRoots,
    entry_id: i64,
) -> Result<Value, (StatusCode, Value)> {
    let record = conn
        .query_row(
            "SELECT original_path, recycled_path FROM recycle_entries WHERE id=?",
            [entry_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()
        .map_err(internal)?;

    let Some((original, recycled)) = record else {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "recycle entry not found"}),
        ));
    };

    let recycled_path = PathBuf::from(&recycled);
    let mut file_removed = false;
    if recycled_path.is_file()
        && recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
    {
        if std::fs::remove_file(&recycled_path).is_ok() {
            file_removed = true;
            if let Some(parent) = recycled_path.parent() {
                let _ = std::fs::remove_dir(parent);
            }
        }
    }

    conn.execute("DELETE FROM recycle_entries WHERE id=?", [entry_id])
        .map_err(internal)?;

    Ok(json!({
        "ok": true,
        "id": entry_id,
        "file_removed": file_removed,
        "message": "已从回收站彻底删除"
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
        conn.prepare("SELECT id, original_path, recycled_path FROM recycle_entries")
            .map_err(internal)?
    } else {
        conn.prepare("SELECT id, original_path, recycled_path FROM recycle_entries WHERE status='recycled'")
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
    for (id, original, recycled) in rows {
        let recycled_path = PathBuf::from(&recycled);
        if recycled_path.is_file()
            && recycle_source_is_trusted(&recycled_path, Path::new(&original), Some(roots))
        {
            if std::fs::remove_file(&recycled_path).is_ok() {
                files_removed += 1;
                if let Some(parent) = recycled_path.parent() {
                    let _ = std::fs::remove_dir(parent);
                }
            }
        }
        if conn.execute("DELETE FROM recycle_entries WHERE id=?", [id]).is_ok() {
            cleared_count += 1;
        }
    }

    Ok(json!({
        "ok": true,
        "cleared_count": cleared_count,
        "files_removed": files_removed,
        "message": format!("已清理 {cleared_count} 项回收站记录")
    }))
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::*;
    use crate::{delete_item_to_recycle, DbConfig, DbPool};

    fn insert_moving_entry(conn: &Connection, original: &str, recycled: &str) -> i64 {
        ensure_recycle_schema(conn).unwrap();
        conn.execute(
            "INSERT INTO recycle_entries
             (original_item_id, artist_id, original_path, recycled_path, item_snapshot, status)
             VALUES (1, 1, ?, ?, '{}', 'moving')",
            params![original, recycled],
        )
        .unwrap();
        conn.last_insert_rowid()
    }

    fn count(conn: &Connection, sql: &str) -> i64 {
        conn.query_row(sql, [], |row| row.get(0)).unwrap()
    }

    #[test]
    fn delete_success_leaves_no_moving_rows() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let conn = pool.get().unwrap();
        delete_item_to_recycle(&conn, &original.to_string_lossy(), &roots).unwrap();

        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM recycle_entries WHERE status='moving'"
            ),
            0,
            "a completed delete must not keep the moving marker"
        );
        let recycled_path: String = conn
            .query_row(
                "SELECT recycled_path FROM recycle_entries WHERE status='recycled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!recycled_path.is_empty());
        assert!(Path::new(&recycled_path).is_file());
    }

    #[test]
    fn reconcile_drops_moving_entry_when_crash_preceded_file_move() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, _data_dir) = fixture();
        let conn = pool.get().unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (0, 1, 0));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM recycle_entries"), 0);
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 1);
        assert!(original.is_file(), "untouched file must stay in place");
    }

    #[test]
    fn reconcile_finalizes_moving_entry_when_crash_followed_file_move() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (dir, pool, _roots, original, _data_dir) = fixture();
        let conn = pool.get().unwrap();
        let trash = dir.path().join("trash-store").join("same.jpg");
        std::fs::create_dir_all(trash.parent().unwrap()).unwrap();
        std::fs::rename(&original, &trash).unwrap();
        insert_moving_entry(
            &conn,
            &original.to_string_lossy().replace('\\', "/"),
            &trash.to_string_lossy().replace('\\', "/"),
        );

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (1, 0, 0));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 0);
        assert_eq!(
            count(
                &conn,
                "SELECT COUNT(*) FROM recycle_entries WHERE status='recycled'"
            ),
            1
        );
        assert!(trash.is_file(), "the recycled copy must be preserved");
    }

    #[test]
    fn reconcile_marks_moving_entry_when_file_is_lost() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, data_dir) = fixture();
        // Point the recovery scan at a store that genuinely lacks the file, so
        // "lost" is asserted against a known-empty location instead of
        // whatever DATA_DIR the ambient environment happens to name.
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_dir);
        let conn = pool.get().unwrap();
        std::fs::remove_file(&original).unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (0, 0, 1));
        let last_error: String = conn
            .query_row(
                "SELECT last_error FROM recycle_entries WHERE status='recycled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!last_error.is_empty());
    }

    #[test]
    fn reconcile_recovers_a_move_whose_destination_was_never_recorded() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_dir);
        let conn = pool.get().unwrap();
        // The move mirrors the source's relative path under the recycle root
        // and is the last thing that happens before the destination is
        // recorded, so this is exactly the state an interruption leaves.
        let recycled = data_dir.join("recycle").join("Artist").join("same.jpg");
        std::fs::create_dir_all(recycled.parent().unwrap()).unwrap();
        std::fs::rename(&original, &recycled).unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (1, 0, 0));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 0);
        let recorded: String = conn
            .query_row(
                "SELECT recycled_path FROM recycle_entries WHERE status='recycled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            Path::new(&recorded),
            recycled.as_path(),
            "the recovered destination must be recorded on the entry"
        );
        assert!(recycled.is_file(), "the recovered copy must be preserved");
    }

    #[test]
    fn reconcile_recovers_a_collision_suffixed_recycled_file() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_dir);
        let conn = pool.get().unwrap();
        // A name already taken at the destination makes the mover suffix the
        // stored file; the scan has to recognise that as this delete's copy.
        let recycled = data_dir
            .join("recycle")
            .join("same__0123456789abcdef0123456789abcdef.jpg");
        std::fs::create_dir_all(recycled.parent().unwrap()).unwrap();
        std::fs::rename(&original, &recycled).unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (1, 0, 0));
        let recorded: String = conn
            .query_row(
                "SELECT recycled_path FROM recycle_entries WHERE status='recycled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(Path::new(&recorded), recycled.as_path());
    }

    #[test]
    fn reconcile_leaves_an_ambiguous_recovery_marked_missing() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_dir);
        let conn = pool.get().unwrap();
        // Two stored copies of the same name: an earlier delete's file and this
        // one's. Claiming either risks restoring the wrong bytes later, so the
        // row must stay missing instead of guessing.
        for (dir, bytes) in [
            (data_dir.join("recycle"), &b"earlier"[..]),
            (data_dir.join("recycle").join("Artist"), &b"this-delete"[..]),
        ] {
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("same.jpg"), bytes).unwrap();
        }
        std::fs::remove_file(&original).unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (0, 0, 1));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 1);
    }

    #[test]
    fn reconcile_does_not_claim_a_stored_file_when_the_original_survived() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, _roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", &data_dir);
        let conn = pool.get().unwrap();
        // The delete never reached the move, so the copy already in recycle
        // storage belongs to an earlier delete of a same-named file and must be
        // left where it is.
        let stale = data_dir.join("recycle").join("same.jpg");
        std::fs::create_dir_all(stale.parent().unwrap()).unwrap();
        std::fs::write(&stale, b"earlier").unwrap();
        insert_moving_entry(&conn, &original.to_string_lossy().replace('\\', "/"), "");

        let (finalized, dropped, missing) = reconcile_moving_recycle_entries(&conn);

        assert_eq!((finalized, dropped, missing), (0, 1, 0));
        assert_eq!(count(&conn, "SELECT COUNT(*) FROM items"), 1);
        assert!(original.is_file(), "untouched file must stay in place");
        assert_eq!(std::fs::read(&stale).unwrap(), b"earlier");
    }

    fn fixture() -> (tempfile::TempDir, Arc<DbPool>, MediaRoots, PathBuf, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("pictures");
        let original = media.join("Artist").join("same.jpg");
        std::fs::create_dir_all(original.parent().unwrap()).unwrap();
        std::fs::write(&original, b"original").unwrap();
        let pool = Arc::new(
            DbPool::with_config(
                dir.path().join("gallery.db"),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        let original_text = original.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', ?)",
            [media.join("Artist").to_string_lossy().replace('\\', "/")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'tag')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (1, 1, ?, 'same.jpg')",
            [original_text],
        )
        .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_favorites (item_id) VALUES (1)", [])
            .unwrap();
        conn.execute(
            "INSERT INTO characters (id, name) VALUES (1, 'Character')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_references
             (id, character_id, embedding, embedding_dim, embedding_model_repo_id,
              embedding_model_variant, embedding_model_file, embedding_updated_at,
              source_type, item_id, created_at)
             VALUES (1, 1, x'0102', 2, 'repo', 'variant', 'model.onnx', 123,
                     'tag_single', 1, 456),
                    (2, 1, x'0304', 2, 'repo', 'variant', 'model.onnx', 123,
                     'manual', 1, 456)",
            [],
        )
        .unwrap();
        drop(conn);
        let roots = MediaRoots::identical(
            vec![media.to_string_lossy().replace('\\', "/")],
            vec!["pictures".into()],
        );
        let data_dir = dir.path().join("data");
        (dir, pool, roots, original, data_dir)
    }

    #[test]
    fn restore_round_trip_preserves_item_relationships_and_model_metadata() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let conn = pool.get().unwrap();
        delete_item_to_recycle(&conn, &original.to_string_lossy(), &roots).unwrap();
        let entry_id: i64 = conn
            .query_row("SELECT id FROM recycle_entries", [], |row| row.get(0))
            .unwrap();

        restore_recycle_entry(&conn, &roots, entry_id).unwrap();

        assert_eq!(std::fs::read(original).unwrap(), b"original");
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM items WHERE id=1", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM item_tags WHERE item_id=1 AND tag_id=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM item_favorites WHERE item_id=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
        let tag_ref: (String, String, String, Option<f64>) = conn
            .query_row(
                "SELECT embedding_model_repo_id, embedding_model_variant,
                        embedding_model_file, embedding_updated_at
                 FROM character_references WHERE item_id=1 AND source_type='tag_single'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            tag_ref,
            (
                "repo".into(),
                "variant".into(),
                "model.onnx".into(),
                Some(123.0)
            )
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM character_references WHERE id=2 AND item_id=1",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );
    }

    #[cfg(windows)]
    #[test]
    fn restore_accepts_mixed_windows_path_separators() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, pool, roots, original, data_dir) = fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let mixed_roots = MediaRoots {
            roots: roots
                .roots
                .iter()
                .map(|path| path.replace('/', "\\"))
                .collect(),
            labels: roots.labels,
            real_paths: roots
                .real_paths
                .iter()
                .map(|path| path.replace('/', "\\"))
                .collect(),
        };
        let conn = pool.get().unwrap();
        delete_item_to_recycle(&conn, &original.to_string_lossy(), &mixed_roots).unwrap();
        let entry_id: i64 = conn
            .query_row("SELECT id FROM recycle_entries", [], |row| row.get(0))
            .unwrap();

        restore_recycle_entry(&conn, &mixed_roots, entry_id).unwrap();

        assert_eq!(std::fs::read(original).unwrap(), b"original");
    }

    /// A restore reports success only when the item really came back whole.
    ///
    /// The tag and favorite inserts used to be `let _ =`, so a failure there
    /// still committed the transaction and still answered `ok:true`, and the
    /// item reappeared without the tags the recycle snapshot had recorded.
    ///
    /// `INSERT OR IGNORE` is not a fix for that: it suppresses foreign-key
    /// violations as silently as `let _ =` suppressed the error, so a tag
    /// deleted while the item sat in the recycle bin would vanish the same way.
    /// The reference is probed instead, and every other failure aborts.
    #[test]
    fn a_failed_tag_restore_aborts_instead_of_reporting_success() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
             CREATE TABLE item_tags (
                 item_id INTEGER NOT NULL,
                 tag_id INTEGER NOT NULL REFERENCES tags(id) CHECK (tag_id > 0),
                 PRIMARY KEY (item_id, tag_id)
             );
             INSERT INTO tags (id, name) VALUES (1, 'keep');",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();

        // The tag still exists: the reference is written.
        assert!(
            reference_target_exists(&conn, "SELECT 1 FROM tags WHERE id=?1", params![1i64])
                .unwrap()
        );
        insert_restored_reference(
            &conn,
            "INSERT INTO item_tags (item_id, tag_id) VALUES (?1, ?2)",
            params![7i64, 1i64],
        )
        .unwrap();
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM item_tags", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1
        );

        // The tag is gone: the probe says so, so no insert is attempted and no
        // orphan row can appear.
        assert!(
            !reference_target_exists(&conn, "SELECT 1 FROM tags WHERE id=?1", params![99i64])
                .unwrap()
        );

        // A fault the schema can state plainly is not swallowed: the table
        // refuses a negative tag id, and `execute` reports the violation to the
        // caller rather than hiding it behind `let _ =`.
        let mut statement = conn
            .prepare("INSERT INTO item_tags (item_id, tag_id) VALUES (?1, ?2)")
            .unwrap();
        let error = statement.execute(params![7i64, -1i64]).unwrap_err();
        assert!(
            error.to_string().to_lowercase().contains("constraint"),
            "unexpected error: {error}"
        );
        drop(statement);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM item_tags", [], |row| row
                .get::<_, i64>(0))
                .unwrap(),
            1,
            "the failed insert must not have written a row"
        );

        // Re-inserting a relationship the snapshot already recorded is success:
        // the row exists, which is the state the restore wanted.
        insert_restored_reference(
            &conn,
            "INSERT INTO item_tags (item_id, tag_id) VALUES (?1, ?2)",
            params![7i64, 1i64],
        )
        .unwrap();
    }

    #[test]
    fn purge_and_clear_recycle_entries_test() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_recycle_schema(&conn).unwrap();
        let roots_dir = tempfile::tempdir().unwrap();
        let roots = MediaRoots {
            roots: vec![roots_dir.path().to_string_lossy().to_string()],
            labels: vec!["root".into()],
            real_paths: vec![roots_dir.path().to_string_lossy().to_string()],
        };

        // Create dummy recycled files
        let recycle_store = roots_dir.path().join(".Recycle_bin");
        std::fs::create_dir_all(&recycle_store).unwrap();
        let file1 = recycle_store.join("file1.jpg");
        let file2 = recycle_store.join("file2.jpg");
        std::fs::write(&file1, b"recycled 1").unwrap();
        std::fs::write(&file2, b"recycled 2").unwrap();

        let orig1 = roots_dir.path().join("orig1.jpg").to_string_lossy().to_string();
        let orig2 = roots_dir.path().join("orig2.jpg").to_string_lossy().to_string();

        conn.execute(
            "INSERT INTO recycle_entries (id, original_item_id, artist_id, original_path, recycled_path, item_snapshot, status)
             VALUES (1, 10, 1, ?1, ?2, '{}', 'recycled')",
            params![orig1, file1.to_string_lossy().to_string()],
        ).unwrap();
        conn.execute(
            "INSERT INTO recycle_entries (id, original_item_id, artist_id, original_path, recycled_path, item_snapshot, status)
             VALUES (2, 20, 1, ?1, ?2, '{}', 'recycled')",
            params![orig2, file2.to_string_lossy().to_string()],
        ).unwrap();

        // 1. Purge single entry #1
        let purge_res = purge_recycle_entry(&conn, &roots, 1).unwrap();
        assert_eq!(purge_res["ok"], true);
        assert_eq!(purge_res["file_removed"], true);
        assert!(!file1.exists());
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM recycle_entries WHERE id=1", [], |r| r.get::<_, i64>(0)).unwrap(), 0);

        // 2. Clear remaining entries
        let clear_res = clear_recycle_entries(&conn, &roots, None).unwrap();
        assert_eq!(clear_res["ok"], true);
        assert_eq!(clear_res["cleared_count"], 1);
        assert_eq!(clear_res["files_removed"], 1);
        assert!(!file2.exists());
        assert_eq!(conn.query_row("SELECT COUNT(*) FROM recycle_entries", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
    }
}
