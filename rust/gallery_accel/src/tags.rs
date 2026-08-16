use std::cmp::Ordering;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
struct TagRow {
    id: i64,
    artist_id: i64,
    name: String,
    sort_order: i64,
    item_count: i64,
}

/// Historical FastAPI contract: bare JSON **array** of tags for `/api/tags?artist_id=`.
/// Static UI does `state.tags = asArray(await API.get(...))`.
pub fn tags_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    Ok(json!(list_tags(conn, artist_id)?))
}

fn list_tags(conn: &Connection, artist_id: i64) -> Result<Vec<TagRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT t.id, t.artist_id, t.name, t.sort_order, COUNT(i.id) AS item_count
        FROM tags t
        LEFT JOIN item_tags it ON it.tag_id = t.id
        LEFT JOIN items i ON i.id = it.item_id
            AND i.missing=0
            AND (i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive=1)
        WHERE t.artist_id=?
        GROUP BY t.id
        ",
    )?;
    let mut tags = stmt
        .query_map([artist_id], |row| {
            Ok(TagRow {
                id: row.get("id")?,
                artist_id: row.get("artist_id")?,
                name: row.get("name")?,
                sort_order: row.get("sort_order")?,
                item_count: row.get("item_count")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort_by(|left, right| {
        compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name)
    });
    Ok(tags)
}

pub(crate) fn compare_tag_order(
    left_sort_order: i64,
    left_name: &str,
    right_sort_order: i64,
    right_name: &str,
) -> Ordering {
    left_sort_order
        .cmp(&right_sort_order)
        .then_with(|| natural_compare(left_name, right_name))
}
