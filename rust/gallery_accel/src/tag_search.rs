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

pub fn tag_search_response(
    conn: &Connection,
    artist_id: Option<i64>,
    search: Option<&str>,
    limit: Option<usize>,
) -> Result<Value> {
    Ok(json!({ "tags": list_tag_search(conn, artist_id, search, limit)? }))
}

fn list_tag_search(
    conn: &Connection,
    artist_id: Option<i64>,
    search: Option<&str>,
    limit: Option<usize>,
) -> Result<Vec<TagSearchRow>> {
    // Type-ahead fires this per keystroke. Aggregating item_count joins
    // item_tags x items for every tag in the library; the search path runs
    // the pinyin match against the lightweight tag/artist names first and
    // only aggregates counts for the matched ids.
    let limit = limit.unwrap_or(100).clamp(1, 500);
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
            a.path AS artist_path
        FROM tags t
        JOIN artists a ON a.id = t.artist_id
        {where_sql}
        ORDER BY t.id
        "
    ))?;
    let mut light_rows = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            Ok(TagSearchRow {
                id: row.get("id")?,
                artist_id: row.get("artist_id")?,
                name: row.get("name")?,
                sort_order: row.get("sort_order")?,
                artist_name: row.get("artist_name")?,
                artist_path: row.get("artist_path")?,
                item_count: 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);

    if let Some(query) = search.map(str::trim).filter(|q| !q.is_empty()) {
        light_rows.retain(|tag| {
            crate::pinyin_search::text_matches_search(query, &[&tag.name, &tag.artist_name])
        });
        // Cap the aggregate work: counts are computed only for the first 500
        // matched ids (deterministic natural-name order).
        light_rows.sort_by(|left, right| {
            natural_compare(&left.name, &right.name)
                .then_with(|| natural_compare(&left.artist_name, &right.artist_name))
                .then_with(|| left.id.cmp(&right.id))
        });
        light_rows.truncate(MAX_SEARCH_TAG_IDS);
    }

    // Chunked aggregate: idx_item_tags_tag makes each chunk an index lookup,
    // so no single query scans the whole join for thousands of variables.
    let mut counts: std::collections::HashMap<i64, i64> = std::collections::HashMap::new();
    let ids: Vec<i64> = light_rows.iter().map(|row| row.id).collect();
    for chunk in ids.chunks(COUNT_CHUNK_IDS) {
        let placeholders = std::iter::repeat_n("?", chunk.len())
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "
            SELECT it.tag_id, COUNT(i.id)
            FROM item_tags it
            JOIN items i ON i.id = it.item_id
                AND i.missing=0
                AND (i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive=1)
            WHERE it.tag_id IN ({placeholders})
            GROUP BY it.tag_id
            "
        );
        let mut stmt = conn.prepare(&sql)?;
        let mut rows = stmt.query_map(rusqlite::params_from_iter(chunk.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })?;
        while let Some((tag_id, count)) = rows.next().transpose()? {
            counts.insert(tag_id, count);
        }
    }
    for row in &mut light_rows {
        row.item_count = counts.get(&row.id).copied().unwrap_or(0);
    }

    light_rows.sort_by(|left, right| {
        right
            .item_count
            .cmp(&left.item_count)
            .then_with(|| natural_compare(&left.name, &right.name))
            .then_with(|| natural_compare(&left.artist_name, &right.artist_name))
    });
    light_rows.truncate(limit);
    Ok(light_rows)
}

const MAX_SEARCH_TAG_IDS: usize = 500;
const COUNT_CHUNK_IDS: usize = 400;
