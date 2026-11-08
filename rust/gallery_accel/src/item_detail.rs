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
    pub(crate) detected_date: String,
    pub(crate) manual_date: Option<String>,
    pub(crate) display_date: String,
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
    pub(crate) favorite: bool,
    pub(crate) artist_name: String,
    pub(crate) artist_path: String,
    /// Intrinsic pixel size; 0 means "unknown" and the grid falls back to 4:3.
    pub(crate) width: i64,
    pub(crate) height: i64,
    /// True when the item is an archive eligible for batch extraction
    /// (excludes secondary volumes and passwordless `.7z` archives).
    pub(crate) extractable_archive: bool,
}

pub fn item_detail_response(conn: &Connection, item_id: i64) -> Result<Value> {
    Ok(json!({ "item": get_item_detail(conn, item_id)? }))
}

pub(crate) fn get_item_detail(conn: &Connection, item_id: i64) -> Result<Option<ItemDetailRow>> {
    let mut stmt = conn.prepare_cached(
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
            i.detected_date,
            i.manual_date,
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
            i.width,
            i.height,
            EXISTS(SELECT 1 FROM item_favorites f WHERE f.item_id=i.id) AS favorite,
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
    let item = {
        let file_path: String = row.get(2)?;
        let file_name: String = row.get(3)?;
        let file_size: i64 = row.get(4)?;
        let file_mtime: f64 = row.get(5)?;
        let legacy_date: String = row.get(7)?;
        let detected_date: String = row.get(8)?;
        let manual_date: Option<String> = row.get(9)?;
        let display_date =
            effective_display_date(&detected_date, manual_date.as_deref(), &legacy_date);
        let is_archive: i64 = row.get(12)?;
        let media_type: String = row.get(13)?;
        let extractable_archive = if is_archive != 0 || media_type == "archive" {
            crate::archive_ops::is_extractable_archive(
                &file_path, &file_name, file_size, file_mtime,
            )
        } else {
            false
        };
        ItemDetailRow {
            id: row.get(0)?,
            artist_id: row.get(1)?,
            file_path,
            file_name,
            file_size,
            file_mtime,
            folder_name: row.get(6)?,
            date: legacy_date,
            detected_date,
            manual_date,
            display_date,
            auto_role: row.get(10)?,
            manual_role: row.get(11)?,
            tags: list_item_detail_tags(conn, item_id)?,
            is_archive,
            media_type,
            content_hash: row.get(14)?,
            hash_status: row.get(15)?,
            hash_updated_at: row.get(16)?,
            st_dev: row.get(17)?,
            st_ino: row.get(18)?,
            missing: row.get(19)?,
            missing_at: row.get(20)?,
            scanned_at: row.get(21)?,
            width: row.get(22).unwrap_or(0),
            height: row.get(23).unwrap_or(0),
            favorite: row.get(24)?,
            artist_name: row.get(25)?,
            artist_path: row.get(26)?,
            extractable_archive,
        }
    };
    Ok(Some(item))
}

/// The effective raw date shown to users: `manual_date` when set, otherwise
/// `detected_date`, falling back to the legacy canonical `date` only when
/// neither source is available.
pub(crate) fn effective_display_date(
    detected_date: &str,
    manual_date: Option<&str>,
    legacy_date: &str,
) -> String {
    manual_date
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .or_else(|| {
            if detected_date.is_empty() {
                None
            } else {
                Some(detected_date.to_string())
            }
        })
        .unwrap_or_else(|| legacy_date.to_string())
}
