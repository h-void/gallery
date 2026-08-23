use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

const TERMINAL_SCAN_CANDIDATE_STATUSES: &str = "'resolved','new','superseded'";

#[derive(Debug, Default, Serialize)]
pub struct HousekeepingResult {
    pub missing_items_deleted: usize,
    pub scan_seen_deleted: usize,
    pub scan_candidates_deleted: usize,
}

pub fn cleanup_scan_seen(conn: &Connection, scan_id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM scan_seen WHERE scan_id=?", params![scan_id])?)
}

pub fn run_housekeeping_batch(
    conn: &Connection,
    missing_item_cutoff: f64,
    scan_seen_cutoff: f64,
    scan_candidate_cutoff: f64,
    batch_size: i64,
) -> Result<HousekeepingResult> {
    let batch_size = batch_size.clamp(1, 50_000);
    let tx = conn.unchecked_transaction()?;
    let missing_items_deleted = tx.execute(
        "DELETE FROM items
         WHERE id IN (
             SELECT i.id
             FROM items i
             WHERE i.missing=1
               AND i.missing_at IS NOT NULL
               AND i.missing_at <= ?
               AND NOT EXISTS (
                   SELECT 1 FROM move_candidates mc
                   WHERE mc.item_id=i.id AND mc.status='pending'
               )
             ORDER BY i.id
             LIMIT ?
         )",
        params![missing_item_cutoff, batch_size],
    )?;
    let scan_seen_deleted = tx.execute(
        "DELETE FROM scan_seen
             WHERE id IN (
                 SELECT ss.id
                 FROM scan_seen ss
                 WHERE ss.created_at <= ?
                 ORDER BY ss.id
                 LIMIT ?
             )",
        params![scan_seen_cutoff, batch_size],
    )?;
    let scan_candidates_deleted = tx.execute(
        &format!(
            "DELETE FROM scan_candidates
             WHERE id IN (
                 SELECT id
                 FROM scan_candidates
                 WHERE status IN ({TERMINAL_SCAN_CANDIDATE_STATUSES})
                   AND COALESCE(resolved_at, created_at) <= ?
                   AND NOT EXISTS (
                       SELECT 1 FROM move_candidates mc
                       WHERE mc.scan_candidate_id=scan_candidates.id
                   )
                 ORDER BY id
                 LIMIT ?
             )"
        ),
        params![scan_candidate_cutoff, batch_size],
    )?;
    tx.commit()?;
    Ok(HousekeepingResult {
        missing_items_deleted,
        scan_seen_deleted,
        scan_candidates_deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::{params, Connection};

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE scan_seen (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_id TEXT NOT NULL,
                artist_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                created_at REAL NOT NULL
            );
            CREATE TABLE scan_candidates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_id TEXT NOT NULL,
                status TEXT NOT NULL,
                created_at REAL NOT NULL,
                resolved_at REAL
            );
            CREATE TABLE items (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                missing INTEGER NOT NULL DEFAULT 0,
                missing_at REAL
            );
            CREATE TABLE item_tags (
                item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                tag_id INTEGER NOT NULL,
                PRIMARY KEY(item_id, tag_id)
            );
            CREATE INDEX idx_scan_candidates_status ON scan_candidates(status);
            CREATE INDEX idx_scan_seen_scan_artist ON scan_seen(scan_id, artist_id);
            CREATE TABLE move_candidates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_candidate_id INTEGER,
                item_id INTEGER REFERENCES items(id) ON DELETE CASCADE,
                status TEXT NOT NULL DEFAULT 'pending'
            );
            ",
        )
        .unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        conn
    }

    #[test]
    fn scan_seen_is_removed_after_scan_even_with_active_candidates() {
        let conn = fixture();
        conn.execute(
            "INSERT INTO scan_seen (scan_id, artist_id, file_path, created_at)
             VALUES ('scan-active', 1, '/a.jpg', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (scan_id, status, created_at)
             VALUES ('scan-active', 'pending', 100)",
            [],
        )
        .unwrap();

        assert_eq!(cleanup_scan_seen(&conn, "scan-active").unwrap(), 1);
    }

    #[test]
    fn bounded_housekeeping_preserves_recent_rows() {
        let conn = fixture();
        for (id, missing, missing_at) in [
            (1, 1, Some(100.0)),
            (2, 1, Some(100.0)),
            (3, 1, Some(900.0)),
            (4, 0, Some(100.0)),
        ] {
            conn.execute(
                "INSERT INTO items (id,missing,missing_at) VALUES (?,?,?)",
                params![id, missing, missing_at],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO item_tags (item_id,tag_id) VALUES (1,10),(2,20)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO move_candidates (item_id,status) VALUES (2,'pending')",
            [],
        )
        .unwrap();
        for (scan_id, created_at) in [
            ("old-done", 100.0),
            ("old-active", 100.0),
            ("recent", 900.0),
        ] {
            conn.execute(
                "INSERT INTO scan_seen (scan_id, artist_id, file_path, created_at) VALUES (?,1,?,?)",
                params![scan_id, format!("/{scan_id}.jpg"), created_at],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO scan_candidates (scan_id, status, created_at) VALUES ('old-active','pending',100)",
            [],
        )
        .unwrap();
        for (scan_id, status, created_at, resolved_at) in [
            ("terminal-old", "resolved", 100.0, Some(200.0)),
            ("new-old", "new", 100.0, Some(200.0)),
            ("active-old", "pending", 100.0, None),
            ("terminal-recent", "resolved", 900.0, Some(900.0)),
        ] {
            conn.execute(
                "INSERT INTO scan_candidates (scan_id,status,created_at,resolved_at) VALUES (?,?,?,?)",
                params![scan_id, status, created_at, resolved_at],
            )
            .unwrap();
        }
        let protected_id: i64 = conn
            .query_row(
                "SELECT id FROM scan_candidates WHERE scan_id='new-old'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        conn.execute(
            "INSERT INTO move_candidates (scan_candidate_id) VALUES (?)",
            params![protected_id],
        )
        .unwrap();

        let result = run_housekeeping_batch(&conn, 500.0, 500.0, 500.0, 100).unwrap();
        assert_eq!(result.missing_items_deleted, 1);
        assert_eq!(result.scan_seen_deleted, 2);
        assert_eq!(result.scan_candidates_deleted, 1);

        let remaining_items: Vec<i64> = conn
            .prepare("SELECT id FROM items ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(remaining_items, vec![2, 3, 4]);
        let remaining_tag_items: Vec<i64> = conn
            .prepare("SELECT item_id FROM item_tags ORDER BY item_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(remaining_tag_items, vec![2]);

        let remaining_seen: Vec<String> = conn
            .prepare("SELECT scan_id FROM scan_seen ORDER BY scan_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(remaining_seen, vec!["recent"]);
        let remaining_candidates: Vec<String> = conn
            .prepare("SELECT scan_id FROM scan_candidates ORDER BY scan_id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(
            remaining_candidates,
            vec!["active-old", "new-old", "old-active", "terminal-recent"]
        );
    }
}
