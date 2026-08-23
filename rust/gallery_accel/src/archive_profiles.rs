use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

use crate::archive_format::{self, RenderContext};
use crate::folder_archive::{recompute_artist_plan_targets, validate_relative_folder};
use crate::media_roots::{path_under_authorized_roots, MediaRoots};

fn target_key(target: &str) -> String {
    target.to_ascii_lowercase()
}

fn suffixed_target(target: &str, number: usize) -> String {
    match target.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/{name} ({number})"),
        None => format!("{target} ({number})"),
    }
}

fn artist_relative_path(artist_root: &PathBuf, value: &str) -> Result<PathBuf> {
    let relative = validate_relative_folder(value)?;
    let path = artist_root.join(relative.replace('/', std::path::MAIN_SEPARATOR_STR));
    let mut existing = path.clone();
    while !existing.exists() {
        if !existing.pop() {
            return Err(anyhow!("folder path is outside artist root"));
        }
    }
    let canonical = existing.canonicalize()?;
    if !canonical.starts_with(artist_root) {
        return Err(anyhow!("folder path escapes artist root"));
    }
    Ok(path)
}

fn available_target(
    artist_root: &PathBuf,
    target: &str,
    source_key: &str,
    seen_targets: &HashSet<String>,
) -> Result<bool> {
    let key = target_key(target);
    Ok(key != source_key
        && !seen_targets.contains(&key)
        && !artist_relative_path(artist_root, target)?.exists())
}

pub(crate) fn load(conn: &Connection) -> Result<Value> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key=?",
            [archive_format::ARCHIVE_FORMAT_SETTINGS_KEY],
            |row| row.get(0),
        )
        .optional()?;
    let Some(raw) = raw else {
        return Ok(archive_format::default_settings());
    };
    let value: Value = serde_json::from_str(&raw)?;
    archive_format::normalize_settings(&value)
}

pub fn folder_rename_format_settings(conn: &Connection, artist_id: Option<i64>) -> Result<Value> {
    let settings = load(conn)?;
    archive_format::settings_response(&settings, artist_id)
}

pub fn set_folder_rename_format_settings(
    conn: &Connection,
    value: &Value,
    artist_id: Option<i64>,
) -> Result<Value> {
    let normalized = archive_format::normalize_settings(value)?;
    conn.execute(
        "INSERT INTO app_settings(key,value,updated_at) VALUES(?,?,strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![
            archive_format::ARCHIVE_FORMAT_SETTINGS_KEY,
            normalized.to_string()
        ],
    )?;
    archive_format::settings_response(&normalized, artist_id)
}

pub fn preview_folder_rename_template(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    plan_ids: Option<&[i64]>,
    profile_id: Option<&str>,
    template: Option<&str>,
    index_start: Option<usize>,
) -> Result<Value> {
    let settings = load(conn)?;
    let artist_path: String =
        conn.query_row("SELECT path FROM artists WHERE id=?", [artist_id], |row| {
            row.get::<_, String>(0)
        })?;
    let artist_root = roots
        .map_to_real(&artist_path)?
        .canonicalize()
        .map_err(|error| anyhow!("artist path is unavailable: {error}"))?;
    if !artist_root.is_dir() || !path_under_authorized_roots(&artist_root, roots) {
        return Err(anyhow!("artist path is outside configured media roots"));
    }
    let (profile, source) = if let Some(template) = template {
        let mut profile = settings["profiles"][0].clone();
        profile["template"] = Value::String(template.to_string());
        (
            archive_format::normalize_profile(&profile)?,
            "inline".to_string(),
        )
    } else if let Some(profile_id) = profile_id {
        (
            archive_format::profile_by_id(&settings, profile_id)?,
            format!("profile:{profile_id}"),
        )
    } else {
        archive_format::profile_for_artist(&settings, artist_id)?
    };
    let selected = plan_ids
        .unwrap_or_default()
        .iter()
        .map(i64::to_string)
        .collect::<Vec<_>>()
        .join(",");
    let sql = if selected.is_empty() {
        "SELECT p.id,p.source_folder,p.original_title,p.parsed_date,a.name,p.selected_tag_ids,p.status
         FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
         WHERE p.artist_id=? ORDER BY p.id"
    } else {
        "SELECT p.id,p.source_folder,p.original_title,p.parsed_date,a.name,p.selected_tag_ids,p.status
         FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
         WHERE p.artist_id=? AND p.id IN (SELECT value FROM json_each(?)) ORDER BY p.id"
    };
    let mut stmt = conn.prepare(sql)?;
    let mut rows = if selected.is_empty() {
        stmt.query(params![artist_id])?
    } else {
        stmt.query(params![artist_id, format!("[{selected}]")])?
    };
    let mut previews = Vec::new();
    let mut seen_targets = HashSet::new();
    let mut conflicts = Vec::new();
    let suffix_collisions = profile["collision_strategy"].as_str() == Some("suffix");
    let merge_targets = profile["collision_strategy"].as_str() == Some("merge");
    let mut index = index_start.unwrap_or(1).max(1);
    while let Some(row) = rows.next()? {
        let id: i64 = row.get(0)?;
        let folder: String = row.get(1)?;
        let title: String = row.get(2)?;
        let mut date: String = row.get(3)?;
        let artist: String = row.get(4)?;
        let mut tags: Vec<String> = row
            .get::<_, String>(5)
            .ok()
            .and_then(|raw| serde_json::from_str::<Vec<i64>>(&raw).ok())
            .unwrap_or_default()
            .into_iter()
            .filter_map(|id| {
                conn.query_row("SELECT name FROM tags WHERE id=?", [id], |r| r.get(0))
                    .optional()
                    .ok()
                    .flatten()
            })
            .filter(|name: &String| !name.is_empty())
            .collect();
        let plan_status: String = row.get(6)?;
        if date.is_empty() {
            date = crate::media_type::extract_date_from_folder(&folder);
        }
        if tags.is_empty() {
            if let Ok(mut stmt) = conn.prepare(
                "SELECT DISTINCT t.name FROM items i
                 JOIN item_tags it ON it.item_id=i.id JOIN tags t ON t.id=it.tag_id
                 WHERE i.artist_id=? AND i.folder_name=? AND COALESCE(i.missing, 0)=0
                 ORDER BY t.name",
            ) {
                if let Ok(item_tags) = stmt.query_map(params![artist_id, folder], |r| r.get(0)) {
                    tags = item_tags.filter_map(|r| r.ok()).collect();
                }
            }
        }
        let rendered = archive_format::render_profile(
            &profile,
            &RenderContext {
                artist,
                date,
                tags,
                title,
                folder: folder.clone(),
                index,
            },
        )?;
        let requested_target = rendered.target_folder.clone();
        let source_key = folder.to_ascii_lowercase();
        let mut target = requested_target.clone();
        if suffix_collisions && target_key(&target) != source_key {
            let mut number = 2;
            while !available_target(&artist_root, &target, &source_key, &seen_targets)? {
                target = suffixed_target(&requested_target, number);
                number += 1;
            }
        }
        let target_key = target_key(&target);
        let source_path = artist_relative_path(&artist_root, &folder)?;
        let target_path = artist_relative_path(&artist_root, &target)?;
        let mut row_conflicts = Vec::new();
        if plan_status == "inconsistent_tags" {
            row_conflicts.push(json!({"code":"inconsistent_tags", "source_folder": folder}));
        }
        if !source_path.is_dir() {
            row_conflicts.push(json!({"code":"source_missing", "source_folder": folder}));
        }
        if target_key == source_key && !merge_targets {
            row_conflicts.push(json!({"code":"same_as_source", "target_folder": target}));
        } else if target_path.exists() && !(merge_targets && target_path.is_dir()) {
            row_conflicts.push(json!({"code":"target_exists", "target_folder": target}));
        }
        if seen_targets.contains(&target_key) && !merge_targets {
            row_conflicts.push(json!({"code":"duplicate_target", "target_folder": target}));
        }
        if target_key.starts_with(&(source_key.clone() + "/")) {
            row_conflicts.push(json!({"code":"target_inside_source", "target_folder": target}));
        }
        seen_targets.insert(target_key);
        for conflict in &row_conflicts {
            conflicts.push(json!({"plan_id": id, "code": conflict["code"], "detail": conflict}));
        }
        // Apply skips plans whose status is no longer editable; surface that
        // here so the preview matches what apply will actually change.
        let will_apply = !matches!(
            plan_status.as_str(),
            "confirmed" | "executed" | "inconsistent_tags"
        );
        previews.push(json!({"id": id, "source_folder": folder, "target_folder": target, "tokens": rendered.tokens, "format_source": source, "status": plan_status, "will_apply": will_apply, "conflicts": row_conflicts, "can_apply": false}));
        index += 1;
    }
    let can_apply = conflicts.is_empty();
    for row in &mut previews {
        let blocked = row
            .get("conflicts")
            .and_then(Value::as_array)
            .is_some_and(|items| !items.is_empty());
        if let Some(object) = row.as_object_mut() {
            object.insert("can_apply".into(), Value::Bool(!blocked && can_apply));
        }
    }
    Ok(json!({
        "ok": can_apply,
        "artist_id": artist_id,
        "profile": profile,
        "format_source": source,
        "plans": previews,
        "conflicts": conflicts,
        "can_apply": can_apply,
    }))
}

pub fn apply_folder_rename_template(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    plan_ids: Option<&[i64]>,
    profile_id: Option<&str>,
    template: Option<&str>,
    index_start: Option<usize>,
) -> Result<Value> {
    let preview = preview_folder_rename_template(
        conn,
        roots,
        artist_id,
        plan_ids,
        profile_id,
        template,
        index_start,
    )?;
    let Some(plans) = preview.get("plans").and_then(Value::as_array) else {
        return Err(anyhow!("preview failed"));
    };
    if preview.get("can_apply").and_then(Value::as_bool) != Some(true) {
        let conflicts = preview
            .get("conflicts")
            .cloned()
            .unwrap_or_else(|| json!([]));
        return Ok(json!({"ok": false, "applied": 0, "preview": preview, "conflicts": conflicts}));
    }
    conn.execute("BEGIN IMMEDIATE", [])?;
    let mut updated = 0i64;
    for plan in plans {
        let snapshot = archive_format::rule_snapshot(
            &preview["profile"],
            preview["format_source"].as_str().unwrap_or(""),
        );
        let target_str = plan["target_folder"].as_str().unwrap_or_default();
        let plan_status = if target_str.is_empty() {
            "draft"
        } else {
            "ready"
        };
        match conn.execute(
            "UPDATE folder_rename_plans
             SET target_folder=?, format_snapshot=?, status=?, confirmed_at=NULL,
                 confirmation_source='', updated_at=strftime('%s','now')
             WHERE id=? AND artist_id=? AND status NOT IN ('confirmed','executed','inconsistent_tags')",
            params![
                target_str,
                snapshot.to_string(),
                plan_status,
                plan["id"].as_i64().unwrap_or_default(),
                artist_id
            ],
        ) {
            Ok(count) => updated += count as i64,
            Err(error) => {
                let _ = conn.execute("ROLLBACK", []);
                return Err(error.into());
            }
        }
    }
    conn.execute("COMMIT", [])?;
    if preview["profile"]["collision_strategy"].as_str() == Some("merge") {
        recompute_artist_plan_targets(conn, Some(roots), artist_id)?;
    }
    // `updated` counts only rows the UPDATE actually changed; plans skipped by
    // their status are reported separately so the UI never overcounts.
    let skipped = plans.len() as i64 - updated;
    Ok(json!({"ok": true, "updated": updated, "skipped": skipped, "preview": preview}))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn fixture() -> (Connection, tempfile::TempDir, std::path::PathBuf) {
        let dir = tempdir().unwrap();
        let root = dir.path().join("artist");
        std::fs::create_dir_all(root.join("old")).unwrap();
        std::fs::create_dir_all(root.join("taken")).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT); CREATE TABLE tags (id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, folder_name TEXT, manual_date TEXT, detected_date TEXT, date TEXT, missing INTEGER DEFAULT 0); CREATE TABLE app_settings (key TEXT PRIMARY KEY, value TEXT, updated_at REAL); CREATE TABLE folder_rename_plans (id INTEGER PRIMARY KEY, artist_id INTEGER, source_folder TEXT, original_title TEXT, parsed_date TEXT, selected_tag_ids TEXT, target_folder TEXT DEFAULT '', status TEXT DEFAULT 'draft', format_snapshot TEXT DEFAULT '{}', confirmed_at REAL, confirmation_source TEXT DEFAULT '', execution_log TEXT DEFAULT '[]', plan_kind TEXT DEFAULT 'rename_folder', split_actions TEXT DEFAULT '[]', updated_at REAL);").unwrap();
        conn.execute(
            "INSERT INTO artists VALUES (1,'A',?)",
            [root.to_string_lossy().to_string()],
        )
        .unwrap();
        conn.execute("INSERT INTO folder_rename_plans (id,artist_id,source_folder,original_title,parsed_date,selected_tag_ids) VALUES (1,1,'old','taken','2026-01-01','[]')", []).unwrap();
        (conn, dir, root)
    }

    #[test]
    fn preview_reports_real_target_conflicts_and_apply_stays_pending() {
        let (conn, _dir, root) = fixture();
        let settings = json!({"version":1,"active_profile_id":"flat","profiles":[{"id":"flat","name":"Flat","template":"{title}","collision_strategy":"reject"}],"artist_profile_ids":{}});
        set_folder_rename_format_settings(&conn, &settings, None).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["root".into()],
        );
        let preview =
            preview_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None)
                .unwrap();
        assert_eq!(preview["can_apply"], false);
        assert_eq!(preview["conflicts"][0]["code"], "target_exists");
        let applied =
            apply_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None).unwrap();
        assert_eq!(applied["applied"], 0);
        assert_eq!(
            conn.query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            ""
        );
    }

    #[test]
    fn suffix_strategy_numbers_existing_and_batch_targets() {
        let (conn, _dir, root) = fixture();
        std::fs::create_dir_all(root.join("other")).unwrap();
        conn.execute("INSERT INTO folder_rename_plans (id,artist_id,source_folder,original_title,parsed_date,selected_tag_ids) VALUES (2,1,'other','taken','2026-01-01','[]')", []).unwrap();
        let settings = json!({"version":1,"active_profile_id":"flat","profiles":[{"id":"flat","name":"Flat","template":"{title}","collision_strategy":"suffix"}],"artist_profile_ids":{}});
        set_folder_rename_format_settings(&conn, &settings, None).unwrap();

        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["root".into()],
        );
        let preview =
            preview_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None)
                .unwrap();
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["plans"][0]["target_folder"], "taken (2)");
        assert_eq!(preview["plans"][1]["target_folder"], "taken (3)");
        let applied =
            apply_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None).unwrap();
        assert_eq!(applied["updated"], 2);
        assert_eq!(
            conn.query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE id=2",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
            "taken (3)"
        );
    }

    #[test]
    fn merge_strategy_allows_existing_directory_and_duplicate_targets() {
        let (conn, _dir, root) = fixture();
        std::fs::create_dir_all(root.join("other")).unwrap();
        conn.execute("INSERT INTO folder_rename_plans (id,artist_id,source_folder,original_title,parsed_date,selected_tag_ids) VALUES (2,1,'other','taken','2026-01-01','[]')", []).unwrap();
        conn.execute("INSERT INTO folder_rename_plans (id,artist_id,source_folder,original_title,parsed_date,selected_tag_ids) VALUES (3,1,'taken','taken','2026-01-01','[]')", []).unwrap();
        let settings = json!({"version":1,"active_profile_id":"flat","profiles":[{"id":"flat","name":"Flat","template":"{title}","collision_strategy":"merge"}],"artist_profile_ids":{}});
        set_folder_rename_format_settings(&conn, &settings, None).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["root".into()],
        );

        let preview =
            preview_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None)
                .unwrap();
        assert_eq!(preview["can_apply"], true);
        assert_eq!(preview["plans"][0]["target_folder"], "taken");
        assert_eq!(preview["plans"][1]["target_folder"], "taken");
        assert_eq!(preview["plans"][2]["target_folder"], "taken");
        let applied =
            apply_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None).unwrap();
        assert_eq!(applied["updated"], 3);

        std::fs::remove_dir_all(root.join("taken")).unwrap();
        std::fs::write(root.join("taken"), b"occupied").unwrap();
        let preview =
            preview_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None)
                .unwrap();
        assert_eq!(preview["can_apply"], false);
        assert!(preview["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|conflict| conflict["code"] == "target_exists"));
    }

    #[test]
    fn preview_rejects_unsafe_plan_folder_without_updating_plans() {
        let (conn, dir, root) = fixture();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        conn.execute(
            "UPDATE folder_rename_plans SET source_folder='../outside' WHERE id=1",
            [],
        )
        .unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["root".into()],
        );

        assert!(preview_folder_rename_template(&conn, &roots, 1, None, None, None, None).is_err());
        assert_eq!(
            conn.query_row(
                "SELECT target_folder FROM folder_rename_plans WHERE id=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .unwrap(),
            ""
        );
    }

    #[test]
    fn preview_reports_inconsistent_tags_conflict() {
        let (conn, _dir, root) = fixture();
        conn.execute(
            "UPDATE folder_rename_plans SET status='inconsistent_tags' WHERE id=1",
            [],
        )
        .unwrap();
        let settings = json!({"version":1,"active_profile_id":"flat","profiles":[{"id":"flat","name":"Flat","template":"{title}","collision_strategy":"reject"}],"artist_profile_ids":{}});
        set_folder_rename_format_settings(&conn, &settings, None).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["root".into()],
        );
        let preview =
            preview_folder_rename_template(&conn, &roots, 1, None, Some("flat"), None, None)
                .unwrap();
        assert_eq!(preview["can_apply"], false);
        assert!(preview["conflicts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c["code"] == "inconsistent_tags"));
    }
}
