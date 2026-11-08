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
    let mut stats_stmt = conn
        .prepare_cached(
            "
            SELECT
                COALESCE(SUM(is_visible), 0),
                COALESCE(SUM(CASE WHEN media_type='video' AND is_archive=0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN media_type='source' AND is_archive=0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_archive=1 OR media_type='archive' THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_visible=1 AND has_tag=0 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_visible=1 AND missing_date=1 THEN 1 ELSE 0 END), 0),
                COALESCE(SUM(CASE WHEN is_visible=1 AND (has_tag=0 OR missing_date=1 OR empty_folder=1) THEN 1 ELSE 0 END), 0)
            FROM (
                SELECT
                    media_type,
                    is_archive,
                    CASE WHEN media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1 THEN 1 ELSE 0 END AS is_visible,
                    CASE WHEN (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)
                        THEN EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id = items.id)
                        ELSE 1 END AS has_tag,
                    CASE WHEN COALESCE(manual_date, '') = '' AND COALESCE(detected_date, '') = '' AND (COALESCE(date, '') = '' OR date LIKE '0000%')
                        THEN 1 ELSE 0 END AS missing_date,
                    CASE WHEN COALESCE(folder_name, '') = '' THEN 1 ELSE 0 END AS empty_folder
                FROM items
                WHERE artist_id=? AND missing=0
            )
            ",
        )
        .context("prepare artist item stats")?;
    let (total, videos, sources, archives, untagged, undated, inbox) = stats_stmt
        .query_row([artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, i64>(5)?,
                row.get::<_, i64>(6)?,
            ))
        })
        .context("count artist items")?;
    let extractable_archives = if archives > 0 {
        count_extractable_artist_archives(conn, artist_id)?
    } else {
        0
    };
    let mut fav_stmt = conn.prepare_cached(
        "SELECT COUNT(*) FROM item_favorites f JOIN items i ON i.id=f.item_id
         WHERE i.artist_id=? AND i.missing=0",
    )?;
    let favorites = fav_stmt.query_row([artist_id], |row| row.get::<_, i64>(0))?;
    let tags = list_stats_tags(conn, artist_id)?;
    Ok(json!({
        "total": total,
        "archives": archives,
        "extractable_archives": extractable_archives,
        "videos": videos,
        "sources": sources,
        "untagged": untagged,
        "undated": undated,
        "inbox": inbox,
        "favorites": favorites,
        "tags": tags,
    }))
}

fn count_extractable_artist_archives(conn: &Connection, artist_id: i64) -> Result<i64> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT file_path, file_name, file_size, file_mtime
        FROM items
        WHERE artist_id=? AND missing=0 AND (is_archive=1 OR media_type='archive')
        ",
    )?;
    let mut rows = stmt.query([artist_id])?;
    let mut count = 0i64;
    while let Some(row) = rows.next()? {
        let file_path: String = row.get(0)?;
        let file_name: String = row.get(1)?;
        let file_size: i64 = row.get(2)?;
        let file_mtime: f64 = row.get(3)?;
        if crate::archive_ops::is_extractable_archive(&file_path, &file_name, file_size, file_mtime)
        {
            count += 1;
        }
    }
    Ok(count)
}

fn list_stats_tags(conn: &Connection, artist_id: i64) -> Result<Vec<StatsTagRow>> {
    let mut stmt = conn.prepare_cached(
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
                id: row.get(0)?,
                name: row.get(1)?,
                sort_order: row.get(2)?,
                count: row.get(3)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    tags.sort_by(|left, right| {
        compare_tag_order(left.sort_order, &left.name, right.sort_order, &right.name)
    });
    Ok(tags)
}
