use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Serialize, Debug)]
struct CharacterReferenceRow {
    id: i64,
    character_id: i64,
    character_name: String,
    source_type: String,
    embedding_dim: i64,
    embedding_model_repo_id: Option<String>,
    embedding_model_variant: Option<String>,
    embedding_model_file: Option<String>,
    embedding_updated_at: Option<f64>,
    item_id: Option<i64>,
    created_at: f64,
    file_path: Option<String>,
    file_name: Option<String>,
    file_size: Option<i64>,
    file_mtime: Option<f64>,
    media_type: Option<String>,
    is_archive: Option<i64>,
}

pub fn character_references_response(
    conn: &Connection,
    character_id: i64,
    limit: Option<i64>,
) -> Result<Value> {
    Ok(json!({ "references": list_character_references(conn, character_id, limit)? }))
}

fn list_character_references(
    conn: &Connection,
    character_id: i64,
    limit: Option<i64>,
) -> Result<Vec<CharacterReferenceRow>> {
    let limit = limit.unwrap_or(200).max(1);
    let mut stmt = conn.prepare(
        "
        SELECT cr.id, cr.character_id, c.name as character_name, cr.source_type,
               cr.embedding_dim, cr.embedding_model_repo_id,
               cr.embedding_model_variant, cr.embedding_model_file,
               cr.embedding_updated_at, cr.item_id, cr.created_at,
               i.file_path, i.file_name, i.file_size, i.file_mtime,
               i.media_type, i.is_archive
        FROM character_references cr
        JOIN characters c ON c.id = cr.character_id
        LEFT JOIN items i ON i.id = cr.item_id
        WHERE cr.character_id = ?
        ORDER BY cr.id DESC
        LIMIT ?
        ",
    )?;
    let references = stmt
        .query_map(rusqlite::params![character_id, limit], |row| {
            Ok(CharacterReferenceRow {
                id: row.get("id")?,
                character_id: row.get("character_id")?,
                character_name: row.get("character_name")?,
                source_type: row.get("source_type")?,
                embedding_dim: row.get("embedding_dim")?,
                embedding_model_repo_id: row.get("embedding_model_repo_id")?,
                embedding_model_variant: row.get("embedding_model_variant")?,
                embedding_model_file: row.get("embedding_model_file")?,
                embedding_updated_at: row.get("embedding_updated_at")?,
                item_id: row.get("item_id")?,
                created_at: row.get("created_at")?,
                file_path: row.get("file_path")?,
                file_name: row.get("file_name")?,
                file_size: row.get("file_size")?,
                file_mtime: row.get("file_mtime")?,
                media_type: row.get("media_type")?,
                is_archive: row.get("is_archive")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("list character references")?;
    Ok(references)
}
