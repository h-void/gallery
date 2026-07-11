use anyhow::{Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

struct ExistingCandidate {
    file_path: String,
    file_name: String,
    file_size: i64,
    file_mtime: f64,
    media_type: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
}

struct HashUniqueCandidate {
    id: i64,
    scan_id: String,
    artist_id: i64,
    file_path: String,
    file_name: String,
    file_size: i64,
    file_mtime: f64,
    folder_name: String,
    date: String,
    is_archive: i64,
    media_type: String,
    content_hash: String,
    hash_status: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
}

/// A pending scan candidate paired with the missing item Python already matched
/// for an inode or category-rename move. Unlike [`HashUniqueCandidate`] this does
/// not require `hash_status='done'` because inode/category matches can fire while
/// hashing is still in progress.
struct MoveTargetCandidate {
    id: i64,
    artist_id: i64,
    file_path: String,
    file_name: String,
    file_size: i64,
    file_mtime: f64,
    folder_name: String,
    date: String,
    is_archive: i64,
    media_type: String,
    content_hash: String,
    hash_status: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
}

struct ItemMissing {
    id: i64,
    artist_id: i64,
    file_path: String,
    missing: i64,
}

const ALLOWED_MOVE_REASONS: &[&str] = &["inode", "category_rename"];

fn inferred_media_type(file_name: &str, media_type: &str) -> String {
    if !media_type.is_empty() {
        return media_type.to_string();
    }
    let ext = file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase());
    match ext.as_deref() {
        Some(
            "mp4" | "mkv" | "mov" | "webm" | "avi" | "wmv" | "m4v" | "mpg" | "mpeg" | "ts" | "m2ts"
            | "flv" | "3gp",
        ) => "video",
        Some("psd" | "psb" | "clip" | "tga" | "dds") => "source",
        Some("rar" | "zip" | "7z" | "tar" | "gz" | "bz2" | "xz") => "archive",
        Some("txt" | "md" | "html" | "htm") => "text",
        _ => "image",
    }
    .to_string()
}

pub fn resolve_existing_scan_candidate_response(
    conn: &Connection,
    candidate_id: i64,
) -> Result<Value> {
    let candidate = conn
        .query_row(
            "
            SELECT file_path, file_name, file_size, file_mtime, media_type, st_dev, st_ino
            FROM scan_candidates
            WHERE id = ?1 AND status IN ('pending', 'candidate')
            ",
            params![candidate_id],
            |row| {
                Ok(ExistingCandidate {
                    file_path: row.get(0)?,
                    file_name: row.get(1)?,
                    file_size: row.get(2)?,
                    file_mtime: row.get(3)?,
                    media_type: row.get(4)?,
                    st_dev: row.get(5)?,
                    st_ino: row.get(6)?,
                })
            },
        )
        .optional()
        .context("fetch scan candidate")?;
    let Some(candidate) = candidate else {
        return Ok(json!({"action": "no_match"}));
    };

    let item_id = conn
        .query_row(
            "SELECT id FROM items WHERE file_path = ?1 LIMIT 1",
            params![candidate.file_path],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("fetch same-path item")?;
    let Some(item_id) = item_id else {
        return Ok(json!({"action": "no_match"}));
    };

    conn.execute(
        "
        UPDATE items
        SET missing = 0,
            missing_at = NULL,
            file_size = ?1,
            file_mtime = ?2,
            media_type = ?3,
            st_dev = ?4,
            st_ino = ?5,
            scanned_at = strftime('%s','now')
        WHERE id = ?6
        ",
        params![
            candidate.file_size,
            candidate.file_mtime,
            inferred_media_type(&candidate.file_name, &candidate.media_type),
            candidate.st_dev,
            candidate.st_ino,
            item_id,
        ],
    )
    .context("update same-path item")?;
    conn.execute(
        "
        UPDATE scan_candidates
        SET status = 'resolved', resolved_at = strftime('%s','now')
        WHERE id = ?1
        ",
        params![candidate_id],
    )
    .context("mark scan candidate resolved")?;

    Ok(json!({"action": "existing", "item_id": item_id}))
}

fn hash_unique_candidate(
    conn: &Connection,
    candidate_id: i64,
) -> Result<Option<HashUniqueCandidate>> {
    conn.query_row(
        "
        SELECT id, scan_id, artist_id, file_path, file_name, file_size, file_mtime,
               folder_name, date, is_archive, media_type, content_hash, hash_status,
               st_dev, st_ino
        FROM scan_candidates
        WHERE id = ?1
          AND status IN ('pending', 'candidate')
          AND hash_status = 'done'
          AND content_hash != ''
        ",
        params![candidate_id],
        |row| {
            Ok(HashUniqueCandidate {
                id: row.get(0)?,
                scan_id: row.get(1)?,
                artist_id: row.get(2)?,
                file_path: row.get(3)?,
                file_name: row.get(4)?,
                file_size: row.get(5)?,
                file_mtime: row.get(6)?,
                folder_name: row.get(7)?,
                date: row.get(8)?,
                is_archive: row.get(9)?,
                media_type: row.get(10)?,
                content_hash: row.get(11)?,
                hash_status: row.get(12)?,
                st_dev: row.get(13)?,
                st_ino: row.get(14)?,
            })
        },
    )
    .optional()
    .context("fetch hash-unique scan candidate")
}

fn unique_missing_hash_item_id(
    conn: &Connection,
    candidate: &HashUniqueCandidate,
) -> Result<Option<i64>> {
    let ids = {
        let mut stmt = conn
            .prepare(
                "
                SELECT id
                FROM items
                WHERE artist_id = ?1
                  AND missing = 1
                  AND hash_status = 'done'
                  AND content_hash = ?2
                ORDER BY id
                ",
            )
            .context("prepare missing hash item query")?;
        let rows = stmt
            .query_map(
                params![candidate.artist_id, &candidate.content_hash],
                |row| row.get::<_, i64>(0),
            )
            .context("query missing hash item ids")?;
        rows.collect::<rusqlite::Result<Vec<_>>>()
            .context("collect missing hash item ids")?
    };
    if ids.len() == 1 {
        Ok(Some(ids[0]))
    } else {
        Ok(None)
    }
}

fn active_duplicate_count(conn: &Connection, candidate: &HashUniqueCandidate) -> Result<i64> {
    conn.query_row(
        "
        SELECT COUNT(*)
        FROM scan_seen ss
        JOIN items i
          ON i.artist_id = ss.artist_id
         AND i.file_path = ss.file_path
        WHERE ss.scan_id = ?1
          AND ss.artist_id = ?2
          AND ss.file_path != ?3
          AND ss.hash_status = 'done'
          AND ss.content_hash = ?4
          AND i.missing = 0
        ",
        params![
            &candidate.scan_id,
            candidate.artist_id,
            &candidate.file_path,
            &candidate.content_hash,
        ],
        |row| row.get(0),
    )
    .context("count active same-hash duplicates")
}

fn rollback(conn: &Connection) {
    let _ = conn.execute_batch("ROLLBACK");
}

pub fn apply_hash_unique_scan_candidate_response(
    conn: &Connection,
    candidate_id: i64,
) -> Result<Value> {
    let Some(candidate) = hash_unique_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };
    let Some(item_id) = unique_missing_hash_item_id(conn, &candidate)? else {
        return Ok(json!({"action": "no_match"}));
    };
    if active_duplicate_count(conn, &candidate)? != 0 {
        return Ok(json!({"action": "no_match"}));
    }

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin hash-unique move")?;
    let result = (|| -> Result<Option<String>> {
        let old_path = conn
            .query_row(
                "SELECT file_path FROM items WHERE id = ?1 AND missing = 1",
                params![item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("refresh missing hash item")?;
        let Some(old_path) = old_path else {
            return Ok(None);
        };
        let occupied = conn
            .query_row(
                "SELECT id FROM items WHERE file_path = ?1 AND id != ?2 LIMIT 1",
                params![&candidate.file_path, item_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("check hash-unique target occupancy")?;
        if occupied.is_some() {
            return Ok(None);
        }

        conn.execute(
            "
            UPDATE items
            SET file_path = ?1,
                file_name = ?2,
                file_size = ?3,
                file_mtime = ?4,
                folder_name = ?5,
                date = ?6,
                is_archive = ?7,
                media_type = ?8,
                content_hash = ?9,
                hash_status = ?10,
                hash_updated_at = strftime('%s','now'),
                st_dev = ?11,
                st_ino = ?12,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?13
            ",
            params![
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &candidate.date,
                candidate.is_archive,
                inferred_media_type(&candidate.file_name, &candidate.media_type),
                &candidate.content_hash,
                &candidate.hash_status,
                candidate.st_dev,
                candidate.st_ino,
                item_id,
            ],
        )
        .context("update hash-unique moved item")?;
        conn.execute(
            "
            INSERT INTO move_history
                (item_id, artist_id, old_path, new_path, reason, status, applied_at)
            VALUES (?1, ?2, ?3, ?4, 'hash_unique', 'applied', strftime('%s','now'))
            ",
            params![
                item_id,
                candidate.artist_id,
                &old_path,
                &candidate.file_path
            ],
        )
        .context("insert hash-unique move history")?;
        conn.execute(
            "
            UPDATE scan_candidates
            SET status = 'resolved', resolved_at = strftime('%s','now')
            WHERE id = ?1
            ",
            params![candidate.id],
        )
        .context("mark hash-unique scan candidate resolved")?;
        conn.execute(
            "
            UPDATE move_candidates
            SET status = 'applied', resolved_at = strftime('%s','now')
            WHERE scan_candidate_id = ?1 AND status = 'pending'
            ",
            params![candidate.id],
        )
        .context("mark pending move candidates applied")?;
        Ok(Some(old_path))
    })();

    match result {
        Ok(Some(_)) => {
            conn.execute_batch("COMMIT")
                .context("commit hash-unique move")?;
            Ok(json!({"action": "moved", "item_id": item_id, "reason": "hash_unique"}))
        }
        Ok(None) => {
            rollback(conn);
            Ok(json!({"action": "no_match"}))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

/// Create a brand-new `items` row from a still-pending scan candidate when the
/// resolver finds no match to an existing item. Mirrors the Python
/// `_create_new_item` write path: revalidate the candidate is still pending and
/// that no item already occupies the candidate path, then insert the new item
/// row, mark the scan candidate `new`, and mark any pending `move_candidates`
/// for the same candidate `new`. Returns `{"action":"new","item_id":...}` on
/// success or `{"action":"no_match"}` when a precondition is no longer satisfied
/// (or a unique-constraint error occurs) so Python can fall back to the more
/// complex `_mark_existing_item_for_candidate` path.
pub fn create_new_item_response(conn: &Connection, candidate_id: i64) -> Result<Value> {
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin scan-candidate create-new-item")?;
    let result = (|| -> Result<Option<i64>> {
        let occupied = conn
            .query_row(
                "SELECT id FROM items WHERE file_path = ?1 LIMIT 1",
                params![&candidate.file_path],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("check scan-candidate new-item path occupancy")?;
        if occupied.is_some() {
            return Ok(None);
        }

        let media_type = inferred_media_type(&candidate.file_name, &candidate.media_type);
        let inserted = conn.execute(
            "
            INSERT INTO items
                (artist_id, file_path, file_name, file_size, file_mtime,
                 folder_name, date, auto_role, tags, is_archive, media_type,
                 content_hash, hash_status, hash_updated_at, st_dev, st_ino,
                 missing, missing_at, scanned_at)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, '', '[]', ?8, ?9, ?10, ?11,
                    CASE WHEN ?11 = 'done' THEN strftime('%s','now') ELSE NULL END,
                    ?12, ?13, 0, NULL, strftime('%s','now'))
            ",
            params![
                candidate.artist_id,
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &candidate.date,
                candidate.is_archive,
                &media_type,
                &candidate.content_hash,
                &candidate.hash_status,
                candidate.st_dev,
                candidate.st_ino,
            ],
        );
        let inserted = match inserted {
            Ok(count) => count,
            Err(error) if error.to_string().contains("UNIQUE") => return Ok(None),
            Err(error) => return Err(error).context("insert new scan-candidate item"),
        };
        if inserted == 0 {
            return Ok(None);
        }
        let item_id = conn.last_insert_rowid();

        conn.execute(
            "
            UPDATE scan_candidates
            SET status = 'new', resolved_at = strftime('%s','now')
            WHERE id = ?1
            ",
            params![candidate.id],
        )
        .context("mark new scan candidate")?;
        conn.execute(
            "
            UPDATE move_candidates
            SET status = 'new', resolved_at = strftime('%s','now')
            WHERE scan_candidate_id = ?1 AND status = 'pending'
            ",
            params![candidate.id],
        )
        .context("mark pending move candidates new")?;
        Ok(Some(item_id))
    })();

    match result {
        Ok(Some(item_id)) => {
            conn.execute_batch("COMMIT")
                .context("commit scan-candidate create-new-item")?;
            Ok(json!({"action": "new", "item_id": item_id}))
        }
        Ok(None) => {
            rollback(conn);
            Ok(json!({"action": "no_match"}))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

fn move_target_candidate(
    conn: &Connection,
    candidate_id: i64,
) -> Result<Option<MoveTargetCandidate>> {
    conn.query_row(
        "
        SELECT id, artist_id, file_path, file_name, file_size, file_mtime,
               folder_name, date, is_archive, media_type, content_hash, hash_status,
               st_dev, st_ino
        FROM scan_candidates
        WHERE id = ?1 AND status IN ('pending', 'candidate')
        ",
        params![candidate_id],
        |row| {
            Ok(MoveTargetCandidate {
                id: row.get(0)?,
                artist_id: row.get(1)?,
                file_path: row.get(2)?,
                file_name: row.get(3)?,
                file_size: row.get(4)?,
                file_mtime: row.get(5)?,
                folder_name: row.get(6)?,
                date: row.get(7)?,
                is_archive: row.get(8)?,
                media_type: row.get(9)?,
                content_hash: row.get(10)?,
                hash_status: row.get(11)?,
                st_dev: row.get(12)?,
                st_ino: row.get(13)?,
            })
        },
    )
    .optional()
    .context("fetch scan-candidate move target")
}

/// Apply an inode or category-rename move that Python already resolved to a
/// single missing item. Mirrors the Python `_apply_move` write path: revalidate
/// the item is still missing, ensure the target path is not occupied by another
/// item, overwrite the item row with the candidate metadata, insert a
/// `move_history` row with the supplied reason, mark the scan candidate
/// resolved, and mark any pending `move_candidates` for the same candidate
/// applied. Returns `{"action":"moved",...}` on success or `{"action":"no_match"}`
/// when a precondition is no longer satisfied so Python can fall back.
pub fn apply_scan_candidate_move_response(
    conn: &Connection,
    candidate_id: i64,
    item_id: i64,
    reason: &str,
) -> Result<Value> {
    if !ALLOWED_MOVE_REASONS.contains(&reason) {
        return Ok(json!({"action": "no_match"}));
    }
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin scan-candidate move")?;
    let result = (|| -> Result<Option<String>> {
        let old_path = conn
            .query_row(
                "SELECT file_path FROM items WHERE id = ?1 AND missing = 1",
                params![item_id],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .context("refresh missing scan-candidate item")?;
        let Some(old_path) = old_path else {
            return Ok(None);
        };
        let occupied = conn
            .query_row(
                "SELECT id FROM items WHERE file_path = ?1 AND id != ?2 LIMIT 1",
                params![&candidate.file_path, item_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("check scan-candidate move target occupancy")?;
        if occupied.is_some() {
            return Ok(None);
        }

        conn.execute(
            "
            UPDATE items
            SET file_path = ?1,
                file_name = ?2,
                file_size = ?3,
                file_mtime = ?4,
                folder_name = ?5,
                date = ?6,
                is_archive = ?7,
                media_type = ?8,
                content_hash = ?9,
                hash_status = ?10,
                hash_updated_at = CASE WHEN ?10 = 'done' THEN strftime('%s','now') ELSE NULL END,
                st_dev = ?11,
                st_ino = ?12,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?13
            ",
            params![
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &candidate.date,
                candidate.is_archive,
                inferred_media_type(&candidate.file_name, &candidate.media_type),
                &candidate.content_hash,
                &candidate.hash_status,
                candidate.st_dev,
                candidate.st_ino,
                item_id,
            ],
        )
        .context("update moved scan-candidate item")?;
        conn.execute(
            "
            INSERT INTO move_history
                (item_id, artist_id, old_path, new_path, reason, status, applied_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'applied', strftime('%s','now'))
            ",
            params![
                item_id,
                candidate.artist_id,
                &old_path,
                &candidate.file_path,
                reason,
            ],
        )
        .context("insert scan-candidate move history")?;
        conn.execute(
            "
            UPDATE scan_candidates
            SET status = 'resolved', resolved_at = strftime('%s','now')
            WHERE id = ?1
            ",
            params![candidate.id],
        )
        .context("mark scan-candidate move resolved")?;
        conn.execute(
            "
            UPDATE move_candidates
            SET status = 'applied', resolved_at = strftime('%s','now')
            WHERE scan_candidate_id = ?1 AND status = 'pending'
            ",
            params![candidate.id],
        )
        .context("mark pending move candidates applied")?;
        Ok(Some(old_path))
    })();

    match result {
        Ok(Some(_)) => {
            conn.execute_batch("COMMIT")
                .context("commit scan-candidate move")?;
            Ok(json!({"action": "moved", "item_id": item_id, "reason": reason}))
        }
        Ok(None) => {
            rollback(conn);
            Ok(json!({"action": "no_match"}))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

struct MoveCandidateRow {
    id: i64,
    item_id: i64,
    scan_candidate_id: Option<i64>,
    new_path: String,
    artist_id: i64,
    reason: String,
    content_hash: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
}

/// Build the empty-metadata defaults Python uses for a synthetic candidate when
/// no `scan_candidate` row is linked to the `move_candidate`.
/// Returns `(file_size, file_mtime, folder_name, date, is_archive, media_type,
/// content_hash, hash_status)` matching the linked-scan_candidate SELECT shape.
fn synthetic_move_metadata(new_path: &str) -> (i64, f64, String, String, i64, String, String, String) {
    let file_name = path_file_name(new_path);
    let media = inferred_media_type(file_name, "");
    (
        0,
        0.0,
        String::new(),
        String::new(),
        0,
        media,
        String::new(),
        String::new(),
    )
}

fn path_file_name(path: &str) -> &str {
    path.rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(path)
}

/// Apply a manually-confirmed `move_candidate` (the "confirm move" button) via
/// the Rust sidecar. Mirrors the Python `confirm_move_candidate` -> `_apply_move`
/// write path for the same-artist, non-duplicate case: revalidate the
/// move_candidate is still pending, the target item is still missing and belongs
/// to the same artist, and the new path is unoccupied; then overwrite the item
/// row (using the linked `scan_candidate` metadata when present, otherwise the
/// synthetic empty-metadata defaults), record `move_history`, resolve the linked
/// scan candidate, and mark this and any sibling pending `move_candidates`
/// applied. Cross-artist moves are declined (`no_match`) so Python can run the
/// tag-migration path. Returns `{"action":"moved",...}` on success or
/// `{"action":"no_match"}` for Python fallback.
pub fn apply_move_candidate_response(
    conn: &Connection,
    move_candidate_id: i64,
) -> Result<Value> {
    let move_row = conn
        .query_row(
            "
            SELECT id, item_id, scan_candidate_id, new_path, artist_id, reason,
                   content_hash, st_dev, st_ino
            FROM move_candidates
            WHERE id = ?1 AND status = 'pending'
            ",
            params![move_candidate_id],
            |row| {
                Ok(MoveCandidateRow {
                    id: row.get(0)?,
                    item_id: row.get(1)?,
                    scan_candidate_id: row.get(2)?,
                    new_path: row.get(3)?,
                    artist_id: row.get(4)?,
                    reason: row.get(5)?,
                    content_hash: row.get(6)?,
                    st_dev: row.get(7)?,
                    st_ino: row.get(8)?,
                })
            },
        )
        .optional()
        .context("fetch move candidate")?;
    let Some(move_row) = move_row else {
        return Ok(json!({"action": "no_match"}));
    };

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin move-candidate apply")?;
    let result = (|| -> Result<
        Option<(i64, String, String, String, i64)>,
    > {
        let item = conn
            .query_row(
                "SELECT id, artist_id, file_path, missing FROM items WHERE id = ?1",
                params![move_row.item_id],
                |row| {
                    Ok(ItemMissing {
                        id: row.get(0)?,
                        artist_id: row.get(1)?,
                        file_path: row.get(2)?,
                        missing: row.get(3)?,
                    })
                },
            )
            .optional()
            .context("refresh move-candidate item")?;
        let Some(item) = item else {
            return Ok(None);
        };
        if item.missing != 1 {
            return Ok(None);
        }
        if item.artist_id != move_row.artist_id {
            // cross-artist: keep Python path (tag migration lives there)
            return Ok(None);
        }
        let occupied = conn
            .query_row(
                "SELECT id FROM items WHERE file_path = ?1 AND id != ?2 LIMIT 1",
                params![&move_row.new_path, move_row.item_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("check move-candidate target occupancy")?;
        if occupied.is_some() {
            return Ok(None);
        }

        let (file_size, file_mtime, folder_name, date, is_archive, media_type, mut content_hash, _sc_hash_status):
            (i64, f64, String, String, i64, String, String, String) =
            match move_row.scan_candidate_id {
                Some(sc_id) if sc_id > 0 => {
                    let sc = conn
                        .query_row(
                            "
                            SELECT file_size, file_mtime, folder_name, date, is_archive,
                                   media_type, content_hash, hash_status
                            FROM scan_candidates WHERE id = ?1
                            ",
                            params![sc_id],
                            |row| {
                                Ok((
                                    row.get::<_, i64>(0)?,
                                    row.get::<_, f64>(1)?,
                                    row.get::<_, String>(2)?,
                                    row.get::<_, String>(3)?,
                                    row.get::<_, i64>(4)?,
                                    row.get::<_, String>(5)?,
                                    row.get::<_, String>(6)?,
                                    row.get::<_, String>(7)?,
                                ))
                            },
                        )
                        .optional()
                        .context("fetch linked scan candidate")?;
                    match sc {
                        Some(t) => t,
                        None => synthetic_move_metadata(&move_row.new_path),
                    }
                }
                _ => synthetic_move_metadata(&move_row.new_path),
            };
        // Prefer hash recorded on the move_candidate when present (Python does this for synthetic).
        if !move_row.content_hash.is_empty() {
            content_hash = move_row.content_hash.clone();
        }
        let file_name = path_file_name(&move_row.new_path).to_string();
        let media = inferred_media_type(&file_name, &media_type);
        let hash_status = if content_hash.is_empty() {
            "pending"
        } else {
            "done"
        };

        conn.execute(
            "
            UPDATE items
            SET file_path = ?1,
                file_name = ?2,
                file_size = ?3,
                file_mtime = ?4,
                folder_name = ?5,
                date = ?6,
                is_archive = ?7,
                media_type = ?8,
                content_hash = ?9,
                hash_status = ?10,
                hash_updated_at = CASE WHEN ?10 = 'done' THEN strftime('%s','now') ELSE NULL END,
                st_dev = ?11,
                st_ino = ?12,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?13
            ",
            params![
                &move_row.new_path,
                &file_name,
                file_size,
                file_mtime,
                &folder_name,
                &date,
                is_archive,
                &media,
                &content_hash,
                hash_status,
                move_row.st_dev,
                move_row.st_ino,
                move_row.item_id,
            ],
        )
        .context("update confirmed move item")?;
        conn.execute(
            "
            INSERT INTO move_history
                (item_id, artist_id, old_path, new_path, reason, status, applied_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'applied', strftime('%s','now'))
            ",
            params![
                move_row.item_id,
                move_row.artist_id,
                &item.file_path,
                &move_row.new_path,
                &move_row.reason,
            ],
        )
        .context("insert confirmed move history")?;
        if let Some(sc_id) = move_row.scan_candidate_id {
            if sc_id > 0 {
                conn.execute(
                    "UPDATE scan_candidates SET status='resolved', resolved_at=strftime('%s','now') WHERE id=?1",
                    params![sc_id],
                )
                .context("mark linked scan candidate resolved")?;
                conn.execute(
                    "UPDATE move_candidates SET status='applied', resolved_at=strftime('%s','now') WHERE scan_candidate_id=?1 AND status='pending'",
                    params![sc_id],
                )
                .context("mark sibling move candidates applied")?;
            }
        }
        conn.execute(
            "UPDATE move_candidates SET status='applied', resolved_at=strftime('%s','now') WHERE id=?1",
            params![move_row.id],
        )
        .context("mark move candidate applied")?;
        Ok(Some((
            move_row.item_id,
            move_row.reason.clone(),
            item.file_path.clone(),
            move_row.new_path.clone(),
            move_row.artist_id,
        )))
    })();

    match result {
        Ok(Some((item_id, reason, _old, _new, _artist))) => {
            conn.execute_batch("COMMIT")
                .context("commit move-candidate apply")?;
            Ok(json!({"action": "moved", "item_id": item_id, "reason": reason}))
        }
        Ok(None) => {
            rollback(conn);
            Ok(json!({"action": "no_match"}))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

/// Ignore a single `move_candidate` (the "ignore candidate" button) via the
/// Rust sidecar. Mirrors the Python `ignore_move_candidate` write: a single
/// status flip to `ignored` scoped to still-`pending` rows, returning the
/// number of affected rows. Always succeeds with `{"action":"ignored"}` (the
/// Python fallback is behaviourally identical for missing/non-pending ids,
/// returning `updated: 0`).
pub fn ignore_move_candidate_response(
    conn: &Connection,
    move_candidate_id: i64,
) -> Result<Value> {
    let updated = conn
        .execute(
            "
            UPDATE move_candidates
            SET status = 'ignored', resolved_at = strftime('%s','now')
            WHERE id = ?1 AND status = 'pending'
            ",
            params![move_candidate_id],
        )
        .context("ignore move candidate")?;
    Ok(json!({"action": "ignored", "updated": updated as i64}))
}

/// Mark a single `move_candidate` as a new item (the "treat as new" button) via
/// the Rust sidecar. Mirrors the Python `mark_move_candidate_new` write: fetch
/// the linked `scan_candidate_id`, delegate the new-item creation to
/// [`create_new_item_response`], and on success flip this `move_candidate` to
/// `new`. A missing or unlinked `scan_candidate` returns `missing` so Python
/// can fall back; a `no_match` from the new-item step (occupied target) does
/// the same, letting Python run the `_mark_existing_item_for_candidate` path.
pub fn mark_move_candidate_new_response(
    conn: &Connection,
    move_candidate_id: i64,
) -> Result<Value> {
    let scan_candidate_id: Option<Option<i64>> = conn
        .query_row(
            "
            SELECT scan_candidate_id
            FROM move_candidates
            WHERE id = ?1 AND status = 'pending'
            ",
            params![move_candidate_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .context("fetch move candidate for mark-new")?;
    let Some(scan_candidate_id) = scan_candidate_id.flatten().filter(|id| *id > 0) else {
        return Ok(json!({"action": "missing", "reason": "scan_candidate_missing"}));
    };

    let created = create_new_item_response(conn, scan_candidate_id)?;
    if created
        .get("action")
        .and_then(|a| a.as_str())
        == Some("new")
    {
        conn.execute(
            "
            UPDATE move_candidates
            SET status = 'new', resolved_at = strftime('%s','now')
            WHERE id = ?1
            ",
            params![move_candidate_id],
        )
        .context("mark move candidate new")?;
    }
    Ok(created)
}
