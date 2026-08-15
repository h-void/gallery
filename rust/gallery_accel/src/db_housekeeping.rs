use anyhow::Result;
use rusqlite::{params, Connection};
use serde::Serialize;

const TERMINAL_SCAN_CANDIDATE_STATUSES: &str = "'resolved','new','superseded'";

#[derive(Debug, Default, Serialize)]
pub struct HousekeepingResult {
    pub scan_seen_deleted: usize,
    pub scan_candidates_deleted: usize,
}

pub fn cleanup_scan_seen(conn: &Connection, scan_id: &str) -> Result<usize> {
    Ok(conn.execute("DELETE FROM scan_seen WHERE scan_id=?", params![scan_id])?)
}

pub fn run_housekeeping_batch(
    conn: &Connection,
    scan_seen_cutoff: f64,
    scan_candidate_cutoff: f64,
    batch_size: i64,
) -> Result<HousekeepingResult> {
    let batch_size = batch_size.clamp(1, 50_000);
    let tx = conn.unchecked_transaction()?;
    let scan_seen_deleted = tx.execute(
        &format!(
            "DELETE FROM scan_seen
             WHERE id IN (
                 SELECT ss.id
                 FROM scan_seen ss
                 WHERE ss.created_at <= ?
                 ORDER BY ss.id
                 LIMIT ?
             )"
        ),
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
            CREATE INDEX idx_scan_candidates_status ON scan_candidates(status);
            CREATE INDEX idx_scan_seen_scan_artist ON scan_seen(scan_id, artist_id);
            CREATE TABLE move_candidates (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                scan_candidate_id INTEGER
            );
            ",
        )
        .unwrap();
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

        let result = run_housekeeping_batch(&conn, 500.0, 500.0, 100).unwrap();
        assert_eq!(result.scan_seen_deleted, 2);
        assert_eq!(result.scan_candidates_deleted, 1);

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
