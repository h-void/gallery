use anyhow::{Context, Result};
use rusqlite::{Connection, Row};
use serde::Serialize;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::move_context::{artist_context, query_optional_i64};
use crate::normalize_pagination;
use crate::path_display::display_path;

#[derive(Clone, Serialize, Debug)]
struct HistoryRow {
    id: i64,
    item_id: i64,
    artist_id: i64,
    old_path: String,
    new_path: String,
    reason: String,
    status: String,
    details: String,
    created_at: f64,
    applied_at: Option<f64>,
    reverted_at: Option<f64>,
    display_old_path: String,
    display_new_path: String,
    item_artist_id: Option<i64>,
    candidate_artist_id: Option<i64>,
    item_artist_name: String,
    candidate_artist_name: String,
    item_artist_path: String,
    candidate_artist_path: String,
    display_item_artist_path: String,
    display_candidate_artist_path: String,
    is_cross_artist: bool,
    same_artist_name: bool,
    can_confirm: bool,
}

#[derive(Clone, Debug)]
struct BasicHistoryRow {
    id: i64,
    item_id: i64,
    artist_id: i64,
    old_path: String,
    new_path: String,
    reason: String,
    status: String,
    details: String,
    created_at: f64,
    applied_at: Option<f64>,
    reverted_at: Option<f64>,
}

pub fn move_history_response(
    conn: &Connection,
    roots: &MediaRoots,
    status: Option<&str>,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Value> {
    let (limit, offset) = normalize_pagination(limit, offset);
    let total = count_move_history(conn, status)?;
    let history = list_move_history(conn, roots, status, limit, offset)?;
    Ok(json!({
        "history": history,
        "total": total,
        "limit": limit,
        "offset": offset,
        "has_more": offset + limit < total,
    }))
}

fn count_move_history(conn: &Connection, status: Option<&str>) -> Result<i64> {
    if let Some(status) = status {
        conn.query_row(
            "SELECT COUNT(*) FROM move_history WHERE status=?",
            [status],
            |row| row.get(0),
        )
        .context("count move history")
    } else {
        conn.query_row("SELECT COUNT(*) FROM move_history", [], |row| row.get(0))
            .context("count move history")
    }
}

fn list_move_history(
    conn: &Connection,
    roots: &MediaRoots,
    status: Option<&str>,
    limit: i64,
    offset: i64,
) -> Result<Vec<HistoryRow>> {
    let mut rows = if let Some(status) = status {
        let mut stmt = conn.prepare(
            "SELECT * FROM move_history WHERE status=? ORDER BY created_at, id LIMIT ? OFFSET ?",
        )?;
        let rows = stmt
            .query_map((status, limit, offset), basic_history_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    } else {
        let mut stmt =
            conn.prepare("SELECT * FROM move_history ORDER BY created_at, id LIMIT ? OFFSET ?")?;
        let rows = stmt
            .query_map((limit, offset), basic_history_from_row)?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    rows.drain(..)
        .map(|row| attach_history_context(conn, roots, row))
        .collect()
}

fn basic_history_from_row(row: &Row<'_>) -> rusqlite::Result<BasicHistoryRow> {
    Ok(BasicHistoryRow {
        id: row.get("id")?,
        item_id: row.get("item_id")?,
        artist_id: row.get("artist_id")?,
        old_path: row.get("old_path")?,
        new_path: row.get("new_path")?,
        reason: row.get("reason")?,
        status: row.get("status")?,
        details: row.get("details")?,
        created_at: row.get("created_at")?,
        applied_at: row.get("applied_at")?,
        reverted_at: row.get("reverted_at")?,
    })
}

fn attach_history_context(
    conn: &Connection,
    roots: &MediaRoots,
    row: BasicHistoryRow,
) -> Result<HistoryRow> {
    let item_artist_id = query_optional_i64(
        conn,
        "SELECT artist_id FROM items WHERE id=?",
        [row.item_id],
    )?;
    let candidate_artist_id = Some(row.artist_id);
    let item_artist = artist_context(conn, item_artist_id)?;
    let candidate_artist = artist_context(conn, candidate_artist_id)?;
    let item_artist_name = item_artist
        .as_ref()
        .map(|artist| artist.name.clone())
        .unwrap_or_default();
    let candidate_artist_name = candidate_artist
        .as_ref()
        .map(|artist| artist.name.clone())
        .unwrap_or_default();
    let item_artist_path = item_artist
        .as_ref()
        .map(|artist| artist.path.clone())
        .unwrap_or_default();
    let candidate_artist_path = candidate_artist
        .as_ref()
        .map(|artist| artist.path.clone())
        .unwrap_or_default();
    let is_cross_artist = match (item_artist.as_ref(), candidate_artist.as_ref()) {
        (Some(item), Some(candidate)) => item.id != candidate.id,
        _ => false,
    };
    let same_artist_name = !item_artist_name.is_empty()
        && !candidate_artist_name.is_empty()
        && item_artist_name.to_lowercase() == candidate_artist_name.to_lowercase();
    Ok(HistoryRow {
        id: row.id,
        item_id: row.item_id,
        artist_id: row.artist_id,
        old_path: row.old_path.clone(),
        new_path: row.new_path.clone(),
        reason: row.reason,
        status: row.status,
        details: row.details,
        created_at: row.created_at,
        applied_at: row.applied_at,
        reverted_at: row.reverted_at,
        display_old_path: if row.old_path.is_empty() {
            String::new()
        } else {
            display_path(&row.old_path, roots)
        },
        display_new_path: if row.new_path.is_empty() {
            String::new()
        } else {
            display_path(&row.new_path, roots)
        },
        item_artist_id,
        candidate_artist_id,
        item_artist_name,
        candidate_artist_name,
        item_artist_path: item_artist_path.clone(),
        candidate_artist_path: candidate_artist_path.clone(),
        display_item_artist_path: if item_artist_path.is_empty() {
            String::new()
        } else {
            display_path(&item_artist_path, roots)
        },
        display_candidate_artist_path: if candidate_artist_path.is_empty() {
            String::new()
        } else {
            display_path(&candidate_artist_path, roots)
        },
        is_cross_artist,
        same_artist_name,
        can_confirm: !is_cross_artist,
    })
}
