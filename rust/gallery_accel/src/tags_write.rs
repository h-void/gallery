use std::collections::{BTreeMap, BTreeSet};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, Transaction, TransactionBehavior};
use serde_json::{json, Value};

/// Create a new tag. Returns the tag dict (`id`, `name`, `sort_order`) on
/// success. If the tag already exists (duplicate artist_id + name), returns
/// the existing row instead.
pub fn create_tag(conn: &Connection, artist_id: i64, name: &str) -> Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        return Err(anyhow!("tag name must not be empty"));
    }
    let max_order: i64 = conn
        .query_row(
            "SELECT COALESCE(MAX(sort_order), 0) FROM tags WHERE artist_id = ?1",
            rusqlite::params![artist_id],
            |row| row.get(0),
        )
        .unwrap_or(0);
    let new_order = max_order + 1;

    conn.execute(
        "INSERT OR IGNORE INTO tags (artist_id, name, sort_order) VALUES (?1, ?2, ?3)",
        rusqlite::params![artist_id, name, new_order],
    )
    .context("insert tag")?;

    // Fetch the inserted or existing row.
    let row = conn
        .query_row(
            "SELECT id, name, sort_order FROM tags WHERE artist_id = ?1 AND name = ?2",
            rusqlite::params![artist_id, name],
            |row| {
                let id: i64 = row.get(0)?;
                let n: String = row.get(1)?;
                let sort_order: i64 = row.get(2)?;
                Ok(json!({"id": id, "name": n, "sort_order": sort_order}))
            },
        )
        .context("fetch tag after insert")?;
    Ok(row)
}

/// Update a tag's name and/or sort_order.  Returns `None` when the tag does
/// not exist (caller falls back to Python).
pub fn update_tag(
    conn: &Connection,
    artist_id: i64,
    tag_id: i64,
    name: Option<&str>,
    sort_order: Option<i64>,
) -> Result<Option<Value>> {
    // Verify tag exists and belongs to artist.
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM tags WHERE id = ?1 AND artist_id = ?2",
            rusqlite::params![tag_id, artist_id],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !exists {
        return Ok(None);
    }

    let new_name = name.map(|s| s.trim());
    if let Some(n) = new_name {
        if n.is_empty() {
            return Err(anyhow!("tag name must not be empty"));
        }
        conn.execute(
            "UPDATE tags SET name = ?1 WHERE id = ?2",
            rusqlite::params![n, tag_id],
        )
        .context("update tag name")?;
    }
    if let Some(o) = sort_order {
        conn.execute(
            "UPDATE tags SET sort_order = ?1 WHERE id = ?2",
            rusqlite::params![o, tag_id],
        )
        .context("update tag sort_order")?;
    }

    // Read back the current name (may have been updated).
    let final_name: String = conn
        .query_row(
            "SELECT name FROM tags WHERE id = ?1",
            rusqlite::params![tag_id],
            |row| row.get(0),
        )
        .context("fetch tag name after update")?;
    Ok(Some(json!({"id": tag_id, "name": final_name})))
}

/// Delete a tag and its item–tag associations.  Returns the deleted tag data
/// (with `affected_item_count`), or `None` when the tag does not exist.
pub fn delete_tag(conn: &Connection, artist_id: i64, tag_id: i64) -> Result<Option<Value>> {
    let tag_row: Option<Value> = conn
        .query_row(
            "SELECT id, name, sort_order FROM tags WHERE id = ?1 AND artist_id = ?2",
            rusqlite::params![tag_id, artist_id],
            |row| {
                let id: i64 = row.get(0)?;
                let n: String = row.get(1)?;
                let s: i64 = row.get(2)?;
                Ok(json!({"id": id, "name": n, "sort_order": s}))
            },
        )
        .ok();
    let tag_row = match tag_row {
        Some(v) => v,
        None => return Ok(None),
    };

    let affected: i64 = conn
        .query_row(
            r#"
            SELECT COUNT(DISTINCT it.item_id)
            FROM item_tags it
            JOIN items i ON i.id = it.item_id
            WHERE it.tag_id = ?1
              AND i.missing = 0
              AND (i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive = 1)
            "#,
            rusqlite::params![tag_id],
            |row| row.get(0),
        )
        .unwrap_or(0);

    conn.execute(
        "DELETE FROM item_tags WHERE tag_id = ?1",
        rusqlite::params![tag_id],
    )
    .context("delete item_tags for tag")?;
    conn.execute("DELETE FROM tags WHERE id = ?1", rusqlite::params![tag_id])
        .context("delete tag")?;

    let mut result = tag_row;
    if let Some(obj) = result.as_object_mut() {
        obj.insert("affected_item_count".into(), json!(affected));
    }
    Ok(Some(result))
}

/// Apply tags by name across items that may belong to multiple artists.
/// Mirrors Python `update_item_tags_by_name_detailed`: groups by artist_id,
/// creates missing tags for set/add, resolves existing tags for remove, then
/// reuses `update_item_tags_response` per artist.
pub fn update_item_tags_by_name_response(
    conn: &Connection,
    item_ids: &[i64],
    tag_names: &[String],
    mode: &str,
) -> Result<Value> {
    if !matches!(mode, "set" | "add" | "remove") {
        return Err(anyhow!("Bad mode"));
    }

    let requested_items = sorted_unique(item_ids);
    if requested_items.is_empty() {
        return Ok(json!({
            "updated": 0,
            "artists": 0,
            "tags": 0,
            "propagated": 0,
            "changed_item_ids": [],
        }));
    }

    let names = normalize_tag_names(tag_names);
    let by_artist = items_grouped_by_artist(conn, &requested_items)?;
    let mut updated = 0i64;
    let mut propagated = 0i64;
    let mut changed_item_ids = BTreeSet::new();

    for (artist_id, ids) in by_artist.iter() {
        let tag_ids = if names.is_empty() {
            Vec::new()
        } else if matches!(mode, "set" | "add") {
            let mut ids_for_artist = Vec::with_capacity(names.len());
            for name in &names {
                let created = create_tag(conn, *artist_id, name)?;
                let tag_id = created
                    .get("id")
                    .and_then(|v| v.as_i64())
                    .ok_or_else(|| anyhow!("create_tag missing id"))?;
                ids_for_artist.push(tag_id);
            }
            ids_for_artist
        } else {
            tag_ids_for_names(conn, *artist_id, &names)?
        };

        let result = update_item_tags_response(conn, *artist_id, ids, &tag_ids, mode)?;
        updated += result.get("updated").and_then(|v| v.as_i64()).unwrap_or(0);
        propagated += result
            .get("propagated")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        if let Some(arr) = result.get("changed_item_ids").and_then(|v| v.as_array()) {
            for value in arr {
                if let Some(item_id) = value.as_i64() {
                    changed_item_ids.insert(item_id);
                }
            }
        }
    }

    Ok(json!({
        "updated": updated,
        "artists": by_artist.len(),
        "tags": names.len(),
        "propagated": propagated,
        "changed_item_ids": changed_item_ids.into_iter().collect::<Vec<_>>(),
    }))
}

pub fn update_item_tags_response(
    conn: &Connection,
    artist_id: i64,
    item_ids: &[i64],
    tag_ids: &[i64],
    mode: &str,
) -> Result<Value> {
    if !matches!(mode, "set" | "add" | "remove") {
        return Err(anyhow!("Bad mode"));
    }

    let requested_items = sorted_unique(item_ids);
    if requested_items.is_empty() {
        return Ok(json!({"updated": 0, "changed_item_ids": []}));
    }

    let ids = valid_item_ids(conn, artist_id, &requested_items)?;
    if ids.is_empty() {
        return Ok(json!({"updated": 0, "changed_item_ids": []}));
    }
    let valid_tags = valid_tag_ids(conn, artist_id, &sorted_unique(tag_ids))?;

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin item tag update")?;
    let before_by_item = item_tag_sets(&tx, &ids)?;

    if mode == "set" {
        let placeholders = placeholders(ids.len());
        tx.execute(
            &format!("DELETE FROM item_tags WHERE item_id IN ({placeholders})"),
            rusqlite::params_from_iter(ids.iter()),
        )
        .context("clear item tags")?;
    }

    if matches!(mode, "set" | "add") {
        let mut insert = tx
            .prepare("INSERT OR IGNORE INTO item_tags (item_id, tag_id) VALUES (?1, ?2)")
            .context("prepare item tag insert")?;
        for item_id in &ids {
            for tag_id in &valid_tags {
                insert
                    .execute(rusqlite::params![item_id, tag_id])
                    .context("insert item tag")?;
            }
        }
    } else if mode == "remove" && !valid_tags.is_empty() {
        let item_placeholders = placeholders(ids.len());
        let tag_placeholders = placeholders(valid_tags.len());
        let mut params = ids.clone();
        params.extend_from_slice(&valid_tags);
        tx.execute(
            &format!(
                "DELETE FROM item_tags WHERE item_id IN ({item_placeholders}) AND tag_id IN ({tag_placeholders})"
            ),
            rusqlite::params_from_iter(params.iter()),
        )
        .context("remove item tags")?;
    }

    let after_by_item = item_tag_sets(&tx, &ids)?;
    let mut changed_item_ids: BTreeSet<i64> = ids
        .iter()
        .copied()
        .filter(|item_id| before_by_item.get(item_id) != after_by_item.get(item_id))
        .collect();

    let mut propagated = 0usize;
    if matches!(mode, "set" | "add") && !valid_tags.is_empty() {
        let propagation = propagate_hash_tags_for_items(&tx, &ids)?;
        propagated = propagation.0;
        changed_item_ids.extend(propagation.1);
    }

    tx.commit().context("commit item tag update")?;
    Ok(json!({
        "updated": ids.len(),
        "propagated": propagated,
        "changed_item_ids": changed_item_ids.into_iter().collect::<Vec<_>>(),
    }))
}

pub fn propagate_hash_tags_response(conn: &Connection, item_ids: &[i64]) -> Result<Value> {
    let requested_items = sorted_unique(item_ids);
    if requested_items.is_empty() {
        return Ok(json!({"inserted": 0, "item_ids": []}));
    }
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin hash tag propagation")?;
    let (inserted, changed_item_ids) = propagate_hash_tags_for_items(&tx, &requested_items)?;
    tx.commit().context("commit hash tag propagation")?;
    Ok(json!({
        "inserted": inserted,
        "item_ids": changed_item_ids.into_iter().collect::<Vec<_>>(),
    }))
}

fn sorted_unique(values: &[i64]) -> Vec<i64> {
    values
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

fn normalize_tag_names(tag_names: &[String]) -> Vec<String> {
    let mut names = Vec::new();
    let mut seen = BTreeSet::new();
    for name in tag_names {
        let clean = name.trim();
        if clean.is_empty() {
            continue;
        }
        let key = clean.to_lowercase();
        if seen.insert(key) {
            names.push(clean.to_string());
        }
    }
    names
}

fn items_grouped_by_artist(
    conn: &Connection,
    item_ids: &[i64],
) -> Result<BTreeMap<i64, Vec<i64>>> {
    let mut by_artist: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
    if item_ids.is_empty() {
        return Ok(by_artist);
    }
    let item_placeholders = placeholders(item_ids.len());
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT id, artist_id FROM items
            WHERE missing=0
              AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)
              AND id IN ({item_placeholders})
            ORDER BY artist_id, id
            "#
        ))
        .context("prepare items grouped by artist")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(item_ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (item_id, artist_id) = row?;
        by_artist.entry(artist_id).or_default().push(item_id);
    }
    Ok(by_artist)
}

fn tag_ids_for_names(conn: &Connection, artist_id: i64, names: &[String]) -> Result<Vec<i64>> {
    if names.is_empty() {
        return Ok(Vec::new());
    }
    let name_placeholders = placeholders(names.len());
    let mut params: Vec<rusqlite::types::Value> = Vec::with_capacity(names.len() + 1);
    params.push(rusqlite::types::Value::Integer(artist_id));
    for name in names {
        params.push(rusqlite::types::Value::Text(name.clone()));
    }
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id FROM tags WHERE artist_id=? AND name IN ({name_placeholders}) ORDER BY id"
        ))
        .context("prepare tag ids for names")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;
    collect_i64(rows)
}

fn placeholders(len: usize) -> String {
    std::iter::repeat("?")
        .take(len)
        .collect::<Vec<_>>()
        .join(",")
}

fn valid_item_ids(conn: &Connection, artist_id: i64, item_ids: &[i64]) -> Result<Vec<i64>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let item_placeholders = placeholders(item_ids.len());
    let mut params = vec![artist_id];
    params.extend_from_slice(item_ids);
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT id FROM items
            WHERE artist_id=? AND missing=0
              AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)
              AND id IN ({item_placeholders})
            ORDER BY id
            "#
        ))
        .context("prepare valid item ids")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;
    collect_i64(rows)
}

fn valid_tag_ids(conn: &Connection, artist_id: i64, tag_ids: &[i64]) -> Result<Vec<i64>> {
    if tag_ids.is_empty() {
        return Ok(Vec::new());
    }
    let tag_placeholders = placeholders(tag_ids.len());
    let mut params = vec![artist_id];
    params.extend_from_slice(tag_ids);
    let mut stmt = conn
        .prepare(&format!(
            "SELECT id FROM tags WHERE artist_id=? AND id IN ({tag_placeholders}) ORDER BY id"
        ))
        .context("prepare valid tag ids")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| row.get(0))?;
    collect_i64(rows)
}

fn item_tag_sets(conn: &Connection, item_ids: &[i64]) -> Result<BTreeMap<i64, BTreeSet<i64>>> {
    let mut by_item: BTreeMap<i64, BTreeSet<i64>> = item_ids
        .iter()
        .copied()
        .map(|item_id| (item_id, BTreeSet::new()))
        .collect();
    if item_ids.is_empty() {
        return Ok(by_item);
    }
    let item_placeholders = placeholders(item_ids.len());
    let mut stmt = conn
        .prepare(&format!(
            "SELECT item_id, tag_id FROM item_tags WHERE item_id IN ({item_placeholders})"
        ))
        .context("prepare item tag sets")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(item_ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (item_id, tag_id) = row?;
        by_item.entry(item_id).or_default().insert(tag_id);
    }
    Ok(by_item)
}

fn propagate_hash_tags_for_items(
    conn: &Connection,
    item_ids: &[i64],
) -> Result<(usize, BTreeSet<i64>)> {
    let mut inserted = 0usize;
    let mut changed_item_ids = BTreeSet::new();
    for (artist_id, content_hash) in active_hash_groups_for_items(conn, item_ids)? {
        let group_item_ids = group_item_ids(conn, artist_id, &content_hash)?;
        let tag_ids = tag_ids_for_items(conn, &group_item_ids)?;
        let existing = existing_pairs(conn, &group_item_ids, &tag_ids)?;
        let mut insert = conn
            .prepare("INSERT OR IGNORE INTO item_tags (item_id, tag_id) VALUES (?1, ?2)")
            .context("prepare propagated item tag insert")?;
        for item_id in &group_item_ids {
            for tag_id in &tag_ids {
                if existing.contains(&(*item_id, *tag_id)) {
                    continue;
                }
                insert
                    .execute(rusqlite::params![item_id, tag_id])
                    .context("insert propagated item tag")?;
                inserted += 1;
                changed_item_ids.insert(*item_id);
            }
        }
    }
    Ok((inserted, changed_item_ids))
}

fn active_hash_groups_for_items(conn: &Connection, item_ids: &[i64]) -> Result<Vec<(i64, String)>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let item_placeholders = placeholders(item_ids.len());
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT DISTINCT artist_id, content_hash
            FROM items
            WHERE id IN ({item_placeholders})
              AND missing=0
              AND is_archive=0
              AND media_type IN ('image', 'video', 'source')
              AND hash_status='done'
              AND content_hash != ''
            "#
        ))
        .context("prepare active hash groups")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(item_ids.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut groups = Vec::new();
    for row in rows {
        groups.push(row?);
    }
    Ok(groups)
}

fn group_item_ids(conn: &Connection, artist_id: i64, content_hash: &str) -> Result<Vec<i64>> {
    let mut stmt = conn
        .prepare(
            r#"
            SELECT id
            FROM items
            WHERE artist_id=?1
              AND content_hash=?2
              AND hash_status='done'
              AND missing=0
              AND is_archive=0
              AND media_type IN ('image', 'video', 'source')
            ORDER BY id
            "#,
        )
        .context("prepare hash group item ids")?;
    let rows = stmt.query_map(rusqlite::params![artist_id, content_hash], |row| row.get(0))?;
    collect_i64(rows)
}

fn tag_ids_for_items(conn: &Connection, item_ids: &[i64]) -> Result<Vec<i64>> {
    if item_ids.is_empty() {
        return Ok(Vec::new());
    }
    let item_placeholders = placeholders(item_ids.len());
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT DISTINCT tag_id
            FROM item_tags
            WHERE item_id IN ({item_placeholders})
            ORDER BY tag_id
            "#
        ))
        .context("prepare hash group tag ids")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(item_ids.iter()), |row| {
        row.get(0)
    })?;
    collect_i64(rows)
}

fn existing_pairs(
    conn: &Connection,
    item_ids: &[i64],
    tag_ids: &[i64],
) -> Result<BTreeSet<(i64, i64)>> {
    if item_ids.is_empty() || tag_ids.is_empty() {
        return Ok(BTreeSet::new());
    }
    let item_placeholders = placeholders(item_ids.len());
    let tag_placeholders = placeholders(tag_ids.len());
    let mut params = item_ids.to_vec();
    params.extend_from_slice(tag_ids);
    let mut stmt = conn
        .prepare(&format!(
            r#"
            SELECT item_id, tag_id
            FROM item_tags
            WHERE item_id IN ({item_placeholders})
              AND tag_id IN ({tag_placeholders})
            "#
        ))
        .context("prepare existing item tag pairs")?;
    let rows = stmt.query_map(rusqlite::params_from_iter(params.iter()), |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut pairs = BTreeSet::new();
    for row in rows {
        pairs.insert(row?);
    }
    Ok(pairs)
}

fn collect_i64(
    rows: rusqlite::MappedRows<'_, impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<i64>>,
) -> Result<Vec<i64>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}
