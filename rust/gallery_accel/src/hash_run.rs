//! In-process hash batch (replaces residual `hash_worker.run_hash_batch`).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::BTreeSet;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::content_hash::hash_file;
use crate::db_housekeeping::run_housekeeping_batch;
use crate::hash_status::hash_status_response;
use crate::link_index::{is_link_source_file, reindex_scanned_artist_links};
use crate::media_roots::{authorized_media_path, MediaRoots};
use crate::product_ui::auto_resolve_move_candidates_with_roots;
use crate::scan_candidates_write::resolve_scan_candidate_response_with_roots;

fn stable_file_hash(path: &Path) -> Result<Option<String>> {
    let before = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    let digest = hash_file(path, 1024 * 1024)?;
    let after = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Ok(None);
        }
    }
    Ok(Some(digest))
}

pub fn run_hash_batch(conn: &Connection, limit: i64) -> Result<Value> {
    let roots = MediaRoots {
        roots: Vec::new(),
        labels: Vec::new(),
        real_paths: Vec::new(),
    };
    run_hash_batch_with_roots(conn, &roots, limit)
}

pub fn run_hash_batch_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
) -> Result<Value> {
    let limit = limit.clamp(1, 500);
    let mut items_done = 0i64;
    let mut cand_done = 0i64;
    let mut resolved = 0i64;
    let mut link_artist_ids = BTreeSet::new();

    // Hash pending scan candidates first.
    let cand_ids: Vec<i64> = conn
        .prepare(
            "
            SELECT id FROM scan_candidates
            WHERE status IN ('pending','candidate')
              AND hash_status IN ('pending','error','')
              AND NOT EXISTS (
                  SELECT 1 FROM move_candidates mc
                  WHERE mc.scan_candidate_id = scan_candidates.id
                    AND mc.status = 'pending'
              )
            ORDER BY id LIMIT ?
            ",
        )?
        .query_map(params![limit], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let candidate_queue_len = cand_ids.len() as i64;
    for id in cand_ids {
        let candidate_state: (String, i64, f64, Option<i64>, Option<i64>, String, String) = conn
            .query_row(
                "SELECT file_path, file_size, file_mtime, st_dev, st_ino, content_hash, hash_status
             FROM scan_candidates WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
        let Ok(path) = authorized_media_path(roots, &candidate_state.0) else {
            conn.execute("UPDATE scan_candidates SET hash_status='error' WHERE id=? AND status IN ('pending','candidate')", params![id])?;
            continue;
        };
        let before = std::fs::metadata(&path).ok();
        match stable_file_hash(&path) {
            Ok(Some(digest)) => {
                let after = std::fs::metadata(&path).ok();
                let identity_matches = match (before.as_ref(), after.as_ref()) {
                    (Some(before), Some(after)) => {
                        file_metadata_matches(before, after)
                            && file_matches_snapshot(
                                before,
                                candidate_state.1,
                                candidate_state.2,
                                candidate_state.3,
                                candidate_state.4,
                            )
                    }
                    _ => false,
                };
                if !identity_matches {
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE scan_candidates SET content_hash=?, hash_status='done'
                     WHERE id=? AND status IN ('pending','candidate')
                       AND file_path=?
                       AND ((file_size=? AND file_mtime=?) OR (file_size=0 AND file_mtime=0))
                       AND (st_dev IS ? OR st_dev=?) AND (st_ino IS ? OR st_ino=?)
                       AND content_hash=? AND hash_status=?",
                    params![
                        digest,
                        id,
                        candidate_state.0,
                        candidate_state.1,
                        candidate_state.2,
                        candidate_state.3,
                        candidate_state.3,
                        candidate_state.4,
                        candidate_state.4,
                        candidate_state.5,
                        candidate_state.6
                    ],
                )?;
                if changed != 1 {
                    continue;
                }
                cand_done += 1;
                if let Ok(v) = resolve_scan_candidate_response_with_roots(conn, roots, id) {
                    if record_resolution(conn, &v, &mut link_artist_ids)? {
                        resolved += 1;
                    }
                }
            }
            Ok(None) | Err(_) => {
                conn.execute(
                    "UPDATE scan_candidates SET hash_status='error' WHERE id=?",
                    params![id],
                )?;
            }
        }
    }

    // Upgrade backlog: candidates that were hashed before the native resolver
    // existed still need the same safety pass. Keep the total candidate work
    // bounded by the caller's batch limit.
    let history_limit = limit.saturating_sub(candidate_queue_len);
    if history_limit > 0 {
        let ready_ids: Vec<i64> = conn
            .prepare(
                "
                SELECT sc.id FROM scan_candidates sc
                WHERE sc.status IN ('pending','candidate')
                  AND sc.hash_status = 'done'
                  AND NOT EXISTS (
                      SELECT 1 FROM move_candidates mc
                      WHERE mc.scan_candidate_id = sc.id AND mc.status = 'pending'
                  )
                ORDER BY sc.id LIMIT ?
                ",
            )?
            .query_map(params![history_limit], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ready_ids {
            if let Ok(v) = resolve_scan_candidate_response_with_roots(conn, roots, id) {
                if record_resolution(conn, &v, &mut link_artist_ids)? {
                    resolved += 1;
                }
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
        let item_state: (String, i64, f64, Option<i64>, Option<i64>, String, String) = conn
            .query_row(
                "SELECT file_path, file_size, file_mtime, st_dev, st_ino, content_hash, hash_status
             FROM items WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
        let Ok(path) = authorized_media_path(roots, &item_state.0) else {
            conn.execute(
                "UPDATE items SET hash_status='error' WHERE id=? AND missing=0",
                params![id],
            )?;
            continue;
        };
        let before = std::fs::metadata(&path).ok();
        match stable_file_hash(&path) {
            Ok(Some(digest)) => {
                let after = std::fs::metadata(&path).ok();
                let identity_matches = match (before.as_ref(), after.as_ref()) {
                    (Some(before), Some(after)) => {
                        file_metadata_matches(before, after)
                            && file_matches_snapshot(
                                before,
                                item_state.1,
                                item_state.2,
                                item_state.3,
                                item_state.4,
                            )
                    }
                    _ => false,
                };
                if !identity_matches {
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE items SET content_hash=?, hash_status='done',
                     hash_updated_at=strftime('%s','now') WHERE id=? AND missing=0
                     AND file_path=?
                     AND ((file_size=? AND file_mtime=?) OR (file_size=0 AND file_mtime=0))
                     AND (st_dev IS ? OR st_dev=?) AND (st_ino IS ? OR st_ino=?)
                     AND content_hash=? AND hash_status=?",
                    params![
                        digest,
                        id,
                        item_state.0,
                        item_state.1,
                        item_state.2,
                        item_state.3,
                        item_state.3,
                        item_state.4,
                        item_state.4,
                        item_state.5,
                        item_state.6
                    ],
                )?;
                if changed == 1 {
                    items_done += 1;
                }
            }
            Ok(None) | Err(_) => {
                conn.execute(
                    "UPDATE items SET hash_status='error' WHERE id=?",
                    params![id],
                )?;
            }
        }
    }

    let move_candidates = auto_resolve_move_candidates_with_roots(conn, roots, limit)?;
    let moves_applied = move_candidates["applied"].as_i64().unwrap_or(0);
    let link_artist_ids: Vec<i64> = link_artist_ids.into_iter().collect();
    let links = if link_artist_ids.is_empty() {
        json!({"ok": true, "artists": 0, "skipped": "no_resolved_text_items"})
    } else {
        reindex_scanned_artist_links(conn, roots, &link_artist_ids)
            .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()}))
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    let housekeeping = run_housekeeping_batch(
        conn,
        now - 90.0 * 24.0 * 60.0 * 60.0,
        now - 7.0 * 24.0 * 60.0 * 60.0,
        now - 30.0 * 24.0 * 60.0 * 60.0,
        10_000,
    )?;
    let status = hash_status_response(conn)?;
    let progress = items_done + cand_done + resolved + moves_applied;
    Ok(json!({
        "ok": true,
        "message": if progress > 0 { "hash_batch_progress" } else { "hash_batch_idle" },
        "items": {"done": items_done},
        "scan_candidates": {"done": cand_done},
        "resolved": resolved,
        "move_candidates": move_candidates,
        "links": links,
        "housekeeping": {
            "missing_items_expired_deleted": housekeeping.missing_items_deleted,
            "scan_seen_expired_deleted": housekeeping.scan_seen_deleted,
            "scan_candidates_terminal_deleted": housekeeping.scan_candidates_deleted,
        },
        "status": status,
    }))
}

fn file_metadata_matches(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return before.dev() == after.dev() && before.ino() == after.ino();
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// A hash belongs to the row snapshot only when the file still has the same
/// observed metadata. Legacy rows with neither size nor mtime remain eligible
/// for their first hash; every populated identity field is authoritative.
fn file_matches_snapshot(
    metadata: &std::fs::Metadata,
    file_size: i64,
    file_mtime: f64,
    _st_dev: Option<i64>,
    _st_ino: Option<i64>,
) -> bool {
    if (file_size != 0 || file_mtime != 0.0)
        && (metadata.len() as i64 != file_size
            || metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .is_none_or(|value| (value.as_secs_f64() - file_mtime).abs() >= 1.0))
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if _st_dev.is_some_and(|value| value != metadata.dev() as i64)
            || _st_ino.is_some_and(|value| value != metadata.ino() as i64)
        {
            return false;
        }
    }
    true
}

fn record_resolution(
    conn: &Connection,
    response: &Value,
    link_artist_ids: &mut BTreeSet<i64>,
) -> Result<bool> {
    if matches!(
        response.get("action").and_then(|action| action.as_str()),
        Some("missing") | Some("waiting_hash") | Some("no_match")
    ) {
        return Ok(false);
    }
    let Some(item_id) = response.get("item_id").and_then(Value::as_i64) else {
        return Ok(true);
    };
    let item = conn
        .query_row(
            "SELECT artist_id, file_name FROM items WHERE id=? AND missing=0",
            params![item_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((artist_id, file_name)) = item {
        if is_link_source_file(&file_name) {
            link_artist_ids.insert(artist_id);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use tempfile::tempdir;

    fn file_state(path: &Path) -> (i64, f64) {
        let metadata = std::fs::metadata(path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        (metadata.len() as i64, mtime)
    }

    fn race_schema() -> &'static str {
        "
        CREATE TABLE items (
          id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
          file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
          date TEXT DEFAULT '', auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
          missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
          content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
          media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER
        );
        CREATE TABLE scan_candidates (
          id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
          file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
          file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
          is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
          content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
          created_at REAL DEFAULT 0, resolved_at REAL
        );
        CREATE TABLE scan_seen (
          id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
          created_at REAL DEFAULT 0
        );
        CREATE TABLE move_candidates (
          id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
          artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
          reason TEXT DEFAULT '', status TEXT, resolved_at REAL
        );
        CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
        "
    }

    /// R3 regression: a concurrent rescan that refreshes the row between hashing
    /// and the final update must prevent a stale `done` digest, and the refreshed
    /// row must keep its newer metadata for a future hash batch.
    #[test]
    fn concurrent_rescan_prevents_stale_done_on_items() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("item.bin");
        std::fs::write(&file, b"item-content").unwrap();
        let db_path = dir.path().join("race.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, hash_status)
             VALUES (1, 1, ?, 'item.bin', ?, ?, 'pending')",
            params![path, size, mtime],
        )
        .unwrap();

        // A second connection plays the concurrent rescan: it refreshes the
        // row exactly when the hash worker finalizes it.
        let rescan = Connection::open(&db_path).unwrap();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "items",
                    ..
                }
            ) {
                rescan
                    .execute(
                        "UPDATE items SET file_size=999, file_mtime=424242.0, hash_status='pending',
                         content_hash='' WHERE id=1",
                        [],
                    )
                    .unwrap();
            }
            Authorization::Allow
        }));

        let out = run_hash_batch(&conn, 10).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(out["ok"], true);
        let (hash_status, content_hash, file_size, file_mtime): (String, String, i64, f64) = conn
            .query_row(
                "SELECT hash_status, content_hash, file_size, file_mtime FROM items WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash_status, "pending");
        assert_eq!(content_hash, "");
        assert_eq!(file_size, 999);
        assert_eq!(file_mtime, 424242.0);
    }

    #[test]
    fn concurrent_rescan_prevents_stale_done_on_scan_candidate() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("cand.bin");
        std::fs::write(&file, b"candidate-content").unwrap();
        let db_path = dir.path().join("race.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (1, 'pending', 'pending', ?, 'cand.bin', ?, ?)",
            params![path, size, mtime],
        )
        .unwrap();

        let rescan = Connection::open(&db_path).unwrap();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "scan_candidates",
                    ..
                }
            ) {
                rescan
                    .execute(
                        "UPDATE scan_candidates SET file_size=777, file_mtime=131313.0,
                         hash_status='pending', content_hash='' WHERE id=1",
                        [],
                    )
                    .unwrap();
            }
            Authorization::Allow
        }));

        let out = run_hash_batch(&conn, 10).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(out["ok"], true);
        let (hash_status, content_hash, file_size, status): (String, String, i64, String) = conn
            .query_row(
                "SELECT hash_status, content_hash, file_size, status FROM scan_candidates WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash_status, "pending");
        assert_eq!(content_hash, "");
        assert_eq!(file_size, 777);
        assert_eq!(status, "pending");
    }

    #[test]
    fn replaced_file_does_not_finalize_a_stale_snapshot() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("changed.bin");
        std::fs::write(&file, b"old").unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, hash_status)
             VALUES (1, 1, ?, 'changed.bin', ?, ?, 'pending')",
            params![&path, size, mtime],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (1, 'pending', 'pending', ?, 'changed.bin', ?, ?)",
            params![&path, size, mtime],
        )
        .unwrap();

        std::fs::write(&file, b"replacement with a different size").unwrap();
        run_hash_batch(&conn, 10).unwrap();

        for table in ["items", "scan_candidates"] {
            let state: (String, String) = conn
                .query_row(
                    &format!("SELECT hash_status, content_hash FROM {table} WHERE id=1"),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, ("pending".into(), "".into()));
        }
    }

    /// R4 regression: with configured media roots, an item or candidate path
    /// outside the roots is never read and is conservatively marked `error`.
    #[test]
    fn unauthorized_paths_are_not_read_and_marked_error() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let secret = outside_dir.join("secret.bin");
        std::fs::write(&secret, b"secret-bytes").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let outside_s = secret.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (1, 1, ?, 'secret.bin', 'pending')",
            params![outside_s],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'secret.bin')",
            params![outside_s],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let out = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        assert_eq!(out["ok"], true);
        let item_status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(item_status, "error");
        let cand_status: String = conn
            .query_row(
                "SELECT hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cand_status, "error");
        // The unauthorized file was never consumed or moved.
        assert_eq!(std::fs::read(&secret).unwrap(), b"secret-bytes");
    }

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
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_seen (id, scan_id, artist_id, file_path, created_at)
             VALUES (1, 'stale-scan', 1, '/stale.jpg', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status) VALUES (1,1,?, 'a.bin','pending')",
            params![path],
        )
        .unwrap();
        let out = run_hash_batch(&conn, 10).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["housekeeping"]["scan_seen_expired_deleted"], 1);
        let status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "done");
        let hash: String = conn
            .query_row("SELECT content_hash FROM items WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!hash.is_empty());
    }

    #[test]
    fn resolves_historical_done_scan_candidate_without_rehashing() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("historical.jpg");
        std::fs::write(&file, b"historical-hash").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/library/Artist');
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        let (file_size, file_mtime) = file_state(&file);
        let content_hash = hash_file(&file, 1024 * 1024).unwrap();
        conn.execute(
            "INSERT INTO scan_candidates
             (id, scan_id, status, hash_status, file_path, file_name, file_size,
              file_mtime, media_type, content_hash, artist_id)
             VALUES (1, 'old-scan', 'pending', 'done', ?, 'historical.jpg', ?, ?,
                     'image', ?, 1)",
            params![path, file_size, file_mtime, content_hash],
        )
        .unwrap();

        let result = run_hash_batch(&conn, 10).unwrap();

        assert_eq!(result["message"], "hash_batch_progress");
        assert_eq!(result["scan_candidates"]["done"], 0);
        let (status, count): (String, i64) = conn
            .query_row(
                "SELECT status, (SELECT COUNT(*) FROM items) FROM scan_candidates WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "new");
        assert_eq!(count, 1);
    }

    #[test]
    fn indexes_links_after_text_candidate_is_auto_imported() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("links.txt");
        std::fs::write(&file, "https://pan.quark.cn/s/example 提取码: A123").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/library/Artist');
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        let (file_size, file_mtime) = file_state(&file);
        conn.execute(
            "INSERT INTO scan_candidates
             (id, scan_id, status, hash_status, file_path, file_name, file_size,
              file_mtime, media_type, artist_id)
             VALUES (1, 'scan', 'pending', 'pending', ?, 'links.txt', ?, ?, 'text', 1)",
            params![path, file_size, file_mtime],
        )
        .unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let roots = MediaRoots::identical(vec![root], vec!["library".into()]);

        let result = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        let response = crate::link_index::artist_links_response(&conn, 1).unwrap();

        assert_eq!(result["links"]["indexed_documents"], 1);
        assert_eq!(response["summary"]["links"], 1);
        assert_eq!(response["summary"]["documents"], 1);
        assert_eq!(response["links"][0]["provider_name"], "夸克网盘");
    }
}
