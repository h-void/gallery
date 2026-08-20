use anyhow::{Context, Result};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

#[derive(Clone, Serialize, Debug)]
struct ArtistReferenceRow {
    id: i64,
    artist_id: i64,
    item_id: Option<i64>,
    style_group: String,
    dino_embedding_dim: Option<i64>,
    wd14_embedding_dim: Option<i64>,
    embedding_model_variant: String,
    embedding_updated_at: Option<f64>,
    created_at: f64,
    artist_name: String,
    file_path: Option<String>,
    file_name: Option<String>,
    file_size: Option<i64>,
    file_mtime: Option<f64>,
    media_type: Option<String>,
    is_archive: Option<i64>,
}

pub fn artist_references_response(
    conn: &Connection,
    artist_id: i64,
    limit: Option<i64>,
) -> Result<Value> {
    Ok(json!({ "references": list_artist_references(conn, artist_id, limit)? }))
}

fn list_artist_references(
    conn: &Connection,
    artist_id: i64,
    limit: Option<i64>,
) -> Result<Vec<ArtistReferenceRow>> {
    let limit = limit.unwrap_or(200).max(1);
    let mut stmt = conn.prepare(
        "
        SELECT
            ar.id,
            ar.artist_id,
            ar.item_id,
            ar.style_group,
            ar.dino_embedding_dim,
            ar.wd14_embedding_dim,
            ar.embedding_model_variant,
            ar.embedding_updated_at,
            ar.created_at,
            a.name AS artist_name,
            i.file_path,
            i.file_name,
            i.file_size,
            i.file_mtime,
            i.media_type,
            i.is_archive
        FROM artist_references ar
        JOIN artists a ON a.id = ar.artist_id
        LEFT JOIN items i ON i.id = ar.item_id
        WHERE ar.artist_id=?
        ORDER BY ar.id DESC
        LIMIT ?
        ",
    )?;
    let references = stmt
        .query_map(rusqlite::params![artist_id, limit], |row| {
            Ok(ArtistReferenceRow {
                id: row.get("id")?,
                artist_id: row.get("artist_id")?,
                item_id: row.get("item_id")?,
                style_group: row.get("style_group")?,
                dino_embedding_dim: row.get("dino_embedding_dim")?,
                wd14_embedding_dim: row.get("wd14_embedding_dim")?,
                embedding_model_variant: row.get("embedding_model_variant")?,
                embedding_updated_at: row.get("embedding_updated_at")?,
                created_at: row.get("created_at")?,
                artist_name: row.get("artist_name")?,
                file_path: row.get("file_path")?,
                file_name: row.get("file_name")?,
                file_size: row.get("file_size")?,
                file_mtime: row.get("file_mtime")?,
                media_type: row.get("media_type")?,
                is_archive: row.get("is_archive")?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("list artist references")?;
    Ok(references)
}
