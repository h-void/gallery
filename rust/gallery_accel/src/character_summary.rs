use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::character_summary_tags::{list_character_summary_tags, CharacterSummaryTagRow};
use crate::natural_sort::natural_compare;

#[derive(Clone, Serialize, Debug)]
pub(crate) struct CharacterSummaryCharacterRow {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) created_at: f64,
    pub(crate) reference_count: i64,
    pub(crate) current_model_reference_count: i64,
    pub(crate) stale_reference_count: i64,
}

pub fn character_summary_response(
    conn: &Connection,
    artist_id: Option<i64>,
    model_repo_id: &str,
    model_variant: &str,
    model_file: &str,
) -> Result<Value> {
    let characters =
        list_character_summary_characters(conn, model_repo_id, model_variant, model_file)?;
    let tags = list_character_summary_tags(conn, artist_id, &characters)?;
    let totals = character_summary_totals(conn, model_repo_id, model_variant, model_file, &tags)?;
    Ok(json!({
        "tags": tags,
        "characters": characters,
        "totals": totals,
    }))
}

fn list_character_summary_characters(
    conn: &Connection,
    model_repo_id: &str,
    model_variant: &str,
    model_file: &str,
) -> Result<Vec<CharacterSummaryCharacterRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            c.id,
            c.name,
            c.created_at,
            COUNT(cr.id) AS reference_count,
            COUNT(
                CASE
                    WHEN cr.embedding IS NOT NULL
                     AND cr.embedding_model_repo_id = ?
                     AND cr.embedding_model_variant = ?
                     AND cr.embedding_model_file = ?
                    THEN 1
                END
            ) AS current_model_reference_count,
            COUNT(
                CASE
                    WHEN cr.embedding IS NOT NULL
                     AND (
                        cr.embedding_model_repo_id != ?
                        OR cr.embedding_model_variant != ?
                        OR cr.embedding_model_file != ?
                     )
                    THEN 1
                END
            ) AS stale_reference_count
        FROM characters c
        LEFT JOIN character_references cr ON cr.character_id = c.id
        GROUP BY c.id
        ",
    )?;
    let mut characters = stmt
        .query_map(
            rusqlite::params![
                model_repo_id,
                model_variant,
                model_file,
                model_repo_id,
                model_variant,
                model_file
            ],
            |row| {
                Ok(CharacterSummaryCharacterRow {
                    id: row.get("id")?,
                    name: row.get("name")?,
                    created_at: row.get("created_at")?,
                    reference_count: row.get("reference_count")?,
                    current_model_reference_count: row.get("current_model_reference_count")?,
                    stale_reference_count: row.get("stale_reference_count")?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("list character summary characters")?;
    characters.sort_by(|left, right| natural_compare(&left.name, &right.name));
    Ok(characters)
}

fn character_summary_totals(
    conn: &Connection,
    model_repo_id: &str,
    model_variant: &str,
    model_file: &str,
    tags: &[CharacterSummaryTagRow],
) -> Result<Value> {
    let reference_row = conn.query_row(
        "
        SELECT
            COUNT(*) AS reference_count,
            COUNT(
                CASE
                    WHEN embedding_model_repo_id = ?
                     AND embedding_model_variant = ?
                     AND embedding_model_file = ?
                    THEN 1
                END
            ) AS current_model_reference_count,
            COUNT(
                CASE
                    WHEN embedding_model_repo_id != ?
                      OR embedding_model_variant != ?
                      OR embedding_model_file != ?
                    THEN 1
                END
            ) AS stale_reference_count
        FROM character_references
        WHERE embedding IS NOT NULL
        ",
        rusqlite::params![
            model_repo_id,
            model_variant,
            model_file,
            model_repo_id,
            model_variant,
            model_file
        ],
        |row| {
            Ok((
                row.get::<_, i64>("reference_count")?,
                row.get::<_, i64>("current_model_reference_count")?,
                row.get::<_, i64>("stale_reference_count")?,
            ))
        },
    )?;
    let character_count: i64 =
        conn.query_row("SELECT COUNT(*) FROM characters", [], |row| row.get(0))?;
    let candidate_items: i64 = tags.iter().map(|tag| tag.single_tag_image_count).sum();
    Ok(json!({
        "characters": character_count,
        "references": reference_row.0,
        "current_model_references": reference_row.1,
        "stale_reference_embeddings": reference_row.2,
        "candidate_items": candidate_items,
    }))
}
