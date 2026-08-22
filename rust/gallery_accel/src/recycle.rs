use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};
use axum::http::StatusCode;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::{normalize_slashes, path_under_authorized_roots, MediaRoots};
use crate::media_serve::{
    move_file_from_authorized_path_no_overwrite, move_file_to_authorized_path_no_overwrite,
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
                eprintln!("recycle reconciliation: failed to list 'moving' rows: {error}");
            }
            return (0, 0, 0);
        }
    };
    let mut finalized = 0;
    let mut dropped = 0;
    let mut missing = 0;
    for (id, item_id, original, recycled) in rows {
        if !recycled.is_empty() && Path::new(&recycled).is_file() {
            // The file reached recycle storage before the crash; finish the
            // delete the interrupted transaction would have committed.
            let done = (|| -> Result<()> {
                let tx = conn.unchecked_transaction()?;
                tx.execute(
                    "DELETE FROM character_references WHERE item_id=? AND source_type='tag_single'",
                    params![item_id],
                )?;
                tx.execute("DELETE FROM items WHERE id=?", params![item_id])?;
                tx.execute(
                    "UPDATE recycle_entries SET status='recycled',
                        last_error='finalized after interrupted delete'
                     WHERE id=? AND status='moving'",
                    params![id],
                )?;
                tx.commit()?;
                Ok(())
            })()
            .is_ok();
            if done {
                finalized += 1;
            }
        } else if Path::new(&original).is_file() {
            // Crash before the file moved: nothing happened on disk, and the
            // item row is still active — drop the stale marker entirely.
            if conn
                .execute(
                    "DELETE FROM recycle_entries WHERE id=? AND status='moving'",
                    params![id],
                )
                .is_ok()
            {
                dropped += 1;
            }
        } else if conn
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
    (finalized, dropped, missing)
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
    let limit = limit.unwrap_or(80).clamp(1, 100);
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
            "recycled_file_exists": Path::new(&recycled).is_file() && recycle_source_is_trusted(Path::new(&recycled), Path::new(&original)),
            "original_file_exists": target.map(|p| p.exists()).unwrap_or(false),
        }))
    })?.collect::<rusqlite::Result<Vec<_>>>()?;
    let next = (offset + (entries.len() as i64) < total).then_some(offset + entries.len() as i64);
    Ok(json!({"entries": entries, "total": total, "next_offset": next}))
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
    if !recycled_path.is_file() || !recycle_source_is_trusted(&recycled_path, Path::new(&original))
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
        for tag_id in tags {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO item_tags (item_id, tag_id) VALUES (?,?)",
                params![new_id, tag_id],
            );
        }
        restore_tag_single_refs(&tx, new_id, &tag_refs_raw)?;
        reattach_non_tag_refs(&tx, new_id, &non_tag_refs_raw)?;
        if snapshot
            .get("favorite")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            let _ = tx.execute(
                "INSERT OR IGNORE INTO item_favorites (item_id) VALUES (?)",
                [new_id],
            );
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
        let (_dir, pool, _roots, original, _data_dir) = fixture();
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
}
