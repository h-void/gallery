//! Batch recognized-date override editing (`PUT /api/items/date`).
//!
//! One transaction applies a normalized `YYYY-MM` / `YYYY-MM-DD` manual date
//! to every selected item, or clears the override so each item falls back to
//! its most recently detected source date. Any invalid ID or date rejects the
//! whole request without partial writes.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::{json, Value};

use crate::item_detail::effective_display_date;
use crate::media_type::parse_manual_date_input;

/// Canonical `YYYY-MM-DD` form of a raw precision-preserving date (day 01 for
/// month-only values), or empty when the raw value is not `YYYY-MM`/`YYYY-MM-DD`.
pub fn canonical_of_raw(raw: &str) -> String {
    let normalized = match parse_manual_date_input(raw) {
        Some(value) => value,
        None => return String::new(),
    };
    if normalized.len() == 7 {
        format!("{normalized}-01")
    } else {
        normalized
    }
}

/// The immediate parent folder name of an item (the value `scan.rs` writes to
/// `items.folder_name`, which keys `folder_rename_plans.source_folder`), or
/// empty for items directly inside the artist root.
pub(crate) fn item_date_folder_name(artist_path: &str, file_path: &str) -> String {
    let artist = artist_path.trim_end_matches('/');
    let full = file_path.trim_end_matches('/');
    if artist.is_empty() || full.is_empty() || full == artist {
        return String::new();
    }
    let artist_segment = artist.rsplit('/').next().unwrap_or("");
    match full.rsplit('/').nth(1) {
        Some(parent) if parent != artist_segment => parent.to_string(),
        _ => String::new(),
    }
}

/// Apply a manual recognized-date override to a batch of items belonging to
/// `artist_id`. `manual_date` is `None` for reset (clear the override and
/// restore the latest detected canonical date).
///
/// Validation rejects the whole request on any invalid input: empty or
/// duplicate item ids, non-positive ids, items that do not exist or belong to
/// another artist, or an unparseable date.
pub fn update_item_dates_response(
    conn: &Connection,
    artist_id: i64,
    item_ids: &[i64],
    manual_date: Option<&str>,
) -> Result<Value> {
    let mut requested = item_ids.to_vec();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
        return Err(anyhow!("item_ids must not be empty"));
    }
    if requested.iter().any(|id| *id <= 0) {
        return Err(anyhow!("item_ids must be positive"));
    }
    if requested.len() != item_ids.len() {
        return Err(anyhow!("item_ids must not contain duplicates"));
    }
    let parsed = match manual_date {
        Some(value) => {
            Some(parse_manual_date_input(value).ok_or_else(|| anyhow!("invalid date: {value}"))?)
        }
        None => None,
    };
    if artist_id <= 0 {
        return Err(anyhow!("artist_id must be positive"));
    }

    let placeholders = std::iter::repeat_n("?", requested.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut lookup_params: Vec<rusqlite::types::Value> = requested
        .iter()
        .map(|id| rusqlite::types::Value::Integer(*id))
        .collect();
    lookup_params.push(rusqlite::types::Value::Integer(artist_id));
    let artist_path: String = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .context("look up artist root")?;
    let rows = conn
        .prepare(&format!(
            "SELECT id, detected_date, date, file_path FROM items WHERE id IN ({placeholders}) AND artist_id=?"
        ))?
        .query_map(rusqlite::params_from_iter(lookup_params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("look up target items")?;
    if rows.len() != requested.len() {
        return Err(anyhow!(
            "{} item(s) do not exist or belong to this artist",
            requested.len() - rows.len()
        ));
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin item date update")?;
    let mut updated: Vec<Value> = Vec::with_capacity(rows.len());
    let mut changed_folders: Vec<String> = Vec::new();
    for (item_id, detected_date, legacy_date, file_path) in rows {
        let (new_manual, new_date): (Option<String>, String) = match &parsed {
            Some(raw) => {
                let canonical = canonical_of_raw(raw);
                (Some(raw.clone()), canonical)
            }
            None => {
                let canonical = canonical_of_raw(&detected_date);
                let effective = if canonical.is_empty() {
                    legacy_date.clone()
                } else {
                    canonical
                };
                (None, effective)
            }
        };
        if new_date != legacy_date {
            let folder = item_date_folder_name(&artist_path, &file_path);
            if !folder.is_empty() && !changed_folders.contains(&folder) {
                changed_folders.push(folder);
            }
        }
        let affected = tx
            .execute(
                "UPDATE items SET manual_date=?, date=? WHERE id=?",
                params![new_manual, new_date, item_id],
            )
            .context("update item date")?;
        if affected != 1 {
            return Err(anyhow!("item {item_id} changed during the batch"));
        }
        updated.push(json!({
            "item_id": item_id,
            "date": new_date,
            "detected_date": detected_date,
            "manual_date": new_manual,
            "display_date": effective_display_date(
                &detected_date,
                new_manual.as_deref(),
                &legacy_date,
            ),
        }));
    }
    // Invalidate stale confirmed plans inside the SAME transaction as the
    // date updates: committing first would leave a crash window where
    // confirmed plans keep outdated targets and later execution renames
    // folders destructively.
    let refreshed = crate::folder_archive::invalidate_plans_after_item_date_change(
        &tx,
        artist_id,
        &changed_folders,
    )?;
    tx.commit().context("commit item date update")?;
    Ok(json!({
        "updated": updated.len(),
        "items": updated,
        "plans_invalidated": refreshed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER NOT NULL, file_path TEXT NOT NULL,
                file_name TEXT NOT NULL, file_size INTEGER NOT NULL DEFAULT 0,
                file_mtime REAL NOT NULL DEFAULT 0, folder_name TEXT NOT NULL DEFAULT '',
                date TEXT NOT NULL DEFAULT '', detected_date TEXT NOT NULL DEFAULT '',
                manual_date TEXT DEFAULT NULL, auto_role TEXT NOT NULL DEFAULT '',
                manual_role TEXT DEFAULT NULL, tags TEXT NOT NULL DEFAULT '[]',
                is_archive INTEGER NOT NULL DEFAULT 0, media_type TEXT NOT NULL DEFAULT 'image',
                content_hash TEXT NOT NULL DEFAULT '', hash_status TEXT NOT NULL DEFAULT 'pending',
                hash_updated_at REAL, st_dev INTEGER, st_ino INTEGER,
                missing INTEGER NOT NULL DEFAULT 0, missing_at REAL,
                scanned_at INTEGER NOT NULL DEFAULT 0
            );
            CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT NOT NULL, updated_at REAL);
            CREATE TABLE folder_rename_plans (
                id INTEGER PRIMARY KEY, artist_id INTEGER NOT NULL, source_folder TEXT NOT NULL,
                original_folder_name TEXT NOT NULL DEFAULT '', original_title TEXT NOT NULL DEFAULT '',
                parsed_date TEXT NOT NULL DEFAULT '', selected_tag_ids TEXT NOT NULL DEFAULT '[]',
                status TEXT NOT NULL DEFAULT 'needs_tags', file_count INTEGER NOT NULL DEFAULT 0,
                total_size INTEGER NOT NULL DEFAULT 0, max_mtime REAL NOT NULL DEFAULT 0,
                created_at REAL NOT NULL DEFAULT 0, updated_at REAL NOT NULL DEFAULT 0,
                confirmed_at REAL, confirmation_source TEXT NOT NULL DEFAULT '',
                target_folder TEXT NOT NULL DEFAULT '', executed_at REAL,
                execution_log TEXT NOT NULL DEFAULT '[]', format_snapshot TEXT NOT NULL DEFAULT '{}',
                plan_kind TEXT NOT NULL DEFAULT 'rename_folder', split_actions TEXT NOT NULL DEFAULT '[]'
            );
            INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist');
            INSERT INTO items (id, artist_id, file_path, file_name, date, detected_date) VALUES
                (1, 1, '/pictures/Artist/2026/202607 x/a.jpg', 'a.jpg', '2026-07-01', '2026-07'),
                (2, 1, '/pictures/Artist/2026-05-01/b.jpg', 'b.jpg', '2026-05-01', '2026-05-01'),
                (3, 2, '/pictures/Other/c.jpg', 'c.jpg', '', '');
            INSERT INTO folder_rename_plans
                (id, artist_id, source_folder, status, target_folder, confirmed_at, confirmation_source)
            VALUES
                (1, 1, '202607 x', 'ready', '2026/2026-07 x', 123.0, 'user'),
                (2, 1, '2026-05-01', 'confirmed', '2026/2026-05 x', 456.0, 'user'),
                (3, 1, '2025-03', 'confirmed', '2025/2025-03 x', 789.0, 'user');
            ",
        )
        .unwrap();
        conn
    }

    #[test]
    fn canonical_of_raw_normalizes_precision() {
        assert_eq!(canonical_of_raw("2026-07"), "2026-07-01");
        assert_eq!(canonical_of_raw("2026-07-15"), "2026-07-15");
        assert_eq!(canonical_of_raw(""), "");
        assert_eq!(canonical_of_raw("garbage"), "");
        assert_eq!(canonical_of_raw("2026-7"), "2026-07-01");
    }

    #[test]
    fn folder_name_is_immediate_parent() {
        assert_eq!(
            item_date_folder_name("/pictures/Artist", "/pictures/Artist/2026/202607 x/a.jpg"),
            "202607 x"
        );
        assert_eq!(
            item_date_folder_name("/pictures/Artist", "/pictures/Artist/a.jpg"),
            ""
        );
        assert_eq!(item_date_folder_name("", "/pictures/Artist/a.jpg"), "");
    }

    #[test]
    fn applies_manual_date_to_all_items_atomically() {
        let conn = fixture_conn();
        let result = update_item_dates_response(&conn, 1, &[1, 2], Some("2026-08")).unwrap();
        assert_eq!(result["updated"], 2);
        assert_eq!(result["items"][0]["manual_date"], "2026-08");
        assert_eq!(result["items"][0]["date"], "2026-08-01");
        assert_eq!(result["items"][0]["display_date"], "2026-08");
        assert_eq!(result["items"][1]["display_date"], "2026-08");
        let (date, detected, manual): (String, String, Option<String>) = conn
            .query_row(
                "SELECT date, detected_date, manual_date FROM items WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(date, "2026-08-01");
        assert_eq!(detected, "2026-07");
        assert_eq!(manual, Some("2026-08".to_string()));
    }

    #[test]
    fn reset_restores_detected_date_and_clears_confirmation_on_affected_plans() {
        let conn = fixture_conn();
        let result = update_item_dates_response(&conn, 1, &[1], Some("2026-08")).unwrap();
        assert_eq!(result["updated"], 1);
        assert_eq!(result["plans_invalidated"], 1);
        let (confirmed_at, confirmation_source): (Option<f64>, String) = conn
            .query_row(
                "SELECT confirmed_at, confirmation_source FROM folder_rename_plans WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(confirmed_at, None);
        assert_eq!(confirmation_source, "");

        let result = update_item_dates_response(&conn, 1, &[1], None).unwrap();
        assert_eq!(result["updated"], 1);
        assert_eq!(result["items"][0]["manual_date"], Value::Null);
        assert_eq!(result["items"][0]["date"], "2026-07-01");
        assert_eq!(result["items"][0]["display_date"], "2026-07");
        assert_eq!(
            result["plans_invalidated"], 1,
            "reset re-invalidates the same plan"
        );

        let result = update_item_dates_response(&conn, 1, &[2], None).unwrap();
        assert_eq!(
            result["plans_invalidated"], 0,
            "unchanged date must not invalidate"
        );
    }

    #[test]
    fn confirmed_plans_are_demoted_with_stale_target_cleared() {
        let conn = fixture_conn();
        let result = update_item_dates_response(&conn, 1, &[2], Some("2026-08")).unwrap();
        assert_eq!(result["updated"], 1);
        assert_eq!(result["plans_invalidated"], 1);
        let (status, target, confirmed_at): (String, String, Option<f64>) = conn
            .query_row(
                "SELECT status, target_folder, confirmed_at FROM folder_rename_plans WHERE id=2",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "ready");
        assert_eq!(target, "");
        assert_eq!(confirmed_at, None);
        let (status, target): (String, String) = conn
            .query_row(
                "SELECT status, target_folder FROM folder_rename_plans WHERE id=3",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "confirmed", "unaffected plans stay confirmed");
        assert_eq!(target, "2025/2025-03 x");
    }

    #[test]
    fn rejects_invalid_requests_entirely() {
        let conn = fixture_conn();
        assert!(update_item_dates_response(&conn, 1, &[], Some("2026-08")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[1, 1], Some("2026-08")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[-1], Some("2026-08")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[1], Some("2026-13")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[1], Some("2026-02-31")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[1], Some("garbage")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[3], Some("2026-08")).is_err());
        assert!(update_item_dates_response(&conn, 1, &[1, 3], Some("2026-08")).is_err());
        let (date, manual): (String, Option<String>) = conn
            .query_row(
                "SELECT date, manual_date FROM items WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(date, "2026-07-01", "rejected requests must not write");
        assert_eq!(manual, None);
    }

    #[test]
    fn full_day_manual_date_keeps_its_day() {
        let conn = fixture_conn();
        let result = update_item_dates_response(&conn, 1, &[1], Some("2026-09-15")).unwrap();
        assert_eq!(result["items"][0]["manual_date"], "2026-09-15");
        assert_eq!(result["items"][0]["date"], "2026-09-15");
        assert_eq!(result["items"][0]["display_date"], "2026-09-15");
    }
}
