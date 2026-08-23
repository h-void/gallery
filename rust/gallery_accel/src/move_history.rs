use anyhow::{Context, Result};
use rusqlite::{Connection, Row};
use serde::Serialize;
use serde_json::{json, Value};
use std::collections::HashMap;

use crate::media_roots::MediaRoots;
use crate::move_context::{ArtistContext, ArtistContextStore};
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
        "has_more": offset.saturating_add(limit) < total,
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
    let item_ids: Vec<i64> = rows.iter().map(|row| row.item_id).collect();
    let artist_by_item = batch_item_artist_lookup(conn, &item_ids)?;
    let store = ArtistContextStore::load(
        conn,
        rows.iter().map(|row| {
            artist_by_item
                .get(&row.item_id)
                .copied()
                .flatten()
                .or(Some(row.artist_id))
        }),
    )?;
    rows.drain(..)
        .map(|row| {
            let item_artist_id = artist_by_item.get(&row.item_id).copied().flatten();
            let candidate_artist_id = Some(row.artist_id);
            let item_artist = store.get(item_artist_id);
            let candidate_artist = store.get(candidate_artist_id);
            Ok(build_history_row(
                roots,
                row,
                item_artist,
                candidate_artist,
                item_artist_id,
                candidate_artist_id,
            ))
        })
        .collect()
}

/// `item_id -> artist_id` resolved with one `IN (...)` query per 500-id chunk.
fn batch_item_artist_lookup(
    conn: &Connection,
    item_ids: &[i64],
) -> Result<HashMap<i64, Option<i64>>> {
    use std::collections::BTreeSet;
    let mut map: HashMap<i64, Option<i64>> = HashMap::new();
    let unique: BTreeSet<i64> = item_ids.iter().copied().collect();
    let unique: Vec<i64> = unique.into_iter().collect();
    for chunk in unique.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, artist_id FROM items WHERE id IN ({placeholders})"
        ))?;
        let found: Vec<(i64, i64)> = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (item_id, artist_id) in found {
            map.insert(item_id, Some(artist_id));
        }
    }
    for id in unique {
        map.entry(id).or_insert(None);
    }
    Ok(map)
}

#[allow(clippy::too_many_arguments)]
fn build_history_row(
    roots: &MediaRoots,
    row: BasicHistoryRow,
    item_artist: Option<&ArtistContext>,
    candidate_artist: Option<&ArtistContext>,
    item_artist_id: Option<i64>,
    candidate_artist_id: Option<i64>,
) -> HistoryRow {
    let item_artist_name = item_artist
        .map(|artist| artist.name.clone())
        .unwrap_or_default();
    let candidate_artist_name = candidate_artist
        .map(|artist| artist.name.clone())
        .unwrap_or_default();
    let item_artist_path = item_artist
        .map(|artist| artist.path.clone())
        .unwrap_or_default();
    let candidate_artist_path = candidate_artist
        .map(|artist| artist.path.clone())
        .unwrap_or_default();
    let is_cross_artist = match (item_artist, candidate_artist) {
        (Some(item), Some(candidate)) => item.id != candidate.id,
        _ => false,
    };
    let same_artist_name = !item_artist_name.is_empty()
        && !candidate_artist_name.is_empty()
        && item_artist_name.to_lowercase() == candidate_artist_name.to_lowercase();
    HistoryRow {
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
    }
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
