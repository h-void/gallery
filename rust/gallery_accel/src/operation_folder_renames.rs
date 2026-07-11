use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Map, Value};

use crate::media_roots::MediaRoots;
use crate::operation_helpers::{
    operation_empty_folders, operation_entry_f64, operation_entry_i64, operation_entry_string,
    operation_entry_string_list, operation_execution_entries,
};
use crate::path_display::display_path;

pub(crate) fn operation_folder_rename_history(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
    artist_names: &HashMap<i64, String>,
) -> Result<Vec<Value>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, artist_id, source_folder, target_folder, status, executed_at, execution_log, plan_kind
        FROM folder_rename_plans
        WHERE executed_at IS NOT NULL OR execution_log != '[]'
        ORDER BY COALESCE(executed_at, updated_at, created_at) DESC, id DESC
        LIMIT ?
        ",
    )?;
    let rows = stmt.query_map([limit], |row| {
        Ok((
            row.get::<_, i64>("id")?,
            row.get::<_, i64>("artist_id")?,
            row.get::<_, Option<String>>("source_folder")?
                .unwrap_or_default(),
            row.get::<_, Option<String>>("target_folder")?
                .unwrap_or_default(),
            row.get::<_, Option<String>>("status")?.unwrap_or_default(),
            row.get::<_, Option<f64>>("executed_at")?,
            row.get::<_, Option<String>>("execution_log")?
                .unwrap_or_default(),
            row.get::<_, Option<String>>("plan_kind")?
                .unwrap_or_default(),
        ))
    })?;
    let mut history = Vec::new();
    for row in rows {
        let (
            id,
            artist_id,
            source_folder,
            target_folder,
            status,
            executed_at,
            execution_log,
            plan_kind,
        ) = row?;
        let mut entries = operation_execution_entries(&execution_log);
        if entries.is_empty() {
            entries.push(Map::new());
        }
        for (index, entry) in entries.iter().enumerate() {
            let source = operation_entry_string(entry.get("source"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| source_folder.clone());
            let target = operation_entry_string(entry.get("target"))
                .filter(|value| !value.is_empty())
                .unwrap_or_else(|| target_folder.clone());
            let at = operation_entry_f64(entry.get("at"))
                .or(executed_at)
                .unwrap_or(0.0);
            let updated_items = operation_entry_i64(entry.get("updated_items")).unwrap_or(0);
            let display_source = if source.is_empty() {
                source_folder.clone()
            } else {
                display_path(&source, roots)
            };
            let display_target = if target.is_empty() {
                target_folder.clone()
            } else {
                display_path(&target, roots)
            };
            let entry_status = operation_entry_string(entry.get("status")).unwrap_or_default();
            let entry_reason = operation_entry_string(entry.get("reason")).unwrap_or_default();
            let event_status = if !entry_status.is_empty() {
                entry_status
            } else if !status.is_empty() {
                status.clone()
            } else {
                "executed".to_string()
            };
            let reason = if !entry_reason.is_empty() {
                entry_reason
            } else if plan_kind.is_empty() {
                "folder_rename".to_string()
            } else {
                plan_kind.clone()
            };
            let message = operation_entry_string(entry.get("message"))
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    let code = reason.as_str();
                    Some(crate::folder_archive::archive_failure_message(code).to_string())
                        .filter(|_| {
                            matches!(
                                code,
                                "backup_failed"
                                    | "source_missing"
                                    | "target_exists"
                                    | "bad_folder_path"
                                    | "db_update_failed"
                                    | "outside_artist"
                                    | "execution_failed"
                            )
                        })
                })
                .unwrap_or_default();
            history.push(json!({
                "id": format!("folder_rename:{id}:{index}"),
                "kind": "folder_rename",
                "status": event_status,
                "at": at,
                "artist_id": artist_id,
                "artist_name": artist_names.get(&artist_id).cloned().unwrap_or_default(),
                "source": source,
                "target": target,
                "display_source": display_source,
                "display_target": display_target,
                "target_folders": operation_entry_string_list(entry.get("targets")),
                "reason": reason,
                "message": message,
                "plan_id": id,
                "updated_items": updated_items,
                "backup": operation_entry_string(entry.get("backup")).unwrap_or_default(),
                "empty_folders": operation_empty_folders(entry.get("empty_folders")),
            }));
        }
    }
    Ok(history)
}
