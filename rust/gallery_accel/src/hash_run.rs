//! In-process hash batch (replaces residual `hash_worker.run_hash_batch`).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;

use crate::content_hash::hash_file;
use crate::hash_status::hash_status_response;
use crate::scan_candidates_write::{
    apply_hash_unique_scan_candidate_response, resolve_existing_scan_candidate_response,
};

pub fn run_hash_batch(conn: &Connection, limit: i64) -> Result<Value> {
    let limit = limit.clamp(1, 500);
    let mut items_done = 0i64;
    let mut cand_done = 0i64;
    let mut resolved = 0i64;

    // Hash pending scan candidates first.
    let cand_ids: Vec<i64> = conn
        .prepare(
            "
            SELECT id FROM scan_candidates
            WHERE status IN ('pending','candidate')
              AND hash_status IN ('pending','error','')
            ORDER BY id LIMIT ?
            ",
        )?
        .query_map(params![limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for id in cand_ids {
        let (path,): (String,) = conn.query_row(
            "SELECT file_path FROM scan_candidates WHERE id=?",
            params![id],
            |r| Ok((r.get(0)?,)),
        )?;
        if !Path::new(&path).is_file() {
            conn.execute(
                "UPDATE scan_candidates SET hash_status='error' WHERE id=?",
                params![id],
            )?;
            continue;
        }
        match hash_file(Path::new(&path), 1024 * 1024) {
            Ok(digest) => {
                conn.execute(
                    "UPDATE scan_candidates SET content_hash=?, hash_status='done' WHERE id=?",
                    params![digest, id],
                )?;
                cand_done += 1;
                // Try safe auto paths.
                if let Ok(v) = resolve_existing_scan_candidate_response(conn, id) {
                    if v.get("action").and_then(|a| a.as_str()) == Some("existing") {
                        resolved += 1;
                        continue;
                    }
                }
                if let Ok(v) = apply_hash_unique_scan_candidate_response(conn, id) {
                    if v.get("action").and_then(|a| a.as_str()) == Some("moved") {
                        resolved += 1;
                    }
                }
            }
            Err(_) => {
                conn.execute(
                    "UPDATE scan_candidates SET hash_status='error' WHERE id=?",
                    params![id],
                )?;
            }
        }
    }

    let item_ids: Vec<i64> = conn
        .prepare(
            "
            SELECT id FROM items
            WHERE missing=0 AND hash_status IN ('pending','error','')
            ORDER BY id LIMIT ?
            ",
        )?
        .query_map(params![limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    for id in item_ids {
        let path: String = conn.query_row(
            "SELECT file_path FROM items WHERE id=?",
            params![id],
            |r| r.get(0),
        )?;
        if !Path::new(&path).is_file() {
            conn.execute(
                "UPDATE items SET hash_status='error' WHERE id=?",
                params![id],
            )?;
            continue;
        }
        match hash_file(Path::new(&path), 1024 * 1024) {
            Ok(digest) => {
                conn.execute(
                    "UPDATE items SET content_hash=?, hash_status='done',
                     hash_updated_at=strftime('%s','now') WHERE id=?",
                    params![digest, id],
                )?;
                items_done += 1;
            }
            Err(_) => {
                conn.execute(
                    "UPDATE items SET hash_status='error' WHERE id=?",
                    params![id],
                )?;
            }
        }
    }

    let status = hash_status_response(conn)?;
    let progress = items_done + cand_done + resolved;
    Ok(json!({
        "ok": true,
        "message": if progress > 0 { "hash_batch_progress" } else { "hash_batch_idle" },
        "items": {"done": items_done},
        "scan_candidates": {"done": cand_done},
        "resolved": resolved,
        "status": status,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn hashes_pending_item() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"hello-hash").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, missing INTEGER DEFAULT 0,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, status TEXT, hash_status TEXT, file_path TEXT,
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, status TEXT
            );
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status) VALUES (1,1,?, 'a.bin','pending')",
            params![path],
        )
        .unwrap();
        let out = run_hash_batch(&conn, 10).unwrap();
        assert_eq!(out["ok"], true);
        let status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "done");
        let hash: String = conn
            .query_row("SELECT content_hash FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(!hash.is_empty());
    }
}
