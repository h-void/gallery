use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::folder_paths::{get_artist_path, list_folder_file_paths};
use crate::folder_tree::{folder_tree_from_paths, new_folder_node};

pub fn folders_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    let Some(artist_path) = get_artist_path(conn, artist_id)? else {
        return Ok(json!(new_folder_node("", "全部文件夹", 0)));
    };
    let file_paths = list_folder_file_paths(conn, artist_id)?;
    Ok(json!(folder_tree_from_paths(&artist_path, file_paths)))
}
