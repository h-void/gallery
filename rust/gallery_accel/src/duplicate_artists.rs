use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::natural_sort::natural_compare;
use crate::path_display::display_path;

#[derive(Clone, Debug)]
struct DuplicateArtistSourceRow {
    id: i64,
    name: String,
    path: String,
    item_count: i64,
}

#[derive(Clone, Serialize, Debug)]
struct DuplicateArtistPath {
    id: i64,
    name: String,
    path: String,
    display_path: String,
    item_count: i64,
}

#[derive(Clone, Serialize, Debug)]
struct DuplicateArtistGroup {
    name: String,
    count: i64,
    paths: Vec<DuplicateArtistPath>,
}

pub fn duplicate_artists_response(conn: &Connection, roots: &MediaRoots) -> Result<Value> {
    let groups = list_duplicate_artists(conn, roots)?;
    Ok(json!({
        "count": groups.len() as i64,
        "groups": groups,
    }))
}

fn list_duplicate_artists(
    conn: &Connection,
    roots: &MediaRoots,
) -> Result<Vec<DuplicateArtistGroup>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, name, path
        FROM artists
        WHERE missing = 0
        ",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok(DuplicateArtistSourceRow {
                id: row.get("id")?,
                name: row.get("name")?,
                path: row.get("path")?,
                item_count: 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut by_name: HashMap<String, Vec<DuplicateArtistSourceRow>> = HashMap::new();
    for row in rows {
        let name = row.name.trim();
        if name.is_empty() {
            continue;
        }
        by_name.entry(name.to_lowercase()).or_default().push(row);
    }

    let mut dup_artist_ids = Vec::new();
    for entries in by_name.values() {
        if entries.len() >= 2 {
            for entry in entries {
                dup_artist_ids.push(entry.id);
            }
        }
    }

    if dup_artist_ids.is_empty() {
        return Ok(Vec::new());
    }

    let mut count_stmt = conn.prepare(
        "
        SELECT COUNT(id)
        FROM items
        WHERE artist_id = ?
            AND missing = 0
            AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive = 1)
        ",
    )?;
    let mut counts: HashMap<i64, i64> = HashMap::new();
    for aid in dup_artist_ids {
        let count: i64 = count_stmt.query_row([aid], |r| r.get(0)).unwrap_or(0);
        counts.insert(aid, count);
    }

    let mut groups = Vec::new();
    for mut entries in by_name.into_values() {
        if entries.len() < 2 {
            continue;
        }
        for entry in &mut entries {
            if let Some(&cnt) = counts.get(&entry.id) {
                entry.item_count = cnt;
            }
        }
        entries.sort_by(|left, right| left.path.cmp(&right.path));
        let display_name = entries
            .first()
            .map(|artist| artist.name.clone())
            .unwrap_or_default();
        groups.push(DuplicateArtistGroup {
            name: display_name,
            count: entries.len() as i64,
            paths: entries
                .into_iter()
                .map(|artist| DuplicateArtistPath {
                    id: artist.id,
                    name: artist.name,
                    display_path: display_path(&artist.path, roots),
                    path: artist.path,
                    item_count: artist.item_count,
                })
                .collect(),
        });
    }
    groups.sort_by(|left, right| {
        right
            .count
            .cmp(&left.count)
            .then_with(|| natural_compare(&left.name, &right.name))
    });
    Ok(groups)
}
