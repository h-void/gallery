use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn folder_rename_auto_response(conn: &Connection) -> Result<Value> {
    crate::folder_archive::purge_folder_rename_auto_last_run(conn)?;
    let enabled = crate::folder_archive::folder_rename_auto_enabled(conn)?;
    Ok(json!({
        "enabled": enabled,
    }))
}
