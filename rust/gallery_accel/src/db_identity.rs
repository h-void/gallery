//! Media-path migration and artist/item identity merging.
//!
//! Split out of `db.rs` (plan M10). The pool/schema half of that module owns
//! connection lifecycle and fails loudly at startup; everything here is one
//! bounded, signature-gated rewrite of rows that are already stored, where a
//! mistake corrupts history once instead. Different failure modes, so they no
//! longer share a file.
//!
//! The entry point is `normalize_configured_media_paths`, re-exported from
//! `crate::db` so existing callers and the migration regression tests keep
//! addressing it exactly as before.

use std::collections::HashMap;

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::db::sql_ident;
use crate::media_roots::MediaRoots;

fn media_path_migration_signature(roots: &MediaRoots) -> String {
    let roots_n: Vec<String> = roots
        .roots
        .iter()
        .map(|r| r.replace('\\', "/").trim_end_matches('/').to_string())
        .collect();
    let reals_n: Vec<String> = roots
        .real_paths
        .iter()
        .map(|r| r.replace('\\', "/").trim_end_matches('/').to_string())
        .collect();
    json!({"roots": roots_n, "real_paths": reals_n}).to_string()
}

fn has_virtual_paths(conn: &Connection, roots: &MediaRoots) -> Result<bool> {
    let columns = [
        ("artists", "path"),
        ("items", "file_path"),
        ("artist_link_documents", "file_path"),
        ("scan_seen", "file_path"),
        ("scan_candidates", "file_path"),
        ("move_candidates", "old_path"),
        ("move_candidates", "new_path"),
        ("move_history", "old_path"),
        ("move_history", "new_path"),
    ];
    for (table, column) in columns {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .with_context(|| format!("probe presence of table {table}"))?;
        if exists == 0 {
            continue;
        }
        for (root_index, root) in roots.roots.iter().enumerate() {
            let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
            if root_n.is_empty() {
                continue;
            }
            // Skip when virtual root already equals real root (no alias). Use the
            // enumeration index: position() misaligns duplicate virtual roots
            // that map to different real paths.
            let idx = root_index;
            if roots
                .real_root_at(idx)
                .map(|r| r.replace('\\', "/").trim_end_matches('/') == root_n.as_str())
                == Some(true)
            {
                continue;
            }
            let hit: i64 = conn
                .query_row(
                    &format!(
                        // Exact-prefix match: LIKE is case-insensitive and
                        // treats `_` as a wildcard, so a root like
                        // /volume1/my_pictures would also match sibling dirs.
                        "SELECT COUNT(1) FROM {table}
                         WHERE {column}=? OR substr({column}, 1, length(?) + 1) = ? || '/'
                         LIMIT 1"
                    ),
                    rusqlite::params![&root_n, &root_n, &root_n],
                    |r| r.get(0),
                )
                .with_context(|| format!("count virtual paths in {table}.{column}"))?;
            if hit > 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn set_migration_signature(conn: &Connection, signature: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at)
         VALUES ('media_path_real_migration_signature', ?, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![signature],
    )?;
    Ok(())
}

/// Probe whether a table exists without creating it. A failed probe is an
/// error, distinct from a genuinely absent optional table.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .with_context(|| format!("probe schema table {name}"))?;
    Ok(count > 0)
}

#[derive(Clone, Debug)]
struct ArtistReferenceMergeRow {
    id: i64,
    artist_id: i64,
    item_id: Option<i64>,
    style_group: String,
    dino_embedding: Option<Vec<u8>>,
    dino_embedding_dim: Option<i64>,
    wd14_embedding: Option<Vec<u8>>,
    wd14_embedding_dim: Option<i64>,
    embedding_model_variant: String,
    embedding_updated_at: Option<f64>,
    created_at: f64,
}

const ARTIST_REFERENCE_COLUMNS: &str = "id, artist_id, item_id, style_group, dino_embedding, \
     dino_embedding_dim, wd14_embedding, wd14_embedding_dim, embedding_model_variant, \
     embedding_updated_at, created_at";

/// Column names of a table that is known to exist (PRAGMA table_info).
fn table_columns(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let columns = conn
        .prepare(&format!("PRAGMA table_info({name})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("probe columns of {name}"))?;
    Ok(columns)
}

/// Whether a known table carries the given column; missing table yields false.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(conn, table)?
        .iter()
        .any(|candidate| candidate == column))
}

fn write_artist_reference_row(
    conn: &Connection,
    target_row_id: i64,
    row: &ArtistReferenceMergeRow,
) -> Result<()> {
    conn.execute(
        "UPDATE artist_references
         SET style_group=?, dino_embedding=?, dino_embedding_dim=?, wd14_embedding=?,
             wd14_embedding_dim=?, embedding_model_variant=?, embedding_updated_at=?,
             created_at=?
         WHERE id=?",
        rusqlite::params![
            row.style_group,
            row.dino_embedding,
            row.dino_embedding_dim,
            row.wd14_embedding,
            row.wd14_embedding_dim,
            row.embedding_model_variant,
            row.embedding_updated_at,
            row.created_at,
            target_row_id
        ],
    )
    .with_context(|| {
        format!(
            "merge artist reference {} onto reference {target_row_id}",
            row.id
        )
    })?;
    Ok(())
}

/// Before a losing reference is deleted, remap every
/// `artist_suggestions.matched_ref_id` pointing at it to the surviving
/// reference so restore/regeneration stays valid (optional column).
fn remap_artist_suggestion_matched_refs(
    conn: &Connection,
    loser_ref_id: i64,
    surviving_ref_id: i64,
) -> Result<()> {
    if table_exists(conn, "artist_suggestions")?
        && column_exists(conn, "artist_suggestions", "matched_ref_id")?
    {
        conn.execute(
            "UPDATE artist_suggestions SET matched_ref_id=? WHERE matched_ref_id=?",
            rusqlite::params![surviving_ref_id, loser_ref_id],
        )
        .with_context(|| {
            format!(
                "repoint suggestion matched_ref_id from reference {loser_ref_id} to {surviving_ref_id}"
            )
        })?;
    }
    Ok(())
}

/// Move `artist_references` rows so one coordinate (item or artist) changes to
/// a value that already exists elsewhere; deterministic newest row wins on
/// UNIQUE(artist_id, item_id) collisions, the loser is removed.
fn repoint_artist_references(
    conn: &Connection,
    move_by_item: Option<(i64, i64)>,
    move_by_artist: Option<(i64, i64)>,
) -> Result<()> {
    let (source_id, target_id, by_item) = match (move_by_item, move_by_artist) {
        (Some((source_item, keep_item)), None) => (source_item, keep_item, true),
        (None, Some((source_artist, target_artist))) => (source_artist, target_artist, false),
        _ => unreachable!("repoint_artist_references takes exactly one coordinate"),
    };
    let column = if by_item { "item_id" } else { "artist_id" };
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIST_REFERENCE_COLUMNS} FROM artist_references WHERE {column}=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_id], |row| {
            Ok(ArtistReferenceMergeRow {
                id: row.get(0)?,
                artist_id: row.get(1)?,
                item_id: row.get(2)?,
                style_group: row.get(3)?,
                dino_embedding: row.get(4)?,
                dino_embedding_dim: row.get(5)?,
                wd14_embedding: row.get(6)?,
                wd14_embedding_dim: row.get(7)?,
                embedding_model_variant: row.get(8)?,
                embedding_updated_at: row.get(9)?,
                created_at: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        if !by_item && row.item_id.is_none() {
            // Detached references are not unique per artist (NULL item_id),
            // so they can never collide: plain artist reassignment.
            conn.execute(
                "UPDATE artist_references SET artist_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint detached artist reference {} artist", row.id))?;
            continue;
        }
        let collision: Option<(i64, f64)> = if by_item {
            conn.query_row(
                "SELECT id, created_at FROM artist_references
                 WHERE artist_id=? AND item_id=? AND id<>?",
                rusqlite::params![row.artist_id, target_id, row.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist reference collision for item {target_id}"))?
        } else {
            conn.query_row(
                "SELECT id, created_at FROM artist_references
                 WHERE artist_id=? AND item_id=? AND id<>?",
                rusqlite::params![target_id, row.item_id.unwrap(), row.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist reference collision for artist {target_id}"))?
        };
        if let Some((target_row_id, target_created)) = collision {
            if (row.created_at, row.id) > (target_created, target_row_id) {
                write_artist_reference_row(conn, target_row_id, &row)?;
            }
            remap_artist_suggestion_matched_refs(conn, row.id, target_row_id)?;
            conn.execute(
                "DELETE FROM artist_references WHERE id=?",
                rusqlite::params![row.id],
            )
            .with_context(|| format!("drop losing artist reference {}", row.id))?;
        } else if by_item {
            conn.execute(
                "UPDATE artist_references SET item_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint artist reference {} item", row.id))?;
        } else {
            conn.execute(
                "UPDATE artist_references SET artist_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint artist reference {} artist", row.id))?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ArtistSuggestionMergeRow {
    artist_id: Option<i64>,
    item_id: Option<i64>,
    status: String,
    dino_score: Option<f64>,
    wd14_score: Option<f64>,
    fused_score: Option<f64>,
    matched_ref_id: Option<i64>,
    reason: String,
    confirmed_at: Option<f64>,
}

fn suggestion_status_priority(status: &str) -> i64 {
    // Explicit user decisions outrank regenerated suggestions.
    match status {
        "confirmed" => 3,
        "rejected" => 2,
        "pending" | "suggested" => 1,
        _ => 0,
    }
}

/// Optional score columns of `artist_suggestions` (the Rust-created minimal
/// table lacks them while the legacy Python table has all of them), probed so
/// merges never fail on either schema.
fn artist_suggestion_optional_columns(conn: &Connection) -> Result<Vec<String>> {
    let columns = table_columns(conn, "artist_suggestions")?;
    Ok(
        ["dino_score", "wd14_score", "fused_score", "matched_ref_id"]
            .iter()
            .filter(|name| columns.iter().any(|column| column.as_str() == **name))
            .map(|name| name.to_string())
            .collect(),
    )
}

/// Rows whose item or artist equals `source_id`; the row identity is the
/// UNIQUE(item_id, artist_id) pair, so no `id` column is required (the
/// Rust-created minimal table has none).
fn read_artist_suggestion_merge_rows(
    conn: &Connection,
    column: &str,
    source_id: i64,
) -> Result<Vec<ArtistSuggestionMergeRow>> {
    let optional = artist_suggestion_optional_columns(conn)?;
    let optional_sql = if optional.is_empty() {
        String::new()
    } else {
        format!(", {}", optional.join(", "))
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT item_id, artist_id, status, reason, confirmed_at{optional_sql}
         FROM artist_suggestions WHERE {column}=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_id], |row| {
            let mut dino_score = None;
            let mut wd14_score = None;
            let mut fused_score = None;
            let mut matched_ref_id = None;
            for (offset, name) in optional.iter().enumerate() {
                let index = 5usize + offset;
                match name.as_str() {
                    "dino_score" => dino_score = row.get(index)?,
                    "wd14_score" => wd14_score = row.get(index)?,
                    "fused_score" => fused_score = row.get(index)?,
                    "matched_ref_id" => matched_ref_id = row.get(index)?,
                    _ => {}
                }
            }
            Ok(ArtistSuggestionMergeRow {
                artist_id: row.get(1)?,
                item_id: row.get(0)?,
                status: row.get(2)?,
                reason: row.get(3)?,
                confirmed_at: row.get(4)?,
                dino_score,
                wd14_score,
                fused_score,
                matched_ref_id,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn sql_value_real(value: Option<f64>) -> rusqlite::types::Value {
    match value {
        Some(v) => rusqlite::types::Value::Real(v),
        None => rusqlite::types::Value::Null,
    }
}

fn write_artist_suggestion_row(
    conn: &Connection,
    row: &ArtistSuggestionMergeRow,
    target_item: i64,
    target_artist: i64,
) -> Result<()> {
    let optional = artist_suggestion_optional_columns(conn)?;
    let mut set: Vec<String> = vec![
        "status=?".to_string(),
        "reason=?".to_string(),
        "confirmed_at=?".to_string(),
    ];
    set.extend(optional.iter().map(|name| format!("{name}=?")));
    let sql = format!(
        "UPDATE artist_suggestions SET {} WHERE item_id=? AND artist_id=?",
        set.join(", ")
    );
    let mut values: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(row.status.clone()),
        rusqlite::types::Value::Text(row.reason.clone()),
        sql_value_real(row.confirmed_at),
    ];
    for name in &optional {
        values.push(match name.as_str() {
            "dino_score" => sql_value_real(row.dino_score),
            "wd14_score" => sql_value_real(row.wd14_score),
            "fused_score" => sql_value_real(row.fused_score),
            "matched_ref_id" => match row.matched_ref_id {
                Some(v) => rusqlite::types::Value::Integer(v),
                None => rusqlite::types::Value::Null,
            },
            _ => rusqlite::types::Value::Null,
        });
    }
    values.push(rusqlite::types::Value::Integer(target_item));
    values.push(rusqlite::types::Value::Integer(target_artist));
    conn.execute(&sql, rusqlite::params_from_iter(values.iter()))
        .with_context(|| {
            format!(
                "merge artist suggestion of item {} onto artist {target_artist}",
                row.item_id.unwrap_or(0)
            )
        })?;
    Ok(())
}

/// Move `artist_suggestions` rows so one coordinate changes to a value that
/// already exists elsewhere; the higher-status winner survives on
/// UNIQUE(item_id, artist_id) collisions, ties keep the existing target row.
/// The (item_id, artist_id) pair identifies rows, so this also works on the
/// minimal Rust-created table without an `id` column.
fn repoint_artist_suggestions(
    conn: &Connection,
    move_by_item: Option<(i64, i64)>,
    move_by_artist: Option<(i64, i64)>,
) -> Result<()> {
    let (source_id, target_id, by_item) = match (move_by_item, move_by_artist) {
        (Some((source_item, keep_item)), None) => (source_item, keep_item, true),
        (None, Some((source_artist, target_artist))) => (source_artist, target_artist, false),
        _ => unreachable!("repoint_artist_suggestions takes exactly one coordinate"),
    };
    let rows = read_artist_suggestion_merge_rows(
        conn,
        if by_item { "item_id" } else { "artist_id" },
        source_id,
    )?;
    for row in rows {
        let (source_item, source_artist) = (row.item_id, row.artist_id);
        if (by_item && source_artist.is_none()) || (!by_item && source_item.is_none()) {
            // A NULL coordinate is not part of the UNIQUE(item_id, artist_id)
            // pair, so it can never collide: bulk-repoint it plainly.
            if by_item {
                conn.execute(
                    "UPDATE artist_suggestions SET item_id=?
                     WHERE item_id=? AND artist_id IS NULL",
                    rusqlite::params![target_id, source_id],
                )
                .with_context(|| {
                    format!("repoint NULL-artist suggestion of item {source_id} onto {target_id}")
                })?;
            } else {
                conn.execute(
                    "UPDATE artist_suggestions SET artist_id=?
                     WHERE item_id IS NULL AND artist_id=?",
                    rusqlite::params![target_id, source_id],
                )
                .with_context(|| {
                    format!("repoint NULL-item suggestion of artist {source_id} onto {target_id}")
                })?;
            }
            continue;
        }
        let collision: Option<(i64, i64)> = if by_item {
            conn.query_row(
                "SELECT item_id, artist_id FROM artist_suggestions
                 WHERE item_id=? AND artist_id IS ?",
                rusqlite::params![target_id, row.artist_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist suggestion collision for item {target_id}"))?
        } else {
            conn.query_row(
                "SELECT item_id, artist_id FROM artist_suggestions
                 WHERE item_id IS ? AND artist_id=?",
                rusqlite::params![row.item_id, target_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist suggestion collision for artist {target_id}"))?
        };
        let (source_item, source_artist) = (row.item_id, row.artist_id);
        if let Some((target_item, target_artist)) = collision {
            let target_status: String = conn.query_row(
                "SELECT status FROM artist_suggestions
                 WHERE item_id=? AND artist_id=?",
                rusqlite::params![target_item, target_artist],
                |r| r.get(0),
            )?;
            if suggestion_status_priority(&row.status) > suggestion_status_priority(&target_status)
            {
                write_artist_suggestion_row(conn, &row, target_item, target_artist)?;
            }
            conn.execute(
                "DELETE FROM artist_suggestions WHERE item_id IS ? AND artist_id IS ?",
                rusqlite::params![source_item, source_artist],
            )
            .with_context(|| format!("drop losing artist suggestion for item {source_item:?}"))?;
        } else if by_item {
            conn.execute(
                "UPDATE artist_suggestions SET item_id=? WHERE item_id=? AND artist_id IS ?",
                rusqlite::params![target_id, source_item, source_artist],
            )
            .with_context(|| {
                format!("repoint artist suggestion item for artist {source_artist:?}")
            })?;
        } else {
            conn.execute(
                "UPDATE artist_suggestions SET artist_id=? WHERE item_id IS ? AND artist_id=?",
                rusqlite::params![target_id, source_item, source_artist],
            )
            .with_context(|| {
                format!("repoint artist suggestion artist for item {source_item:?}")
            })?;
        }
    }
    Ok(())
}

/// Remap `selected_tag_ids` of the merged artist's folder plans through a tag
/// alias map after its tags were folded into the target artist's tags. Strict
/// JSON: a malformed selection on an affected plan is a migration error, not
/// an empty default, and unrelated plans are never read.
fn remap_folder_plan_tag_ids(
    conn: &Connection,
    tag_map: &HashMap<i64, i64>,
    artist_ids: &[i64],
) -> Result<()> {
    if tag_map.is_empty()
        || artist_ids.is_empty()
        || !table_exists(conn, "folder_rename_plans")?
        || !column_exists(conn, "folder_rename_plans", "selected_tag_ids")?
    {
        return Ok(());
    }
    let placeholders = std::iter::repeat_n("?", artist_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, selected_tag_ids FROM folder_rename_plans
         WHERE artist_id IN ({placeholders})"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(artist_ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (plan_id, raw) in rows {
        let ids = serde_json::from_str::<Vec<i64>>(&raw)
            .with_context(|| format!("parse selected_tag_ids of folder plan {plan_id}"))?;
        let mut mapped: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            let remapped = tag_map.get(&id).copied().unwrap_or(id);
            if !mapped.contains(&remapped) {
                mapped.push(remapped);
            }
        }
        let encoded = serde_json::to_string(&mapped)
            .with_context(|| format!("encode selected_tag_ids of folder plan {plan_id}"))?;
        if encoded != raw {
            conn.execute(
                "UPDATE folder_rename_plans SET selected_tag_ids=? WHERE id=?",
                rusqlite::params![encoded, plan_id],
            )
            .with_context(|| format!("remap selected_tag_ids of folder plan {plan_id}"))?;
        }
    }
    Ok(())
}

/// Semantic identity fields of a folder plan, compared across an artist merge;
/// legacy tables may lack some, and identity is judged on what is available.
const FOLDER_PLAN_SEMANTIC_COLUMNS: &[&str] = &[
    "original_folder_name",
    "original_title",
    "parsed_date",
    "selected_tag_ids",
    "status",
    "file_count",
    "total_size",
    "max_mtime",
    "created_at",
    "updated_at",
    "confirmed_at",
    "confirmation_source",
    "target_folder",
    "executed_at",
    "execution_log",
    "format_snapshot",
    "plan_kind",
    "split_actions",
];

/// The folder-plan columns actually present in the connected schema; legacy
/// tables may lack some, and identity is judged on what is available.
fn folder_plan_semantic_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut present = Vec::new();
    for name in FOLDER_PLAN_SEMANTIC_COLUMNS {
        if column_exists(conn, "folder_rename_plans", name)? {
            present.push((*name).to_string());
        }
    }
    Ok(present)
}

/// Reassign folder plans of a merged alias artist. A plan whose source folder
/// already exists for the target artist is removed only when every available
/// semantic field matches after tag remapping; otherwise the migration aborts
/// and rolls back instead of silently dropping an executed/distinct plan.
fn merge_artist_folder_plans(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if !table_exists(conn, "folder_rename_plans")? {
        return Ok(());
    }
    let semantic = folder_plan_semantic_columns(conn)?;
    if semantic.is_empty() {
        conn.execute(
            "UPDATE folder_rename_plans SET artist_id=? WHERE artist_id=?",
            rusqlite::params![target_artist, source_artist],
        )
        .with_context(|| format!("reassign folder plans of artist {source_artist}"))?;
        return Ok(());
    }
    let select = format!("id, source_folder, {}", semantic.join(", "));
    let mut stmt = conn.prepare(&format!(
        "SELECT {select} FROM folder_rename_plans WHERE artist_id=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_artist], |row| {
            let mut values: Vec<Value> = Vec::with_capacity(semantic.len() + 2);
            values.push(json!(row.get::<_, i64>(0)?));
            values.push(json!(row.get::<_, String>(1)?));
            for index in 0..semantic.len() {
                values.push(column_value_json(row, 2 + index)?);
            }
            Ok(values)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for source in rows {
        let plan_id = source[0].as_i64().unwrap();
        let source_folder = source[1].as_str().unwrap();
        let target: Option<Vec<Value>> = conn
            .query_row(
                &format!(
                    "SELECT {select} FROM folder_rename_plans
                     WHERE artist_id=? AND source_folder=?"
                ),
                rusqlite::params![target_artist, source_folder],
                |row| {
                    let mut values: Vec<Value> = Vec::with_capacity(semantic.len() + 2);
                    values.push(json!(row.get::<_, i64>(0)?));
                    values.push(json!(row.get::<_, String>(1)?));
                    for index in 0..semantic.len() {
                        values.push(column_value_json(row, 2 + index)?);
                    }
                    Ok(values)
                },
            )
            .optional()
            .with_context(|| {
                format!(
                    "probe folder plan conflict of artist {target_artist} for folder {source_folder}"
                )
            })?;
        if let Some(target) = target {
            let target_plan_id = target[0].as_i64().unwrap();
            if source[2..] == target[2..] {
                conn.execute(
                    "DELETE FROM folder_rename_plans WHERE id=?",
                    rusqlite::params![plan_id],
                )
                .with_context(|| format!("drop duplicate folder plan {plan_id}"))?;
            } else {
                return Err(anyhow!(
                    "folder plan conflict: plan {plan_id} of artist {source_artist} and \
                     plan {target_plan_id} of artist {target_artist} share source folder \
                     {source_folder} but differ in semantic fields; refusing to merge"
                ));
            }
        } else {
            conn.execute(
                "UPDATE folder_rename_plans SET artist_id=? WHERE id=?",
                rusqlite::params![target_artist, plan_id],
            )
            .with_context(|| format!("reassign folder plan {plan_id} to artist {target_artist}"))?;
        }
    }
    Ok(())
}

/// Lossless JSON copy of one row column for identity comparisons.
fn column_value_json(row: &rusqlite::Row, index: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => json!(value.to_vec()),
    })
}

/// Merge every authoritative relationship from a source item onto a kept item
/// before the source row is deleted. Optional tables are probed (missing
/// allowed, probe failure fatal) and merged or repointed deterministically.
fn merge_item_into(
    conn: &Connection,
    source_id: i64,
    keep_id: i64,
    target_artist: Option<i64>,
) -> Result<()> {
    if let Some(target_artist) = target_artist {
        // Artifact of the artist-scoped merge: the kept item must belong to
        // the surviving artist before any derived document coordinates are
        // read from it (its own row may live under a different artist).
        conn.execute(
            "UPDATE items SET artist_id=? WHERE id=?",
            rusqlite::params![target_artist, keep_id],
        )
        .with_context(|| format!("assign kept item {keep_id} to artist {target_artist}"))?;
    }
    if column_exists(conn, "items", "missing")? {
        let (source_missing, keep_missing): (i64, i64) = conn
            .query_row(
                "SELECT
                   (SELECT missing FROM items WHERE id=?),
                   (SELECT missing FROM items WHERE id=?)",
                rusqlite::params![source_id, keep_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("probe missing state of items {source_id}/{keep_id}"))?;
        if source_missing == 0 || keep_missing == 0 {
            let sql = if column_exists(conn, "items", "missing_at")? {
                "UPDATE items SET missing=0, missing_at=NULL WHERE id=?"
            } else {
                "UPDATE items SET missing=0 WHERE id=?"
            };
            conn.execute(sql, rusqlite::params![keep_id])
                .with_context(|| format!("keep merged item {keep_id} active"))?;
        }
    }
    conn.execute(
        "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
         SELECT ?, tag_id FROM item_tags WHERE item_id=?",
        rusqlite::params![keep_id, source_id],
    )
    .with_context(|| format!("merge item_tags from item {source_id} onto item {keep_id}"))?;
    conn.execute(
        "DELETE FROM item_tags WHERE item_id=?",
        rusqlite::params![source_id],
    )
    .with_context(|| format!("drop item_tags links of merged item {source_id}"))?;
    if table_exists(conn, "item_favorites")? {
        conn.execute(
            "INSERT OR IGNORE INTO item_favorites (item_id, created_at)
             SELECT ?, created_at FROM item_favorites WHERE item_id=?",
            rusqlite::params![keep_id, source_id],
        )
        .with_context(|| format!("merge favorites of item {source_id} onto item {keep_id}"))?;
        conn.execute(
            "DELETE FROM item_favorites WHERE item_id=?",
            rusqlite::params![source_id],
        )
        .with_context(|| format!("drop favorites of merged item {source_id}"))?;
    }
    if table_exists(conn, "character_references")?
        && column_exists(conn, "character_references", "item_id")?
    {
        conn.execute(
            "UPDATE character_references SET item_id=? WHERE item_id=?",
            rusqlite::params![keep_id, source_id],
        )
        .with_context(|| {
            format!("repoint character references of item {source_id} onto {keep_id}")
        })?;
    }
    if table_exists(conn, "character_recognition_results")? {
        let source_has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM character_recognition_results WHERE item_id=?)",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .with_context(|| format!("probe recognition result of item {source_id}"))?;
        if source_has {
            let target_has: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM character_recognition_results WHERE item_id=?)",
                    rusqlite::params![keep_id],
                    |row| row.get(0),
                )
                .with_context(|| format!("probe recognition result of item {keep_id}"))?;
            if target_has {
                // The kept item's computation survives; the source row drops.
                conn.execute(
                    "DELETE FROM character_recognition_results WHERE item_id=?",
                    rusqlite::params![source_id],
                )
                .with_context(|| {
                    format!("drop superseded recognition result of item {source_id}")
                })?;
            } else {
                conn.execute(
                    "UPDATE character_recognition_results SET item_id=? WHERE item_id=?",
                    rusqlite::params![keep_id, source_id],
                )
                .with_context(|| {
                    format!("repoint recognition result of item {source_id} onto {keep_id}")
                })?;
            }
        }
    }
    for table in ["move_candidates", "move_history"] {
        if table_exists(conn, table)? && column_exists(conn, table, "item_id")? {
            let table = sql_ident(table);
            conn.execute(
                &format!("UPDATE {table} SET item_id=? WHERE item_id=?"),
                rusqlite::params![keep_id, source_id],
            )
            .with_context(|| format!("repoint {table} rows of item {source_id} onto {keep_id}"))?;
        }
    }
    if table_exists(conn, "artist_references")? {
        repoint_artist_references(conn, Some((source_id, keep_id)), None)?;
    }
    if table_exists(conn, "artist_suggestions")? {
        repoint_artist_suggestions(conn, Some((source_id, keep_id)), None)?;
    }
    if table_exists(conn, "artist_link_documents")?
        && column_exists(conn, "artist_link_documents", "item_id")?
    {
        let source_doc: Option<i64> = conn
            .query_row(
                "SELECT id FROM artist_link_documents WHERE item_id=?",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("probe link document of item {source_id}"))?;
        if let Some(source_doc_id) = source_doc {
            let target_doc: Option<i64> = conn
                .query_row(
                    "SELECT id FROM artist_link_documents WHERE item_id=?",
                    rusqlite::params![keep_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| format!("probe link document of item {keep_id}"))?;
            if let Some(_target_doc_id) = target_doc {
                // Both items were indexed: the kept item's valid document
                // survives, the derived source document is invalidated for
                // reindexing (occurrences cascade only with it).
                conn.execute(
                    "DELETE FROM artist_link_documents WHERE id=?",
                    rusqlite::params![source_doc_id],
                )
                .with_context(|| format!("invalidate link document {source_doc_id}"))?;
            } else {
                // Repoint every document coordinate to the kept item so item,
                // artist, and path never disagree.
                let (keep_artist, keep_path): (i64, String) = conn
                    .query_row(
                        "SELECT artist_id, file_path FROM items WHERE id=?",
                        rusqlite::params![keep_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .with_context(|| format!("probe kept item {keep_id} coordinates"))?;
                conn.execute(
                    "UPDATE artist_link_documents SET item_id=?, artist_id=?, file_path=?
                     WHERE id=?",
                    rusqlite::params![keep_id, keep_artist, keep_path, source_doc_id],
                )
                .with_context(|| {
                    format!("repoint link document {source_doc_id} onto item {keep_id}")
                })?;
            }
        }
    }
    Ok(())
}

/// Remap `tag_ids_snapshot` JSON of the merged artist's active recycle entries
/// through the tag alias map before duplicate tag rows are deleted. A
/// malformed active snapshot is a migration error, not an empty default.
fn remap_recycle_tag_ids(
    conn: &Connection,
    tag_map: &HashMap<i64, i64>,
    artist_id: i64,
) -> Result<()> {
    if tag_map.is_empty()
        || !table_exists(conn, "recycle_entries")?
        || !column_exists(conn, "recycle_entries", "tag_ids_snapshot")?
    {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id, tag_ids_snapshot FROM recycle_entries
         WHERE artist_id=? AND status='recycled'",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![artist_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (entry_id, raw) in rows {
        let ids = serde_json::from_str::<Vec<i64>>(&raw)
            .with_context(|| format!("parse tag_ids_snapshot of recycle entry {entry_id}"))?;
        let mut mapped: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            let remapped = tag_map.get(&id).copied().unwrap_or(id);
            if !mapped.contains(&remapped) {
                mapped.push(remapped);
            }
        }
        let encoded = serde_json::to_string(&mapped)
            .with_context(|| format!("encode tag_ids_snapshot of recycle entry {entry_id}"))?;
        if encoded != raw {
            conn.execute(
                "UPDATE recycle_entries SET tag_ids_snapshot=? WHERE id=?",
                rusqlite::params![encoded, entry_id],
            )
            .with_context(|| format!("remap tag_ids_snapshot of recycle entry {entry_id}"))?;
        }
    }
    Ok(())
}

/// Repoint active recycle entries of a merged alias artist to the target: the
/// record `artist_id` and the strict `item_snapshot` JSON artist must agree
/// and move together, or restore would reject the entry. Historical restored
/// records stay unchanged.
fn remap_recycle_entries_artist(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if !table_exists(conn, "recycle_entries")? {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id, item_snapshot FROM recycle_entries
         WHERE artist_id=? AND status='recycled'",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![source_artist], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (entry_id, snapshot_raw) in rows {
        let mut snapshot: Value = serde_json::from_str(&snapshot_raw)
            .with_context(|| format!("parse item_snapshot of recycle entry {entry_id}"))?;
        if snapshot.get("artist_id").and_then(Value::as_i64) != Some(source_artist) {
            return Err(anyhow!(
                "recycle entry {entry_id} item_snapshot artist does not match its record"
            ));
        }
        snapshot["artist_id"] = json!(target_artist);
        let encoded = serde_json::to_string(&snapshot)
            .with_context(|| format!("encode item_snapshot of recycle entry {entry_id}"))?;
        conn.execute(
            "UPDATE recycle_entries SET artist_id=?, item_snapshot=? WHERE id=?",
            rusqlite::params![target_artist, encoded, entry_id],
        )
        .with_context(|| format!("repoint recycle entry {entry_id} to artist {target_artist}"))?;
    }
    Ok(())
}

/// Merge every artist-scoped relationship from a merged alias artist onto the
/// target before the alias row is deleted: union profile links, repoint
/// references/suggestions/documents deterministically, repoint scan_state.
fn merge_artist_relationships(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if table_exists(conn, "artist_profile_links")? {
        let mut stmt = conn.prepare("SELECT id FROM artist_profile_links WHERE artist_id=?")?;
        let rows = stmt
            .query_map(rusqlite::params![source_artist], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for link_id in rows {
            let duplicate: Option<i64> = conn
                .query_row(
                    "SELECT l2.id
                     FROM artist_profile_links l1
                     JOIN artist_profile_links l2
                       ON l2.artist_id=? AND l2.kind=l1.kind AND l2.url=l1.url
                     WHERE l1.id=?",
                    rusqlite::params![target_artist, link_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| {
                    format!("probe profile link duplicate for artist {target_artist}")
                })?;
            if let Some(_duplicate_id) = duplicate {
                conn.execute(
                    "DELETE FROM artist_profile_links WHERE id=?",
                    rusqlite::params![link_id],
                )
                .with_context(|| format!("drop duplicate profile link {link_id}"))?;
            } else {
                conn.execute(
                    "UPDATE artist_profile_links SET artist_id=? WHERE id=?",
                    rusqlite::params![target_artist, link_id],
                )
                .with_context(|| {
                    format!("repoint profile link {link_id} to artist {target_artist}")
                })?;
            }
        }
    }
    if table_exists(conn, "artist_references")? {
        repoint_artist_references(conn, None, Some((source_artist, target_artist)))?;
    }
    if table_exists(conn, "artist_suggestions")? {
        repoint_artist_suggestions(conn, None, Some((source_artist, target_artist)))?;
    }
    if table_exists(conn, "artist_link_documents")? {
        let mut stmt =
            conn.prepare("SELECT id, file_path FROM artist_link_documents WHERE artist_id=?")?;
        let rows = stmt
            .query_map(rusqlite::params![source_artist], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (doc_id, file_path) in rows {
            let duplicate: Option<i64> = conn
                .query_row(
                    "SELECT id FROM artist_link_documents
                     WHERE artist_id=? AND file_path=? AND id<>?",
                    rusqlite::params![target_artist, file_path, doc_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| {
                    format!("probe link document duplicate for artist {target_artist}")
                })?;
            if duplicate.is_some() {
                conn.execute(
                    "DELETE FROM artist_link_documents WHERE id=?",
                    rusqlite::params![doc_id],
                )
                .with_context(|| format!("invalidate superseded link document {doc_id}"))?;
            } else {
                conn.execute(
                    "UPDATE artist_link_documents SET artist_id=? WHERE id=?",
                    rusqlite::params![target_artist, doc_id],
                )
                .with_context(|| {
                    format!("repoint link document {doc_id} to artist {target_artist}")
                })?;
            }
        }
    }
    if table_exists(conn, "scan_state")? && column_exists(conn, "scan_state", "artist_id")? {
        conn.execute(
            "UPDATE scan_state SET artist_id=? WHERE artist_id=?",
            rusqlite::params![target_artist, source_artist],
        )
        .with_context(|| {
            format!("repoint scan_state from artist {source_artist} to {target_artist}")
        })?;
    }
    remap_recycle_entries_artist(conn, source_artist, target_artist)?;
    Ok(())
}

/// Rewrite legacy virtual media-root aliases in path columns to real authorized paths.
///
/// Signature-gated so the same root mapping runs only once. Conflicts reuse simple merge:
/// keep the target path row, reassign foreign keys from the source artist/item.
pub fn normalize_configured_media_paths(conn: &Connection, roots: &MediaRoots) -> Result<Value> {
    let signature = media_path_migration_signature(roots);
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
            [],
            |r| r.get(0),
        )
        .optional()
        .with_context(|| "read media_path_real_migration_signature marker")?;
    if existing.as_deref() == Some(signature.as_str()) {
        return Ok(json!({"updated": 0, "skipped": "already_applied"}));
    }
    if !has_virtual_paths(conn, roots)? {
        set_migration_signature(conn, &signature)?;
        return Ok(json!({"updated": 0, "skipped": "no_virtual_paths"}));
    }

    let pairs: Vec<(String, String)> = roots
        .roots
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
            let real = roots.real_root_at(i)?;
            let real_n = real.replace('\\', "/").trim_end_matches('/').to_string();
            if root_n.is_empty() || real_n.is_empty() || root_n == real_n {
                None
            } else {
                Some((root_n, real_n))
            }
        })
        .collect();
    if pairs.is_empty() {
        set_migration_signature(conn, &signature)?;
        return Ok(json!({"updated": 0, "skipped": "no_pairs"}));
    }

    let mut updated = 0i64;
    let mut merged_artists = 0i64;
    let mut merged_items = 0i64;
    let mut merged_link_documents = 0i64;

    // Batched migration: each phase commits separately so a big library does
    // not hold the single SQLite write lock for minutes at a time (other
    // writers' 30s busy timeouts would expire). The signature marker is only
    // written after every phase succeeds: a crash mid-way leaves the migration
    // to re-run at next startup, which converges because already-migrated rows
    // no longer match the virtual prefixes.
    fn migration_tx<T>(
        conn: &Connection,
        phase: &str,
        work: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        conn.execute_batch("BEGIN IMMEDIATE")
            .with_context(|| format!("begin media path migration: {phase}"))?;
        match work() {
            Ok(value) => {
                if let Err(error) = conn.execute_batch("COMMIT") {
                    // A failed COMMIT leaves the transaction open on this
                    // connection: roll it back here (the pool guards its
                    // connections at return as a second layer) instead of
                    // letting the dirty connection escape.
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(error)
                        .with_context(|| format!("commit media path migration: {phase}"));
                }
                Ok(value)
            }
            Err(error) => {
                let _ = conn.execute_batch("ROLLBACK");
                Err(error)
            }
        }
    }

    // Artists first: resolve unique path conflicts by merging source into target.
    migration_tx(conn, "artists", || {
        for (root_n, real_n) in &pairs {
            let rows = conn
                .prepare(
                    // Exact-prefix match, not LIKE: `_` is a wildcard and LIKE
                    // is case-insensitive for ASCII.
                    "SELECT id, path FROM artists
                     WHERE path=? OR substr(path, 1, length(?) + 1) = ? || '/'
                     ORDER BY id",
                )?
                .query_map(rusqlite::params![root_n, root_n, root_n], |r| {
                    Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            for (artist_id, old_path) in rows {
                let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
                if new_path == old_path {
                    continue;
                }
                let existing_id: Option<i64> = conn
                    .query_row(
                        "SELECT id FROM artists WHERE path=? AND id<>?",
                        rusqlite::params![&new_path, artist_id],
                        |r| r.get(0),
                    )
                    .optional()
                    .with_context(|| format!("probe artist path conflict for {new_path}"))?;
                if let Some(target_id) = existing_id {
                    // Merge tags onto the real-path artist. A source tag sharing a
                    // name with a target tag cannot be reassigned because of
                    // UNIQUE(artist_id, name): attach the source tag's item links
                    // to the target tag first, then delete the alias tag row only
                    // after every link is moved.
                    let mut tag_map: HashMap<i64, i64> = HashMap::new();
                    let source_tags = conn
                        .prepare("SELECT id, name FROM tags WHERE artist_id=? ORDER BY id")?
                        .query_map(rusqlite::params![artist_id], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (tag_id, tag_name) in source_tags {
                        let target_tag_id: Option<i64> = conn
                            .query_row(
                                "SELECT id FROM tags WHERE artist_id=? AND name=?",
                                rusqlite::params![target_id, tag_name],
                                |r| r.get(0),
                            )
                            .optional()
                            .with_context(|| {
                                format!("probe tag {tag_name} of artist {target_id}")
                            })?;
                        if let Some(target_tag_id) = target_tag_id {
                            conn.execute(
                                "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
                             SELECT item_id, ? FROM item_tags WHERE tag_id=?",
                                rusqlite::params![target_tag_id, tag_id],
                            )
                            .with_context(|| {
                                format!(
                                    "merge item_tags from tag {tag_id} onto tag {target_tag_id}"
                                )
                            })?;
                            conn.execute("DELETE FROM tags WHERE id=?", rusqlite::params![tag_id])
                                .with_context(|| format!("drop duplicate source tag {tag_id}"))?;
                            tag_map.insert(tag_id, target_tag_id);
                        } else {
                            conn.execute(
                                "UPDATE tags SET artist_id=? WHERE id=?",
                                rusqlite::params![target_id, tag_id],
                            )
                            .with_context(|| {
                                format!("reassign tag {tag_id} to artist {target_id}")
                            })?;
                            tag_map.insert(tag_id, tag_id);
                        }
                    }
                    remap_folder_plan_tag_ids(conn, &tag_map, &[artist_id])?;
                    remap_recycle_tag_ids(conn, &tag_map, artist_id)?;
                    // Items: reassign or merge on path conflict.
                    let items = conn
                        .prepare("SELECT id, file_path FROM items WHERE artist_id=?")?
                        .query_map(rusqlite::params![artist_id], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (item_id, file_path) in items {
                        let mapped = if file_path == *root_n
                            || file_path.starts_with(&format!("{root_n}/"))
                        {
                            format!("{real_n}{}", &file_path[root_n.len()..])
                        } else {
                            roots.normalize_db_path(&file_path)
                        };
                        let conflict: Option<i64> = conn
                            .query_row(
                                "SELECT id FROM items WHERE file_path=? AND id<>?",
                                rusqlite::params![&mapped, item_id],
                                |r| r.get(0),
                            )
                            .optional()
                            .with_context(|| format!("probe item path conflict for {mapped}"))?;
                        if let Some(keep_id) = conflict {
                            merge_item_into(conn, item_id, keep_id, Some(target_id))?;
                            conn.execute(
                                "DELETE FROM items WHERE id=?",
                                rusqlite::params![item_id],
                            )
                            .with_context(|| format!("drop merged item {item_id}"))?;
                            merged_items += 1;
                        } else {
                            conn.execute(
                                "UPDATE items SET artist_id=?, file_path=? WHERE id=?",
                                rusqlite::params![target_id, mapped, item_id],
                            )
                            .with_context(|| {
                                format!("reassign item {item_id} to artist {target_id}")
                            })?;
                            updated += 1;
                        }
                    }
                    for table in [
                        "scan_seen",
                        "scan_candidates",
                        "move_candidates",
                        "move_history",
                    ] {
                        if !table_exists(conn, table)? {
                            continue;
                        }
                        let table = sql_ident(table);
                        conn.execute(
                            &format!("UPDATE {table} SET artist_id=? WHERE artist_id=?"),
                            rusqlite::params![target_id, artist_id],
                        )
                        .with_context(|| format!("reassign {table} rows to artist {target_id}"))?;
                    }
                    merge_artist_folder_plans(conn, artist_id, target_id)?;
                    merge_artist_relationships(conn, artist_id, target_id)?;
                    if column_exists(conn, "artists", "missing")? {
                        let (source_missing, target_missing): (i64, i64) = conn
                            .query_row(
                                "SELECT
                               (SELECT missing FROM artists WHERE id=?),
                               (SELECT missing FROM artists WHERE id=?)",
                                rusqlite::params![artist_id, target_id],
                                |r| Ok((r.get(0)?, r.get(1)?)),
                            )
                            .with_context(|| {
                                format!("probe missing state of artists {artist_id}/{target_id}")
                            })?;
                        if source_missing == 0 || target_missing == 0 {
                            let sql = if column_exists(conn, "artists", "missing_at")? {
                                "UPDATE artists SET missing=0, missing_at=NULL WHERE id=?"
                            } else {
                                "UPDATE artists SET missing=0 WHERE id=?"
                            };
                            conn.execute(sql, rusqlite::params![target_id])
                                .with_context(|| {
                                    format!("keep merged artist {target_id} active")
                                })?;
                        }
                    }
                    conn.execute(
                        "DELETE FROM artists WHERE id=?",
                        rusqlite::params![artist_id],
                    )
                    .with_context(|| format!("drop merged alias artist {artist_id}"))?;
                    merged_artists += 1;
                } else {
                    conn.execute(
                        "UPDATE artists SET path=? WHERE id=?",
                        rusqlite::params![&new_path, artist_id],
                    )?;
                    updated += 1;
                }
            }
        }
        Ok(())
    })?;

    // Bulk-rewrite remaining path columns (non-conflicting rows first for UNIQUE columns).
    let path_columns = [
        ("items", "file_path", true),
        ("artist_link_documents", "file_path", true),
        ("scan_seen", "file_path", false),
        ("scan_candidates", "file_path", false),
        ("move_candidates", "old_path", false),
        ("move_candidates", "new_path", false),
        ("move_history", "old_path", false),
        ("move_history", "new_path", false),
    ];
    for (table, column, unique) in path_columns {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .with_context(|| format!("probe presence of table {table}"))?;
        if exists == 0 {
            continue;
        }
        let (table, column) = (sql_ident(table), sql_ident(column));
        migration_tx(conn, &format!("rewrite {table}.{column}"), || {
            for (root_n, real_n) in &pairs {
                // Exact-prefix match, not LIKE: `_` is a wildcard and LIKE is
                // case-insensitive for ASCII, so sibling roots could be
                // rewritten by mistake.
                let sql = if unique {
                    format!(
                        "UPDATE {table}
                         SET {column} = ? || substr({column}, length(?) + 1)
                         WHERE ({column}=? OR substr({column}, 1, length(?) + 1) = ? || '/')
                           AND {column} NOT LIKE '%/../%'
                           AND {column} NOT LIKE '%/..'
                           AND NOT EXISTS (
                             SELECT 1 FROM {table} AS existing
                             WHERE existing.{column} = ? || substr({table}.{column}, length(?) + 1)
                               AND existing.rowid <> {table}.rowid
                           )"
                    )
                } else {
                    format!(
                        "UPDATE {table}
                         SET {column} = ? || substr({column}, length(?) + 1)
                         WHERE ({column}=? OR substr({column}, 1, length(?) + 1) = ? || '/')
                           AND {column} NOT LIKE '%/../%'
                           AND {column} NOT LIKE '%/..'"
                    )
                };
                let n = if unique {
                    conn.execute(
                        &sql,
                        rusqlite::params![real_n, root_n, root_n, root_n, root_n, real_n, root_n],
                    )?
                } else {
                    conn.execute(
                        &sql,
                        rusqlite::params![real_n, root_n, root_n, root_n, root_n],
                    )?
                };
                updated += n as i64;
            }
            // Remaining unique conflicts on items: merge into the real path row.
            if unique && table == "items" {
                for (root_n, real_n) in &pairs {
                    let rows = conn
                        .prepare(&format!(
                            "SELECT id, file_path FROM {table}
                         WHERE {column}=? OR substr({column}, 1, length(?) + 1) = ? || '/'"
                        ))?
                        .query_map(rusqlite::params![root_n, root_n, root_n], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (item_id, old_path) in rows {
                        let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
                        let conflict: Option<i64> = conn
                            .query_row(
                                "SELECT id FROM items WHERE file_path=? AND id<>?",
                                rusqlite::params![&new_path, item_id],
                                |r| r.get(0),
                            )
                            .optional()
                            .with_context(|| format!("probe item path conflict for {new_path}"))?;
                        if let Some(keep_id) = conflict {
                            merge_item_into(conn, item_id, keep_id, None)?;
                            conn.execute(
                                "DELETE FROM items WHERE id=?",
                                rusqlite::params![item_id],
                            )
                            .with_context(|| format!("drop merged item {item_id}"))?;
                            merged_items += 1;
                        } else {
                            conn.execute(
                                "UPDATE items SET file_path=? WHERE id=?",
                                rusqlite::params![&new_path, item_id],
                            )?;
                            updated += 1;
                        }
                    }
                }
            }
            // Remaining unique conflicts on link documents: the real-path row is
            // the canonical parse of the same physical file, so drop the stale
            // virtual-path duplicate (occurrences cascade). Without this channel
            // the skipped row would keep a dead `/picturesN/...` path forever.
            if unique && table == "artist_link_documents" {
                for (root_n, real_n) in &pairs {
                    let rows = conn
                        .prepare(&format!(
                            "SELECT id, file_path FROM {table}
                         WHERE {column}=? OR substr({column}, 1, length(?) + 1) = ? || '/'"
                        ))?
                        .query_map(rusqlite::params![root_n, root_n, root_n], |r| {
                            Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                        })?
                        .collect::<rusqlite::Result<Vec<_>>>()?;
                    for (doc_id, old_path) in rows {
                        let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
                        let conflict: Option<i64> = conn
                            .query_row(
                                &format!(
                                    "SELECT id FROM {table}
                                 WHERE artist_id=(SELECT artist_id FROM {table} WHERE id=?)
                                   AND {column}=? AND id<>?"
                                ),
                                rusqlite::params![doc_id, &new_path, doc_id],
                                |r| r.get(0),
                            )
                            .optional()
                            .with_context(|| {
                                format!("probe link document conflict for {new_path}")
                            })?;
                        if conflict.is_some() {
                            conn.execute(
                                &format!("DELETE FROM {table} WHERE id=?"),
                                rusqlite::params![doc_id],
                            )
                            .with_context(|| {
                                format!("drop stale virtual-path link document {doc_id}")
                            })?;
                            merged_link_documents += 1;
                        } else {
                            updated += conn.execute(
                                &format!("UPDATE {table} SET {column}=? WHERE id=?"),
                                rusqlite::params![&new_path, doc_id],
                            )? as i64;
                        }
                    }
                }
            }
            Ok(())
        })?;
        log_info!("media path migration: {table}.{column} phase committed");
    }

    set_migration_signature(conn, &signature)?;
    if merged_link_documents > 0 {
        log_info!(
            "media path migration: dropped {merged_link_documents} stale virtual-path link document(s)"
        );
    }
    Ok(json!({
        "updated": updated,
        "merged_artists": merged_artists,
        "merged_items": merged_items,
        "merged_link_documents": merged_link_documents,
    }))
}
