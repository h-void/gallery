use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn folder_rename_auto_response(conn: &Connection) -> Result<Value> {
    let query_only = conn.query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))? != 0;
    if !query_only {
        crate::folder_archive::purge_folder_rename_auto_last_run(conn)?;
    }
    let enabled = crate::folder_archive::folder_rename_auto_enabled(conn)?;
    Ok(json!({
        "enabled": enabled,
    }))
}
