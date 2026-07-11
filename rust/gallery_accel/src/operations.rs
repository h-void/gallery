use std::cmp::Ordering;
use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::operation_folder_renames::operation_folder_rename_history;
use crate::operation_helpers::{
    history_entry_at, history_entry_id, operation_details_empty_folders,
};
use crate::path_display::display_path;

pub fn operation_history_response(
    conn: &Connection,
    roots: &MediaRoots,
    limit: Option<i64>,
) -> Result<Value> {
    let limit = normalize_operation_log_limit(limit);
    let artist_names = operation_artist_names(conn)?;
    let mut history = operation_move_history(conn, roots, limit, &artist_names)?;
    history.extend(operation_folder_rename_history(
        conn,
        roots,
        limit,
        &artist_names,
    )?);
    history.sort_by(|left, right| {
        history_entry_at(right)
            .partial_cmp(&history_entry_at(left))
            .unwrap_or(Ordering::Equal)
            .then_with(|| history_entry_id(right).cmp(&history_entry_id(left)))
    });
    let total = history.len() as i64;
    history.truncate(limit as usize);
    Ok(json!({
        "history": history,
        "total": total,
        "limit": limit,
    }))
}

fn normalize_operation_log_limit(limit: Option<i64>) -> i64 {
    match limit {
        Some(value) if value > 0 => value.min(300),
        _ => 80,
    }
}

fn operation_artist_names(conn: &Connection) -> Result<HashMap<i64, String>> {
    let mut stmt = conn.prepare("SELECT id, name FROM artists")?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut names = HashMap::new();
    for row in rows {
        let (id, name) = row?;
        names.insert(id, name);
    }
    Ok(names)
}

fn operation_move_history(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
    artist_names: &HashMap<i64, String>,
) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, item_id, artist_id, old_path, new_path, reason, status,
               details, created_at, applied_at, reverted_at
        FROM move_history
        ORDER BY COALESCE(applied_at, created_at) DESC, id DESC
        LIMIT ?
        ",
    )?;
    let rows = stmt.query_map([limit], |row| {
        Ok((
            row.get::<_, i64>("id")?,
            row.get::<_, i64>("item_id")?,
            row.get::<_, i64>("artist_id")?,
            row.get::<_, String>("old_path")?,
            row.get::<_, String>("new_path")?,
            row.get::<_, String>("reason")?,
            row.get::<_, String>("status")?,
            row.get::<_, Option<String>>("details")?.unwrap_or_default(),
            row.get::<_, f64>("created_at")?,
            row.get::<_, Option<f64>>("applied_at")?,
            row.get::<_, Option<f64>>("reverted_at")?,
        ))
    })?;
    let mut history = Vec::new();
    for row in rows {
        let (
            id,
            item_id,
            artist_id,
            old_path,
            new_path,
            reason,
            status,
            details,
            created_at,
            applied_at,
            _reverted_at,
        ) = row?;
        let display_source = if old_path.is_empty() {
            String::new()
        } else {
            display_path(&old_path, roots)
        };
        let display_target = if new_path.is_empty() {
            String::new()
        } else {
            display_path(&new_path, roots)
        };
        let updated_items = if status == "applied" { 1 } else { 0 };
        history.push(json!({
            "id": format!("move:{id}"),
            "kind": "move",
            "status": status,
            "at": applied_at.unwrap_or(created_at),
            "artist_id": artist_id,
            "artist_name": artist_names.get(&artist_id).cloned().unwrap_or_default(),
            "source": old_path,
            "target": new_path,
            "display_source": display_source,
            "display_target": display_target,
            "reason": reason,
            "item_id": item_id,
            "updated_items": updated_items,
            "empty_folders": operation_details_empty_folders(&details),
        }));
    }
    Ok(history)
}
