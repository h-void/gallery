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
    items_page_query_response_ext(
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
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_query_response_ext(
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
    inbox: Option<bool>,
) -> Result<Value> {
    items_page_query_response_with_undated(
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
        inbox,
        None,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_query_response_with_undated(
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
    inbox: Option<bool>,
    undated: Option<bool>,
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
        inbox,
        undated,
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
    items_page_cursor_query_response_ext(
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
        cursor,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_cursor_query_response_ext(
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
    inbox: Option<bool>,
    cursor: Option<&str>,
) -> Result<Value> {
    items_page_cursor_query_response_with_undated(
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
        inbox,
        None,
        cursor,
    )
}

#[allow(clippy::too_many_arguments)]
pub fn items_page_cursor_query_response_with_undated(
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
    inbox: Option<bool>,
    undated: Option<bool>,
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
        inbox,
        undated,
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
    inbox: Option<bool>,
    undated: Option<bool>,
    cursor: Option<&ItemCursor>,
    cursor_mode: bool,
) -> Result<Value> {
    // Clamped here as well as at the route: `LIMIT` is a raw i64 in SQL, and a
    // negative one means "no limit" — an unclamped caller could pull the whole
    // table in one response (and `page_limit + 1` would overflow at i64::MAX).
    let page_limit = limit
        .unwrap_or(DEFAULT_LIMIT)
        .clamp(1, crate::MAX_PAGINATION_LIMIT);
    let page_offset = if cursor_mode {
        0
    } else {
        offset.unwrap_or(0).max(0)
    };
    // The count is the size of the filtered set, not of the page's remainder.
    // Build the cursor-free filter once for `total`, then append the cursor
    // predicate only when `cursor` is present so `item_page_where` (including
    // artist path lookup and Pinyin tag matching) never runs twice per request.
    let (mut where_sql, mut page_params) = item_page_where(
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
        inbox,
        undated,
        None,
    )?;
    let total = conn.query_row(
        &format!("SELECT COUNT(*) FROM items i WHERE {where_sql}"),
        params_from_iter(page_params.iter()),
        |row| row.get::<_, i64>(0),
    )?;
    if let Some(cursor) = cursor {
        let mut cursor_conditions = Vec::with_capacity(1);
        append_cursor_condition(&mut cursor_conditions, &mut page_params, cursor);
        if let Some(cond) = cursor_conditions.pop() {
            where_sql.push_str(" AND ");
            where_sql.push_str(&cond);
        }
    }
    page_params.push(SqlValue::Integer(if cursor_mode {
        page_limit.saturating_add(1)
    } else {
        page_limit
    }));
    page_params.push(SqlValue::Integer(page_offset));
    let sql = item_page_sql(
        &where_sql,
        item_order_sql(sort, duplicates_only.unwrap_or(false) && !cursor_mode),
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut page_items = stmt
        .query_map(params_from_iter(page_params.iter()), item_detail_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    attach_page_tags(conn, &mut page_items)?;
    let has_more = cursor_mode && page_items.len() > page_limit as usize;
    if has_more {
        page_items.truncate(page_limit as usize);
    }
    fill_missing_page_image_dimensions(conn, &mut page_items);
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

/// Probe intrinsic dimensions for image rows on the current page whose database
/// dimensions are still unset, updating both the response items and SQLite so
/// justified rows (especially hover-only GIF cards) lay out at their real
/// aspect ratio on initial render without waiting for a hover preview.
fn fill_missing_page_image_dimensions(conn: &Connection, items: &mut [ItemDetailRow]) {
    let mut lazy_roots: Option<crate::media_roots::MediaRoots> = None;
    let mut updates = Vec::new();
    for item in items.iter_mut() {
        if item.missing != 0 || item.media_type != "image" || (item.width > 0 && item.height > 0) {
            continue;
        }
        let direct = std::path::Path::new(&item.file_path);
        let mapped;
        let target = if direct.is_file() {
            Some(direct)
        } else {
            let roots = lazy_roots.get_or_insert_with(crate::media_roots::env_media_roots);
            mapped = roots.map_to_real(&item.file_path).ok();
            mapped.as_deref().filter(|path| path.is_file())
        };
        let Some(path) = target else {
            continue;
        };
        if let Ok((width, height)) = crate::dimensions::media_dimensions(path, "image") {
            if width > 0 && height > 0 {
                item.width = i64::from(width);
                item.height = i64::from(height);
                updates.push((item.id, item.width, item.height));
            }
        }
    }
    if updates.is_empty() {
        return;
    }
    let query_only = conn
        .query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        != 0;
    if query_only {
        return;
    }
    if let Ok(tx) = conn.unchecked_transaction() {
        if let Ok(mut stmt) = tx.prepare_cached(
            "UPDATE items
             SET width=?, height=?
             WHERE id=? AND missing=0 AND (width <= 0 OR height <= 0)",
        ) {
            for (id, width, height) in updates {
                if stmt.execute(rusqlite::params![width, height, id]).is_err() {
                    return;
                }
            }
        }
        let _ = tx.commit();
    }
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
    inbox: Option<bool>,
    undated: Option<bool>,
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

    if let Some(inbox_val) = inbox {
        if inbox_val {
            conditions.push(format!(
                "(NOT EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id={alias}.id)                  OR (COALESCE({alias}.manual_date, '') = '' AND COALESCE({alias}.detected_date, '') = '' AND (COALESCE({alias}.date, '') = '' OR {alias}.date LIKE '0000%'))                  OR COALESCE({alias}.folder_name, '') = '')"
            ));
        } else {
            conditions.push(format!(
                "(EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id={alias}.id)                  AND (COALESCE({alias}.manual_date, '') != '' OR COALESCE({alias}.detected_date, '') != '' OR (COALESCE({alias}.date, '') != '' AND {alias}.date NOT LIKE '0000%'))                  AND COALESCE({alias}.folder_name, '') != '')"
            ));
        }
    }

    if let Some(undated_val) = undated {
        if undated_val {
            conditions.push(format!(
                "(COALESCE({alias}.manual_date, '') = '' AND COALESCE({alias}.detected_date, '') = '' AND (COALESCE({alias}.date, '') = '' OR {alias}.date LIKE '0000%'))"
            ));
        } else {
            conditions.push(format!(
                "(COALESCE({alias}.manual_date, '') != '' OR COALESCE({alias}.detected_date, '') != '' OR (COALESCE({alias}.date, '') != '' AND {alias}.date NOT LIKE '0000%'))"
            ));
        }
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
        Some(kind @ ("image" | "video" | "source" | "text")) => {
            conditions.push(format!("{alias}.media_type=?"));
            conditions.push(format!("{alias}.is_archive=0"));
            params.push(SqlValue::Text(kind.to_string()));
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
    inbox: Option<bool>,
    undated: Option<bool>,
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
        inbox,
        undated,
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
            inbox,
            undated,
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
        append_cursor_condition(&mut conditions, &mut params, cursor);
    }
    Ok((conditions.join(" AND "), params))
}

fn append_cursor_condition(
    conditions: &mut Vec<String>,
    params: &mut Vec<SqlValue>,
    cursor: &ItemCursor,
) {
    let condition = match cursor.sort.as_str() {
        "date_asc" => "(i.date > ? OR (i.date = ? AND (i.file_name COLLATE NATURAL_NOCASE > ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id > ?))))",
        "name" => "(i.file_name COLLATE NATURAL_NOCASE > ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id > ?))",
        "name_desc" => "(i.file_name COLLATE NATURAL_NOCASE < ? OR (i.file_name COLLATE NATURAL_NOCASE = ? AND i.id < ?))",
        "size" => "(i.file_size < ? OR (i.file_size = ? AND i.id < ?))",
        "size_asc" => "(i.file_size > ? OR (i.file_size = ? AND i.id > ?))",
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
        "name" | "name_desc" => {
            params.push(SqlValue::Text(cursor.file_name.clone()));
            params.push(SqlValue::Text(cursor.file_name.clone()));
            params.push(SqlValue::Integer(cursor.id));
        }
        "size" | "size_asc" => {
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

fn matching_tag_ids(conn: &Connection, query: &str, artist_id: Option<i64>) -> Result<Vec<i64>> {
    let mut out = Vec::new();
    match artist_id {
        Some(artist_id) => {
            let mut stmt = conn.prepare_cached("SELECT id, name FROM tags WHERE artist_id=?")?;
            let mut rows = stmt.query([artist_id])?;
            while let Some(row) = rows.next()? {
                let name = row.get_ref(1)?.as_str()?;
                if crate::pinyin_search::text_matches_search(query, &[name]) {
                    out.push(row.get::<_, i64>(0)?);
                }
            }
        }
        None => {
            let mut stmt = conn.prepare_cached("SELECT id, name FROM tags")?;
            let mut rows = stmt.query([])?;
            while let Some(row) = rows.next()? {
                let name = row.get_ref(1)?.as_str()?;
                if crate::pinyin_search::text_matches_search(query, &[name]) {
                    out.push(row.get::<_, i64>(0)?);
                }
            }
        }
    }
    Ok(out)
}

/// The page query. Kept as its own function so the join-order guarantee below
/// can be asserted by a test instead of living only in a comment.
///
/// `CROSS JOIN` returns exactly the same rows as `JOIN` here; its only effect is
/// that SQLite may not reorder the two tables. That pin is deliberate. With
/// planner statistics present (the normal state: writable startup bootstraps
/// `ANALYZE`), the planner otherwise drives this query from `artists` and probes
/// `items` once per artist -- 488 nested index scans -- which measured 1.7-2.2x
/// slower than the single `idx_items_hash_queue` range scan it picks when
/// `items` is the outer loop.
///
/// Measured with bundled SQLite on 755,548 items / 488 artists, query-only
/// medians: default sort 347ms -> 209ms, offset 5000 532ms -> 233ms, sort=name
/// 428ms -> 197ms, by-artist unchanged at 0.8ms. End-to-end HTTP on the same
/// library agreed: 595.7 -> 430.7, 770.6 -> 436.5, 598.3 -> 414.3. With the pin,
/// statistics cost 1.03-1.04x instead of 1.42-1.76x, so the pin removes the
/// regression rather than trading it for a different one.
///
/// Do not "simplify" this back to `JOIN`.
fn item_page_sql(where_sql: &str, order_sql: &str) -> String {
    format!(
        "SELECT i.id, i.artist_id, i.file_path, i.file_name, i.file_size, i.file_mtime,
                i.folder_name, i.date, i.detected_date, i.manual_date, i.auto_role,
                i.manual_role, i.is_archive, i.media_type,
                i.content_hash, i.hash_status, i.hash_updated_at, i.st_dev, i.st_ino, i.missing,
                i.missing_at, i.scanned_at, i.width, i.height,
                EXISTS(SELECT 1 FROM item_favorites f WHERE f.item_id=i.id) AS favorite,
                a.name AS artist_name, a.path AS artist_path
         FROM items i CROSS JOIN artists a ON a.id=i.artist_id
         WHERE {where_sql} ORDER BY {order_sql} LIMIT ? OFFSET ?"
    )
}

fn item_order_sql(sort: Option<&str>, group_duplicates: bool) -> &'static str {
    if group_duplicates {
        return match sort.unwrap_or("date_desc") {
            "date_asc" => {
                "i.content_hash ASC, i.date ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC"
            }
            "name" => "i.content_hash ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
            "name_desc" => "i.content_hash ASC, i.file_name COLLATE NATURAL_NOCASE DESC, i.id DESC",
            "size" => "i.content_hash ASC, i.file_size DESC, i.id DESC",
            "size_asc" => "i.content_hash ASC, i.file_size ASC, i.id ASC",
            "scanned_desc" => "i.content_hash ASC, i.scanned_at DESC, i.id DESC",
            _ => {
                "i.content_hash ASC, i.date DESC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC"
            }
        };
    }
    match sort.unwrap_or("date_desc") {
        "date_asc" => "i.date ASC, i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
        "name" => "i.file_name COLLATE NATURAL_NOCASE ASC, i.id ASC",
        "name_desc" => "i.file_name COLLATE NATURAL_NOCASE DESC, i.id DESC",
        "size" => "i.file_size DESC, i.id DESC",
        "size_asc" => "i.file_size ASC, i.id ASC",
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
        "name_desc" => "name_desc",
        "size" => "size",
        "size_asc" => "size_asc",
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
    let file_path: String = row.get(2)?;
    let file_name: String = row.get(3)?;
    let file_size: i64 = row.get(4)?;
    let file_mtime: f64 = row.get(5)?;
    let legacy_date: String = row.get(7)?;
    let detected_date: String = row.get(8)?;
    let manual_date: Option<String> = row.get(9)?;
    let display_date = crate::item_detail::effective_display_date(
        &detected_date,
        manual_date.as_deref(),
        &legacy_date,
    );
    let is_archive: i64 = row.get(12)?;
    let media_type: String = row.get(13)?;
    let extractable_archive = if is_archive != 0 || media_type == "archive" {
        crate::archive_ops::is_extractable_archive(&file_path, &file_name, file_size, file_mtime)
    } else {
        false
    };
    Ok(ItemDetailRow {
        id: row.get(0)?,
        artist_id: row.get(1)?,
        file_path,
        file_name,
        file_size,
        file_mtime,
        folder_name: row.get(6)?,
        date: legacy_date,
        detected_date,
        manual_date,
        display_date,
        auto_role: row.get(10)?,
        manual_role: row.get(11)?,
        tags: Vec::new(),
        is_archive,
        media_type,
        content_hash: row.get(14)?,
        hash_status: row.get(15)?,
        hash_updated_at: row.get(16)?,
        st_dev: row.get(17)?,
        st_ino: row.get(18)?,
        missing: row.get(19)?,
        missing_at: row.get(20)?,
        scanned_at: row.get(21)?,
        width: row.get(22).unwrap_or(0),
        height: row.get(23).unwrap_or(0),
        favorite: row.get(24)?,
        artist_name: row.get(25)?,
        artist_path: row.get(26)?,
        extractable_archive,
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
