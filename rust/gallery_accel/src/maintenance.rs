use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn folder_rename_auto_response(conn: &Connection) -> Result<Value> {
    // Pure read: no purge, no settings rewrite. Obsolete-key cleanup belongs
    // to writable startup (ensure_folder_schema) and explicit write routes.
    let enabled = crate::folder_archive::folder_rename_auto_enabled(conn)?;
    Ok(json!({
        "enabled": enabled,
    }))
}
