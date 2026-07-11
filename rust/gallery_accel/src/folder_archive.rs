//! Folder archive plan list + execute (pure Rust product path).

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Reject absolute paths and traversal segments for artist-relative folders.
pub(crate) fn validate_relative_folder(folder: &str) -> Result<String> {
    let raw = folder.replace('\\', "/").trim().to_string();
    if raw.is_empty() {
        return Err(anyhow!("Bad folder path"));
    }
    if raw.starts_with('/') || raw.starts_with('\\') {
        return Err(anyhow!("Bad folder path"));
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        return Err(anyhow!("Bad folder path"));
    }
    if raw.starts_with("//") || raw.starts_with("\\\\") {
        return Err(anyhow!("Bad folder path"));
    }
    let mut parts = Vec::new();
    for part in raw.trim_matches('/').split('/') {
        if part.is_empty() {
            continue;
        }
        // Reject "." and ".." explicitly (do not silently strip).
        if part == "." || part == ".." {
            return Err(anyhow!("Bad folder path"));
        }
        parts.push(part);
    }
    if parts.is_empty() {
        return Err(anyhow!("Bad folder path"));
    }
    Ok(parts.join("/"))
}

fn path_under_artist(path: &Path, artist: &Path) -> bool {
    let path = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let artist = artist
        .canonicalize()
        .unwrap_or_else(|_| artist.to_path_buf());
    path.starts_with(&artist)
}

pub fn ensure_folder_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS folder_rename_plans (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL,
            source_folder TEXT NOT NULL,
            original_folder_name TEXT NOT NULL DEFAULT '',
            original_title TEXT NOT NULL DEFAULT '',
            parsed_date TEXT NOT NULL DEFAULT '',
            selected_tag_ids TEXT NOT NULL DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'needs_tags',
            file_count INTEGER NOT NULL DEFAULT 0,
            total_size INTEGER NOT NULL DEFAULT 0,
            max_mtime REAL NOT NULL DEFAULT 0,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            confirmed_at REAL,
            confirmation_source TEXT NOT NULL DEFAULT '',
            target_folder TEXT NOT NULL DEFAULT '',
            executed_at REAL,
            execution_log TEXT NOT NULL DEFAULT '[]',
            plan_kind TEXT NOT NULL DEFAULT 'rename_folder',
            split_actions TEXT NOT NULL DEFAULT '[]',
            UNIQUE(artist_id, source_folder)
        );
        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        ",
    )?;
    // Drop obsolete auto-archive summary only; plans and media paths stay untouched.
    purge_folder_rename_auto_last_run(conn)?;
    Ok(())
}

pub fn purge_folder_rename_auto_last_run(conn: &Connection) -> Result<()> {
    conn.execute(
        "DELETE FROM app_settings WHERE key='folder_rename_auto_last_run'",
        [],
    )?;
    Ok(())
}

pub(crate) fn archive_failure_message(reason: &str) -> &'static str {
    match reason {
        "backup_failed" => "数据库备份失败",
        "source_missing" => "来源文件夹不存在",
        "target_exists" => "目标已存在",
        "bad_folder_path" => "文件夹路径无效",
        "db_update_failed" => "数据库路径更新失败",
        "outside_artist" => "路径不在画师目录内",
        "execution_failed" => "执行失败",
        _ => "归档失败",
    }
}

/// Record a failed attempt without marking the plan executed so it stays retriable.
pub(crate) fn record_plan_execution_failure(
    conn: &Connection,
    plan_id: i64,
    reason: &str,
    source: &str,
    target: &str,
    extra: Option<Value>,
) -> Result<()> {
    let mut entry = json!({
        "at": now(),
        "status": "failed",
        "reason": reason,
        "message": archive_failure_message(reason),
        "source": source,
        "target": target,
        "automatic": true,
    });
    if let Some(Value::Object(map)) = extra {
        if let Some(object) = entry.as_object_mut() {
            for (key, value) in map {
                object.insert(key, value);
            }
        }
    }
    conn.execute(
        "UPDATE folder_rename_plans SET execution_log=?, updated_at=? WHERE id=?",
        params![json!([entry]).to_string(), now(), plan_id],
    )?;
    Ok(())
}

pub fn list_folder_renames(conn: &Connection, artist_id: Option<i64>) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let mut sql = String::from(
        "SELECT id, artist_id, source_folder, target_folder, status, plan_kind, file_count,
                selected_tag_ids, parsed_date, execution_log, confirmed_at, executed_at
         FROM folder_rename_plans",
    );
    let mut plans = Vec::new();
    if let Some(aid) = artist_id {
        sql.push_str(" WHERE artist_id=? ORDER BY id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![aid], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    } else {
        sql.push_str(" ORDER BY id DESC LIMIT 500");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
    }
    Ok(json!({"plans": plans, "total": plans.len()}))
}

fn map_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    Ok(json!({
        "id": r.get::<_, i64>(0)?,
        "artist_id": r.get::<_, i64>(1)?,
        "source_folder": r.get::<_, String>(2)?,
        "target_folder": r.get::<_, String>(3)?,
        "status": r.get::<_, String>(4)?,
        "plan_kind": r.get::<_, String>(5)?,
        "file_count": r.get::<_, i64>(6)?,
        "selected_tag_ids": r.get::<_, String>(7)?,
        "parsed_date": r.get::<_, String>(8)?,
        "execution_log": r.get::<_, String>(9)?,
        "confirmed_at": r.get::<_, Option<f64>>(10)?,
        "executed_at": r.get::<_, Option<f64>>(11)?,
    }))
}

pub fn upsert_folder_rename_plans(
    conn: &Connection,
    artist_id: i64,
    plans: &[Value],
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let mut upserted = 0i64;
    for plan in plans {
        let source_raw = plan
            .get("source_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if source_raw.is_empty() {
            continue;
        }
        let source = validate_relative_folder(source_raw)
            .with_context(|| format!("invalid source_folder {source_raw:?}"))?;
        let target_raw = plan
            .get("target_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let target = if target_raw.is_empty() {
            String::new()
        } else {
            validate_relative_folder(target_raw)
                .with_context(|| format!("invalid target_folder {target_raw:?}"))?
        };
        let status = plan
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("needs_tags");
        let tags = plan
            .get("selected_tag_ids")
            .map(|v| v.to_string())
            .unwrap_or_else(|| "[]".into());
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status, selected_tag_ids, updated_at)
             VALUES (?,?,?,?,?,?)
             ON CONFLICT(artist_id, source_folder) DO UPDATE SET
               target_folder=excluded.target_folder,
               status=excluded.status,
               selected_tag_ids=excluded.selected_tag_ids,
               updated_at=excluded.updated_at",
            params![artist_id, source, target, status, tags, now()],
        )?;
        upserted += 1;
    }
    Ok(json!({"ok": true, "upserted": upserted}))
}

pub fn set_folder_rename_auto(conn: &Connection, enabled: bool) -> Result<Value> {
    ensure_folder_schema(conn)?;
    conn.execute(
        "INSERT INTO app_settings(key, value, updated_at) VALUES('folder_rename_auto_enabled', ?, ?)
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        params![if enabled { "1" } else { "0" }, now()],
    )?;
    conn.execute(
        "DELETE FROM app_settings WHERE key='folder_rename_auto'",
        [],
    )?;
    purge_folder_rename_auto_last_run(conn)?;
    Ok(json!({"enabled": enabled}))
}

pub fn folder_rename_auto_enabled(conn: &Connection) -> Result<bool> {
    ensure_folder_schema(conn)?;
    let value: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='folder_rename_auto_enabled'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(value) = value {
        return Ok(matches!(value.trim(), "1" | "true" | "yes" | "on"));
    }
    let legacy: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='folder_rename_auto'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    let enabled = legacy
        .as_deref()
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false);
    if legacy.is_some() {
        set_folder_rename_auto(conn, enabled)?;
    }
    Ok(enabled)
}

/// Execute confirmed plans for an artist: online SQLite backup then rename folders + update item paths.
pub fn execute_folder_renames(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    dry_run: bool,
) -> Result<Value> {
    execute_folder_renames_with_backup(conn, roots, artist_id, dry_run, None)
}

pub fn execute_folder_renames_with_backup(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    dry_run: bool,
    backup_override: Option<&str>,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let artist_path: String = conn.query_row(
        "SELECT path FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let artist_root = PathBuf::from(&artist_path);
    let plans: Vec<(i64, String, String)> = conn
        .prepare(
            "SELECT id, source_folder, target_folder FROM folder_rename_plans
             WHERE artist_id=? AND status IN ('confirmed','ready') AND target_folder != ''",
        )?
        .query_map(params![artist_id], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<rusqlite::Result<_>>()?;

    let mut backup_path = backup_override.unwrap_or("").to_string();
    if !dry_run && !plans.is_empty() && backup_path.is_empty() {
        backup_path = create_db_backup(conn)?;
    }

    let mut executed = Vec::new();
    for (id, source_raw, target_raw) in plans {
        let source = match validate_relative_folder(&source_raw) {
            Ok(v) => v,
            Err(_) => {
                if !dry_run {
                    let _ = record_plan_execution_failure(
                        conn,
                        id,
                        "bad_folder_path",
                        &source_raw,
                        &target_raw,
                        None,
                    );
                }
                executed
                    .push(json!({"plan_id": id, "status": "error", "reason": "bad_folder_path"}));
                continue;
            }
        };
        let target = match validate_relative_folder(&target_raw) {
            Ok(v) => v,
            Err(_) => {
                if !dry_run {
                    let _ = record_plan_execution_failure(
                        conn, id, "bad_folder_path", &source, &target_raw, None,
                    );
                }
                executed
                    .push(json!({"plan_id": id, "status": "error", "reason": "bad_folder_path"}));
                continue;
            }
        };
        let src = artist_root.join(&source);
        let dst = artist_root.join(&target);
        let src_s = src.to_string_lossy().replace('\\', "/");
        let dst_s = dst.to_string_lossy().replace('\\', "/");

        // Revalidate: source exists, target free.
        if !src.is_dir() {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn, id, "source_missing", &source, &target, None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "source_missing"}));
            continue;
        }
        if dst.exists() {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn, id, "target_exists", &source, &target, None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "target_exists"}));
            continue;
        }
        // Safety: stay under artist path.
        if !path_under_artist(&src, &artist_root)
            || !path_under_artist(dst.parent().unwrap_or(&dst), &artist_root)
        {
            if !dry_run {
                let _ = record_plan_execution_failure(
                    conn, id, "outside_artist", &source, &target, None,
                );
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "outside_artist"}));
            continue;
        }
        if dry_run {
            executed.push(
                json!({"plan_id": id, "status": "dry_run", "source": source, "target": target}),
            );
            continue;
        }
        // Rename first, then DB in one transaction. On DB failure, restore folder
        // AND reverse any partial item path rewrite.
        std::fs::rename(&src, &dst).with_context(|| format!("rename {src_s} -> {dst_s}"))?;
        let db_result = (|| -> Result<()> {
            conn.execute("BEGIN IMMEDIATE", [])?;
            let update = (|| -> Result<()> {
                conn.execute(
                    "UPDATE items SET file_path = replace(file_path, ?, ?)
                     WHERE artist_id=? AND (file_path = ? OR file_path LIKE ?)",
                    params![
                        src_s,
                        dst_s,
                        artist_id,
                        src_s,
                        format!("{}/%", src_s.trim_end_matches('/'))
                    ],
                )?;
                let log = json!([{
                    "at": now(),
                    "status": "executed",
                    "source": src_s,
                    "target": dst_s,
                    "backup": backup_path,
                    "updated_items": true
                }]);
                conn.execute(
                    "UPDATE folder_rename_plans SET status='executed', executed_at=?, execution_log=?, updated_at=?
                     WHERE id=?",
                    params![now(), log.to_string(), now(), id],
                )?;
                Ok(())
            })();
            match update {
                Ok(()) => match conn.execute("COMMIT", []) {
                    Ok(_) => Ok(()),
                    Err(err) => {
                        let _ = conn.execute("ROLLBACK", []);
                        Err(err.into())
                    }
                },
                Err(err) => {
                    let _ = conn.execute("ROLLBACK", []);
                    Err(err)
                }
            }
        })();
        if let Err(err) = db_result {
            let _ = std::fs::rename(&dst, &src);
            let _ = record_plan_execution_failure(
                conn,
                id,
                "db_update_failed",
                &source,
                &target,
                Some(json!({"error": err.to_string()})),
            );
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "db_update_failed",
                "error": err.to_string(),
            }));
            continue;
        }
        executed
            .push(json!({"plan_id": id, "status": "executed", "source": source, "target": target}));
        let _ = roots;
    }
    Ok(json!({
        "ok": true,
        "dry_run": dry_run,
        "backup": backup_path,
        "results": executed
    }))
}

/// Run automatic archive only after a successful full-library scan.
/// Returns an immediate summary for the caller; does not persist a last_run summary.
pub fn run_folder_rename_auto_after_full_scan(
    conn: &Connection,
    roots: &MediaRoots,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    purge_folder_rename_auto_last_run(conn)?;
    if !folder_rename_auto_enabled(conn)? {
        return Ok(json!({
            "ok": true,
            "status": "disabled",
            "scope": "full",
            "artist_id": Value::Null,
            "at": now(),
            "reason": "disabled",
            "backup": "",
            "executed_count": 0,
            "skipped_count": 0,
            "failed_count": 0,
            "actions": [],
            "skipped": [],
            "failed": [],
            "errors": []
        }));
    }
    let artists = conn
        .prepare("SELECT id FROM artists WHERE COALESCE(missing, 0)=0 ORDER BY id")?
        .query_map([], |row| row.get::<_, i64>(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if artists.is_empty() {
        return Ok(json!({
            "ok": true,
            "status": "no_actions",
            "scope": "full",
            "artist_id": Value::Null,
            "at": now(),
            "reason": "no_artists",
            "backup": "",
            "executed_count": 0,
            "skipped_count": 0,
            "failed_count": 0,
            "actions": [],
            "skipped": [],
            "failed": [],
            "errors": []
        }));
    }
    let mut executed_count = 0i64;
    let mut skipped_count = 0i64;
    let mut failed_count = 0i64;
    let mut actions = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let mut executable = Vec::new();
    for artist_id in artists {
        let confirmed = match crate::product_ui::folder_rename_auto_run(conn, artist_id) {
            Ok(confirmed) => confirmed,
            Err(error) => {
                failed_count += 1;
                let failure = json!({"artist_id": artist_id, "error": error.to_string()});
                failed.push(failure.clone());
                errors.push(failure);
                continue;
            }
        };
        let plans: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans
             WHERE artist_id=? AND status IN ('confirmed','ready') AND target_folder != ''",
            params![artist_id],
            |row| row.get(0),
        )?;
        if plans == 0 {
            skipped_count += 1;
            continue;
        }
        executable.push((artist_id, confirmed));
    }
    let backup = if executable.is_empty() {
        String::new()
    } else {
        create_db_backup(conn)?
    };
    for (artist_id, confirmed) in executable {
        match execute_folder_renames_with_backup(conn, roots, artist_id, false, Some(&backup)) {
            Ok(executed) => {
                if let Some(rows) = executed["results"].as_array() {
                    for row in rows {
                        if row["status"] == "executed" {
                            executed_count += 1;
                            actions.push(row.clone());
                        } else {
                            failed_count += 1;
                            failed.push(row.clone());
                            errors.push(row.clone());
                        }
                    }
                }
                let _ = confirmed;
            }
            Err(error) => {
                failed_count += 1;
                let failure = json!({
                    "artist_id": artist_id,
                    "confirmed": confirmed,
                    "error": error.to_string()
                });
                failed.push(failure.clone());
                errors.push(failure);
            }
        }
    }
    if skipped_count > 0 {
        skipped.push(json!({
            "reason": "no_confirmed_plans",
            "count": skipped_count
        }));
    }
    let status = if failed_count > 0 && executed_count > 0 {
        "partial"
    } else if failed_count > 0 {
        "failed"
    } else if executed_count > 0 {
        "executed"
    } else if skipped_count > 0 {
        "skipped"
    } else {
        "no_actions"
    };
    Ok(json!({
        "ok": true,
        "status": status,
        "scope": "full",
        "artist_id": Value::Null,
        "at": now(),
        "backup": backup,
        "executed_count": executed_count,
        "skipped_count": skipped_count,
        "failed_count": failed_count,
        "actions": actions,
        "skipped": skipped,
        "failed": failed,
        "errors": errors
    }))
}

pub fn create_db_backup(conn: &Connection) -> Result<String> {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let root = PathBuf::from(data_dir).join("db-backups");
    std::fs::create_dir_all(&root)?;
    let label = chrono::Local::now().format("%Y%m%d-%H%M%S").to_string();
    let dir = root.join(&label);
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join("gallery.db");
    // Online backup API
    let mut dst = Connection::open(&dest)?;
    let backup = rusqlite::backup::Backup::new(conn, &mut dst)?;
    backup.run_to_completion(5, std::time::Duration::from_millis(0), None)?;
    std::fs::write(
        dir.join("metadata.json"),
        json!({"created_at": now(), "label": label}).to_string(),
    )?;
    Ok(dest.display().to_string())
}

pub fn recheck_plan(conn: &Connection, plan_id: i64) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let row = conn
        .query_row(
            "SELECT id, status, source_folder, target_folder FROM folder_rename_plans WHERE id=?",
            params![plan_id],
            |r| {
                Ok(json!({
                    "id": r.get::<_, i64>(0)?,
                    "status": r.get::<_, String>(1)?,
                    "source_folder": r.get::<_, String>(2)?,
                    "target_folder": r.get::<_, String>(3)?,
                    "rechecked": true
                }))
            },
        )
        .optional()?;
    row.ok_or_else(|| anyhow!("plan not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn execute_renames_folder_and_updates_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        let src = artist.join("old");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::write(src.join("a.jpg"), b"x").unwrap();
        let db_path = dir.path().join("g.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists VALUES (1,'a',?)", params![ap])
            .unwrap();
        let fp = src.join("a.jpg").to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO items VALUES (1,1,?,'a.jpg',0)", params![fp])
            .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1,'old','new','confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };
        let out = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(out["ok"], true);
        assert!(artist.join("new").join("a.jpg").is_file());
        let new_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert!(new_path.contains("/new/"));
    }

    #[test]
    fn rejects_traversal_folder() {
        assert!(validate_relative_folder("../etc").is_err());
        assert!(validate_relative_folder("/abs").is_err());
        assert!(validate_relative_folder("a/../b").is_err());
        assert!(validate_relative_folder("a/./b").is_err());
        assert!(validate_relative_folder("./b").is_err());
        assert_eq!(validate_relative_folder("2024/ok").unwrap(), "2024/ok");
    }

    #[test]
    fn migrates_legacy_auto_setting_to_canonical_key() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto', '1')",
            [],
        )
        .unwrap();
        assert!(folder_rename_auto_enabled(&conn).unwrap());
        let canonical: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto_enabled'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(canonical, "1");
        assert!(conn
            .query_row::<String, _, _>(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto'",
                [],
                |row| row.get(0)
            )
            .is_err());
    }

    #[test]
    fn auto_archive_returns_disabled_run_counts_without_summary_setting() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_last_run', '{\"legacy\":true}')",
            [],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new().clone(),
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["status"], "disabled");
        assert_eq!(result["scope"], "full");
        assert_eq!(result["executed_count"], 0);
        assert_eq!(result["skipped_count"], 0);
        assert_eq!(result["failed_count"], 0);
        for key in ["actions", "skipped", "failed", "errors"] {
            assert!(result[key].is_array(), "missing array: {key}");
        }
        assert!(conn
            .query_row::<String, _, _>(
                "SELECT value FROM app_settings WHERE key='folder_rename_auto_last_run'",
                [],
                |row| row.get(0)
            )
            .is_err());
    }

    #[test]
    fn auto_archive_does_not_store_one_result_per_artist_without_plans() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT, missing INTEGER DEFAULT 0);",
        )
        .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO app_settings(key, value) VALUES('folder_rename_auto_enabled', '1')",
            [],
        )
        .unwrap();
        conn.execute_batch(
            "INSERT INTO artists (id, name, path) VALUES (1, 'a', '/a'), (2, 'b', '/b');",
        )
        .unwrap();
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new().clone(),
        };

        let result = run_folder_rename_auto_after_full_scan(&conn, &roots).unwrap();

        assert_eq!(result["status"], "skipped");
        assert_eq!(result["skipped_count"], 2);
        assert!(result["results"].is_null());
        assert!(result["skipped"].as_array().unwrap().len() <= 1);
    }

    #[test]
    fn execute_rejects_traversal_plan() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let artist = dir.path().join("artist");
        std::fs::create_dir_all(&artist).unwrap();
        let db_path = dir.path().join("g.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT, missing INTEGER DEFAULT 0);
            ",
        )
        .unwrap();
        let ap = artist.to_string_lossy().replace('\\', "/");
        conn.execute("INSERT INTO artists VALUES (1,'a',?)", params![ap])
            .unwrap();
        ensure_folder_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status)
             VALUES (1,'../escape','new','confirmed')",
            [],
        )
        .unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec![dir.path().to_string_lossy().into()],
            labels: vec!["r".into()],
            real_paths: vec![dir.path().to_string_lossy().into()],
        };
        let out = execute_folder_renames(&conn, &roots, 1, false).unwrap();
        assert_eq!(out["results"][0]["reason"], "bad_folder_path");
    }
}
