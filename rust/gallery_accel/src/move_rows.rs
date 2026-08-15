use anyhow::Result;
use rusqlite::{Connection, Params, Row};
use serde::Serialize;

use crate::media_roots::MediaRoots;
use crate::move_context::{artist_context, query_optional_i64};
use crate::path_display::display_path;

#[derive(Clone, Serialize, Debug)]
pub(crate) struct MoveRow {
    pub(crate) id: i64,
    scan_candidate_id: Option<i64>,
    item_id: Option<i64>,
    artist_id: i64,
    old_path: String,
    new_path: String,
    reason: String,
    content_hash: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
    status: String,
    created_at: f64,
    resolved_at: Option<f64>,
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

pub(crate) fn query_move_rows<P: Params>(
    conn: &Connection,
    roots: &MediaRoots,
    sql: &str,
    params: P,
) -> Result<Vec<MoveRow>> {
    let mut stmt = conn.prepare(sql)?;
    let basic_rows: Vec<BasicMoveRow> = stmt
        .query_map(params, basic_move_from_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    basic_rows
        .into_iter()
        .map(|row| attach_move_context(conn, roots, row))
        .collect()
}

fn basic_move_from_row(row: &Row<'_>) -> rusqlite::Result<BasicMoveRow> {
    Ok(BasicMoveRow {
        id: row.get("id")?,
        scan_candidate_id: row.get("scan_candidate_id")?,
        item_id: row.get("item_id")?,
        artist_id: row.get("artist_id")?,
        old_path: row.get("old_path")?,
        new_path: row.get("new_path")?,
        reason: row.get("reason")?,
        content_hash: row.get("content_hash")?,
        st_dev: row.get("st_dev")?,
        st_ino: row.get("st_ino")?,
        status: row.get("status")?,
        created_at: row.get("created_at")?,
        resolved_at: row.get("resolved_at")?,
    })
}

#[derive(Clone, Debug)]
struct BasicMoveRow {
    id: i64,
    scan_candidate_id: Option<i64>,
    item_id: Option<i64>,
    artist_id: i64,
    old_path: String,
    new_path: String,
    reason: String,
    content_hash: String,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
    status: String,
    created_at: f64,
    resolved_at: Option<f64>,
}

fn attach_move_context(
    conn: &Connection,
    roots: &MediaRoots,
    row: BasicMoveRow,
) -> Result<MoveRow> {
    let item_artist_id = if let Some(item_id) = row.item_id {
        query_optional_i64(conn, "SELECT artist_id FROM items WHERE id=?", [item_id])?
    } else {
        None
    };
    let candidate_artist_id = if let Some(scan_candidate_id) = row.scan_candidate_id {
        query_optional_i64(
            conn,
            "SELECT artist_id FROM scan_candidates WHERE id=?",
            [scan_candidate_id],
        )?
        .or(Some(row.artist_id))
    } else {
        Some(row.artist_id)
    };
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
    Ok(MoveRow {
        id: row.id,
        scan_candidate_id: row.scan_candidate_id,
        item_id: row.item_id,
        artist_id: row.artist_id,
        old_path: row.old_path.clone(),
        new_path: row.new_path.clone(),
        reason: row.reason,
        content_hash: row.content_hash,
        st_dev: row.st_dev,
        st_ino: row.st_ino,
        status: row.status,
        created_at: row.created_at,
        resolved_at: row.resolved_at,
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
        can_confirm: row.item_id.is_some() && !is_cross_artist,
    })
}
