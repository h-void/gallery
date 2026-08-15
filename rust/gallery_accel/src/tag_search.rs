use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
struct TagSearchRow {
    id: i64,
    artist_id: i64,
    name: String,
    sort_order: i64,
    artist_name: String,
    artist_path: String,
    item_count: i64,
}

pub fn tag_search_response(conn: &Connection, artist_id: Option<i64>) -> Result<Value> {
    Ok(json!({ "tags": list_tag_search(conn, artist_id)? }))
}

fn list_tag_search(conn: &Connection, artist_id: Option<i64>) -> Result<Vec<TagSearchRow>> {
    let where_sql = if artist_id.is_some() {
        "WHERE t.artist_id=?"
    } else {
        ""
    };
    let params = artist_id.into_iter().collect::<Vec<_>>();
    let mut stmt = conn.prepare(&format!(
        "
        SELECT
            t.id,
            t.artist_id,
            t.name,
            t.sort_order,
            a.name AS artist_name,
            a.path AS artist_path,
            COUNT(i.id) AS item_count
        FROM tags t
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN item_tags it ON it.tag_id = t.id
        LEFT JOIN items i ON i.id = it.item_id
            AND i.missing=0
            AND (i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive=1)
        {where_sql}
        GROUP BY t.id
        "
    ))?;
    let mut tags = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(TagSearchRow {
                id: row.get("id")?,
                artist_id: row.get("artist_id")?,
                name: row.get("name")?,
                sort_order: row.get("sort_order")?,
                artist_name: row.get("artist_name")?,
                artist_path: row.get("artist_path")?,
                item_count: row.get("item_count")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort_by(|left, right| {
        right
            .item_count
            .cmp(&left.item_count)
            .then_with(|| natural_compare(&left.name, &right.name))
            .then_with(|| natural_compare(&left.artist_name, &right.artist_name))
    });
    Ok(tags)
}
