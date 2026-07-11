use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{params_from_iter, types::Value as SqlValue, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::folder_tree::normalize_folder;
use crate::item_detail::ItemDetailRow;
use crate::item_detail_tags::ItemDetailTagRow;
use crate::media_roots::split_csv;
use crate::tags::compare_tag_order;
use crate::DEFAULT_LIMIT;

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
        conn, Some(artist_id), limit, offset, sort, media_type, folder, date_from, date_to,
        image_only, untagged, tag_id, duplicates_only, tag_names, search, false,
    )
}

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
) -> Result<Value> {
    let page_limit = limit.unwrap_or(DEFAULT_LIMIT);
    let page_offset = offset.unwrap_or(0).max(0);
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
    )?;
    let total = conn.query_row(
        &format!("SELECT COUNT(*) FROM items i WHERE {where_sql}"),
        params_from_iter(params.iter()),
        |row| row.get::<_, i64>(0),
    )?;
    let mut page_params = params;
    page_params.push(SqlValue::Integer(page_limit));
    page_params.push(SqlValue::Integer(page_offset));
    let mut stmt = conn.prepare(&format!(
        "SELECT i.id, i.artist_id, i.file_path, i.file_name, i.file_size, i.file_mtime,
                i.folder_name, i.date, i.auto_role, i.manual_role, i.is_archive, i.media_type,
                i.content_hash, i.hash_status, i.hash_updated_at, i.st_dev, i.st_ino, i.missing,
                i.missing_at, i.scanned_at, a.name AS artist_name, a.path AS artist_path
         FROM items i JOIN artists a ON a.id=i.artist_id
         WHERE {where_sql} ORDER BY {} LIMIT ? OFFSET ?",
        item_order_sql(sort),
    ))?;
    let mut page_items = stmt
        .query_map(params_from_iter(page_params.iter()), item_detail_row)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    attach_page_tags(conn, &mut page_items)?;
    Ok(json!({
        "items": page_items,
        "total": total,
        "offset": page_offset,
        "limit": page_limit,
    }))
}

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
) -> Result<(String, Vec<SqlValue>)> {
    let mut conditions = vec!["i.missing=0".to_string()];
    let mut params = Vec::new();
    if let Some(artist_id) = artist_id {
        conditions.push("i.artist_id=?".to_string());
        params.push(SqlValue::Integer(artist_id));
    }

    if tag_id.is_some() || tag_names.is_some() || untagged.unwrap_or(false) || search_tags_only {
        conditions.push(
            "(i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive=1)"
                .to_string(),
        );
    }
    if duplicates_only.unwrap_or(false) {
        conditions.push("i.media_type IN ('image', 'video', 'source')".to_string());
        conditions.push("i.is_archive=0".to_string());
    }
    let media_type = media_type
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    match media_type.as_deref() {
        Some("archive") => conditions.push("(i.media_type='archive' OR i.is_archive=1)".to_string()),
        Some("image" | "video" | "source" | "text") => {
            conditions.push("i.media_type=?".to_string());
            conditions.push("i.is_archive=0".to_string());
            params.push(SqlValue::Text(media_type.unwrap()));
        }
        Some(_) => conditions.push("1=0".to_string()),
        None if image_only.unwrap_or(false) => {
            conditions.push("i.media_type IN ('image', 'video', 'source')".to_string());
            conditions.push("i.is_archive=0".to_string());
        }
        None => conditions.push(
            "(i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive=1)"
                .to_string(),
        ),
    }

    if let Some(tag_id) = tag_id {
        conditions.push(
            "EXISTS (SELECT 1 FROM item_tags it JOIN tags t ON t.id=it.tag_id \
             WHERE it.item_id=i.id AND it.tag_id=? AND t.artist_id=i.artist_id)"
                .to_string(),
        );
        params.push(SqlValue::Integer(tag_id));
    }
    for name in tag_names.into_iter().flat_map(|names| split_csv(names, false)) {
        conditions.push(
            "EXISTS (SELECT 1 FROM item_tags it JOIN tags t ON t.id=it.tag_id \
             WHERE it.item_id=i.id AND t.artist_id=i.artist_id AND t.name=?)"
                .to_string(),
        );
        params.push(SqlValue::Text(name));
    }
    if untagged.unwrap_or(false) {
        conditions.push("NOT EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id=i.id)".to_string());
    }

    let folder = normalize_folder(folder.unwrap_or(""));
    let mut folder_prefix = None;
    if !folder.is_empty() {
        let artist_path = artist_id
            .map(|id| {
                conn.query_row("SELECT path FROM artists WHERE id=?", [id], |row| row.get::<_, String>(0))
                    .optional()
            })
            .transpose()?;
        if let Some(artist_path) = artist_path.flatten() {
            let prefix = format!(
                "{}/{}/",
                artist_path.replace('\\', "/").trim_end_matches('/'),
                folder
            );
            conditions.push(r#"substr(replace(i.file_path, '\', '/'), 1, ?) = ?"#.to_string());
            params.push(SqlValue::Integer(prefix.chars().count() as i64));
            params.push(SqlValue::Text(prefix.clone()));
            folder_prefix = Some(prefix);
        } else {
            conditions.push("1=0".to_string());
        }
    }

    if duplicates_only.unwrap_or(false) {
        conditions.push("i.hash_status='done'".to_string());
        conditions.push("i.content_hash != ''".to_string());
        let mut duplicate_where = vec![
            "d.artist_id=i.artist_id",
            "d.id != i.id",
            "d.missing=0",
            "d.hash_status='done'",
            "d.content_hash=i.content_hash",
            "d.content_hash != ''",
        ];
        if let Some(prefix) = folder_prefix {
            duplicate_where.push(r#"substr(replace(d.file_path, '\', '/'), 1, ?) = ?"#);
            params.push(SqlValue::Integer(prefix.chars().count() as i64));
            params.push(SqlValue::Text(prefix));
        }
        conditions.push(format!(
            "EXISTS (SELECT 1 FROM items d WHERE {})",
            duplicate_where.join(" AND ")
        ));
    }

    if let Some(query) = search {
        let query = query.trim();
        if query.is_empty() {
            conditions.push("1=0".to_string());
        } else {
            let like = format!("%{query}%");
            let tag_ids = matching_tag_ids(conn, query, artist_id)?;
            let tag_clause = if tag_ids.is_empty() {
                "st.name LIKE ?".to_string()
            } else {
                format!(
                    "st.name LIKE ? OR st.id IN ({})",
                    std::iter::repeat_n("?", tag_ids.len()).collect::<Vec<_>>().join(",")
                )
            };
            let tag_search = format!(
                "EXISTS (SELECT 1 FROM item_tags sit JOIN tags st ON st.id=sit.tag_id \
                 WHERE sit.item_id=i.id AND st.artist_id=i.artist_id AND ({tag_clause}))"
            );
            if search_tags_only {
                conditions.push(tag_search);
            } else {
                conditions.push(format!(
                    "(i.file_name LIKE ? OR i.folder_name LIKE ? OR i.file_path LIKE ? OR {tag_search})"
                ));
                params.extend((0..3).map(|_| SqlValue::Text(like.clone())));
            }
            params.push(SqlValue::Text(like));
            params.extend(tag_ids.into_iter().map(SqlValue::Integer));
        }
    }

    if let Some(date_from) = date_from.map(str::trim).filter(|value| !value.is_empty()) {
        conditions.push("i.date >= ?".to_string());
        params.push(SqlValue::Text(date_from.to_string()));
    }
    if let Some(date_to) = date_to.map(str::trim).filter(|value| !value.is_empty()) {
        conditions.push("i.date <= ?".to_string());
        params.push(SqlValue::Text(date_to.to_string()));
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
        .filter_map(|(id, name)| crate::pinyin_search::text_matches_search(query, &[&name]).then_some(id))
        .collect())
}

fn item_order_sql(sort: Option<&str>) -> &'static str {
    match sort.unwrap_or("date_desc") {
        "date_asc" => "i.date ASC, i.file_name COLLATE NATURAL_NOCASE",
        "name" => "i.file_name COLLATE NATURAL_NOCASE",
        "size" => "i.file_size DESC",
        _ => "i.date DESC, i.file_name COLLATE NATURAL_NOCASE",
    }
}

fn item_detail_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ItemDetailRow> {
    Ok(ItemDetailRow {
        id: row.get("id")?, artist_id: row.get("artist_id")?, file_path: row.get("file_path")?,
        file_name: row.get("file_name")?, file_size: row.get("file_size")?, file_mtime: row.get("file_mtime")?,
        folder_name: row.get("folder_name")?, date: row.get("date")?, auto_role: row.get("auto_role")?,
        manual_role: row.get("manual_role")?, tags: Vec::new(), is_archive: row.get("is_archive")?,
        media_type: row.get("media_type")?, content_hash: row.get("content_hash")?,
        hash_status: row.get("hash_status")?, hash_updated_at: row.get("hash_updated_at")?,
        st_dev: row.get("st_dev")?, st_ino: row.get("st_ino")?, missing: row.get("missing")?,
        missing_at: row.get("missing_at")?, scanned_at: row.get("scanned_at")?,
        artist_name: row.get("artist_name")?, artist_path: row.get("artist_path")?,
    })
}

fn attach_page_tags(conn: &Connection, items: &mut [ItemDetailRow]) -> Result<()> {
    if items.is_empty() {
        return Ok(());
    }
    let ids = items.iter().map(|item| item.id).collect::<Vec<_>>();
    let placeholders = std::iter::repeat_n("?", ids.len()).collect::<Vec<_>>().join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT it.item_id, t.id, t.name, t.sort_order FROM item_tags it JOIN tags t ON t.id=it.tag_id \
         WHERE it.item_id IN ({placeholders})"
    ))?;
    let mut by_item = HashMap::<i64, Vec<ItemDetailTagRow>>::new();
    let mut rows = stmt.query(params_from_iter(ids.iter()))?;
    while let Some(row) = rows.next()? {
        by_item.entry(row.get(0)?).or_default().push(ItemDetailTagRow {
            id: row.get(1)?, name: row.get(2)?, sort_order: row.get(3)?,
        });
    }
    for tags in by_item.values_mut() {
        tags.sort_by(|left, right| compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name));
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
}
