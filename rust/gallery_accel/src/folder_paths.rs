use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn folder_paths_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    let artist_path = get_artist_path(conn, artist_id)?;
    let file_paths = if artist_path.is_some() {
        list_folder_file_paths(conn, artist_id)?
    } else {
        Vec::new()
    };
    Ok(json!({
        "artist_path": artist_path,
        "file_paths": file_paths,
    }))
}

pub(crate) fn list_folder_file_paths(conn: &Connection, artist_id: i64) -> Result<Vec<String>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT file_path FROM items
        WHERE artist_id=? AND missing=0
          AND (media_type IN ('image', 'video', 'source', 'archive', 'text') OR is_archive=1)
        ORDER BY file_path
        ",
    )?;
    let paths = stmt
        .query_map([artist_id], |row| row.get::<_, String>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(paths)
}

pub(crate) fn get_artist_path(conn: &Connection, artist_id: i64) -> Result<Option<String>> {
    let mut stmt = conn.prepare_cached("SELECT path FROM artists WHERE id=?")?;
    match stmt.query_row([artist_id], |row| row.get::<_, String>(0)) {
        Ok(path) => Ok(Some(path)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(error) => Err(error.into()),
    }
}
