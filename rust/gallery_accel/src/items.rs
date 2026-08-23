use std::collections::HashMap;

use anyhow::{anyhow, Result};
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::folder_tree::normalize_folder;
use crate::item_detail::ItemDetailRow;
use crate::item_detail_tags::ItemDetailTagRow;
use crate::media_roots::split_csv;
use crate::tags::compare_tag_order;
use crate::DEFAULT_LIMIT;

#[allow(clippy::too_many_arguments)]
pub fn items_page_response(
    conn: &Connection,
    artist_id: i64,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<&str>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    duplicates_only: Option<bool>,
    tag_names: Option<&str>,
    search: Option<&str>,
) -> Result<Value> {
    items_page_query_response(
        conn,
        Some(artist_id),
        limit,
        offset,
        sort,
        media_type,
        folder,
        date_from,
        date_to,
        image_only,
        untagged,
        tag_id,
        duplicates_only,
        tag_names,
        search,
        false,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_query_response(
    conn: &Connection,
    artist_id: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<&str>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    duplicates_only: Option<bool>,
    tag_names: Option<&str>,
    search: Option<&str>,
    search_tags_only: bool,
    favorite_only: Option<bool>,
) -> Result<Value> {
    items_page_query_response_inner(
        conn,
        artist_id,
        limit,
        offset,
        sort,
        media_type,
        folder,
        date_from,
        date_to,
        image_only,
        untagged,
        tag_id,
        duplicates_only,
        tag_names,
        search,
        search_tags_only,
        favorite_only,
        None,
        false,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_cursor_query_response(
    conn: &Connection,
    artist_id: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<&str>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    duplicates_only: Option<bool>,
    tag_names: Option<&str>,
    search: Option<&str>,
    search_tags_only: bool,
    favorite_only: Option<bool>,
    cursor: Option<&str>,
) -> Result<Value> {
    let parsed_cursor = cursor
        .map(|value| parse_item_cursor(value, sort))
        .transpose()?;
    items_page_query_response_inner(
        conn,
        artist_id,
        limit,
        offset,
        sort,
        media_type,
        folder,
        date_from,
        date_to,
        image_only,
        untagged,
        tag_id,
        duplicates_only,
        tag_names,
        search,
        search_tags_only,
        favorite_only,
        parsed_cursor.as_ref(),
        true,
    )
}

#[allow(clippy::too_many_arguments)]
fn items_page_query_response_inner(
    conn: &Connection,
    artist_id: Option<i64>,
    limit: Option<i64>,
    offset: Option<i64>,
    sort: Option<&str>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    duplicates_only: Option<bool>,
    tag_names: Option<&str>,
    search: Option<&str>,
    search_tags_only: bool,
    favorite_only: Option<bool>,
    cursor: Option<&ItemCursor>,
    cursor_mode: bool,
) -> Result<Value> {
    let page_limit = limit.unwrap_or(DEFAULT_LIMIT);
    let page_offset = if cursor_mode {
        0
    } else {
        offset.unwrap_or(0).max(0)
    };
    let (where_sql, params) = item_page_where(
        conn,
        artist_id,
        media_type,
        folder,
        date_from,
        date_to,
        image_only,
        untagged,
        tag_id,
        duplicates_only,
        tag_names,
        search,
        search_tags_only,
        favorite_only,
        cursor,
    )?;
    let total = conn.query_row(
        &format!("SELECT COUNT(*) FROM items i WHERE {where_sql}"),
        params_from_iter(params.iter()),
        |row| row.get::<_, i64>(0),
    )?;
    let mut page_params = params;
    page_params.push(SqlValue::Integer(if cursor_mode {
        page_limit + 1
    } else {
        page_limit
    }));
    page_params.push(SqlValue::Integer(page_offset));
    let mut stmt = conn.prepare(&format!(
        "SELECT i.id, i.artist_id, i.file_path, i.file_name, i.file_size, i.file_mtime,
                i.folder_name, i.date, i.detected_date, i.manual_date, i.auto_role,
                i.manual_role, i.is_archive, i.media_type,
                i.content_hash, i.hash_status, i.hash_updated_at, i.st_dev, i.st_ino, i.missing,
                i.missing_at, i.scanned_at,
                EXISTS(SELECT 1 FROM item_favorites f WHERE f.item_id=i.id) AS favorite,
                a.name AS artist_name, a.path AS artist_path
         FROM items i JOIN artists a ON a.id=i.artist_id
         WHERE {where_sql} ORDER BY {} LIMIT ? OFFSET ?",
        item_order_sql(sort, duplicates_only.unwrap_or(false) && !cursor_mode),
    ))?;
    let mut page_items = stmt
        .query_map(params_from_iter(page_params.iter()), item_detail_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    attach_page_tags(conn, &mut page_items)?;
    let has_more = cursor_mode && page_items.len() > page_limit as usize;
    if has_more {
        page_items.truncate(page_limit as usize);
    }
    let next_cursor = if has_more {
        page_items
            .last()
            .map(|item| serde_json::to_string(&item_cursor(item, sort)))
            .transpose()?
    } else {
        None
    };
    let mut body = json!({
        "items": page_items,
        "total": total,
        "offset": page_offset,
        "limit": page_limit,
    });
    if cursor_mode {
        body["has_more"] = json!(has_more);
        body["next_cursor"] = next_cursor.map(Value::String).unwrap_or(Value::Null);
    }
    Ok(body)
}

/// Base filter conditions for the items page, generated for one row alias.
///
/// Called twice when `duplicates_only` needs a correlated `items d` subquery,
/// so both aliases are produced by the same builder instead of relying on a
/// blind text replace that could corrupt future conditions.
#[allow(clippy::too_many_arguments)]
fn item_base_conditions(
    conn: &Connection,
    alias: &str,
    artist_id: Option<i64>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    tag_names: Option<&str>,
    search: Option<&str>,
    search_tags_only: bool,
    favorite_only: Option<bool>,
) -> Result<(Vec<String>, Vec<SqlValue>)> {
    let mut conditions = vec![format!("{alias}.missing=0")];
    let mut params = Vec::new();
    if let Some(artist_id) = artist_id {
        conditions.push(format!("{alias}.artist_id=?"));
        params.push(SqlValue::Integer(artist_id));
    }
    if favorite_only.unwrap_or(false) {
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM item_favorites f WHERE f.item_id={alias}.id)"
        ));
    }

    if tag_id.is_some() || tag_names.is_some() || untagged.unwrap_or(false) || search_tags_only {
        conditions.push(format!(
            "({alias}.media_type IN ('image', 'video', 'source', 'archive', 'text') OR {alias}.is_archive=1)"
        ));
    }
    if image_only.unwrap_or(false) && media_type.is_none() {
        conditions.push(format!(
            "{alias}.media_type IN ('image', 'video', 'source')"
        ));
    }
    let media_type = media_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    match media_type.as_deref() {
        Some("archive") => conditions.push(format!(
            "({alias}.media_type='archive' OR {alias}.is_archive=1)"
        )),
        Some("image" | "video" | "source" | "text") => {
            conditions.push(format!("{alias}.media_type=?"));
            conditions.push(format!("{alias}.is_archive=0"));
            params.push(SqlValue::Text(media_type.unwrap()));
        }
        Some(_) => conditions.push("1=0".to_string()),
        None => conditions.push(format!(
            "({alias}.media_type IN ('image', 'video', 'source', 'archive', 'text') OR {alias}.is_archive=1)"
        )),
    }

    if let Some(tag_id) = tag_id {
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM item_tags it JOIN tags t ON t.id=it.tag_id \
             WHERE it.item_id={alias}.id AND it.tag_id=? AND t.artist_id={alias}.artist_id)"
        ));
        params.push(SqlValue::Integer(tag_id));
    }
    for name in tag_names
        .into_iter()
        .flat_map(|names| split_csv(names, false))
    {
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM item_tags it JOIN tags t ON t.id=it.tag_id \
             WHERE it.item_id={alias}.id AND t.artist_id={alias}.artist_id AND t.name=?)"
        ));
        params.push(SqlValue::Text(name));
    }
    if untagged.unwrap_or(false) {
        conditions.push(format!(
            "NOT EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id={alias}.id)"
        ));
    }

    let folder = normalize_folder(folder.unwrap_or(""));
    if !folder.is_empty() {
        let artist_path = artist_id
            .map(|id| {
                conn.query_row("SELECT path FROM artists WHERE id=?", [id], |row| {
                    row.get::<_, String>(0)
                })
                .optional()
            })
            .transpose()?;
        if let Some(artist_path) = artist_path.flatten() {
            let prefix = format!(
                "{}/{}/",
                artist_path.replace('\\', "/").trim_end_matches('/'),
                folder
            );
            conditions.push(format!(
                r#"substr(replace({alias}.file_path, '\', '/'), 1, ?) = ?"#
            ));
            params.push(SqlValue::Integer(prefix.chars().count() as i64));
            params.push(SqlValue::Text(prefix));
        } else {
            conditions.push("1=0".to_string());
        }
    }

    if let Some(query) = search {
        let query = query.trim();
        if query.is_empty() {
            conditions.push("1=0".to_string());
        } else {
            let like = format!("%{}%", crate::product_ui::escape_like(query));
            let tag_ids = matching_tag_ids(conn, query, artist_id)?;
            let tag_clause = if tag_ids.is_empty() {
                "st.name LIKE ? ESCAPE '\\'".to_string()
            } else {
                format!(
                    "st.name LIKE ? ESCAPE '\\' OR st.id IN ({})",
                    std::iter::repeat_n("?", tag_ids.len())
                        .collect::<Vec<_>>()
                        .join(",")
                )
            };
            let tag_search = format!(
                "EXISTS (SELECT 1 FROM item_tags sit JOIN tags st ON st.id=sit.tag_id \
                 WHERE sit.item_id={alias}.id AND st.artist_id={alias}.artist_id AND ({tag_clause}))"
            );
            if search_tags_only {
                conditions.push(tag_search);
            } else {
                conditions.push(format!(
                    "({alias}.file_name LIKE ? ESCAPE '\\' OR {alias}.folder_name LIKE ? ESCAPE '\\' \
                     OR {alias}.file_path LIKE ? ESCAPE '\\' OR {tag_search})"
                ));
                params.extend((0..3).map(|_| SqlValue::Text(like.clone())));
            }
            params.push(SqlValue::Text(like));
            params.extend(tag_ids.into_iter().map(SqlValue::Integer));
        }
    }

    if let Some(date_from) = date_from.map(str::trim).filter(|value| !value.is_empty()) {
        conditions.push(format!("{alias}.date >= ?"));
        params.push(SqlValue::Text(date_from.to_string()));
    }
    if let Some(date_to) = date_to.map(str::trim).filter(|value| !value.is_empty()) {
        conditions.push(format!("{alias}.date <= ?"));
        params.push(SqlValue::Text(date_to.to_string()));
    }
    Ok((conditions, params))
}

#[allow(clippy::too_many_arguments)]
fn item_page_where(
    conn: &Connection,
    artist_id: Option<i64>,
    media_type: Option<&str>,
    folder: Option<&str>,
    date_from: Option<&str>,
    date_to: Option<&str>,
    image_only: Option<bool>,
    untagged: Option<bool>,
    tag_id: Option<i64>,
    duplicates_only: Option<bool>,
    tag_names: Option<&str>,
    search: Option<&str>,
    search_tags_only: bool,
    favorite_only: Option<bool>,
    cursor: Option<&ItemCursor>,
) -> Result<(String, Vec<SqlValue>)> {
    let (mut conditions, mut params) = item_base_conditions(
        conn,
        "i",
        artist_id,
        media_type,
        folder,
        date_from,
        date_to,
        image_only,
        untagged,
        tag_id,
        tag_names,
        search,
        search_tags_only,
        favorite_only,
    )?;
    if duplicates_only.unwrap_or(false) {
        // Regenerate the same filters under the twin alias instead of a blind
        // textual replace, then require a second row with identical content
        // hash satisfying them all.
        let (mut duplicate_conditions, duplicate_params) = item_base_conditions(
            conn,
            "d",
            artist_id,
            media_type,
            folder,
            date_from,
            date_to,
            image_only,
            untagged,
            tag_id,
            tag_names,
            search,
            search_tags_only,
            favorite_only,
        )?;
        conditions.push("i.media_type IN ('image', 'video', 'source')".to_string());
        conditions.push("i.is_archive=0".to_string());
        conditions.push("i.hash_status='done'".to_string());
        conditions.push("i.content_hash != ''".to_string());
        duplicate_conditions.insert(0, "d.hash_status='done'".to_string());
        duplicate_conditions.insert(1, "d.content_hash != ''".to_string());
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM items d WHERE d.artist_id=i.artist_id AND d.id != i.id \
             AND d.content_hash=i.content_hash AND {})",
            duplicate_conditions.join(" AND ")
        ));
        params.extend(duplicate_params);
    }
    if let Some(cursor) = cursor {
        let condition = match cursor.sort.as_str() {
            "date_asc" => "(i.date > ? OR (i.date = ? AND (i.file_name COLLATE NATURAL_NOCASE > ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id > ?))))",
            "name" => "(i.file_name COLLATE NATURAL_NOCASE > ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id > ?))",
            "size" => "(i.file_size < ? OR (i.file_size = ? AND i.id < ?))",
            "scanned_desc" => "(i.scanned_at < ? OR (i.scanned_at = ? AND i.id < ?))",
            _ => "(i.date < ? OR (i.date = ? AND (i.file_name COLLATE NATURAL_NOCASE > ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id > ?))))",
        };
        conditions.push(condition.to_string());
        match cursor.sort.as_str() {
            "date_asc" | "date_desc" => {
                params.push(SqlValue::Text(cursor.date.clone()));
                params.push(SqlValue::Text(cursor.date.clone()));
                params.push(SqlValue::Text(cursor.file_name.clone()));
                params.push(SqlValue::Text(cursor.file_name.clone()));
                params.push(SqlValue::Integer(cursor.id));
            }
            "name" => {
                params.push(SqlValue::Text(cursor.file_name.clone()));
                params.push(SqlValue::Text(cursor.file_name.clone()));
                params.push(SqlValue::Integer(cursor.id));
            }
            "size" => {
                params.push(SqlValue::Integer(cursor.file_size));
                params.push(SqlValue::Integer(cursor.file_size));
                params.push(SqlValue::Integer(cursor.id));
            }
            "scanned_desc" => {
                params.push(SqlValue::Integer(cursor.scanned_at));
                params.push(SqlValue::Integer(cursor.scanned_at));
                params.push(SqlValue::Integer(cursor.id));
            }
            _ => unreachable!(),
        }
    }
    Ok((conditions.join(" AND "), params))
}

fn matching_tag_ids(conn: &Connection, query: &str, artist_id: Option<i64>) -> Result<Vec<i64>> {
    let (sql, params) = match artist_id {
        Some(artist_id) => (
            "SELECT id, name FROM tags WHERE artist_id=?",
            vec![SqlValue::Integer(artist_id)],
        ),
        None => ("SELECT id, name FROM tags", Vec::new()),
    };
    let mut stmt = conn.prepare(sql)?;
    let rows = stmt
        .query_map(params_from_iter(params.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter_map(|(id, name)| {
            crate::pinyin_search::text_matches_search(query, &[&name]).then_some(id)
        })
        .collect())
}

fn item_order_sql(sort: Option<&str>, group_duplicates: bool) -> &'static str {
    if group_duplicates {
        return match sort.unwrap_or("date_desc") {
            "date_asc" => {
                "i.content_hash ASC, i.date ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC"
            }
            "name" => "i.content_hash ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
            "size" => "i.content_hash ASC, i.file_size DESC, i.id DESC",
            "scanned_desc" => "i.content_hash ASC, i.scanned_at DESC, i.id DESC",
            _ => {
                "i.content_hash ASC, i.date DESC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC"
            }
        };
    }
    match sort.unwrap_or("date_desc") {
        "date_asc" => "i.date ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
        "name" => "i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
        "size" => "i.file_size DESC, i.id DESC",
        "scanned_desc" => "i.scanned_at DESC, i.id DESC",
        _ => "i.date DESC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
    }
}

#[derive(Debug, Deserialize, Serialize)]
struct ItemCursor {
    sort: String,
    date: String,
    file_name: String,
    file_size: i64,
    scanned_at: i64,
    id: i64,
}

fn item_cursor(item: &ItemDetailRow, sort: Option<&str>) -> ItemCursor {
    ItemCursor {
        sort: normalized_sort(sort).to_string(),
        date: item.date.clone(),
        file_name: item.file_name.clone(),
        file_size: item.file_size,
        scanned_at: item.scanned_at,
        id: item.id,
    }
}

fn normalized_sort(sort: Option<&str>) -> &'static str {
    match sort.unwrap_or("date_desc") {
        "date_asc" => "date_asc",
        "name" => "name",
        "size" => "size",
        "scanned_desc" => "scanned_desc",
        _ => "date_desc",
    }
}

fn parse_item_cursor(value: &str, sort: Option<&str>) -> Result<ItemCursor> {
    let cursor: ItemCursor =
        serde_json::from_str(value).map_err(|error| anyhow!("invalid cursor: {error}"))?;
    if cursor.sort != normalized_sort(sort) || cursor.id <= 0 {
        return Err(anyhow!("invalid cursor for requested sort"));
    }
    Ok(cursor)
}

fn item_detail_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemDetailRow> {
    let detected_date: String = row.get("detected_date")?;
    let manual_date: Option<String> = row.get("manual_date")?;
    let legacy_date: String = row.get("date")?;
    Ok(ItemDetailRow {
        id: row.get("id")?,
        artist_id: row.get("artist_id")?,
        file_path: row.get("file_path")?,
        file_name: row.get("file_name")?,
        file_size: row.get("file_size")?,
        file_mtime: row.get("file_mtime")?,
        folder_name: row.get("folder_name")?,
        date: legacy_date.clone(),
        detected_date: detected_date.clone(),
        manual_date: manual_date.clone(),
        display_date: crate::item_detail::effective_display_date(
            &detected_date,
            manual_date.as_deref(),
            &legacy_date,
        ),
        auto_role: row.get("auto_role")?,
        manual_role: row.get("manual_role")?,
        tags: Vec::new(),
        is_archive: row.get("is_archive")?,
        media_type: row.get("media_type")?,
        content_hash: row.get("content_hash")?,
        hash_status: row.get("hash_status")?,
        hash_updated_at: row.get("hash_updated_at")?,
        st_dev: row.get("st_dev")?,
        st_ino: row.get("st_ino")?,
        missing: row.get("missing")?,
        missing_at: row.get("missing_at")?,
        scanned_at: row.get("scanned_at")?,
        favorite: row.get("favorite")?,
        artist_name: row.get("artist_name")?,
        artist_path: row.get("artist_path")?,
    })
}

pub fn set_item_favorite_response(
    conn: &Connection,
    item_id: i64,
    favorite: bool,
) -> Result<Value> {
    let exists = conn
        .query_row("SELECT 1 FROM items WHERE id=?", [item_id], |_| Ok(()))
        .optional()?
        .is_some();
    if !exists {
        return Ok(Value::Null);
    }
    if favorite {
        conn.execute(
            "INSERT OR IGNORE INTO item_favorites (item_id) VALUES (?)",
            [item_id],
        )?;
    } else {
        conn.execute("DELETE FROM item_favorites WHERE item_id=?", [item_id])?;
    }
    Ok(json!({"item_id": item_id, "favorite": favorite}))
}

fn attach_page_tags(conn: &Connection, items: &mut [ItemDetailRow]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let ids = items.iter().map(|item| item.id).collect::<Vec<_>>();
    let placeholders = std::iter::repeat_n("?", ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT it.item_id, t.id, t.name, t.sort_order FROM item_tags it JOIN tags t ON t.id=it.tag_id \
         WHERE it.item_id IN ({placeholders})"
    ))?;
    let mut by_item = HashMap::<i64, Vec<ItemDetailTagRow>>::new();
    let mut rows = stmt.query(params_from_iter(ids.iter()))?;
    while let Some(row) = rows.next()? {
        by_item
            .entry(row.get(0)?)
            .or_default()
            .push(ItemDetailTagRow {
                id: row.get(1)?,
                name: row.get(2)?,
                sort_order: row.get(3)?,
            });
    }
    for tags in by_item.values_mut() {
        tags.sort_by(|left, right| {
            compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name)
        });
    }
    for item in items {
        item.tags = by_item.remove(&item.id).unwrap_or_default();
    }
    Ok(())
}

/// Mirror `app/api/items.py list_items` search semantics:
/// - file_name / folder_name / file_path match by raw case-insensitive substring
///   (Python `LIKE '%q%'`), NOT pinyin.
/// - tag names match by pinyin-aware `text_matches_search` (Python `_matching_tag_ids`).
#[cfg(test)]
pub(crate) fn item_matches_search(item: &ItemDetailRow, q: &str) -> bool {
    let q = q.trim();
    if q.is_empty() {
        return false;
    }
    let ql = q.to_lowercase();
    let raw_hit = item.file_name.to_lowercase().contains(&ql)
        || item.folder_name.to_lowercase().contains(&ql)
        || item.file_path.to_lowercase().contains(&ql);
    let tag_hit = item
        .tags
        .iter()
        .any(|t| crate::pinyin_search::text_matches_search(q, &[t.name.as_str()]));
    raw_hit || tag_hit
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_item(file_name: &str, tag_names: &[&str]) -> ItemDetailRow {
        ItemDetailRow {
            id: 1,
            artist_id: 1,
            file_path: format!("/pictures/{}", file_name),
            file_name: file_name.to_string(),
            file_size: 0,
            file_mtime: 0.0,
            folder_name: "folder".to_string(),
            date: "2020-01-01".to_string(),
            detected_date: "2020-01-01".to_string(),
            manual_date: None,
            display_date: "2020-01-01".to_string(),
            auto_role: String::new(),
            manual_role: None,
            tags: tag_names
                .iter()
                .map(|n| ItemDetailTagRow {
                    id: 0,
                    name: n.to_string(),
                    sort_order: 0,
                })
                .collect(),
            is_archive: 0,
            media_type: "image".to_string(),
            content_hash: String::new(),
            hash_status: String::new(),
            hash_updated_at: None,
            st_dev: None,
            st_ino: None,
            missing: 0,
            missing_at: None,
            scanned_at: 0,
            favorite: false,
            artist_name: String::new(),
            artist_path: String::new(),
        }
    }

    #[test]
    fn matches_raw_substring_in_filename() {
        let item = sample_item("beach_day.jpg", &[]);
        assert!(item_matches_search(&item, "beach"));
        assert!(item_matches_search(&item, "BEACH")); // case-insensitive
        assert!(!item_matches_search(&item, "xyz"));
    }

    #[test]
    fn matches_tag_by_pinyin_but_not_filename_pinyin() {
        // Tag name pinyin matches; a Chinese filename does NOT get pinyin matching
        // (mirrors Python raw-LIKE-on-file-fields semantics).
        let tagged = sample_item("abc.jpg", &["泳装"]);
        assert!(item_matches_search(&tagged, "yong")); // pinyin of 泳
        assert!(item_matches_search(&tagged, "泳装")); // raw tag name
        let cn = sample_item("泳装.jpg", &[]);
        assert!(!item_matches_search(&cn, "yong")); // filename not pinyin-matched
        assert!(item_matches_search(&cn, "泳装")); // raw filename substring
    }

    #[test]
    fn empty_query_matches_nothing() {
        let item = sample_item("x.jpg", &["tag"]);
        assert!(!item_matches_search(&item, "   "));
        assert!(!item_matches_search(&item, ""));
    }

    fn visibility_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.create_collation("NATURAL_NOCASE", crate::natural_sort::natural_compare)
            .unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE items (
                id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
                file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
                date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
                auto_role TEXT DEFAULT '', manual_role TEXT,
                is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
                content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending',
                hash_updated_at REAL, st_dev INTEGER, st_ino INTEGER, missing INTEGER DEFAULT 0,
                missing_at REAL, scanned_at INTEGER DEFAULT 0
             );
             CREATE TABLE item_favorites (item_id INTEGER PRIMARY KEY);
             CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER);
             CREATE TABLE tags (id INTEGER PRIMARY KEY, artist_id INTEGER, name TEXT, sort_order INTEGER DEFAULT 0);
             INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/artist');
             INSERT INTO items (id, artist_id, file_path, file_name, date, scanned_at)
                VALUES (1, 1, '/artist/a.jpg', 'a.jpg', '2020-01-01', 100);
             INSERT INTO items (id, artist_id, file_path, file_name, date, scanned_at)
                VALUES (2, 1, '/artist/b.jpg', 'b.jpg', '', 200);",
        )
        .unwrap();
        conn
    }

    fn page_file_names(body: &Value) -> Vec<String> {
        body["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["file_name"].as_str().unwrap().to_string())
            .collect()
    }

    #[test]
    fn newly_scanned_empty_date_item_visibility_per_sort() {
        let conn = visibility_fixture();
        let page = |sort, date_from: Option<&str>, date_to: Option<&str>| {
            items_page_query_response(
                &conn,
                Some(1),
                Some(10),
                Some(0),
                sort,
                None,
                None,
                date_from,
                date_to,
                Some(false),
                Some(false),
                None,
                Some(false),
                None,
                None,
                false,
                Some(false),
            )
            .unwrap()
        };

        let date_desc = page(Some("date_desc"), None, None);
        assert_eq!(page_file_names(&date_desc), ["a.jpg", "b.jpg"]);
        assert_eq!(date_desc["total"], 2);

        let scanned_desc = page(Some("scanned_desc"), None, None);
        assert_eq!(page_file_names(&scanned_desc), ["b.jpg", "a.jpg"]);
        assert_eq!(scanned_desc["total"], 2);

        let name_asc = page(Some("name"), None, None);
        assert_eq!(page_file_names(&name_asc), ["a.jpg", "b.jpg"]);

        let ranged = page(Some("date_desc"), Some("2020-01-01"), Some("2020-12-31"));
        assert_eq!(page_file_names(&ranged), ["a.jpg"]);
        assert_eq!(ranged["total"], 1);
    }
}
