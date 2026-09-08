use anyhow::Result;
use rusqlite::{Connection, Params, Row};
use serde::Serialize;
use std::collections::{BTreeSet, HashMap};

use crate::media_roots::MediaRoots;
use crate::move_context::{ArtistContext, ArtistContextStore};
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
    /// The linked item's own content hash, so the UI can state whether both
    /// sides actually agree instead of claiming "matched" from one side.
    item_hash: String,
    /// True only when both hashes are present and equal (P3 wording gate).
    hash_match: bool,
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
    attach_move_context_batch(conn, roots, basic_rows)
}

/// `id -> artist_id` for one table, resolved with a single `IN (...)` query
/// per 500-id chunk instead of one query per row.
fn batch_artist_lookup(
    conn: &Connection,
    table: &str,
    ids: &[i64],
) -> Result<HashMap<i64, Option<i64>>> {
    let mut map: HashMap<i64, Option<i64>> = HashMap::new();
    let unique: BTreeSet<i64> = ids.iter().copied().collect();
    let unique: Vec<i64> = unique.into_iter().collect();
    for chunk in unique.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, artist_id FROM {table} WHERE id IN ({placeholders})"
        ))?;
        let found: Vec<(i64, i64)> = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, artist_id) in found {
            map.insert(id, Some(artist_id));
        }
    }
    for id in unique {
        map.entry(id).or_insert(None);
    }
    Ok(map)
}

/// Batched `id -> integer column` lookup, mirroring `batch_artist_lookup`.
/// Used for evidence the row needs to judge an action but must not expose as
/// a new serialized field.
fn batch_i64_lookup(
    conn: &Connection,
    table: &str,
    column: &str,
    ids: &[i64],
) -> Result<HashMap<i64, Option<i64>>> {
    let mut map: HashMap<i64, Option<i64>> = HashMap::new();
    let unique: BTreeSet<i64> = ids.iter().copied().collect();
    let unique: Vec<i64> = unique.into_iter().collect();
    for chunk in unique.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, {column} FROM {table} WHERE id IN ({placeholders})"
        ))?;
        let found: Vec<(i64, i64)> = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, value) in found {
            map.insert(id, Some(value));
        }
    }
    for id in unique {
        map.entry(id).or_insert(None);
    }
    Ok(map)
}

fn attach_move_context_batch(
    conn: &Connection,
    roots: &MediaRoots,
    rows: Vec<BasicMoveRow>,
) -> Result<Vec<MoveRow>> {
    let item_ids: Vec<i64> = rows.iter().filter_map(|row| row.item_id).collect();
    let candidate_ids: Vec<i64> = rows
        .iter()
        .filter_map(|row| row.scan_candidate_id)
        .collect();
    let artist_by_item = batch_artist_lookup(conn, "items", &item_ids)?;
    let artist_by_scan_candidate = batch_artist_lookup(conn, "scan_candidates", &candidate_ids)?;
    let hash_by_item = batch_text_lookup(conn, "items", "content_hash", &item_ids)?;
    let missing_by_item = batch_i64_lookup(conn, "items", "missing", &item_ids)?;
    let store = ArtistContextStore::load(
        conn,
        rows.iter().flat_map(|row| {
            let item_artist = row
                .item_id
                .and_then(|id| artist_by_item.get(&id).copied())
                .flatten();
            let candidate_artist = row
                .scan_candidate_id
                .and_then(|id| artist_by_scan_candidate.get(&id).copied())
                .flatten()
                .or(Some(row.artist_id));
            [item_artist, candidate_artist]
        }),
    )?;
    rows.into_iter()
        .map(|row| {
            assemble_move_row(
                roots,
                row,
                &artist_by_item,
                &artist_by_scan_candidate,
                &hash_by_item,
                &missing_by_item,
                &store,
            )
        })
        .collect()
}

/// Batched `id -> text column` lookup, mirroring `batch_artist_lookup`.
fn batch_text_lookup(
    conn: &Connection,
    table: &str,
    column: &str,
    ids: &[i64],
) -> Result<HashMap<i64, Option<String>>> {
    let mut map: HashMap<i64, Option<String>> = HashMap::new();
    let unique: BTreeSet<i64> = ids.iter().copied().collect();
    let unique: Vec<i64> = unique.into_iter().collect();
    for chunk in unique.chunks(500) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let mut stmt = conn.prepare(&format!(
            "SELECT id, {column} FROM {table} WHERE id IN ({placeholders})"
        ))?;
        let found: Vec<(i64, Option<String>)> = stmt
            .query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (id, value) in found {
            map.insert(id, value);
        }
    }
    for id in unique {
        map.entry(id).or_insert(None);
    }
    Ok(map)
}

fn assemble_move_row(
    roots: &MediaRoots,
    row: BasicMoveRow,
    artist_by_item: &HashMap<i64, Option<i64>>,
    artist_by_scan_candidate: &HashMap<i64, Option<i64>>,
    hash_by_item: &HashMap<i64, Option<String>>,
    missing_by_item: &HashMap<i64, Option<i64>>,
    store: &ArtistContextStore,
) -> Result<MoveRow> {
    let item_artist_id = row
        .item_id
        .and_then(|id| artist_by_item.get(&id).copied())
        .flatten();
    let candidate_artist_id = row
        .scan_candidate_id
        .and_then(|id| artist_by_scan_candidate.get(&id).copied())
        .flatten()
        .or(Some(row.artist_id));
    let item_hash = row
        .item_id
        .and_then(|id| hash_by_item.get(&id).cloned())
        .flatten()
        .unwrap_or_default();
    let item_missing = row
        .item_id
        .and_then(|id| missing_by_item.get(&id).copied())
        .flatten();
    let item_artist = store.get(item_artist_id);
    let candidate_artist = store.get(candidate_artist_id);
    Ok(build_move_row(
        roots,
        row,
        item_artist,
        candidate_artist,
        item_artist_id,
        candidate_artist_id,
        item_hash,
        item_missing,
    ))
}

fn build_move_row(
    roots: &MediaRoots,
    row: BasicMoveRow,
    item_artist: Option<&ArtistContext>,
    candidate_artist: Option<&ArtistContext>,
    item_artist_id: Option<i64>,
    candidate_artist_id: Option<i64>,
    item_hash: String,
    item_missing: Option<i64>,
) -> MoveRow {
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
    let is_pending = row.status == "pending";
    let is_cross_artist = match (item_artist, candidate_artist) {
        (Some(item), Some(candidate)) => item.id != candidate.id,
        _ => false,
    };
    let same_artist_name = !item_artist_name.is_empty()
        && !candidate_artist_name.is_empty()
        && item_artist_name.to_lowercase() == candidate_artist_name.to_lowercase();
    MoveRow {
        id: row.id,
        scan_candidate_id: row.scan_candidate_id,
        item_id: row.item_id,
        artist_id: row.artist_id,
        old_path: row.old_path.clone(),
        new_path: row.new_path.clone(),
        reason: row.reason,
        content_hash: row.content_hash.clone(),
        item_hash: item_hash.clone(),
        hash_match: !item_hash.is_empty()
            && !row.content_hash.is_empty()
            && item_hash == row.content_hash,
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
        // Confirming repairs one old record's path, so the row must still be
        // actionable: pending, linked to an existing record that is actually
        // missing, and with both sides resolving to the same artist. An
        // unknown artist is not evidence that the move is same-artist.
        can_confirm: is_pending
            && row.item_id.is_some()
            && item_missing == Some(1)
            && item_artist_id.is_some()
            && candidate_artist_id.is_some()
            && item_artist_id == candidate_artist_id,
    }
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
