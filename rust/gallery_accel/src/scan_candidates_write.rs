use anyhow::{bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::path::Path;
use std::time::UNIX_EPOCH;

use crate::content_hash::hash_file;
use crate::dimensions::media_dimensions;
use crate::media_roots::{authorized_media_path, MediaRoots};

#[derive(Clone)]
struct MoveTargetCandidate {
    id: i64,
    artist_id: i64,
    artist_path: String,
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

/// Derive the precision-preserving raw date from the candidate's folder chain
/// relative to its artist root (mirroring `scan.rs`), falling back to the
/// candidate's canonical `date` when the folder no longer reveals precision.
fn candidate_detected_raw(candidate: &MoveTargetCandidate) -> String {
    let artist = candidate.artist_path.trim_end_matches('/');
    let full = candidate.file_path.trim_end_matches('/');
    let chain = if artist.is_empty() || full == artist {
        ""
    } else if let Some(rest) = full.strip_prefix(&format!("{artist}/")) {
        rest
    } else {
        full
    };
    let date_folder = chain.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    crate::media_type::extract_date_value_from_folder(date_folder)
        .map(|value| value.raw)
        .unwrap_or_else(|| candidate.date.clone())
}

struct ItemMissing {
    artist_id: i64,
    file_path: String,
    file_name: String,
    file_size: i64,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
    missing: i64,
    content_hash: String,
}

const ALLOWED_MOVE_REASONS: &[&str] = &["inode", "category_rename"];

#[cfg(unix)]
fn metadata_identity(metadata: &std::fs::Metadata) -> Option<(i64, i64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev() as i64, metadata.ino() as i64))
}

#[cfg(not(unix))]
fn metadata_identity(_metadata: &std::fs::Metadata) -> Option<(i64, i64)> {
    None
}

fn candidate_file_is_current(
    roots: Option<&MediaRoots>,
    candidate: &MoveTargetCandidate,
) -> Result<bool> {
    let path = match roots {
        // Outside the configured media roots: treat as stale evidence without
        // touching the filesystem; callers supersede the row conservatively.
        Some(roots) => match authorized_media_path(roots, &candidate.file_path) {
            Ok(path) => path,
            Err(_) => return Ok(false),
        },
        None => Path::new(&candidate.file_path).to_path_buf(),
    };
    candidate_file_is_current_path(&path, candidate)
}

fn candidate_file_is_current_path(path: &Path, candidate: &MoveTargetCandidate) -> Result<bool> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(false),
    };
    let file_size = metadata.len() as i64;
    let file_mtime = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    let metadata_matches =
        candidate.file_size == file_size && (candidate.file_mtime - file_mtime).abs() < 1.0;
    let identity = metadata_identity(&metadata);
    let identity_matches = match ((candidate.st_dev, candidate.st_ino), identity) {
        ((Some(expected_dev), Some(expected_ino)), Some(actual)) => {
            (expected_dev, expected_ino) == actual
        }
        _ => true,
    };
    let hash_matches = candidate.hash_status != "done"
        || (!candidate.content_hash.is_empty()
            && hash_file(path, 1024 * 1024).is_ok_and(|digest| digest == candidate.content_hash));
    if metadata_matches && identity_matches && hash_matches {
        return Ok(true);
    }
    Ok(false)
}

/// Stat-only identity check for use INSIDE a write transaction.
///
/// The full content hash runs outside the lock (`candidate_file_is_current`);
/// inside BEGIN IMMEDIATE we only confirm size/mtime/dev/ino are unchanged,
/// which shrinks the write-lock hold time from seconds-long hashes to stat
/// calls. This narrows the TOCTOU window; it is not an atomic file snapshot.
fn candidate_stat_is_current(roots: Option<&MediaRoots>, candidate: &MoveTargetCandidate) -> bool {
    let path = match roots {
        Some(roots) => authorized_media_path(roots, &candidate.file_path),
        None => Ok(Path::new(&candidate.file_path).to_path_buf()),
    };
    let Ok(path) = path else {
        return false;
    };
    let Some(metadata) = std::fs::metadata(path).ok().filter(|m| m.is_file()) else {
        return false;
    };
    let file_size = metadata.len() as i64;
    let file_mtime = metadata
        .modified()
        .ok()
        .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    if candidate.file_size != file_size || (candidate.file_mtime - file_mtime).abs() >= 1.0 {
        return false;
    }
    let identity = metadata_identity(&metadata);
    match ((candidate.st_dev, candidate.st_ino), identity) {
        ((Some(expected_dev), Some(expected_ino)), Some(actual)) => {
            (expected_dev, expected_ino) == actual
        }
        _ => true,
    }
}

/// In-transaction content verification for durable `hash_status='done'` writes.
///
/// `candidate_stat_is_current` cannot detect an in-place rewrite that preserves
/// size, mtime, and dev/inode, so a digest computed earlier could be committed
/// for bytes that are no longer on disk. Re-reading the file and comparing with
/// the candidate hash immediately before the durable write closes that window;
/// a mismatch fails the transaction as `candidate_stale`, which rolls back and
/// requeues the candidate for a fresh hash.
fn candidate_content_is_current(
    roots: Option<&MediaRoots>,
    candidate: &MoveTargetCandidate,
) -> bool {
    let path = match roots {
        Some(roots) => authorized_media_path(roots, &candidate.file_path),
        None => Ok(Path::new(&candidate.file_path).to_path_buf()),
    };
    let Ok(path) = path else {
        return false;
    };
    !candidate.content_hash.is_empty()
        && hash_file(&path, 1024 * 1024).is_ok_and(|digest| digest == candidate.content_hash)
}

fn supersede_candidate(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    candidate: &MoveTargetCandidate,
) -> Result<()> {
    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin scan-candidate supersede")?;
    let result = (|| -> Result<()> {
        // Guard and UPDATE share one write transaction: a concurrent rescan
        // rewriting this row must flip the guard before it can be clobbered.
        if !candidate_row_is_unchanged(conn, candidate)? {
            return Ok(());
        }
        let path = match roots {
            Some(roots) => authorized_media_path(roots, &candidate.file_path),
            None => Ok(Path::new(&candidate.file_path).to_path_buf()),
        };
        match path.ok().and_then(|path| std::fs::metadata(path).ok()) {
            Some(metadata) if metadata.is_file() => {
                // The file was replaced: keep the row eligible for a fresh hash of
                // the new content once a rescan requeues it.
                let file_size = metadata.len() as i64;
                let file_mtime = metadata
                    .modified()
                    .ok()
                    .and_then(|mtime| mtime.duration_since(UNIX_EPOCH).ok())
                    .map(|duration| duration.as_secs_f64())
                    .unwrap_or(0.0);
                let (st_dev, st_ino) = metadata_identity(&metadata)
                    .map(|value| (Some(value.0), Some(value.1)))
                    .unwrap_or((candidate.st_dev, candidate.st_ino));
                conn.execute(
                    "UPDATE scan_candidates
                     SET file_size=?, file_mtime=?, content_hash='', hash_status='pending',
                         st_dev=?, st_ino=?, status='superseded',
                         resolved_at=strftime('%s','now')
                     WHERE id=? AND status IN ('pending','candidate')",
                    params![file_size, file_mtime, st_dev, st_ino, candidate.id],
                )?;
            }
            _ => {
                conn.execute(
                    "UPDATE scan_candidates
                     SET content_hash='', hash_status='error', status='superseded',
                         resolved_at=strftime('%s','now')
                     WHERE id=? AND status IN ('pending','candidate')",
                    params![candidate.id],
                )?;
            }
        }
        conn.execute(
            "UPDATE move_candidates SET status='superseded', resolved_at=strftime('%s','now')
             WHERE scan_candidate_id=? AND status='pending'",
            params![candidate.id],
        )?;
        Ok(())
    })();
    match result {
        Ok(()) => {
            conn.execute_batch("COMMIT")
                .context("commit scan-candidate supersede")?;
            Ok(())
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

fn candidate_row_is_unchanged(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<bool> {
    let current = conn
        .query_row(
            "SELECT artist_id, file_path, file_name, file_size, file_mtime,
                    folder_name, date, is_archive, media_type, content_hash,
                    hash_status, st_dev, st_ino, status
             FROM scan_candidates WHERE id=?",
            params![candidate.id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, String>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, String>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, Option<i64>>(11)?,
                    row.get::<_, Option<i64>>(12)?,
                    row.get::<_, String>(13)?,
                ))
            },
        )
        .optional()?;
    Ok(current.is_some_and(|row| {
        row.0 == candidate.artist_id
            && row.1 == candidate.file_path
            && row.2 == candidate.file_name
            && row.3 == candidate.file_size
            && row.4 == candidate.file_mtime
            && row.5 == candidate.folder_name
            && row.6 == candidate.date
            && row.7 == candidate.is_archive
            && row.8 == candidate.media_type
            && row.9 == candidate.content_hash
            && row.10 == candidate.hash_status
            && row.11 == candidate.st_dev
            && row.12 == candidate.st_ino
            && matches!(row.13.as_str(), "pending" | "candidate")
    }))
}

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

/// Probe dimensions before promoting a scan candidate; failures leave zero so
/// the maintenance backfill can retry without blocking item creation.
fn candidate_dimensions(
    roots: Option<&MediaRoots>,
    candidate: &MoveTargetCandidate,
    media_type: &str,
) -> (i64, i64) {
    let path = match roots {
        Some(roots) => authorized_media_path(roots, &candidate.file_path).ok(),
        None => Some(Path::new(&candidate.file_path).to_path_buf()),
    };
    path.and_then(|path| media_dimensions(&path, media_type).ok())
        .filter(|(width, height)| *width > 0 && *height > 0)
        .map(|(width, height)| (i64::from(width), i64::from(height)))
        .unwrap_or_default()
}

/// Return scan candidates that still need a user decision.
///
/// This is deliberately read-only; promotion continues through the existing
/// transactional `create_new_item_response` handler and its write capability
/// check in the route layer.
pub fn scan_candidates_response(
    conn: &Connection,
    status: &str,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Value> {
    let (limit, offset) = crate::normalize_pagination(limit, offset);
    let table_exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='scan_candidates')",
        [],
        |row| row.get(0),
    )?;
    if table_exists == 0 {
        return Ok(json!({
            "candidates": [],
            "total": 0,
            "offset": offset,
            "limit": limit,
            "next_offset": Value::Null,
        }));
    }

    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM scan_candidates WHERE status=?1",
        params![status],
        |row| row.get(0),
    )?;
    let mut stmt = conn.prepare(
        "SELECT sc.id, sc.scan_id, sc.artist_id, a.name, a.path,
                sc.file_path, sc.file_name, sc.file_size, sc.file_mtime,
                sc.folder_name, sc.date, sc.is_archive, sc.media_type,
                sc.content_hash, sc.hash_status, sc.status, sc.created_at
         FROM scan_candidates sc
         LEFT JOIN artists a ON a.id=sc.artist_id
         WHERE sc.status=?1
         ORDER BY sc.id ASC
         LIMIT ?2 OFFSET ?3",
    )?;
    let candidates = stmt
        .query_map(params![status, limit, offset], |row| {
            Ok(json!({
                "id": row.get::<_, i64>(0)?,
                "scan_id": row.get::<_, String>(1)?,
                "artist_id": row.get::<_, i64>(2)?,
                "artist_name": row.get::<_, Option<String>>(3)?,
                "artist_path": row.get::<_, Option<String>>(4)?,
                "file_path": row.get::<_, String>(5)?,
                "file_name": row.get::<_, String>(6)?,
                "file_size": row.get::<_, i64>(7)?,
                "file_mtime": row.get::<_, f64>(8)?,
                "folder_name": row.get::<_, String>(9)?,
                "date": row.get::<_, String>(10)?,
                "is_archive": row.get::<_, i64>(11)?,
                "media_type": row.get::<_, String>(12)?,
                "content_hash": row.get::<_, String>(13)?,
                "hash_status": row.get::<_, String>(14)?,
                "status": row.get::<_, String>(15)?,
                "created_at": row.get::<_, f64>(16)?,
            }))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let next_offset_value = offset.saturating_add(candidates.len() as i64);
    let next_offset = (next_offset_value < total).then_some(next_offset_value);
    Ok(json!({
        "candidates": candidates,
        "total": total,
        "offset": offset,
        "limit": limit,
        "next_offset": next_offset,
    }))
}

pub fn resolve_existing_scan_candidate_response(
    conn: &Connection,
    candidate_id: i64,
) -> Result<Value> {
    resolve_existing_scan_candidate_response_inner(conn, None, candidate_id)
}

pub fn resolve_existing_scan_candidate_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
) -> Result<Value> {
    resolve_existing_scan_candidate_response_inner(conn, Some(roots), candidate_id)
}

fn resolve_existing_scan_candidate_response_inner(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    candidate_id: i64,
) -> Result<Value> {
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };
    if !candidate_file_is_current(roots, &candidate)? {
        supersede_candidate(conn, roots, &candidate)?;
        return Ok(json!({"action": "no_match", "reason": "candidate_stale"}));
    }

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin same-path scan-candidate resolution")?;
    let result = (|| -> Result<Option<i64>> {
        // Revalidate inside the write lock like the sibling paths: metadata
        // collected outside the transaction may already be stale.
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        let item_id = conn
            .query_row(
                "SELECT id FROM items WHERE file_path = ?1 LIMIT 1",
                params![candidate.file_path],
                |row| row.get::<_, i64>(0),
            )
            .optional()
            .context("fetch same-path item")?;
        let Some(item_id) = item_id else {
            return Ok(None);
        };

        conn.execute(
            "UPDATE items
             SET missing=0, missing_at=NULL, file_size=?1, file_mtime=?2, media_type=?3,
                 st_dev=?4, st_ino=?5, scanned_at=strftime('%s','now')
             WHERE id=?6",
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
        if conn.execute(
            "UPDATE scan_candidates SET status='resolved', resolved_at=strftime('%s','now')
             WHERE id=?1 AND status IN ('pending','candidate')",
            params![candidate_id],
        )? != 1
        {
            return Ok(None);
        }
        // The path is already in the library, so every pending move candidate
        // hanging off this scan candidate can never execute again: the
        // executor re-reads the scan candidate with status IN
        // ('pending','candidate') and would return no_match forever. Close
        // them in the same transaction instead of leaving dead rows in the
        // 待判断 list (A2).
        conn.execute(
            "UPDATE move_candidates SET status='superseded', resolved_at=strftime('%s','now')
             WHERE scan_candidate_id=?1 AND status='pending'",
            params![candidate_id],
        )
        .context("close resolved same-path move candidates")?;
        Ok(Some(item_id))
    })();

    match result {
        Ok(Some(item_id)) => {
            conn.execute_batch("COMMIT")?;
            Ok(json!({"action": "existing", "item_id": item_id}))
        }
        Ok(None) => {
            rollback(conn);
            Ok(json!({"action": "no_match"}))
        }
        Err(error) if error.to_string() == "candidate_stale" => {
            rollback(conn);
            supersede_candidate(conn, roots, &candidate)?;
            Ok(json!({
                "action": "no_match",
                "reason": "candidate_stale",
                "scan_candidate_id": candidate.id
            }))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

fn unique_missing_hash_item_id(
    conn: &Connection,
    candidate: &MoveTargetCandidate,
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

fn active_duplicate_count(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<i64> {
    conn.query_row(
        "
        SELECT COUNT(*)
        FROM items
        WHERE artist_id = ?1
          AND file_path != ?2
          AND hash_status = 'done'
          AND content_hash = ?3
          AND missing = 0
        ",
        params![
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
    apply_hash_unique_scan_candidate_response_inner(conn, None, candidate_id)
}

pub fn apply_hash_unique_scan_candidate_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
) -> Result<Value> {
    apply_hash_unique_scan_candidate_response_inner(conn, Some(roots), candidate_id)
}

fn apply_hash_unique_scan_candidate_response_inner(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    candidate_id: i64,
) -> Result<Value> {
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };
    if candidate.hash_status != "done" || candidate.content_hash.is_empty() {
        return Ok(json!({"action": "no_match"}));
    }
    if !candidate_file_is_current(roots, &candidate)? {
        supersede_candidate(conn, roots, &candidate)?;
        return Ok(json!({"action": "no_match"}));
    }
    let Some(item_id) = unique_missing_hash_item_id(conn, &candidate)? else {
        return Ok(json!({"action": "no_match"}));
    };
    if active_duplicate_count(conn, &candidate)? != 0 {
        return Ok(json!({"action": "no_match"}));
    }

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin hash-unique move")?;
    let result = (|| -> Result<Option<String>> {
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        if !candidate_content_is_current(roots, &candidate) {
            bail!("candidate_stale");
        }
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

        let detected_raw = candidate_detected_raw(&candidate);
        conn.execute(
            "
            UPDATE items
            SET file_path = ?1,
                file_name = ?2,
                file_size = ?3,
                file_mtime = ?4,
                folder_name = ?5,
                detected_date = ?6,
                date = CASE WHEN manual_date IS NULL THEN ?7 ELSE date END,
                is_archive = ?8,
                media_type = ?9,
                content_hash = ?10,
                hash_status = ?11,
                hash_updated_at = strftime('%s','now'),
                st_dev = ?12,
                st_ino = ?13,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?14
            ",
            params![
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &detected_raw,
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
        Err(error) if error.to_string() == "candidate_stale" => {
            rollback(conn);
            supersede_candidate(conn, roots, &candidate)?;
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
    create_new_item_response_inner(conn, None, candidate_id, &[])
}

pub fn create_new_item_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
) -> Result<Value> {
    create_new_item_response_inner(conn, Some(roots), candidate_id, &[])
}

/// Create the new file's own item row and copy the given source items' tags
/// onto it inside the same transaction, so a tag inheritance can never land
/// half-applied next to a committed record.
pub fn create_new_item_response_with_tags(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
    inherit_tag_item_ids: &[i64],
) -> Result<Value> {
    create_new_item_response_inner(conn, Some(roots), candidate_id, inherit_tag_item_ids)
}

fn create_new_item_response_inner(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    candidate_id: i64,
    inherit_tag_item_ids: &[i64],
) -> Result<Value> {
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "no_match"}));
    };
    if !candidate_file_is_current(roots, &candidate)? {
        supersede_candidate(conn, roots, &candidate)?;
        return Ok(json!({"action": "no_match"}));
    }

    // Do media I/O before BEGIN IMMEDIATE; a probe must not hold the SQLite
    // write lock while reading a JPEG header or invoking ffprobe.
    let media_type = inferred_media_type(&candidate.file_name, &candidate.media_type);
    let (width, height) = candidate_dimensions(roots, &candidate, &media_type);

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin scan-candidate create-new-item")?;
    let result = (|| -> Result<Option<i64>> {
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        if candidate.hash_status == "done" && !candidate_content_is_current(roots, &candidate) {
            bail!("candidate_stale");
        }
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

        let detected_raw = candidate_detected_raw(&candidate);
        let inserted = conn.execute(
            "
            INSERT INTO items
                (artist_id, file_path, file_name, file_size, file_mtime,
                 folder_name, date, detected_date, auto_role, tags, is_archive, media_type,
                 content_hash, hash_status, hash_updated_at, st_dev, st_ino,
                 missing, missing_at, scanned_at, width, height)
            VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '', '[]', ?9, ?10, ?11, ?12,
                    CASE WHEN ?12 = 'done' THEN strftime('%s','now') ELSE NULL END,
                    ?13, ?14, 0, NULL, strftime('%s','now'), ?15, ?16)
            ",
            params![
                candidate.artist_id,
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &candidate.date,
                &detected_raw,
                candidate.is_archive,
                &media_type,
                &candidate.content_hash,
                &candidate.hash_status,
                candidate.st_dev,
                candidate.st_ino,
                width,
                height,
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

        if !inherit_tag_item_ids.is_empty() {
            move_item_tags_to_item(conn, inherit_tag_item_ids, item_id, candidate.artist_id)
                .context("inherit source tags into new scan-candidate item")?;
        }

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
        Err(error) if error.to_string() == "candidate_stale" => {
            rollback(conn);
            supersede_candidate(conn, roots, &candidate)?;
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
        SELECT sc.id, sc.artist_id, a.path, sc.file_path, sc.file_name, sc.file_size,
               sc.file_mtime, sc.folder_name, sc.date, sc.is_archive, sc.media_type,
               sc.content_hash, sc.hash_status, sc.st_dev, sc.st_ino
        FROM scan_candidates sc
        JOIN artists a ON a.id = sc.artist_id
        WHERE sc.id = ?1 AND sc.status IN ('pending', 'candidate')
        ",
        params![candidate_id],
        |row| {
            Ok(MoveTargetCandidate {
                id: row.get(0)?,
                artist_id: row.get(1)?,
                artist_path: row.get(2)?,
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
    .context("fetch scan-candidate move target")
}

fn target_occupied(conn: &Connection, path: &str) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM items WHERE file_path=?1 LIMIT 1",
            params![path],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn missing_inode_ids(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<Vec<i64>> {
    let (Some(st_dev), Some(st_ino)) = (candidate.st_dev, candidate.st_ino) else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT id FROM items
         WHERE artist_id=?1 AND missing=1 AND st_dev=?2 AND st_ino=?3
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![candidate.artist_id, st_dev, st_ino], |row| {
            row.get(0)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn strip_category_prefix(segment: &str) -> String {
    segment.trim_start_matches('-').trim_start().to_string()
}

fn category_normalized_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    let parts: Vec<&str> = normalized.split('/').collect();
    parts
        .iter()
        .enumerate()
        .map(|(index, part)| {
            if index + 1 == parts.len() {
                (*part).to_string()
            } else {
                strip_category_prefix(part)
            }
        })
        .collect::<Vec<_>>()
        .join("/")
}

fn missing_category_ids(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<Vec<i64>> {
    let target = category_normalized_path(&candidate.file_path);
    let mut stmt = conn.prepare(
        "SELECT id, file_path FROM items
         WHERE artist_id=?1 AND missing=1 AND file_name=?2 AND file_size=?3
         ORDER BY id",
    )?;
    let rows = stmt.query_map(
        params![
            candidate.artist_id,
            &candidate.file_name,
            candidate.file_size
        ],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
    )?;
    let mut ids = Vec::new();
    for row in rows {
        let (id, path) = row?;
        if category_normalized_path(&path) == target {
            ids.push(id);
        }
    }
    Ok(ids)
}

fn missing_hash_ids(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM items
         WHERE artist_id=?1 AND missing=1 AND hash_status='done' AND content_hash=?2
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map(
            params![candidate.artist_id, &candidate.content_hash],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn cross_artist_hash_ids(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<Vec<i64>> {
    let mut stmt = conn.prepare(
        "SELECT id FROM items
         WHERE artist_id != ?1 AND missing=1 AND hash_status='done' AND content_hash=?2
         ORDER BY id",
    )?;
    let rows = stmt
        .query_map(
            params![candidate.artist_id, &candidate.content_hash],
            |row| row.get(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn has_missing_same_size(conn: &Connection, candidate: &MoveTargetCandidate) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM items WHERE artist_id=?1 AND missing=1 AND file_size=?2 LIMIT 1",
            params![candidate.artist_id, candidate.file_size],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some()
        || conn
            .query_row(
                "SELECT 1 FROM items WHERE artist_id != ?1 AND missing=1 AND file_size=?2 LIMIT 1",
                params![candidate.artist_id, candidate.file_size],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some())
}

fn has_unhashed_missing_same_size(
    conn: &Connection,
    candidate: &MoveTargetCandidate,
) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM items
             WHERE artist_id=?1 AND missing=1 AND hash_status != 'done' AND file_size=?2 LIMIT 1",
            params![candidate.artist_id, candidate.file_size],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn active_duplicate_count_for_candidate(
    conn: &Connection,
    candidate: &MoveTargetCandidate,
) -> Result<i64> {
    conn.query_row(
        "SELECT COUNT(*) FROM items
         WHERE artist_id=?1 AND file_path != ?2 AND hash_status='done'
           AND content_hash=?3 AND missing=0",
        params![
            candidate.artist_id,
            &candidate.file_path,
            &candidate.content_hash
        ],
        |row| row.get(0),
    )
    .context("count active same-hash duplicates")
}

fn create_scan_move_candidate(
    conn: &Connection,
    candidate: &MoveTargetCandidate,
    item_id: Option<i64>,
    reason: &str,
) -> Result<()> {
    let old_path = item_id
        .map(|id| {
            conn.query_row(
                "SELECT file_path FROM items WHERE id=?1",
                params![id],
                |row| row.get::<_, String>(0),
            )
        })
        .transpose()?
        .unwrap_or_default();
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM move_candidates
         WHERE status='pending' AND new_path=?1
           AND COALESCE(item_id,0)=COALESCE(?2,0) AND reason=?3",
        params![&candidate.file_path, item_id, reason],
        |row| row.get(0),
    )?;
    if exists == 0 {
        conn.execute(
            "INSERT INTO move_candidates
             (scan_candidate_id, item_id, artist_id, old_path, new_path, reason,
              content_hash, st_dev, st_ino, status)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 'pending')",
            params![
                candidate.id,
                item_id,
                candidate.artist_id,
                old_path,
                &candidate.file_path,
                reason,
                &candidate.content_hash,
                candidate.st_dev,
                candidate.st_ino,
            ],
        )?;
    } else {
        // Reuse the still-pending row for this (new path, old record, reason)
        // pair instead of stacking duplicates, but refresh its evidence so it
        // tracks the current scan version (A3): the scanner abandons resolved
        // scan-candidate rows and inserts a fresh one for the same path, so a
        // row kept from the previous round would otherwise stay linked to a
        // terminal scan candidate and display a stale hash.
        conn.execute(
            "UPDATE move_candidates
             SET scan_candidate_id=?1, old_path=?2, content_hash=?3, st_dev=?4, st_ino=?5
             WHERE status='pending' AND new_path=?6
               AND COALESCE(item_id,0)=COALESCE(?7,0) AND reason=?8",
            params![
                candidate.id,
                old_path,
                &candidate.content_hash,
                candidate.st_dev,
                candidate.st_ino,
                &candidate.file_path,
                item_id,
                reason,
            ],
        )?;
    }
    conn.execute(
        "UPDATE scan_candidates SET status='candidate', resolved_at=NULL WHERE id=?1",
        params![candidate.id],
    )?;
    Ok(())
}

fn waiting_for_hash(conn: &Connection, candidate_id: i64, reason: &str) -> Result<Value> {
    // Status guard: a concurrent resolver may have just moved this row to
    // resolved/new; never flip such rows back to pending.
    conn.execute(
        "UPDATE scan_candidates SET status='pending', resolved_at=NULL
         WHERE id=?1 AND status IN ('pending','candidate')",
        params![candidate_id],
    )?;
    Ok(json!({"action": "waiting_hash", "reason": reason}))
}

/// Resolve a scanned file using the same safety order as the reference matcher.
/// Only unambiguous same-artist repairs and genuinely new paths are automatic.
pub fn resolve_scan_candidate_response(conn: &Connection, candidate_id: i64) -> Result<Value> {
    let roots = MediaRoots {
        roots: Vec::new(),
        labels: Vec::new(),
        real_paths: Vec::new(),
    };
    resolve_scan_candidate_response_with_roots(conn, &roots, candidate_id)
}

pub fn resolve_scan_candidate_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
) -> Result<Value> {
    let existing = resolve_existing_scan_candidate_response_with_roots(conn, roots, candidate_id)?;
    if existing.get("action").and_then(|value| value.as_str()) != Some("no_match") {
        return Ok(existing);
    }
    let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
        return Ok(json!({"action": "missing"}));
    };

    let inode_ids = missing_inode_ids(conn, &candidate)?;
    if inode_ids.len() == 1 {
        if target_occupied(conn, &candidate.file_path)? {
            create_scan_move_candidate(conn, &candidate, Some(inode_ids[0]), "target_occupied")?;
            return Ok(json!({"action": "candidate", "reason": "target_occupied"}));
        }
        return apply_scan_candidate_move_response_with_roots(
            conn,
            roots,
            candidate.id,
            inode_ids[0],
            "inode",
        );
    }
    if inode_ids.len() > 1 {
        for item_id in inode_ids {
            create_scan_move_candidate(conn, &candidate, Some(item_id), "inode_untrusted")?;
        }
        return Ok(json!({"action": "candidate", "reason": "inode_untrusted"}));
    }

    let category_ids = missing_category_ids(conn, &candidate)?;
    if category_ids.len() == 1 {
        if target_occupied(conn, &candidate.file_path)? {
            create_scan_move_candidate(conn, &candidate, Some(category_ids[0]), "target_occupied")?;
            return Ok(json!({"action": "candidate", "reason": "target_occupied"}));
        }
        return apply_scan_candidate_move_response_with_roots(
            conn,
            roots,
            candidate.id,
            category_ids[0],
            "category_rename",
        );
    }
    if category_ids.len() > 1 {
        for item_id in category_ids {
            create_scan_move_candidate(conn, &candidate, Some(item_id), "manual_needed")?;
        }
        return Ok(json!({"action": "candidate", "reason": "manual_needed"}));
    }

    if candidate.hash_status != "done" || candidate.content_hash.is_empty() {
        if has_missing_same_size(conn, &candidate)? {
            return waiting_for_hash(conn, candidate.id, "missing_hash_not_ready");
        }
        return create_new_item_response_with_roots(conn, roots, candidate.id);
    }

    let hash_ids = missing_hash_ids(conn, &candidate)?;
    let active_duplicates = active_duplicate_count_for_candidate(conn, &candidate)?;
    if hash_ids.len() == 1 && active_duplicates == 0 {
        if target_occupied(conn, &candidate.file_path)? {
            create_scan_move_candidate(conn, &candidate, Some(hash_ids[0]), "target_occupied")?;
            return Ok(json!({"action": "candidate", "reason": "target_occupied"}));
        }
        return apply_hash_unique_scan_candidate_response_with_roots(conn, roots, candidate.id);
    }
    if hash_ids.len() == 1 && active_duplicates > 0 {
        create_scan_move_candidate(conn, &candidate, Some(hash_ids[0]), "hash_duplicate_active")?;
        return Ok(json!({"action": "candidate", "reason": "hash_duplicate_active"}));
    }
    if hash_ids.len() > 1 {
        for item_id in hash_ids {
            create_scan_move_candidate(conn, &candidate, Some(item_id), "hash_multiple_missing")?;
        }
        return Ok(json!({"action": "candidate", "reason": "hash_multiple_missing"}));
    }

    let cross_artist_ids = cross_artist_hash_ids(conn, &candidate)?;
    if !cross_artist_ids.is_empty() {
        for item_id in cross_artist_ids {
            create_scan_move_candidate(conn, &candidate, Some(item_id), "manual_needed")?;
        }
        return Ok(json!({"action": "candidate", "reason": "manual_needed"}));
    }
    if has_unhashed_missing_same_size(conn, &candidate)? {
        return waiting_for_hash(conn, candidate.id, "missing_hash_not_ready");
    }
    create_new_item_response_with_roots(conn, roots, candidate.id)
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
    apply_scan_candidate_move_response_inner(conn, None, candidate_id, item_id, reason)
}

pub fn apply_scan_candidate_move_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    candidate_id: i64,
    item_id: i64,
    reason: &str,
) -> Result<Value> {
    apply_scan_candidate_move_response_inner(conn, Some(roots), candidate_id, item_id, reason)
}

fn apply_scan_candidate_move_response_inner(
    conn: &Connection,
    roots: Option<&MediaRoots>,
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
    if !candidate_file_is_current(roots, &candidate)? {
        supersede_candidate(conn, roots, &candidate)?;
        return Ok(json!({"action": "no_match"}));
    }

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin scan-candidate move")?;
    let result = (|| -> Result<Option<String>> {
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        if candidate.hash_status == "done" && !candidate_content_is_current(roots, &candidate) {
            bail!("candidate_stale");
        }
        let item = conn
            .query_row(
                "SELECT artist_id, file_path, file_name, file_size, st_dev, st_ino, missing, content_hash
                 FROM items WHERE id=?1 AND missing=1",
                params![item_id],
                |row| Ok(ItemMissing {
                    artist_id: row.get(0)?,
                    file_path: row.get(1)?,
                    file_name: row.get(2)?,
                    file_size: row.get(3)?,
                    st_dev: row.get(4)?,
                    st_ino: row.get(5)?,
                    missing: row.get(6)?,
                    content_hash: row.get(7)?,
                }),
            )
            .optional()
            .context("refresh missing scan-candidate item")?;
        let Some(item) = item else {
            return Ok(None);
        };
        if item.artist_id != candidate.artist_id
            || match reason {
                "inode" => {
                    (item.st_dev, item.st_ino) != (candidate.st_dev, candidate.st_ino)
                        || candidate.st_dev.is_none()
                        || candidate.st_ino.is_none()
                }
                "category_rename" => {
                    item.file_name != candidate.file_name
                        || item.file_size != candidate.file_size
                        || category_normalized_path(&item.file_path)
                            != category_normalized_path(&candidate.file_path)
                }
                _ => true,
            }
        {
            return Ok(None);
        }
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

        let detected_raw = candidate_detected_raw(&candidate);
        conn.execute(
            "
            UPDATE items
            SET file_path = ?1,
                file_name = ?2,
                file_size = ?3,
                file_mtime = ?4,
                folder_name = ?5,
                detected_date = ?6,
                date = CASE WHEN manual_date IS NULL THEN ?7 ELSE date END,
                is_archive = ?8,
                media_type = ?9,
                content_hash = ?10,
                hash_status = ?11,
                hash_updated_at = CASE WHEN ?11 = 'done' THEN strftime('%s','now') ELSE NULL END,
                st_dev = ?12,
                st_ino = ?13,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?14
            ",
            params![
                &candidate.file_path,
                &candidate.file_name,
                candidate.file_size,
                candidate.file_mtime,
                &candidate.folder_name,
                &detected_raw,
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
                &item.file_path,
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
        Ok(Some(item.file_path))
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
        Err(error) if error.to_string() == "candidate_stale" => {
            rollback(conn);
            supersede_candidate(conn, roots, &candidate)?;
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
///
/// Successful apply outcome: (item_id, reason, old_path, new_path, tag_count).
type AppliedMoveOutcome = (i64, String, String, String, i64);

pub fn apply_move_candidate_response(conn: &Connection, move_candidate_id: i64) -> Result<Value> {
    apply_move_candidate_response_inner(conn, move_candidate_id, None)
}

pub fn apply_move_candidate_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    move_candidate_id: i64,
) -> Result<Value> {
    apply_move_candidate_response_inner_with_roots(conn, Some(roots), move_candidate_id, None)
}

/// Apply one revalidated cross-artist group member. This is intentionally not
/// exposed through the single-candidate route.
pub(crate) fn apply_move_candidate_group_item_response_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    move_candidate_id: i64,
    old_artist_id: i64,
    new_artist_id: i64,
) -> Result<Value> {
    apply_move_candidate_response_inner_with_roots(
        conn,
        Some(roots),
        move_candidate_id,
        Some((old_artist_id, new_artist_id)),
    )
}

fn apply_move_candidate_response_inner(
    conn: &Connection,
    move_candidate_id: i64,
    group_artists: Option<(i64, i64)>,
) -> Result<Value> {
    apply_move_candidate_response_inner_with_roots(conn, None, move_candidate_id, group_artists)
}

fn apply_move_candidate_response_inner_with_roots(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    move_candidate_id: i64,
    group_artists: Option<(i64, i64)>,
) -> Result<Value> {
    let move_row = conn
        .query_row(
            "
            SELECT id, item_id, scan_candidate_id, new_path, artist_id, reason
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
                })
            },
        )
        .optional()
        .context("fetch move candidate")?;
    let Some(move_row) = move_row else {
        return Ok(json!({"action": "no_match"}));
    };
    let candidate = if let Some(candidate_id) = move_row.scan_candidate_id.filter(|id| *id > 0) {
        let Some(candidate) = move_target_candidate(conn, candidate_id)? else {
            return Ok(json!({"action": "no_match"}));
        };
        if candidate.artist_id != move_row.artist_id || candidate.file_path != move_row.new_path {
            return Ok(json!({"action": "no_match"}));
        }
        if !candidate_file_is_current(roots, &candidate)? {
            supersede_candidate(conn, roots, &candidate)?;
            return Ok(json!({"action": "no_match", "reason": "candidate_stale"}));
        }
        candidate
    } else {
        return Ok(json!({"action": "no_match"}));
    };

    conn.execute_batch("BEGIN IMMEDIATE")
        .context("begin move-candidate apply")?;
    let result = (|| -> Result<Option<AppliedMoveOutcome>> {
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        let item = conn
            .query_row(
                "SELECT artist_id, file_path, file_name, file_size, st_dev, st_ino, missing, content_hash
                 FROM items WHERE id=?1",
                params![move_row.item_id],
                |row| {
                    Ok(ItemMissing {
                        artist_id: row.get(0)?,
                        file_path: row.get(1)?,
                        file_name: row.get(2)?,
                        file_size: row.get(3)?,
                        st_dev: row.get(4)?,
                        st_ino: row.get(5)?,
                        missing: row.get(6)?,
                        content_hash: row.get(7)?,
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
        let mut group_tag_item_ids = Vec::new();
        match group_artists {
            Some((old_artist_id, new_artist_id)) => {
                if item.artist_id != old_artist_id
                    || move_row.artist_id != new_artist_id
                    || move_row.reason != "manual_needed"
                {
                    return Ok(None);
                }
                let Some(scan_candidate_id) = move_row.scan_candidate_id.filter(|id| *id > 0)
                else {
                    return Ok(None);
                };
                let candidate = conn
                    .query_row(
                        "SELECT artist_id, file_path, status, content_hash
                         FROM scan_candidates WHERE id=?1",
                        params![scan_candidate_id],
                        |row| {
                            Ok((
                                row.get::<_, i64>(0)?,
                                row.get::<_, String>(1)?,
                                row.get::<_, String>(2)?,
                                row.get::<_, String>(3)?,
                            ))
                        },
                    )
                    .optional()
                    .context("fetch group scan candidate")?;
                let Some((candidate_artist_id, candidate_path, candidate_status, candidate_hash)) =
                    candidate
                else {
                    return Ok(None);
                };
                if candidate_artist_id != new_artist_id
                    || !matches!(candidate_status.as_str(), "pending" | "candidate")
                    || candidate_path != move_row.new_path
                    || candidate_hash.is_empty()
                    || item.content_hash.is_empty()
                    || candidate_hash != item.content_hash
                {
                    return Ok(None);
                }
                let group_rows: Vec<(i64, i64, i64, String, String, i64, String)> = conn
                    .prepare(
                        "SELECT mc.item_id, mc.artist_id, i.artist_id, mc.reason, mc.new_path,
                                i.missing, i.content_hash
                         FROM move_candidates mc
                         JOIN items i ON i.id=mc.item_id
                         WHERE mc.status='pending' AND mc.scan_candidate_id=?1
                         ORDER BY mc.id",
                    )?
                    .query_map(params![scan_candidate_id], |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                            row.get(6)?,
                        ))
                    })?
                    .collect::<rusqlite::Result<_>>()?;
                let group_total: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM move_candidates
                     WHERE status='pending' AND scan_candidate_id=?1",
                    params![scan_candidate_id],
                    |row| row.get(0),
                )?;
                let target_total: i64 = conn.query_row(
                    "SELECT COUNT(*) FROM move_candidates
                     WHERE status='pending' AND (new_path=?1 OR scan_candidate_id=?2)",
                    params![&move_row.new_path, scan_candidate_id],
                    |row| row.get(0),
                )?;
                if group_rows.is_empty()
                    || group_rows.len() as i64 != group_total
                    || target_total != group_total
                    || group_rows.iter().any(
                        |(
                            item_id,
                            row_candidate_artist,
                            row_item_artist,
                            row_reason,
                            row_path,
                            missing,
                            hash,
                        )| {
                            *item_id <= 0
                                || *row_candidate_artist != new_artist_id
                                || *row_item_artist != old_artist_id
                                || row_reason != "manual_needed"
                                || row_path != &move_row.new_path
                                || *missing != 1
                                || hash.is_empty()
                                || hash != &candidate_hash
                        },
                    )
                {
                    return Ok(None);
                }
                // Identity gate. Several pending rows for the same
                // (old record, target) pair are repeated evidence for one
                // identity; several *distinct* old records are competing
                // identities. Taking the lowest id would silently adopt one
                // record's path, tags and history on the user's behalf, so the
                // whole cluster stays pending until a human or stronger
                // evidence picks one.
                let mut distinct_sources: Vec<i64> = group_rows.iter().map(|row| row.0).collect();
                distinct_sources.sort_unstable();
                distinct_sources.dedup();
                if distinct_sources.len() != 1 || distinct_sources[0] != move_row.item_id {
                    return Ok(None);
                }
                // Reverse competition: the selected old record must not also
                // be waiting on a different target, and no live copy may
                // already hold this content at another path.
                let other_targets: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM move_candidates
                         WHERE status='pending' AND item_id=?1
                           AND COALESCE(scan_candidate_id,0) <> COALESCE(?2,0)",
                        params![move_row.item_id, scan_candidate_id],
                        |row| row.get(0),
                    )
                    .context("count competing targets for move source")?;
                let active_copies: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM items
                         WHERE missing=0 AND id<>?1 AND content_hash<>'' AND content_hash=?2",
                        params![move_row.item_id, &item.content_hash],
                        |row| row.get(0),
                    )
                    .context("count active copies of move source")?;
                if other_targets > 0 || active_copies > 0 {
                    return Ok(None);
                }
                // Only the selected record's own tags migrate. Competing
                // candidates are unverified hypotheses, not proven copies, so
                // their tags, paths and dates are left exactly as they are.
                group_tag_item_ids = vec![move_row.item_id];
            }
            None if item.artist_id != move_row.artist_id => {
                // Cross-artist changes are deliberately group-only.
                return Ok(None);
            }
            None => {}
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

        if let Some((_, target_artist_id)) = group_artists {
            move_item_tags_to_item(
                conn,
                &group_tag_item_ids,
                move_row.item_id,
                target_artist_id,
            )?;
        }

        let (
            file_size,
            file_mtime,
            folder_name,
            date,
            is_archive,
            media_type,
            content_hash,
            _sc_hash_status,
        ): (i64, f64, String, String, i64, String, String, String) = conn
            .query_row(
                "
                SELECT file_size, file_mtime, folder_name, date, is_archive,
                       media_type, content_hash, hash_status
                FROM scan_candidates WHERE id = ?1
                ",
                params![candidate.id],
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
            .context("fetch linked scan candidate")?;
        // SQLite now prevents candidate-row changes; repeat the filesystem
        // check immediately before the durable path update.
        if !candidate_row_is_unchanged(conn, &candidate)?
            || !candidate_stat_is_current(roots, &candidate)
        {
            bail!("candidate_stale");
        }
        // The update below persists content_hash with hash_status='done' when
        // the candidate carries a digest; re-verify the bytes first.
        if !content_hash.is_empty() && !candidate_content_is_current(roots, &candidate) {
            bail!("candidate_stale");
        }
        let file_name = path_file_name(&move_row.new_path).to_string();
        let media = inferred_media_type(&file_name, &media_type);
        let hash_status = if content_hash.is_empty() {
            "pending"
        } else {
            "done"
        };
        let detected_raw = candidate_detected_raw(&candidate);

        let updated = conn
            .execute(
                "
            UPDATE items
            SET artist_id = COALESCE(?1, artist_id),
                file_path = ?2,
                file_name = ?3,
                file_size = ?4,
                file_mtime = ?5,
                folder_name = ?6,
                detected_date = ?7,
                date = CASE WHEN manual_date IS NULL THEN ?8 ELSE date END,
                is_archive = ?9,
                media_type = ?10,
                content_hash = ?11,
                hash_status = ?12,
                hash_updated_at = CASE WHEN ?12 = 'done' THEN strftime('%s','now') ELSE NULL END,
                st_dev = ?13,
                st_ino = ?14,
                missing = 0,
                missing_at = NULL,
                scanned_at = strftime('%s','now')
            WHERE id = ?15
            ",
                params![
                    group_artists.map(|(_, target_artist_id)| target_artist_id),
                    &move_row.new_path,
                    &file_name,
                    file_size,
                    file_mtime,
                    &folder_name,
                    &detected_raw,
                    &date,
                    is_archive,
                    &media,
                    &content_hash,
                    hash_status,
                    candidate.st_dev,
                    candidate.st_ino,
                    move_row.item_id,
                ],
            )
            .context("update confirmed move item")?;
        if updated != 1 {
            // The disk-side target was validated, but the durable item row did
            // not update exactly once. Roll the whole apply back instead of
            // reporting success with zero rows written.
            anyhow::bail!("target item row did not update exactly once");
        }
        conn.execute(
            "
            INSERT INTO move_history
                (item_id, artist_id, old_path, new_path, reason, status, applied_at)
            VALUES (?1, ?2, ?3, ?4, ?5, 'applied', strftime('%s','now'))
            ",
            params![
                move_row.item_id,
                item.artist_id,
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
        Err(error) if error.to_string() == "candidate_stale" => {
            rollback(conn);
            supersede_candidate(conn, roots, &candidate)?;
            Ok(json!({
                "action": "no_match",
                "reason": "candidate_stale",
                "scan_candidate_id": candidate.id
            }))
        }
        Err(error) => {
            rollback(conn);
            Err(error)
        }
    }
}

/// Move the selected source item's tags onto the target item (reused by
/// target-artist tag name). The target itself moved across artists, so its
/// old artist-owned tag rows are replaced by the target-artist rows.
///
/// Callers pass exactly the selected old record: competing candidates are
/// unverified hypotheses rather than proven copies, so their tags, paths,
/// dates and favourites are never merged into the survivor.
fn move_item_tags_to_item(
    conn: &Connection,
    source_item_ids: &[i64],
    target_item_id: i64,
    target_artist_id: i64,
) -> Result<()> {
    let mut tags = Vec::new();
    for item_id in source_item_ids {
        tags.extend(
            conn.prepare(
                "SELECT t.name, t.sort_order FROM item_tags it
                 JOIN tags t ON t.id=it.tag_id
                 WHERE it.item_id=?1 ORDER BY t.sort_order, t.name",
            )?
            .query_map(params![item_id], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<rusqlite::Result<Vec<(String, i64)>>>()?,
        );
    }
    if tags.is_empty() {
        return Ok(());
    }
    let mut target_tag_ids = Vec::new();
    for (name, source_sort_order) in tags {
        let name = name.trim();
        if name.is_empty() {
            continue;
        }
        let existing = conn
            .query_row(
                "SELECT id FROM tags WHERE artist_id=?1 AND name=?2 LIMIT 1",
                params![target_artist_id, name],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let tag_id = match existing {
            Some(id) => id,
            None => {
                let max_sort_order: i64 = conn.query_row(
                    "SELECT COALESCE(MAX(sort_order), 0) FROM tags WHERE artist_id=?1",
                    params![target_artist_id],
                    |row| row.get(0),
                )?;
                conn.execute(
                    "INSERT INTO tags (artist_id, name, sort_order) VALUES (?1, ?2, ?3)",
                    params![
                        target_artist_id,
                        name,
                        max_sort_order.max(source_sort_order) + 1
                    ],
                )?;
                conn.last_insert_rowid()
            }
        };
        target_tag_ids.push(tag_id);
    }
    // Tags belong to an artist: the moved item must not keep its previous
    // artist's tag rows. Sibling sources are untouched.
    conn.execute(
        "DELETE FROM item_tags WHERE item_id=?1",
        params![target_item_id],
    )?;
    for tag_id in target_tag_ids {
        conn.execute(
            "INSERT OR IGNORE INTO item_tags (item_id, tag_id) VALUES (?1, ?2)",
            params![target_item_id, tag_id],
        )?;
    }
    Ok(())
}

/// Ignore a single `move_candidate` (the "ignore candidate" button) via the
/// Rust sidecar. Mirrors the Python `ignore_move_candidate` write: a single
/// status flip to `ignored` scoped to still-`pending` rows, returning the
/// number of affected rows. Always succeeds with `{"action":"ignored"}` (the
/// Python fallback is behaviourally identical for missing/non-pending ids,
/// returning `updated: 0`).
pub fn ignore_move_candidate_response(conn: &Connection, move_candidate_id: i64) -> Result<Value> {
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
    if created.get("action").and_then(|a| a.as_str()) == Some("new") {
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

/// Resolve a cross-artist cluster where several missing old records all claim
/// the same new file. Rewriting one of them would silently pick an identity
/// and hand the new file that record's history, so the new file gets its own
/// record instead and inherits the union of every source's tags. Sources that
/// disagree on tags (or that also wait on another target) are not blocked:
/// this path rewrites nothing, all sources share the candidate's content
/// hash, and an extra tag on the new record is reversible by editing it,
/// while leaving the cluster pending would demand a human decision for every
/// such cluster.
pub fn resolve_ambiguous_cluster_as_new_response(
    conn: &Connection,
    roots: &MediaRoots,
    move_candidate_id: i64,
) -> Result<Value> {
    let scan_candidate_id: Option<Option<i64>> = conn
        .query_row(
            "SELECT scan_candidate_id FROM move_candidates
             WHERE id = ?1 AND status = 'pending' AND reason = 'manual_needed'",
            params![move_candidate_id],
            |row| row.get::<_, Option<i64>>(0),
        )
        .optional()
        .context("fetch move candidate for ambiguous cluster")?;
    let Some(scan_candidate_id) = scan_candidate_id.flatten().filter(|id| *id > 0) else {
        return Ok(json!({"action": "no_match"}));
    };
    let candidate = conn
        .query_row(
            "SELECT artist_id, file_path, status, content_hash
             FROM scan_candidates WHERE id = ?1",
            params![scan_candidate_id],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                ))
            },
        )
        .optional()
        .context("fetch ambiguous-cluster scan candidate")?;
    let Some((candidate_artist_id, candidate_path, candidate_status, candidate_hash)) = candidate
    else {
        return Ok(json!({"action": "no_match"}));
    };
    if !matches!(candidate_status.as_str(), "pending" | "candidate") || candidate_hash.is_empty() {
        return Ok(json!({"action": "no_match"}));
    }
    let rows: Vec<(i64, i64, i64, String, String, i64, String)> = conn
        .prepare(
            "SELECT mc.item_id, mc.artist_id, i.artist_id, mc.reason, mc.new_path,
                    i.missing, i.content_hash
             FROM move_candidates mc
             JOIN items i ON i.id = mc.item_id
             WHERE mc.status = 'pending' AND mc.scan_candidate_id = ?1
             ORDER BY mc.id",
        )?
        .query_map(params![scan_candidate_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;
    if rows.len() < 2 {
        return Ok(json!({"action": "no_match"}));
    }
    let old_artist_id = rows[0].2;
    if old_artist_id == candidate_artist_id
        || rows.iter().any(
            |(item_id, row_candidate_artist, row_item_artist, reason, new_path, missing, hash)| {
                *item_id <= 0
                    || *row_candidate_artist != candidate_artist_id
                    || *row_item_artist != old_artist_id
                    || reason != "manual_needed"
                    || new_path != &candidate_path
                    || *missing != 1
                    || hash != &candidate_hash
            },
        )
    {
        return Ok(json!({"action": "no_match"}));
    }
    let mut sources: Vec<i64> = rows.iter().map(|row| row.0).collect();
    sources.sort_unstable();
    sources.dedup();
    if sources.len() < 2 {
        // Repeated rows for one old record are duplicate evidence for a single
        // identity, not a dispute; the normal group apply owns that case.
        return Ok(json!({"action": "no_match"}));
    }
    let active_copies: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM items
             WHERE missing = 0 AND content_hash <> '' AND content_hash = ?1",
            params![&candidate_hash],
            |row| row.get(0),
        )
        .context("count live copies for ambiguous cluster")?;
    if active_copies > 0 {
        return Ok(json!({"action": "no_match"}));
    }
    // Sources that also wait on a *different* new file are deliberately not
    // blocked here: every pending target of a source shares its content hash,
    // so the tags follow the content and every same-content copy may carry
    // them. Unlike the single-record identity gate above (which rewrites an
    // old record and must stay strict), this path only creates a new record
    // and copies tags, so a wrong extra tag is reversible by editing the new
    // record, while blocking would leave the cluster for a human.
    create_new_item_response_with_tags(conn, roots, scan_candidate_id, &sources)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn candidate_with(path: &str, artist: &str, date: &str) -> MoveTargetCandidate {
        MoveTargetCandidate {
            id: 1,
            artist_id: 1,
            artist_path: artist.to_string(),
            file_path: path.to_string(),
            file_name: String::new(),
            file_size: 0,
            file_mtime: 0.0,
            folder_name: String::new(),
            date: date.to_string(),
            is_archive: 0,
            media_type: String::new(),
            content_hash: String::new(),
            hash_status: String::new(),
            st_dev: None,
            st_ino: None,
        }
    }

    #[test]
    fn candidate_detected_raw_preserves_folder_precision() {
        let c = candidate_with(
            "/pictures/artist/2026/202607 works/pic.jpg",
            "/pictures/artist",
            "2026-07-01",
        );
        assert_eq!(candidate_detected_raw(&c), "2026-07");

        let c = candidate_with(
            "/pictures/artist/2026-05-01_title/pic.jpg",
            "/pictures/artist",
            "2026-05-01",
        );
        assert_eq!(candidate_detected_raw(&c), "2026-05-01");

        let c = candidate_with(
            "/pictures/artist/202508/01_1536_title/pic.jpg",
            "/pictures/artist",
            "2025-08-01",
        );
        assert_eq!(candidate_detected_raw(&c), "2025-08-01");

        let c = candidate_with(
            "/pictures/artist/plain/pic.jpg",
            "/pictures/artist",
            "2026-08-15",
        );
        assert_eq!(
            candidate_detected_raw(&c),
            "2026-08-15",
            "unparseable folder falls back to the candidate canonical date"
        );

        let c = candidate_with("/pictures/artist/pic.jpg", "/pictures/artist", "");
        assert_eq!(candidate_detected_raw(&c), "");
    }

    #[test]
    fn candidate_content_check_detects_same_stat_replacement() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, b"AAAAAAAAAAAAAAAA").unwrap();
        let old_digest = hash_file(&path, 1024 * 1024).unwrap();
        let mut candidate = candidate_with(&path.to_string_lossy(), "/pictures/artist", "");
        candidate.hash_status = "done".into();
        candidate.content_hash = old_digest;
        assert!(candidate_content_is_current(None, &candidate));

        // Same-size in-place replacement inside the same wall-clock second
        // keeps size/mtime/inode identical while the bytes differ, so the
        // stat-only check passes; the content check must catch it.
        std::fs::write(&path, b"BBBBBBBBBBBBBBBB").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().len(), 16);
        assert!(
            !candidate_content_is_current(None, &candidate),
            "a digest must not be committed for replaced bytes"
        );
    }

    #[test]
    fn create_new_item_fails_closed_when_content_no_longer_matches() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, path TEXT);
             CREATE TABLE items (
               id INTEGER PRIMARY KEY AUTOINCREMENT,
               artist_id INTEGER, file_path TEXT UNIQUE, file_name TEXT,
               file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0,
               folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
               detected_date TEXT DEFAULT '', auto_role TEXT DEFAULT '',
               tags TEXT DEFAULT '[]', is_archive INTEGER DEFAULT 0,
               media_type TEXT DEFAULT 'image', content_hash TEXT DEFAULT '',
               hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
               st_dev INTEGER, st_ino INTEGER, missing INTEGER DEFAULT 0,
               missing_at REAL, scanned_at INTEGER,
               width INTEGER DEFAULT 0, height INTEGER DEFAULT 0
             );
             CREATE TABLE scan_candidates (
               id INTEGER PRIMARY KEY AUTOINCREMENT, scan_id TEXT DEFAULT '',
               artist_id INTEGER, file_path TEXT, file_name TEXT,
               file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0,
               folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
               is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
               content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending',
               status TEXT DEFAULT 'pending', resolved_at REAL,
               st_dev INTEGER, st_ino INTEGER
             );
             CREATE TABLE move_candidates (
               id INTEGER PRIMARY KEY AUTOINCREMENT, scan_candidate_id INTEGER,
               item_id INTEGER, artist_id INTEGER, old_path TEXT, new_path TEXT,
               reason TEXT, content_hash TEXT, st_dev INTEGER, st_ino INTEGER,
               status TEXT DEFAULT 'pending', resolved_at REAL
             );",
        )
        .unwrap();
        conn.execute("INSERT INTO artists (id, path) VALUES (1, '/pictures/artist')", [])
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jpg");
        std::fs::write(&path, b"AAAAAAAAAAAAAAAA").unwrap();
        let stale_digest = hash_file(&path, 1024 * 1024).unwrap();
        let metadata = std::fs::metadata(&path).unwrap();
        let file_mtime = metadata
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        conn.execute(
            "INSERT INTO scan_candidates
             (scan_id, artist_id, file_path, file_name, file_size, file_mtime,
              folder_name, date, is_archive, media_type, content_hash, hash_status, status)
             VALUES ('s1', 1, ?, 'a.jpg', 16, ?, '', '', 0, 'image', ?, 'done', 'pending')",
            rusqlite::params![path.to_string_lossy(), file_mtime, stale_digest],
        )
        .unwrap();
        let candidate_id: i64 = conn.last_insert_rowid();
        // Replace the bytes after the candidate row was recorded; the hash no
        // longer describes the file, so the promotion must not commit it.
        std::fs::write(&path, b"BBBBBBBBBBBBBBBB").unwrap();

        let result = create_new_item_response(&conn, candidate_id).unwrap();
        assert_eq!(result["action"], "no_match", "{result}");
        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))
            .unwrap();
        assert_eq!(items, 0, "no item may be created from replaced content");
        let (status, hash_status, content_hash): (String, String, String) = conn
            .query_row(
                "SELECT status, hash_status, content_hash FROM scan_candidates WHERE id=?",
                [candidate_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(status, "superseded");
        assert_eq!(hash_status, "pending", "stale hash must be requeued, not stored");
        assert_eq!(content_hash, "");
    }
}

