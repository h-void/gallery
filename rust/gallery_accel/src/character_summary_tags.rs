use std::collections::HashMap;

use anyhow::Result;
use rusqlite::{Connection, Row};
use serde::Serialize;

use crate::character_summary::CharacterSummaryCharacterRow;
use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
pub(crate) struct CharacterSummaryTagRow {
    id: i64,
    name: String,
    tag_ids: Vec<i64>,
    artist_count: i64,
    pub(crate) single_tag_image_count: i64,
    character_id: Option<i64>,
    reference_count: i64,
}

pub(crate) fn list_character_summary_tags(
    conn: &Connection,
    artist_id: Option<i64>,
    characters: &[CharacterSummaryCharacterRow],
) -> Result<Vec<CharacterSummaryTagRow>> {
    let characters_by_name: HashMap<String, (i64, i64)> = characters
        .iter()
        .map(|row| (row.name.clone(), (row.id, row.reference_count)))
        .collect();
    let sql = format!(
        "
        SELECT
            MIN(t.id) AS id,
            t.name,
            GROUP_CONCAT(DISTINCT t.id) AS tag_ids,
            COUNT(DISTINCT t.artist_id) AS artist_count,
            COUNT(DISTINCT i.id) AS single_tag_image_count
        FROM tags t
        JOIN artists a ON a.id = t.artist_id
        LEFT JOIN item_tags it ON it.tag_id = t.id
        LEFT JOIN items i ON i.id = it.item_id
            AND i.missing = 0
            AND i.media_type = 'image'
            AND i.is_archive = 0
            AND (
                SELECT COUNT(*)
                FROM item_tags only_it
                WHERE only_it.item_id = i.id
            ) = 1
        {}
        GROUP BY t.name
        ",
        if artist_id.is_some() {
            "WHERE t.artist_id = ?"
        } else {
            ""
        }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = if let Some(artist_id) = artist_id {
        stmt.query_map([artist_id], |row| {
            character_summary_tag_from_row(row, &characters_by_name)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        stmt.query_map([], |row| {
            character_summary_tag_from_row(row, &characters_by_name)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut tags = rows;
    tags.sort_by(|left, right| {
        right
            .single_tag_image_count
            .cmp(&left.single_tag_image_count)
            .then_with(|| natural_compare(&left.name, &right.name))
    });
    Ok(tags)
}

fn character_summary_tag_from_row(
    row: &Row<'_>,
    characters_by_name: &HashMap<String, (i64, i64)>,
) -> rusqlite::Result<CharacterSummaryTagRow> {
    let name: String = row.get("name")?;
    let raw_tag_ids: Option<String> = row.get("tag_ids")?;
    let tag_ids = raw_tag_ids
        .unwrap_or_default()
        .split(',')
        .filter_map(|value| value.parse::<i64>().ok())
        .collect();
    let (character_id, reference_count) = characters_by_name
        .get(&name)
        .copied()
        .map(|(id, count)| (Some(id), count))
        .unwrap_or((None, 0));
    Ok(CharacterSummaryTagRow {
        id: row.get("id")?,
        name,
        tag_ids,
        artist_count: row.get("artist_count")?,
        single_tag_image_count: row.get("single_tag_image_count")?,
        character_id,
        reference_count,
    })
}
