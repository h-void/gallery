use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};

use crate::item_detail_tags::{list_item_detail_tags, ItemDetailTagRow};

#[derive(Clone, Serialize, Debug)]
pub(crate) struct ItemDetailRow {
    pub(crate) id: i64,
    pub(crate) artist_id: i64,
    pub(crate) file_path: String,
    pub(crate) file_name: String,
    pub(crate) file_size: i64,
    pub(crate) file_mtime: f64,
    pub(crate) folder_name: String,
    pub(crate) date: String,
    pub(crate) auto_role: String,
    pub(crate) manual_role: Option<String>,
    pub(crate) tags: Vec<ItemDetailTagRow>,
    pub(crate) is_archive: i64,
    pub(crate) media_type: String,
    pub(crate) content_hash: String,
    pub(crate) hash_status: String,
    pub(crate) hash_updated_at: Option<f64>,
    pub(crate) st_dev: Option<i64>,
    pub(crate) st_ino: Option<i64>,
    pub(crate) missing: i64,
    pub(crate) missing_at: Option<f64>,
    pub(crate) scanned_at: i64,
    pub(crate) artist_name: String,
    pub(crate) artist_path: String,
}

pub fn item_detail_response(conn: &Connection, item_id: i64) -> Result<Value> {
    Ok(json!({ "item": get_item_detail(conn, item_id)? }))
}

pub(crate) fn get_item_detail(conn: &Connection, item_id: i64) -> Result<Option<ItemDetailRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT
            i.id,
            i.artist_id,
            i.file_path,
            i.file_name,
            i.file_size,
            i.file_mtime,
            i.folder_name,
            i.date,
            i.auto_role,
            i.manual_role,
            i.is_archive,
            i.media_type,
            i.content_hash,
            i.hash_status,
            i.hash_updated_at,
            i.st_dev,
            i.st_ino,
            i.missing,
            i.missing_at,
            i.scanned_at,
            a.name AS artist_name,
            a.path AS artist_path
        FROM items i
        JOIN artists a ON a.id = i.artist_id
        WHERE i.id=?
        ",
    )?;
    let mut rows = stmt.query([item_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    let item = ItemDetailRow {
        id: row.get("id")?,
        artist_id: row.get("artist_id")?,
        file_path: row.get("file_path")?,
        file_name: row.get("file_name")?,
        file_size: row.get("file_size")?,
        file_mtime: row.get("file_mtime")?,
        folder_name: row.get("folder_name")?,
        date: row.get("date")?,
        auto_role: row.get("auto_role")?,
        manual_role: row.get("manual_role")?,
        tags: list_item_detail_tags(conn, item_id)?,
        is_archive: row.get("is_archive")?,
        media_type: row.get("media_type")?,
        content_hash: row.get("content_hash")?,
        hash_status: row.get("hash_status")?,
        hash_updated_at: row.get("hash_updated_at")?,
        st_dev: row.get("st_dev")?,
        st_ino: row.get("st_ino")?,
        missing: row.get("missing")?,
        missing_at: row.get("missing_at")?,
        scanned_at: row.get("scanned_at")?,
        artist_name: row.get("artist_name")?,
        artist_path: row.get("artist_path")?,
    };
    Ok(Some(item))
}
