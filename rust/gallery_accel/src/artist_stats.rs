use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::tags::compare_tag_order;

#[derive(Clone, Serialize, Debug)]
struct StatsTagRow {
    id: i64,
    name: String,
    count: i64,
    #[serde(skip_serializing)]
    sort_order: i64,
}

pub fn artist_stats_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    let total = count_artist_items(
        conn,
        artist_id,
        "AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)",
    )?;
    let videos = count_artist_items(conn, artist_id, "AND media_type='video' AND is_archive=0")?;
    let sources = count_artist_items(conn, artist_id, "AND media_type='source' AND is_archive=0")?;
    let archives = count_artist_items(
        conn,
        artist_id,
        "AND (is_archive=1 OR media_type='archive')",
    )?;
    let untagged = conn.query_row(
        "
        SELECT COUNT(*) FROM items
        WHERE artist_id=? AND missing=0
          AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)
          AND NOT EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id = items.id)
        ",
        [artist_id],
        |row| row.get::<_, i64>(0),
    )?;
    let tags = list_stats_tags(conn, artist_id)?;
    Ok(json!({
        "total": total,
        "archives": archives,
        "videos": videos,
        "sources": sources,
        "untagged": untagged,
        "tags": tags,
    }))
}

fn count_artist_items(conn: &Connection, artist_id: i64, extra_where: &str) -> Result<i64> {
    conn.query_row(
        &format!(
            "
            SELECT COUNT(*) FROM items
            WHERE artist_id=? AND missing=0 {extra_where}
            "
        ),
        [artist_id],
        |row| row.get(0),
    )
    .context("count artist items")
}

fn list_stats_tags(conn: &Connection, artist_id: i64) -> Result<Vec<StatsTagRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT t.id, t.name, t.sort_order, COUNT(i.id) AS item_count
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
            Ok(StatsTagRow {
                id: row.get("id")?,
                name: row.get("name")?,
                count: row.get("item_count")?,
                sort_order: row.get("sort_order")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort_by(|left, right| {
        compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name)
    });
    Ok(tags)
}
