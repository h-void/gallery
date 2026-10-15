//! Folder archive plan list + execute (pure Rust product path).

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(not(target_os = "linux"))]
use std::path::Component;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};

use crate::archive_format::{self, RenderContext};
use crate::archive_profiles;
use crate::fs_util::safe_canonicalize;
use crate::item_detail::effective_display_date;
use crate::media_roots::{path_under_authorized_roots, MediaRoots};
use crate::media_serve::{
    move_file_from_authorized_path_no_overwrite, move_file_to_authorized_path_no_overwrite,
};

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

#[path = "folder_archive_fs.rs"]
mod fs_ops;
#[allow(unused_imports)]
pub(crate) use crate::db_backup::prune_backup_root;
pub use crate::db_backup::{create_db_backup, create_housekeeping_db_backup};
#[cfg(target_os = "linux")]
pub(crate) use fs_ops::{
    create_authorized_directory_no_follow, open_authorized_directory_no_follow, open_dir_at,
};
use fs_ops::{
    rename_artist_dir_no_overwrite, rename_permission_denied_parent, rollback_undo_folder_rename,
};
pub(crate) use fs_ops::{
    rename_dir_no_overwrite, rename_directory_under_authorized_roots_no_overwrite_expected,
};

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

pub(crate) fn path_under_artist(path: &Path, artist: &Path) -> bool {
    let path = safe_canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    let artist = safe_canonicalize(artist).unwrap_or_else(|_| artist.to_path_buf());
    path.starts_with(&artist)
}

fn target_parent_under_artist(parent: &Path, artist: &Path) -> bool {
    let mut current = Some(parent);
    while let Some(candidate) = current {
        if candidate.exists() {
            return candidate.is_dir() && path_under_artist(candidate, artist);
        }
        current = candidate.parent();
    }
    false
}

fn folder_db_path(artist_path: &str, folder: &str) -> String {
    PathBuf::from(artist_path)
        .join(folder)
        .to_string_lossy()
        .replace('\\', "/")
}

fn update_folder_item_paths(
    tx: &rusqlite::Transaction<'_>,
    artist_id: i64,
    logical_source: &str,
    real_source: &str,
    logical_target: &str,
    real_target: &str,
) -> Result<i64> {
    let logical_prefix = format!("{}/", logical_source.trim_end_matches('/'));
    let real_prefix = format!("{}/", real_source.trim_end_matches('/'));
    Ok(tx.execute(
        "UPDATE items SET file_path=CASE
           WHEN file_path=?1 OR instr(file_path, ?2)=1
             THEN ?3 || substr(file_path, length(?1) + 1)
           ELSE ?4 || substr(file_path, length(?5) + 1)
         END
         WHERE artist_id=?6 AND (
           file_path=?1 OR instr(file_path, ?2)=1 OR
           file_path=?5 OR instr(file_path, ?7)=1
         )",
        params![
            real_source,
            real_prefix,
            real_target,
            logical_target,
            logical_source,
            artist_id,
            logical_prefix,
        ],
    )? as i64)
}

/// Prove that `adopted_dir` really is `src` after a rename, before the ledger is
/// repointed at it.
///
/// `adopted_rename` fires on the shape a kill between rename and commit leaves:
/// the source name is gone and the target name is a directory. On its own that
/// shape is also produced by unrelated events — the user moved or deleted the
/// source by hand and something else now occupies the archive name — and
/// adopting then silently repoints every item row under the source at a
/// directory whose contents have nothing to do with the plan.
///
/// The evidence is the plan's own items: they still name the source folder, and
/// they must be present in the directory being adopted. Extra files are allowed
/// (the plan can cover a subset of a folder, and a scan may not have indexed
/// everything); an adoption whose item names are entirely absent is refused.
fn adoption_names_match_items(
    conn: &Connection,
    artist_id: i64,
    src: &Path,
    adopted_dir: &Path,
) -> Result<bool> {
    let src_prefix = format!(
        "{}/",
        src.to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
    );
    let mut stmt = conn.prepare(
        "SELECT file_name FROM items
         WHERE artist_id=?1 AND COALESCE(missing, 0)=0
           AND (file_path=?2 OR instr(replace(file_path,'\\','/'), ?3)=1)",
    )?;
    let item_names = stmt
        .query_map(
            params![
                artist_id,
                src.to_string_lossy().replace('\\', "/"),
                src_prefix
            ],
            |row| row.get::<_, Option<String>>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let item_names: HashSet<String> = item_names
        .into_iter()
        .flatten()
        .map(|name| name.trim().to_string())
        .filter(|name| !name.is_empty())
        .collect();
    // No indexed names to check against: keep the previous behaviour rather than
    // refusing every adoption for a library that has not been scanned yet.
    if item_names.is_empty() {
        return Ok(true);
    }
    let mut disk_names: HashSet<String> = HashSet::new();
    for entry in walkdir::WalkDir::new(adopted_dir)
        .into_iter()
        .filter_map(|entry| entry.ok())
    {
        if entry.file_type().is_file() {
            if let Some(name) = entry.file_name().to_str() {
                disk_names.insert(name.trim().to_string());
            }
        }
    }
    let overlap = item_names.intersection(&disk_names).count();
    Ok(overlap > 0 && overlap * 2 >= item_names.len())
}

#[cfg(not(target_os = "linux"))]
fn ensure_target_parent_under_artist(parent: &Path, artist: &Path) -> Result<()> {
    let relative = parent
        .strip_prefix(artist)
        .map_err(|_| anyhow!("target parent is outside the artist directory"))?;
    if !artist.is_dir() || !path_under_artist(artist, artist) {
        bail!("artist directory is unavailable or outside its real path");
    }
    let mut current = artist.to_path_buf();
    for component in relative.components() {
        let Component::Normal(component) = component else {
            bail!("target parent contains an invalid path component");
        };
        current.push(component);
        if !current.exists() {
            match std::fs::create_dir(&current) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
                Err(error) => return Err(error.into()),
            }
        }
        if !current.is_dir() || !path_under_artist(&current, artist) {
            bail!("target parent leaves the artist directory");
        }
    }
    Ok(())
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
    let has_snapshot: bool = conn
        .prepare("PRAGMA table_info(folder_rename_plans)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .iter()
        .any(|name| name == "format_snapshot");
    if !has_snapshot {
        conn.execute(
            "ALTER TABLE folder_rename_plans ADD COLUMN format_snapshot TEXT NOT NULL DEFAULT '{}'",
            [],
        )?;
    }
    conn.execute(
        // Execution failures already demote confirmed plans at the moment they happen.
        // Do not override a later manual reconfirmation from its historical log.
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='', updated_at=?
         WHERE status='ready'
           AND (
             execution_log LIKE '%failed%'
             OR execution_log LIKE '%source_missing%'
             OR execution_log LIKE '%target_exists%'
             OR execution_log LIKE '%bad_folder_path%'
             OR execution_log LIKE '%db_update_failed%'
             OR execution_log LIKE '%outside_artist%'
             OR execution_log LIKE '%\"status\":\"error\"%'
           )",
        params![now()],
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
        "target_parent_failed" => "无法创建目标文件夹",
        "target_inside_source" => "目标文件夹不能位于来源文件夹内",
        "bad_folder_path" => "文件夹路径无效",
        "db_update_failed" => "数据库路径更新失败",
        "rollback_failed" => "执行回滚失败，需要人工核对",
        "outside_artist" => "路径不在画师目录内",
        "permission_denied" => "文件夹没有改名权限",
        "stale_split_plan" => "文件日期、标签或路径已变化",
        "missing_split_actions" => "没有可执行的拆分文件",
        "execution_failed" => "执行失败",
        "blocked" => "当前不安全，已跳过",
        "manual_review" => "需要人工处理",
        _ => "整理失败",
    }
}

const RESOLVED_PLAN_STATUSES: [&str; 4] = ["executed", "reverted", "stale", "aligned"];
const IGNORED_FAILURE_REASONS: [&str; 4] =
    ["needs_date", "date_conflict", "stale_target", "stale_state"];

fn failure_reason(raw: &str) -> Option<String> {
    let entries = serde_json::from_str::<Value>(raw).ok()?.as_array()?.clone();
    entries.into_iter().rev().find_map(|entry| {
        let status = entry.get("status").and_then(Value::as_str)?;
        if !matches!(status, "failed" | "error" | "blocked") {
            return None;
        }
        let reason =
            entry
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or(if status == "blocked" {
                    "blocked"
                } else {
                    "execution_failed"
                });
        if IGNORED_FAILURE_REASONS.contains(&reason) {
            return None;
        }
        Some(match reason {
            "backup_failed"
            | "source_missing"
            | "target_exists"
            | "target_parent_failed"
            | "target_inside_source"
            | "bad_folder_path"
            | "db_update_failed"
            | "rollback_failed"
            | "outside_artist"
            | "permission_denied"
            | "stale_split_plan"
            | "missing_split_actions"
            | "execution_failed"
            | "blocked"
            | "bad_target"
            | "revalidation_failed" => reason.to_string(),
            _ => "execution_failed".to_string(),
        })
    })
}

fn plan_failure(status: &str, execution_log: &str) -> Option<String> {
    if RESOLVED_PLAN_STATUSES.contains(&status) {
        return None;
    }
    if status == "manual_review" {
        return Some(failure_reason(execution_log).unwrap_or_else(|| "manual_review".to_string()));
    }
    failure_reason(execution_log)
}

pub fn folder_archive_failed_plans_count(conn: &Connection) -> Result<i64> {
    let exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='folder_rename_plans'",
        [],
        |row| row.get(0),
    )?;
    if exists == 0 {
        return Ok(0);
    }
    let mut stmt = conn.prepare(
        "SELECT status, execution_log FROM folder_rename_plans
         WHERE status NOT IN ('executed','reverted','stale','aligned')
           AND (status='manual_review' OR execution_log LIKE '%failed%'
             OR execution_log LIKE '%error%' OR execution_log LIKE '%blocked%'
             OR execution_log LIKE '%target_exists%' OR execution_log LIKE '%source_missing%'
             OR execution_log LIKE '%bad_folder_path%' OR execution_log LIKE '%db_update_failed%'
             OR execution_log LIKE '%outside_artist%' OR execution_log LIKE '%permission_denied%')",
    )?;
    let mut count = 0;
    for row in stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (status, log) = row?;
        if plan_failure(&status, &log).is_some() {
            count += 1;
        }
    }
    Ok(count)
}

pub fn folder_error_artists(
    conn: &Connection,
    query: Option<&str>,
    sort: Option<&str>,
    offset: i64,
    limit: i64,
) -> Result<Value> {
    let query = query.unwrap_or("").trim();
    let escaped = query
        .replace('\\', "\\\\")
        .replace('%', "\\%")
        .replace('_', "\\_");
    let mut sql = String::from(
        "SELECT p.id, p.artist_id, p.status, p.execution_log, p.created_at, p.updated_at,
                a.name, a.path
         FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
         WHERE COALESCE(a.missing,0)=0 AND p.status NOT IN ('executed','reverted','stale','aligned')
           AND (p.status='manual_review' OR p.execution_log LIKE '%failed%'
             OR p.execution_log LIKE '%error%' OR p.execution_log LIKE '%blocked%'
             OR p.execution_log LIKE '%target_exists%' OR p.execution_log LIKE '%source_missing%'
             OR p.execution_log LIKE '%bad_folder_path%' OR p.execution_log LIKE '%db_update_failed%'
             OR p.execution_log LIKE '%outside_artist%' OR p.execution_log LIKE '%permission_denied%')",
    );
    if !escaped.is_empty() {
        sql.push_str(" AND (LOWER(a.name) LIKE LOWER(?) ESCAPE '\\' OR CAST(a.id AS TEXT) LIKE ? ESCAPE '\\')");
    }
    let mut stmt = conn.prepare(&sql)?;
    let rows = if escaped.is_empty() {
        stmt.query_map([], map_error_plan)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    } else {
        let pattern = format!("%{escaped}%");
        stmt.query_map([pattern.clone(), pattern], map_error_plan)?
            .collect::<rusqlite::Result<Vec<_>>>()?
    };
    let mut grouped: HashMap<i64, Value> = HashMap::new();
    for row in rows {
        let (plan_id, artist_id, status, log, created, updated, name, path) = row;
        let Some(reason) = plan_failure(&status, &log) else {
            continue;
        };
        let latest_at = updated.or(created).unwrap_or(0.0);
        let entry = grouped.entry(artist_id).or_insert_with(|| {
            json!({
                "artist_id": artist_id, "artist_name": name, "artist_path": path,
                "error_count": 0, "latest_at": latest_at, "latest_plan_id": plan_id,
                "reason": reason, "message": archive_failure_message(&reason),
            })
        });
        entry["error_count"] = json!(entry["error_count"].as_i64().unwrap_or(0) + 1);
        let old_latest = (
            entry["latest_at"].as_f64().unwrap_or(0.0),
            entry["latest_plan_id"].as_i64().unwrap_or(0),
        );
        if (latest_at, plan_id) > old_latest {
            entry["latest_at"] = json!(latest_at);
            entry["latest_plan_id"] = json!(plan_id);
            entry["reason"] = json!(reason);
            entry["message"] = json!(archive_failure_message(&reason));
        }
    }
    let mut artists: Vec<Value> = grouped.into_values().collect();
    if sort.unwrap_or("recent") == "count" {
        artists.sort_by(|a, b| {
            (
                -a["error_count"].as_i64().unwrap_or(0),
                -a["latest_at"].as_f64().unwrap_or(0.0),
                -a["latest_plan_id"].as_i64().unwrap_or(0),
                a["artist_id"].as_i64().unwrap_or(0),
            )
                .partial_cmp(&(
                    -b["error_count"].as_i64().unwrap_or(0),
                    -b["latest_at"].as_f64().unwrap_or(0.0),
                    -b["latest_plan_id"].as_i64().unwrap_or(0),
                    b["artist_id"].as_i64().unwrap_or(0),
                ))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    } else {
        artists.sort_by(|a, b| {
            (
                -a["latest_at"].as_f64().unwrap_or(0.0),
                -a["latest_plan_id"].as_i64().unwrap_or(0),
                -a["error_count"].as_i64().unwrap_or(0),
                a["artist_id"].as_i64().unwrap_or(0),
            )
                .partial_cmp(&(
                    -b["latest_at"].as_f64().unwrap_or(0.0),
                    -b["latest_plan_id"].as_i64().unwrap_or(0),
                    -b["error_count"].as_i64().unwrap_or(0),
                    b["artist_id"].as_i64().unwrap_or(0),
                ))
                .unwrap_or(std::cmp::Ordering::Equal)
        });
    }
    let page_limit = limit.clamp(1, crate::MAX_ITEM_PAGE_LIMIT);
    let total = artists.len() as i64;
    let start = offset.max(0) as usize;
    let end = (start + page_limit as usize).min(artists.len());
    let page = if start < artists.len() {
        artists[start..end].to_vec()
    } else {
        Vec::new()
    };
    Ok(
        json!({"artists": page, "total": total, "offset": offset.max(0), "limit": page_limit, "has_more": end < artists.len()}),
    )
}

type ErrorPlanRow = (
    i64,
    i64,
    String,
    String,
    Option<f64>,
    Option<f64>,
    String,
    String,
);

fn map_error_plan(row: &rusqlite::Row<'_>) -> rusqlite::Result<ErrorPlanRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

/// Demote a plan with a failure log entry without forcing manual review;
/// used when a confirmed target no longer matches the current item dates.
fn demote_plan_with_log(
    conn: &Connection,
    plan_id: i64,
    status: &str,
    target: &str,
    reason: &str,
    source: &str,
) -> Result<()> {
    let entry = json!({
        "at": now(),
        "status": "failed",
        "reason": reason,
        "message": archive_failure_message(reason),
        "source": source,
        "target": target,
        "automatic": true,
    });
    // Append to the existing history so earlier failure entries survive.
    let existing_log: String = conn
        .query_row(
            "SELECT execution_log FROM folder_rename_plans WHERE id=?",
            params![plan_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "[]".to_string());
    let mut log = serde_json::from_str::<Value>(&existing_log)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    log.push(entry);
    conn.execute(
        "UPDATE folder_rename_plans
         SET status=?, target_folder=?, confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=?",
        params![
            status,
            target,
            Value::Array(log).to_string(),
            now(),
            plan_id
        ],
    )?;
    Ok(())
}

/// Record a failed attempt as manual review so automatic runs do not retry it.
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
    // Append to the existing history so earlier failure entries survive.
    let existing_log: String = conn
        .query_row(
            "SELECT execution_log FROM folder_rename_plans WHERE id=?",
            params![plan_id],
            |row| row.get(0),
        )
        .unwrap_or_else(|_| "[]".to_string());
    let mut log = serde_json::from_str::<Value>(&existing_log)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    log.push(entry);
    conn.execute(
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=? AND status='confirmed'",
        params![Value::Array(log).to_string(), now(), plan_id],
    )?;
    Ok(())
}

/// [`record_plan_execution_failure`] for call sites inside the execution loop.
///
/// Those sites used `let _ =`, which hid the failure completely: a plan that
/// could not be demoted to `manual_review` stayed `confirmed`, and the next
/// automatic run retried the same destructive move with no trace anywhere. They
/// cannot use `?` either — that aborts the whole batch, and the contract is that
/// a failing plan never takes the plans after it down with it. So the failure is
/// logged and the loop continues; what was silent is now in `gallery.log` and in
/// the health error window.
fn record_plan_failure_or_log(
    conn: &Connection,
    plan_id: i64,
    reason: &str,
    source: &str,
    target: &str,
    extra: Option<Value>,
) {
    if let Err(error) = record_plan_execution_failure(conn, plan_id, reason, source, target, extra)
    {
        log_error!(
            "folder archive: could not record plan {plan_id} failure ({reason}); \
             the plan stays confirmed and an automatic run may retry it: {error:#}"
        );
    }
}

fn record_undo_reconciliation_failure(
    conn: &Connection,
    plan_id: i64,
    source: &str,
    target: &str,
    backup: &str,
    error: &anyhow::Error,
    rollback_error: &std::io::Error,
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    let (status, execution_log): (String, String) = tx.query_row(
        "SELECT status, execution_log FROM folder_rename_plans WHERE id=?",
        params![plan_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if status != "executed" {
        bail!("plan state changed while recording undo reconciliation failure");
    }
    let mut log = serde_json::from_str::<Value>(&execution_log)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    log.push(json!({
        "at": now(),
        "status": "failed",
        "reason": "rollback_failed",
        "message": archive_failure_message("rollback_failed"),
        "operation": "folder_rename_undo",
        "source": source,
        "target": target,
        "backup": backup,
        "error": error.to_string(),
        "rollback_error": rollback_error.to_string(),
        "automatic": false,
    }));
    let changed = tx.execute(
        "UPDATE folder_rename_plans
         SET status='manual_review', confirmed_at=NULL, confirmation_source='',
             execution_log=?, updated_at=?
         WHERE id=? AND status='executed' AND execution_log=?",
        params![Value::Array(log).to_string(), now(), plan_id, execution_log,],
    )?;
    if changed != 1 {
        bail!("plan state changed while recording undo reconciliation failure");
    }
    tx.commit()?;
    Ok(())
}

/// Clear stale confirmation on unexecuted plans whose source folder items had
/// their effective date changed by a manual date batch. Confirmed plans are
/// demoted to `ready` and their stale target cleared so neither the manual
/// apply flow nor auto-run can execute a target that no longer matches the
/// item dates; the fixed target builder re-derives targets on next preview.
pub fn invalidate_plans_after_item_date_change(
    conn: &Connection,
    artist_id: i64,
    folders: &[String],
) -> Result<i64> {
    if folders.is_empty() {
        return Ok(0);
    }
    let placeholders = std::iter::repeat_n("?", folders.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut params: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Real(now()),
        rusqlite::types::Value::Integer(artist_id),
    ];
    params.extend(
        folders
            .iter()
            .map(|folder| rusqlite::types::Value::Text(folder.clone())),
    );
    let affected = conn.execute(
        &format!(
            "UPDATE folder_rename_plans
             SET confirmed_at=NULL, confirmation_source='',
                 status=CASE WHEN status='confirmed' THEN 'ready' ELSE status END,
                 target_folder=CASE WHEN status='confirmed' THEN '' ELSE target_folder END,
                 updated_at=?
             WHERE artist_id=? AND source_folder IN ({placeholders})
               AND status NOT IN ('executed','reverted')"
        ),
        rusqlite::params_from_iter(params.iter()),
    )?;
    Ok(affected as i64)
}

// Effective raw date resolution uses `crate::item_detail::effective_display_date`.

/// `YYYY-MM-DD` or `YYYY-MM` key from a manual/recognized effective date.
/// A full day is preserved whenever the source carries one.
fn effective_date_key(raw: &str) -> Option<String> {
    let digits = raw
        .trim()
        .chars()
        .filter(char::is_ascii_digit)
        .collect::<String>();
    if digits.len() >= 8 {
        Some(format!(
            "{}-{}-{}",
            &digits[..4],
            &digits[4..6],
            &digits[6..8]
        ))
    } else if digits.len() == 6 {
        Some(format!("{}-{}", &digits[..4], &digits[4..]))
    } else {
        None
    }
}

/// First calendar-valid date at the head of a raw date string, for seeding a
/// plan's `parsed_date` when the folder name itself carries no date.
///
/// Slices at most 10 bytes and never mid-character, keeps month precision
/// (`2024-05` stays `2024-05`), and rejects impossible calendar dates instead
/// of naming an archive folder `2024-13-99`. A `YYYYMMDD` head followed by
/// more digits (a timestamp tail) still yields its leading 8 digits.
fn date_head_from_raw(raw: &str) -> Option<String> {
    let head = raw.get(..10).unwrap_or(raw);
    if head.len() < 7 || !head.chars().all(|c| c.is_ascii_digit() || c == '-') {
        return None;
    }
    if let Some(value) = crate::media_type::extract_date_value_from_folder(head) {
        return Some(value.raw);
    }
    if head.len() >= 8 && head.bytes().all(|b| b.is_ascii_digit()) {
        if let Some(value) = crate::media_type::extract_date_value_from_folder(&head[..8]) {
            return Some(value.raw);
        }
    }
    None
}

/// Distinct effective dates of the active items per source folder for one
/// artist, built with a single pass over the artist's items.
///
/// Callers that evaluate many plans (recompute, execute recheck) previously
/// re-ran one full items query per plan; sharing this grouping turns
/// O(plans × items) work into O(items). The map must be rebuilt whenever item
/// paths change (after an executed plan rewrote them).
struct ItemDatesByFolder {
    by_folder: HashMap<String, Vec<String>>,
}

impl ItemDatesByFolder {
    fn build(conn: &Connection, artist_id: i64) -> Result<Self> {
        // Minimal schemas (tests, pre-scan databases) may lack items entirely;
        // an empty grouping matches the previous lazy per-plan behavior.
        let has_items: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='items')",
            [],
            |row| row.get(0),
        )?;
        let mut by_folder: HashMap<String, Vec<String>> = HashMap::new();
        if !has_items {
            return Ok(Self { by_folder });
        }
        let artist_path = artist_plan_path(conn, artist_id)?;
        let mut stmt = conn.prepare(
            "SELECT file_path, folder_name, manual_date, COALESCE(detected_date, ''), COALESCE(date, '') FROM items
             WHERE artist_id=? AND COALESCE(missing, 0)=0",
        )?;
        let rows = stmt.query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
            ))
        })?;
        for row in rows {
            let (file_path, folder_name, manual_date, detected_date, legacy_date) = row?;
            let Some(folder) = source_folder_for_item(&artist_path, &file_path, &folder_name)
            else {
                continue;
            };
            let raw = effective_display_date(&detected_date, manual_date.as_deref(), &legacy_date);
            if let Some(date) = effective_date_key(&raw) {
                let dates = by_folder.entry(folder).or_default();
                if !dates.contains(&date) {
                    dates.push(date);
                }
            }
        }
        Ok(Self { by_folder })
    }

    fn get(&self, folder: &str) -> Vec<String> {
        self.by_folder.get(folder).cloned().unwrap_or_default()
    }
}

/// Tag names for a plan: the stored `selected_tag_ids` order when present,
/// otherwise the distinct artist folder tag names ordered by name.
fn plan_tag_names(
    conn: &Connection,
    artist_id: i64,
    folder: &str,
    selected_json: &str,
) -> Result<Vec<String>> {
    let ids: Vec<i64> = serde_json::from_str(selected_json).unwrap_or_default();
    if !ids.is_empty() {
        let mut names = Vec::with_capacity(ids.len());
        let mut stmt = conn.prepare("SELECT name FROM tags WHERE id=? AND artist_id=?")?;
        for id in ids {
            let name: Option<String> = stmt
                .query_row(params![id, artist_id], |row| row.get(0))
                .optional()?;
            if let Some(name) = name {
                if !name.is_empty() {
                    names.push(name);
                }
            }
        }
        return Ok(names);
    }
    let artist_path = artist_plan_path(conn, artist_id)?;
    let mut names = BTreeSet::new();
    let table_exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='item_tags')",
        [],
        |row| row.get(0),
    )?;
    if !table_exists {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT i.file_path, i.folder_name, t.name FROM items i
         JOIN item_tags it ON it.item_id=i.id
         JOIN tags t ON t.id=it.tag_id
         WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
         ORDER BY t.name",
    )?;
    let rows = stmt.query_map(params![artist_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (file_path, folder_name, name) = row?;
        if source_folder_for_item(&artist_path, &file_path, &folder_name).as_deref() == Some(folder)
        {
            names.insert(name);
        }
    }
    Ok(names.into_iter().collect())
}

fn is_reserved_component(component: &str) -> bool {
    let stem = component
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .and_then(|number| number.parse::<u8>().ok())
            .map(|number| (1..=9).contains(&number))
            .unwrap_or(false)
}

/// Validate a fixed archive target as a portable artist-relative folder path.
fn validate_fixed_target(value: &str) -> Result<String> {
    let raw = value.replace('\\', "/").trim().to_string();
    if raw.is_empty() || raw.starts_with('/') || raw.starts_with("//") {
        bail!("fixed archive target must be an artist-relative folder");
    }
    let bytes = raw.as_bytes();
    if bytes.len() >= 2 && bytes[1] == b':' {
        bail!("fixed archive target must not use a drive path");
    }
    let mut components = Vec::new();
    for component in raw.split('/') {
        if component.is_empty() {
            bail!("fixed archive target cannot contain empty path segments");
        }
        if component == "." || component == ".." {
            bail!("fixed archive target cannot contain traversal segments");
        }
        if component.chars().count() > 180 {
            bail!("fixed archive target has an overlong path segment");
        }
        if component.ends_with(' ') || component.ends_with('.') {
            bail!("fixed archive target has an unsafe trailing path character");
        }
        if component
            .chars()
            .any(|ch| ch.is_control() || matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
        {
            bail!("fixed archive target contains unsafe filename characters");
        }
        if is_reserved_component(component) {
            bail!("fixed archive target contains a reserved filename");
        }
        components.push(component);
    }
    if components.is_empty() {
        bail!("fixed archive target must not be empty");
    }
    Ok(components.join("/"))
}

/// The item's parent directory relative to the artist root. Older databases
/// only have the final path component in `folder_name`, so use it solely when
/// the stored file path cannot be safely made relative to the artist path.
fn source_folder_for_item(artist_path: &str, file_path: &str, folder_name: &str) -> Option<String> {
    let artist_root = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let file_path = file_path.replace('\\', "/");
    let from_path = if artist_root.is_empty() {
        None
    } else {
        file_path
            .strip_prefix(&format!("{artist_root}/"))
            .and_then(|relative| relative.rsplit_once('/').map(|(folder, _)| folder))
            .and_then(|folder| validate_relative_folder(folder).ok())
    };
    from_path.or_else(|| validate_relative_folder(folder_name).ok())
}

fn artist_plan_path(conn: &Connection, artist_id: i64) -> Result<String> {
    Ok(conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default())
}

fn archive_profile_for_artist(conn: &Connection, artist_id: i64) -> Result<(Value, String)> {
    let settings = archive_profiles::load(conn)?;
    archive_format::profile_for_artist(&settings, artist_id)
}

fn suffixed_archive_target(target: &str, number: usize) -> String {
    match target.rsplit_once('/') {
        Some((parent, name)) => format!("{parent}/{name} ({number})"),
        None => format!("{target} ({number})"),
    }
}

fn is_suffixed_archive_target(target: &str, requested_target: &str) -> bool {
    target
        .strip_prefix(requested_target)
        .and_then(|suffix| suffix.strip_prefix(" ("))
        .and_then(|suffix| suffix.strip_suffix(')'))
        .and_then(|number| number.parse::<usize>().ok())
        .is_some_and(|number| number >= 2)
}

pub(crate) struct ArchiveRenderParams<'a> {
    pub artist: &'a str,
    pub date: &'a str,
    pub tags: &'a [String],
    pub title: &'a str,
    pub folder: &'a str,
    pub index: usize,
    pub user_id: Option<i64>,
    pub id: Option<i64>,
    pub ext: Option<&'a str>,
}

fn render_archive_target(profile: &Value, params: &ArchiveRenderParams<'_>) -> Result<String> {
    Ok(archive_format::render_profile(
        profile,
        &RenderContext {
            artist: params.artist.to_string(),
            date: params.date.to_string(),
            tags: params.tags.to_vec(),
            title: params.title.to_string(),
            folder: params.folder.to_string(),
            index: params.index,
            user_id: params.user_id.map(|v| v.to_string()),
            id: params.id.map(|v| v.to_string()),
            ext: params.ext.map(|v| v.to_string()),
            site: None,
            service: None,
            task_date: None,
        },
    )?
    .target_folder)
}

#[derive(Clone, Debug)]
struct DiscoveredFolderItem {
    id: i64,
    file_path: String,
    file_name: String,
    folder_name: String,
    manual_date: Option<String>,
    detected_date: String,
    legacy_date: String,
    tags: Vec<i64>,
}

#[derive(Clone, Debug)]
struct SplitFileMove {
    item_id: i64,
    source_db: String,
    target_db: String,
    source: PathBuf,
    target: PathBuf,
    target_folder: String,
    /// The interrupt already moved this file: the item row still names the old
    /// path, the bytes are at the target, and `move_into_recycle`-style
    /// no-overwrite moves cannot be replayed. Recorded by the plan preparation
    /// so the split can finish instead of failing on a source that is gone.
    already_moved: bool,
}

/// Per-artist tag-id → name map.
///
/// Split discovery and execution resolve tag names once per item, so a direct
/// query per tag id costs `items × tags` statement preparations per artist. The
/// map is preloaded with one query and then answers in memory; ids outside the
/// artist (or not named) resolve to `None` exactly as the old lookup did.
#[derive(Default)]
struct TagNameCache {
    artist_id: Option<i64>,
    names: std::collections::HashMap<i64, String>,
}

impl TagNameCache {
    fn for_artist(conn: &Connection, artist_id: i64) -> Result<Self> {
        let mut stmt = conn.prepare("SELECT id, name FROM tags WHERE artist_id=?")?;
        let rows = stmt.query_map(params![artist_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?;
        let mut names = std::collections::HashMap::new();
        for row in rows {
            let (id, name) = row?;
            if !name.is_empty() {
                names.insert(id, name);
            }
        }
        Ok(Self {
            artist_id: Some(artist_id),
            names,
        })
    }

    fn names_for(&self, artist_id: i64, tag_ids: &[i64]) -> Vec<String> {
        if self.artist_id != Some(artist_id) {
            return Vec::new();
        }
        tag_ids
            .iter()
            .filter_map(|tag_id| self.names.get(tag_id).cloned())
            .collect()
    }
}

fn tag_names_for_ids(
    conn: &Connection,
    artist_id: i64,
    tag_ids: &[i64],
    cache: &TagNameCache,
) -> Result<Vec<String>> {
    if cache.artist_id == Some(artist_id) {
        return Ok(cache.names_for(artist_id, tag_ids));
    }
    let mut names = Vec::with_capacity(tag_ids.len());
    let mut stmt = conn.prepare("SELECT name FROM tags WHERE id=? AND artist_id=?")?;
    for tag_id in tag_ids {
        if let Some(name) = stmt
            .query_row(params![tag_id, artist_id], |row| row.get::<_, String>(0))
            .optional()?
            .filter(|name| !name.is_empty())
        {
            names.push(name);
        }
    }
    Ok(names)
}

fn source_folder_items(
    conn: &Connection,
    artist_id: i64,
    artist_path: &str,
    source_folder: &str,
) -> Result<Vec<DiscoveredFolderItem>> {
    let prefix = format!("{}/", folder_db_path(artist_path, source_folder));
    let has_item_tags: bool = conn
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='item_tags')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    let tags_subquery = if has_item_tags {
        "(SELECT json_group_array(it.tag_id)
          FROM (SELECT it.tag_id FROM item_tags it WHERE it.item_id=i.id ORDER BY it.tag_id) it)"
    } else {
        "'[]'"
    };
    let sql = format!(
        "SELECT i.id, i.file_path, i.file_name, i.folder_name, i.manual_date,
                COALESCE(i.detected_date, ''), COALESCE(i.date, ''),
                {tags_subquery}
         FROM items i
         WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
           AND instr(REPLACE(i.file_path, '\\', '/'), ?)=1
         ORDER BY i.id"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artist_id, prefix], |row| {
        let raw: Option<String> = row.get(7)?;
        Ok(DiscoveredFolderItem {
            id: row.get(0)?,
            file_path: row.get(1)?,
            file_name: row.get(2)?,
            folder_name: row.get(3)?,
            manual_date: row.get(4)?,
            detected_date: row.get(5)?,
            legacy_date: row.get(6)?,
            tags: raw
                .and_then(|value| serde_json::from_str::<Vec<i64>>(&value).ok())
                .unwrap_or_default(),
        })
    })?;
    rows.filter_map(|row| match row {
        Ok(item)
            if source_folder_for_item(artist_path, &item.file_path, &item.folder_name)
                .as_deref()
                == Some(source_folder) =>
        {
            Some(Ok(item))
        }
        Ok(_) => None,
        Err(error) => Some(Err(error)),
    })
    .collect::<rusqlite::Result<Vec<_>>>()
    .map_err(Into::into)
}

fn build_split_actions(
    conn: &Connection,
    artist_id: i64,
    artist: &str,
    profile: &Value,
    source_folder: &str,
    items: &[DiscoveredFolderItem],
) -> Result<(String, bool)> {
    let mut actions = Vec::new();
    let mut missing_date = false;
    let tag_cache = TagNameCache::for_artist(conn, artist_id)?;
    for (index, item) in items
        .iter()
        .filter(|item| !item.tags.is_empty())
        .enumerate()
    {
        let raw_date = effective_display_date(
            &item.detected_date,
            item.manual_date.as_deref(),
            &item.legacy_date,
        );
        let Some(_date) = effective_date_key(&raw_date) else {
            missing_date = true;
            continue;
        };
        let tag_names = tag_names_for_ids(conn, artist_id, &item.tags, &tag_cache)?;
        if tag_names.is_empty() {
            continue;
        }
        let ext = Path::new(&item.file_name)
            .extension()
            .and_then(|ext| ext.to_str());
        actions.push(json!({
            "item_id": item.id,
            "source_file_path": item.file_path,
            "source_relative_path": item.file_name,
            "target_folder": render_archive_target(
                profile,
                &ArchiveRenderParams {
                    artist,
                    date: &raw_date,
                    tags: &tag_names,
                    title: &item.file_name,
                    folder: source_folder,
                    index: index + 1,
                    user_id: Some(artist_id),
                    id: Some(item.id),
                    ext,
                },
            )?,
            "target_relative_path": item.file_name,
            "format_index": index + 1,
        }));
    }
    actions.sort_by(|left, right| {
        left["target_folder"]
            .as_str()
            .cmp(&right["target_folder"].as_str())
            .then_with(|| {
                left["source_relative_path"]
                    .as_str()
                    .cmp(&right["source_relative_path"].as_str())
            })
    });
    Ok((Value::Array(actions).to_string(), missing_date))
}

fn split_actions_complete(
    conn: &Connection,
    artist_id: i64,
    source_folder: &str,
    split_actions: &str,
) -> Result<bool> {
    let artist_path = artist_plan_path(conn, artist_id)?;
    let action_ids = serde_json::from_str::<Value>(split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| action["item_id"].as_i64())
        .collect::<HashSet<_>>();
    if action_ids.is_empty() {
        return Ok(false);
    }
    let tagged_ids = conn
        .prepare(
            "SELECT i.id, i.file_path, i.folder_name FROM items i
             WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
               AND EXISTS (SELECT 1 FROM item_tags it WHERE it.item_id=i.id)",
        )?
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .filter_map(|row| match row {
            Ok((id, file_path, folder_name))
                if source_folder_for_item(&artist_path, &file_path, &folder_name).as_deref()
                    == Some(source_folder) =>
            {
                Some(Ok(id))
            }
            Ok(_) => None,
            Err(error) => Some(Err(error)),
        })
        .collect::<rusqlite::Result<HashSet<_>>>()?;
    Ok(action_ids == tagged_ids)
}

fn split_failure_reason(error: &anyhow::Error) -> &str {
    let message = error.to_string();
    [
        "backup_failed",
        "source_missing",
        "target_exists",
        "bad_folder_path",
        "db_update_failed",
        "rollback_failed",
        "outside_artist",
        "stale_split_plan",
        "missing_split_actions",
    ]
    .into_iter()
    .find(|reason| message.starts_with(reason))
    .unwrap_or("execution_failed")
}

fn prepare_split_file_moves(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    artist_path: &str,
    source_folder: &str,
    split_actions: &str,
) -> Result<Vec<SplitFileMove>> {
    let raw_actions = serde_json::from_str::<Value>(split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default();
    if raw_actions.is_empty() {
        bail!("missing_split_actions");
    }
    let artist_root = roots.map_to_real(artist_path)?;
    if !artist_root.is_dir() || !path_under_authorized_roots(&artist_root, roots) {
        bail!("outside_artist");
    }
    let source_folder =
        validate_relative_folder(source_folder).map_err(|_| anyhow!("bad_folder_path"))?;
    let artist: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .map_err(|_| anyhow!("stale_split_plan"))?;
    let (profile, _) =
        archive_profile_for_artist(conn, artist_id).map_err(|_| anyhow!("stale_split_plan"))?;
    let tag_cache = TagNameCache::for_artist(conn, artist_id)?;
    let mut targets = HashSet::new();
    let mut moves = Vec::with_capacity(raw_actions.len());
    for raw in raw_actions {
        let item_id = raw["item_id"]
            .as_i64()
            .filter(|id| *id > 0)
            .ok_or_else(|| anyhow!("stale_split_plan"))?;
        let target_folder_raw = raw["target_folder"]
            .as_str()
            .ok_or_else(|| anyhow!("bad_folder_path"))?;
        let target_folder =
            validate_fixed_target(target_folder_raw).map_err(|_| anyhow!("bad_folder_path"))?;
        let (source_db, file_name, folder_name, manual_date, detected_date, legacy_date): (
            String,
            String,
            String,
            Option<String>,
            String,
            String,
        ) = conn
            .query_row(
                "SELECT file_path, file_name, folder_name, manual_date, detected_date, date
                 FROM items
                 WHERE id=? AND artist_id=? AND COALESCE(missing, 0)=0",
                params![item_id, artist_id],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| anyhow!("source_missing"))?;
        if source_folder_for_item(artist_path, &source_db, &folder_name).as_deref()
            != Some(source_folder.as_str())
        {
            bail!("stale_split_plan");
        }
        if raw["source_file_path"]
            .as_str()
            .is_some_and(|saved| saved != source_db)
        {
            bail!("stale_split_plan");
        }
        let target_relative_raw = raw["target_relative_path"].as_str().unwrap_or(&file_name);
        let target_relative =
            validate_fixed_target(target_relative_raw).map_err(|_| anyhow!("bad_folder_path"))?;
        let raw_date = effective_display_date(&detected_date, manual_date.as_deref(), &legacy_date);
        effective_date_key(&raw_date).ok_or_else(|| anyhow!("stale_split_plan"))?;
        let tag_ids = conn
            .prepare("SELECT tag_id FROM item_tags WHERE item_id=? ORDER BY tag_id")?
            .query_map(params![item_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let tag_names = tag_names_for_ids(conn, artist_id, &tag_ids, &tag_cache)?;
        let format_index = raw["format_index"].as_u64().unwrap_or(1) as usize;
        let ext = Path::new(&file_name)
            .extension()
            .and_then(|ext| ext.to_str());
        if tag_names.is_empty()
            || render_archive_target(
                &profile,
                &ArchiveRenderParams {
                    artist: &artist,
                    date: &raw_date,
                    tags: &tag_names,
                    title: &file_name,
                    folder: &source_folder,
                    index: format_index,
                    user_id: Some(artist_id),
                    id: Some(item_id),
                    ext,
                },
            )? != target_folder
        {
            bail!("stale_split_plan");
        }
        let source = roots.map_to_real(&source_db)?;
        let target_db = PathBuf::from(artist_path)
            .join(&target_folder)
            .join(&target_relative)
            .to_string_lossy()
            .replace('\\', "/");
        let target = roots.map_to_real(&target_db)?;
        // Resume support for an interrupted split: a kill can land after some
        // files were moved but before the ledger transaction recorded them. Those
        // rows still name the old path while the bytes sit at the target, and the
        // move is a no-overwrite rename, so it cannot simply be retried. A scan
        // running in between can also have already repointed the row at the
        // target. Classify both as already moved and let the caller finish the
        // remaining files and commit the ledger — otherwise every retry fails on
        // a source that is gone and the plan can never complete.
        let already_moved = if source == target {
            source.is_file()
        } else {
            !source.is_file() && std::fs::symlink_metadata(&source).is_err() && target.is_file()
        };
        if !already_moved {
            if !source.is_file() {
                bail!("source_missing");
            }
            if !path_under_artist(&source, &artist_root)
                || !target_parent_under_artist(target.parent().unwrap_or(&target), &artist_root)
            {
                bail!("outside_artist");
            }
            if target.exists() {
                bail!("target_exists");
            }
        }
        let target_real = target.to_string_lossy().replace('\\', "/");
        let occupied: bool = conn.query_row(
            "SELECT EXISTS(
               SELECT 1 FROM items
               WHERE id!=? AND COALESCE(missing, 0)=0 AND file_path IN (?, ?)
             )",
            params![item_id, target_db, target_real],
            |row| row.get(0),
        )?;
        if occupied || !targets.insert(target_real) {
            bail!("target_exists");
        }
        moves.push(SplitFileMove {
            item_id,
            source_db,
            target_db,
            source,
            target,
            target_folder,
            already_moved,
        });
    }
    Ok(moves)
}

/// Remove the directories a move has just emptied, and only those.
///
/// The loop decides for itself whether a directory is empty. It cannot read
/// `remove_dir` failing as that answer: the call is documented to refuse a
/// non-empty directory, but nothing guarantees the deletion that actually runs
/// is the operating system's. A filesystem filter driver or a desktop deletion
/// interceptor can implement the same call as a recursive move, and then a
/// folder that still holds files the plan did not select is gone while the call
/// reports success. Reading the directory first makes the answer independent of
/// who implements the delete.
fn remove_empty_parents(paths: impl IntoIterator<Item = PathBuf>, artist_root: &Path) {
    let mut dirs = paths
        .into_iter()
        .filter_map(|path| path.parent().map(Path::to_path_buf))
        .collect::<Vec<_>>();
    dirs.sort_by_key(|path| std::cmp::Reverse(path.components().count()));
    dirs.dedup();
    for mut dir in dirs {
        while dir != artist_root && dir.starts_with(artist_root) {
            if !directory_is_empty(&dir) || std::fs::remove_dir(&dir).is_err() {
                break;
            }
            let Some(parent) = dir.parent() else {
                break;
            };
            dir = parent.to_path_buf();
        }
    }
}

/// Harmless desktop or NAS metadata files/directories that can be safely removed
/// when cleaning up otherwise-empty parent directories.
fn is_harmless_junk_entry(entry_name: &str) -> bool {
    let lower = entry_name.to_ascii_lowercase();
    matches!(
        lower.as_str(),
        ".ds_store"
            | "thumbs.db"
            | "desktop.ini"
            | "ehthumbs.db"
            | "ehthumbs_vista.db"
            | "@eadir"
            | "@synoresource"
            | "@synoeastream"
            | ".spotlight-v100"
            | ".trashes"
            | ".fseventsd"
    ) || lower.starts_with("._")
}

/// Whether `path` is a directory that currently holds no entries, or only holds
/// harmless OS metadata files (such as `.DS_Store`, `Thumbs.db`, `@eaDir`).
///
/// If only harmless metadata entries are present, they are removed so the
/// directory can be cleanly deleted by the caller. An unreadable or absent path
/// answers `false`: the caller uses this to decide what it may delete, and
/// "I could not look" is not a reason to delete.
fn directory_is_empty(path: &Path) -> bool {
    let entries = match std::fs::read_dir(path) {
        Ok(e) => e,
        Err(_) => return false,
    };
    let mut junk_entries = Vec::new();
    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => return false,
        };
        let file_name = entry.file_name();
        let name_str = file_name.to_string_lossy();
        if is_harmless_junk_entry(&name_str) {
            junk_entries.push(entry.path());
        } else {
            return false;
        }
    }
    for junk in junk_entries {
        if junk.is_dir() {
            let _ = std::fs::remove_dir_all(&junk);
        } else {
            let _ = std::fs::remove_file(&junk);
        }
    }
    true
}

fn rollback_split_moves(moved: &[SplitFileMove], roots: &MediaRoots) -> Result<()> {
    let mut failures = Vec::new();
    for file in moved.iter().rev() {
        if let Err(error) =
            move_file_from_authorized_path_no_overwrite(&file.target, &file.source, roots)
        {
            failures.push(error.to_string());
        }
    }
    if failures.is_empty() {
        Ok(())
    } else {
        bail!("rollback_failed: {}", failures.join("; "))
    }
}

fn execute_split_plan(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
    artist_id: i64,
    artist_path: &str,
    source_folder: &str,
    split_actions: &str,
) -> Result<Value> {
    let artist_root = roots.map_to_real(artist_path)?;
    let moves = prepare_split_file_moves(
        conn,
        roots,
        artist_id,
        artist_path,
        source_folder,
        split_actions,
    )?;
    // §6.3: same fence as a rename. The split moves files out of the source
    // directory, so the groups under it are the ones the intent has to cover.
    let mut intent_guard = register_move_intents(
        conn,
        &artist_root.join(source_folder).to_string_lossy(),
        "split_by_tag",
    )?;
    let mut moved = Vec::with_capacity(moves.len());
    for file in &moves {
        // A file the interrupt already moved is not moved again: the ledger
        // below is what makes it official. `prepare_split_file_moves` proves
        // that state (old path gone without a trace, bytes at the target) before
        // setting the flag, so this is a resume, not a guess. Without it every
        // retry failed on a source that no longer exists and the split could
        // never be completed.
        if file.already_moved {
            continue;
        }
        if let Err(error) =
            move_file_to_authorized_path_no_overwrite(&file.source, &file.target, roots)
        {
            if let Err(rollback_error) = rollback_split_moves(&moved, roots) {
                bail!("rollback_failed: {error}; {rollback_error}");
            }
            remove_empty_parents(moved.iter().map(|file| file.target.clone()), &artist_root);
            bail!("execution_failed: {error}");
        }
        moved.push(file.clone());
    }

    let db_result = (|| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        for file in &moves {
            let folder_name = Path::new(&file.target_folder)
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| anyhow!("bad_folder_path"))?;
            let changed = tx.execute(
                "UPDATE items SET file_path=?, folder_name=?
                 WHERE id=? AND artist_id=? AND file_path=? AND COALESCE(missing, 0)=0",
                params![
                    file.target_db,
                    folder_name,
                    file.item_id,
                    artist_id,
                    file.source_db,
                ],
            )?;
            if changed == 1 {
                continue;
            }
            // A resume can meet a row a scan already repointed at the target:
            // then there is nothing left to record, and the target path is the
            // evidence. A row naming anything else is a genuine conflict.
            let current: Option<String> = tx
                .query_row(
                    "SELECT file_path FROM items WHERE id=? AND artist_id=? AND COALESCE(missing, 0)=0",
                    params![file.item_id, artist_id],
                    |row| row.get(0),
                )
                .optional()?;
            if current.as_deref() != Some(file.target_db.as_str()) {
                bail!("stale_state");
            }
        }
        // Split moves members out of the source directory, so the groups that
        // owned them have to be re-pointed in this transaction. Deleting the
        // plan right after is exactly the "successful plan deletion must not
        // lose the mapping" case.
        let moved = moves
            .iter()
            .map(|file| (file.source_db.clone(), file.target_db.clone()))
            .collect::<Vec<_>>();
        crate::pawchive_groups::relocate_groups_in_tx(
            &tx,
            &artist_root.to_string_lossy(),
            &moved,
            "split_by_tag",
        )?;
        let changed = tx.execute(
            "DELETE FROM folder_rename_plans
             WHERE id=? AND status='confirmed' AND plan_kind='split_by_tag'
               AND source_folder=? AND split_actions=?",
            params![plan_id, source_folder, split_actions],
        )?;
        if changed != 1 {
            bail!("stale_state");
        }
        tx.commit()?;
        Ok(moves.len() as i64)
    })();
    let updated_items = match db_result {
        Ok(updated) => updated,
        Err(error) => {
            rollback_split_moves(&moved, roots)
                .with_context(|| format!("rollback_failed: database update failed: {error}"))?;
            remove_empty_parents(moved.iter().map(|file| file.target.clone()), &artist_root);
            bail!("db_update_failed: {error}");
        }
    };
    // Only directories this run actually emptied: an already-moved entry left
    // its source directory in the interrupted run, which is the run that should
    // have pruned it (and its move is what `moved` records for rollback).
    remove_empty_parents(
        moves
            .iter()
            .filter(|file| !file.already_moved)
            .map(|file| file.source.clone()),
        &artist_root,
    );
    intent_guard.finish(crate::pawchive_groups::GROUP_MOVE_INTENT_APPLIED, "");
    Ok(json!({
        "plan_id": plan_id,
        "status": "executed",
        "kind": "split_by_tag",
        "source": source_folder,
        "targets": moves
            .iter()
            .map(|file| file.target_folder.clone())
            .collect::<BTreeSet<_>>(),
        "updated_items": updated_items,
    }))
}

/// Recompute the fixed target and status of every unexecuted plan of one
/// artist from the current effective item dates. Plans without any effective
/// date become `needs_date`; plans whose active items span several dates
/// become `date_conflict`; a folder already at its derived target becomes
/// `aligned`; source-and-target-missing legacy plans become hidden `stale`
/// records; other physical conflicts become `manual_review`. Confirmed,
/// executed and reverted plans are left untouched; date edits already demoted
/// stale confirmations via `invalidate_plans_after_item_date_change`.
pub fn recompute_artist_plan_targets(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    artist_id: i64,
) -> Result<usize> {
    ensure_folder_schema(conn)?;
    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?;
    let artist_path = artist_path.unwrap_or_default();
    let artist_name: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or_default();
    let (archive_profile, profile_source) = archive_profile_for_artist(conn, artist_id)?;
    let tag_tables_ready: bool = conn
        .query_row(
            "SELECT COUNT(*)=2 FROM sqlite_master
             WHERE type='table' AND name IN ('tags','item_tags')",
            [],
            |row| row.get(0),
        )
        .unwrap_or(false);
    let requires_tags = tag_tables_ready
        && archive_profile["template"]
            .as_str()
            .map(|t| t.contains("{tags}"))
            .unwrap_or(true);
    let artist_root = roots.and_then(|roots| {
        roots
            .map_to_real(&artist_path)
            .ok()
            .filter(|root| path_under_authorized_roots(root, roots))
    });
    let suffix_collisions = archive_profile["collision_strategy"].as_str() == Some("suffix");
    let merge_targets = archive_profile["collision_strategy"].as_str() == Some("merge");
    let mut seen_targets = HashSet::new();
    let mut stmt = conn.prepare(
        "SELECT id, source_folder, original_title, selected_tag_ids, status, plan_kind, split_actions, format_snapshot, parsed_date
         FROM folder_rename_plans
         WHERE artist_id=? AND status NOT IN ('confirmed','executed','reverted')
         ORDER BY id",
    )?;
    let plans = stmt
        .query_map(params![artist_id], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, String>(8)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    // One items pass for every plan: recompute only rewrites plan targets,
    // never item paths, so the grouping stays valid across the loop.
    let item_dates = ItemDatesByFolder::build(conn, artist_id)?;
    let mut changed = 0usize;
    for (
        plan_id,
        source_folder,
        original_title,
        selected_json,
        _status,
        plan_kind,
        split_actions,
        existing_snapshot,
        parsed_date,
    ) in plans
    {
        let row_format_snapshot = {
            let mut snapshot_value =
                archive_format::rule_snapshot(&archive_profile, &profile_source);
            if let Ok(existing_val) = serde_json::from_str::<Value>(&existing_snapshot) {
                if let Some(keep) = existing_val.get("keep_together") {
                    if let Some(obj) = snapshot_value.as_object_mut() {
                        obj.insert("keep_together".to_string(), keep.clone());
                    }
                }
            }
            snapshot_value.to_string()
        };

        if plan_kind == "split_by_tag" {
            let complete = split_actions_complete(conn, artist_id, &source_folder, &split_actions)?;
            let new_status = if !complete {
                "needs_date"
            } else if let Some(roots) = roots {
                match prepare_split_file_moves(
                    conn,
                    roots,
                    artist_id,
                    &artist_path,
                    &source_folder,
                    &split_actions,
                ) {
                    Ok(_) => "ready",
                    Err(_) => "manual_review",
                }
            } else {
                "ready"
            };
            changed += conn.execute(
                "UPDATE folder_rename_plans
                 SET target_folder='', status=?, confirmed_at=NULL, confirmation_source='',
                     format_snapshot=?, updated_at=?
                 WHERE id=? AND status NOT IN ('confirmed','executed','reverted')",
                params![new_status, row_format_snapshot, now(), plan_id],
            )? as usize;
            continue;
        }
        let mut dates = item_dates.get(&source_folder);
        if dates.is_empty() && !parsed_date.is_empty() {
            if let Some(date_key) = effective_date_key(&parsed_date) {
                dates.push(date_key);
            }
        }
        let tags = plan_tag_names(conn, artist_id, &source_folder, &selected_json)?;
        let mut target =
            if (requires_tags && tags.is_empty()) || dates.is_empty() || dates.len() > 1 {
                String::new()
            } else {
                render_archive_target(
                    &archive_profile,
                    &ArchiveRenderParams {
                        artist: &artist_name,
                        date: &dates[0],
                        tags: &tags,
                        title: &original_title,
                        folder: &source_folder,
                        index: plan_id as usize,
                        user_id: Some(artist_id),
                        id: Some(plan_id),
                        ext: None,
                    },
                )?
            };
        if suffix_collisions && !target.is_empty() && target != source_folder {
            let requested_target = target.clone();
            let mut number = 2usize;
            if is_suffixed_archive_target(&source_folder, &requested_target) {
                target = suffixed_archive_target(&requested_target, number);
            }
            while seen_targets.contains(&target.to_ascii_lowercase())
                || (target != source_folder
                    && artist_root
                        .as_ref()
                        .is_some_and(|root| root.join(&target).exists()))
            {
                target = suffixed_archive_target(&requested_target, number);
                number += 1;
            }
        }
        if tag_tables_ready && merge_targets && !target.is_empty() && target != source_folder {
            let items = source_folder_items(conn, artist_id, &artist_path, &source_folder)?;
            let (merge_actions, missing_date) = build_split_actions(
                conn,
                artist_id,
                &artist_name,
                &archive_profile,
                &source_folder,
                &items,
            )?;
            let complete = !missing_date
                && split_actions_complete(conn, artist_id, &source_folder, &merge_actions)?;
            let new_status = if !complete {
                "needs_date"
            } else if let Some(roots) = roots {
                match prepare_split_file_moves(
                    conn,
                    roots,
                    artist_id,
                    &artist_path,
                    &source_folder,
                    &merge_actions,
                ) {
                    Ok(_) => "ready",
                    Err(_) => "manual_review",
                }
            } else {
                "ready"
            };
            changed += conn.execute(
                "UPDATE folder_rename_plans
                 SET target_folder='', status=?, plan_kind='split_by_tag', split_actions=?,
                     confirmed_at=NULL, confirmation_source='', format_snapshot=?, updated_at=?
                 WHERE id=? AND status NOT IN ('confirmed','executed','reverted')",
                params![
                    new_status,
                    merge_actions,
                    row_format_snapshot,
                    now(),
                    plan_id,
                ],
            )? as usize;
            continue;
        }
        if !target.is_empty() {
            seen_targets.insert(target.to_ascii_lowercase());
        }
        let new_status = if requires_tags && tags.is_empty() {
            "needs_tags"
        } else if dates.is_empty() {
            "needs_date"
        } else if dates.len() > 1 {
            "date_conflict"
        } else if let Some(roots) = roots {
            if target.is_empty() {
                "needs_date"
            } else {
                match evaluate_plan_paths(roots, artist_id, &artist_path, &source_folder, &target) {
                    Ok(check) if source_folder == target && check.source_exists => "aligned",
                    Ok(check)
                        if !check.source_exists
                            && !check.target_exists
                            && roots
                                .map_to_real(&artist_path)
                                .ok()
                                .is_some_and(|root| path_under_authorized_roots(&root, roots)) =>
                    {
                        "stale"
                    }
                    Ok(check) if check.reason.is_some() => "manual_review",
                    Ok(_) => "ready",
                    Err(_) => "manual_review",
                }
            }
        } else if source_folder == target {
            "aligned"
        } else {
            "ready"
        };
        let n = conn.execute(
            "UPDATE folder_rename_plans
             SET target_folder=?, status=?, confirmed_at=NULL, confirmation_source='',
                 format_snapshot=?, updated_at=?
             WHERE id=? AND status NOT IN ('confirmed','executed','reverted')",
            params![target, new_status, row_format_snapshot, now(), plan_id],
        )?;
        changed += n as usize;
    }
    Ok(changed)
}

pub fn auto_discover_artist_folder_plans(conn: &Connection, artist_id: i64) -> Result<()> {
    ensure_folder_schema(conn)?;
    let query_only = conn
        .query_row("PRAGMA query_only", [], |row| row.get::<_, i64>(0))
        .unwrap_or(0)
        != 0;
    if query_only {
        return Ok(());
    }
    let tag_tables_ready: bool = conn.query_row(
        "SELECT COUNT(*)=2 FROM sqlite_master
         WHERE type='table' AND name IN ('tags','item_tags')",
        [],
        |row| row.get(0),
    )?;
    if !tag_tables_ready {
        return Ok(());
    }
    let artist_path = artist_plan_path(conn, artist_id)?;
    let artist_name: String = conn.query_row(
        "SELECT name FROM artists WHERE id=?",
        params![artist_id],
        |row| row.get(0),
    )?;
    let (archive_profile, _) = archive_profile_for_artist(conn, artist_id)?;
    let mut stmt = conn.prepare(
        "SELECT i.file_path, i.folder_name, i.id, i.file_name, i.manual_date,
                COALESCE(i.detected_date, ''), COALESCE(i.date, ''), (
             SELECT json_group_array(it.tag_id)
             FROM (SELECT it.tag_id FROM item_tags it WHERE it.item_id=i.id ORDER BY it.tag_id) it
         )
         FROM items i
         WHERE i.artist_id=? AND COALESCE(i.missing, 0)=0
         ORDER BY i.id",
    )?;
    let discovered_items: Vec<(String, DiscoveredFolderItem)> = stmt
        .query_map(params![artist_id], |row| {
            let raw: Option<String> = row.get(7)?;
            let tags: Vec<i64> = raw
                .and_then(|value| serde_json::from_str::<Vec<i64>>(&value).ok())
                .unwrap_or_default();
            Ok((
                row.get(1)?,
                DiscoveredFolderItem {
                    id: row.get(2)?,
                    file_path: row.get(0)?,
                    file_name: row.get(3)?,
                    folder_name: row.get(1)?,
                    manual_date: row.get(4)?,
                    detected_date: row.get(5)?,
                    legacy_date: row.get(6)?,
                    tags,
                },
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut folders = BTreeMap::<String, (String, Vec<DiscoveredFolderItem>)>::new();
    for (folder_name, item) in discovered_items {
        let Some(folder) = source_folder_for_item(&artist_path, &item.file_path, &folder_name)
        else {
            continue;
        };
        let entry = folders
            .entry(folder)
            .or_insert_with(|| (item.legacy_date.clone(), Vec::new()));
        if item.legacy_date < entry.0 {
            entry.0 = item.legacy_date.clone();
        }
        entry.1.push(item);
    }

    for folder in folders.keys().filter(|folder| folder.contains('/')) {
        let legacy_folder = folder.rsplit('/').next().unwrap_or(folder);
        // Legacy plans stored a bare folder name, and this cleanup used to match
        // `source_folder=legacy_folder` alone. With both `pack` and
        // `incoming/pack` in the library that also deleted the *top-level*
        // `pack` plan, which the insert below then recreated as a bare draft —
        // losing its chosen target, its status and its execution_log on every
        // discovery. The boundary keeps the top-level name out of it.
        conn.execute(
            "DELETE FROM folder_rename_plans
             WHERE artist_id=?1 AND status NOT IN ('confirmed', 'executed', 'reverted')
               AND (source_folder=?2 OR source_folder LIKE ?3 ESCAPE '\\')",
            params![
                artist_id,
                legacy_folder,
                format!(
                    "%/{}",
                    legacy_folder
                        .replace('\\', "\\\\")
                        .replace('%', "\\%")
                        .replace('_', "\\_")
                )
            ],
        )?;
    }

    for (folder, (min_date, items)) in folders {
        if folder.trim().is_empty() {
            continue;
        }
        let mut parsed_date = crate::media_type::extract_date_from_folder(&folder);
        // Legacy dates may hold junk text: keep only a leading, calendar-valid
        // date (`date_head_from_raw` slices without splitting a multi-byte char
        // and rejects impossible dates like `2024-13-99`).
        if parsed_date.is_empty() {
            if let Some(head) = date_head_from_raw(&min_date) {
                parsed_date = head;
            }
        }
        if parsed_date.is_empty() {
            for it in &items {
                let candidate = if let Some(m) = it.manual_date.as_deref().filter(|s| !s.is_empty())
                {
                    m
                } else if !it.detected_date.is_empty() {
                    &it.detected_date
                } else {
                    &it.legacy_date
                };
                if let Some(head) = date_head_from_raw(candidate) {
                    parsed_date = head;
                    break;
                }
            }
        }

        let total_items = items.len();
        if total_items == 0 {
            continue;
        }

        let existing: Option<(i64, String, String, String, i64, String, String)> = conn
            .query_row(
                "SELECT id, status, parsed_date, selected_tag_ids, file_count, target_folder, format_snapshot
                 FROM folder_rename_plans
                 WHERE artist_id=? AND source_folder=?",
                params![artist_id, folder],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                        row.get(6)?,
                    ))
                },
            )
            .optional()?;

        let is_locked_keep_together = existing
            .as_ref()
            .map(|(_, _, _, _, _, _, snapshot)| {
                serde_json::from_str::<Value>(snapshot)
                    .ok()
                    .and_then(|v| v.get("keep_together").and_then(|k| k.as_bool()))
                    .unwrap_or(false)
            })
            .unwrap_or(false);

        let has_any_tags = items.iter().any(|item| !item.tags.is_empty());
        let (plan_kind, selected_tag_ids_json, split_actions, initial_status) = if !has_any_tags {
            (
                "rename_folder",
                "[]".to_string(),
                "[]".to_string(),
                "needs_tags",
            )
        } else {
            let mut groups = BTreeSet::new();
            let mut union_set = BTreeSet::new();
            for item in items.iter().filter(|item| !item.tags.is_empty()) {
                let raw_date = effective_display_date(
                    &item.detected_date,
                    item.manual_date.as_deref(),
                    &item.legacy_date,
                );
                groups.insert((effective_date_key(&raw_date), item.tags.clone()));
                for tag_id in &item.tags {
                    union_set.insert(*tag_id);
                }
            }
            let split_needed = !is_locked_keep_together
                && (items.iter().any(|item| item.tags.is_empty()) || groups.len() > 1);
            let plan_kind = if split_needed {
                "split_by_tag"
            } else {
                "rename_folder"
            };
            let selected_tag_ids = union_set.into_iter().collect::<Vec<_>>();
            let selected_tag_ids_json = serde_json::to_string(&selected_tag_ids)?;
            let (split_actions, split_missing_date) = if split_needed {
                build_split_actions(
                    conn,
                    artist_id,
                    &artist_name,
                    &archive_profile,
                    &folder,
                    &items,
                )?
            } else {
                ("[]".to_string(), false)
            };
            let initial_status = if split_missing_date {
                "needs_date"
            } else {
                "draft"
            };
            (
                plan_kind,
                selected_tag_ids_json,
                split_actions,
                initial_status,
            )
        };

        match existing {
            None => {
                let _ = conn.execute(
                    "INSERT INTO folder_rename_plans
                     (artist_id, source_folder, original_folder_name, original_title, parsed_date,
                     selected_tag_ids, status, file_count, target_folder, execution_log, plan_kind,
                     split_actions, confirmation_source, updated_at)
                     VALUES (?, ?, ?, ?, ?, ?, ?, ?, '', '[]', ?, ?, '', ?)",
                    params![
                        artist_id,
                        folder,
                        folder,
                        folder,
                        parsed_date,
                        selected_tag_ids_json,
                        initial_status,
                        total_items as i64,
                        plan_kind,
                        split_actions,
                        now()
                    ],
                );
            }
            Some((plan_id, status, old_date, _old_tags, _old_count, _old_target, _)) => {
                if status != "confirmed" && status != "executed" {
                    let final_date = if old_date.is_empty() {
                        parsed_date
                    } else {
                        old_date
                    };
                    let _ = conn.execute(
                        "UPDATE folder_rename_plans
                         SET selected_tag_ids=?, parsed_date=?, file_count=?, status=?, target_folder='',
                             plan_kind=?, split_actions=?, updated_at=?
                         WHERE id=?",
                        params![
                            selected_tag_ids_json,
                            final_date,
                            total_items as i64,
                            initial_status,
                            plan_kind,
                            split_actions,
                            now(),
                            plan_id,
                        ],
                    );
                }
            }
        }
    }
    Ok(())
}

/// List folder rename plans.
///
/// With `refresh=false` this is a pure read: no DDL, no auto-discovery, no
/// target recompute. Read-only pools can therefore serve the list, and a GET
/// never mutates data. Pass `refresh=true` (explicit refresh endpoint) to run
/// discovery and target recomputation for one artist first.
pub fn list_folder_renames(
    conn: &Connection,
    roots: Option<&MediaRoots>,
    artist_id: Option<i64>,
    refresh: bool,
) -> Result<Value> {
    if refresh {
        if let Some(aid) = artist_id {
            if let Err(error) = auto_discover_artist_folder_plans(conn, aid) {
                log_error!("folder rename auto-discover failed for artist {aid}: {error:#}");
            }
            if let Err(error) = recompute_artist_plan_targets(conn, roots, aid) {
                log_error!("folder rename target recompute failed for artist {aid}: {error:#}");
            }
        }
    }
    let mut sql = String::from(
        "SELECT id, artist_id, source_folder, target_folder, status, plan_kind, file_count,
                selected_tag_ids, parsed_date, execution_log, confirmed_at, executed_at,
                split_actions
         FROM folder_rename_plans",
    );
    let mut plans = Vec::new();
    let (total, truncated) = if let Some(aid) = artist_id {
        sql.push_str(" WHERE artist_id=? AND status!='stale' ORDER BY id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![aid], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans WHERE artist_id=? AND status!='stale'",
            params![aid],
            |row| row.get(0),
        )?;
        (total, false)
    } else {
        sql.push_str(&format!(
            " WHERE status!='stale' ORDER BY id DESC LIMIT {}",
            crate::MAX_PAGINATION_LIMIT
        ));
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([], map_plan)?;
        for row in rows {
            plans.push(row?);
        }
        // `total` used to be `plans.len()`, which reported the page size as the
        // library's total and hid the truncation — while 全部执行 auto-confirms
        // and runs every 'ready' plan, including ones this list never showed.
        // Report the real count and say when the page is short.
        let total: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans WHERE status!='stale'",
            [],
            |row| row.get(0),
        )?;
        (total, total > plans.len() as i64)
    };
    Ok(json!({
        "plans": plans,
        "total": total,
        "truncated": truncated,
        "limit": crate::MAX_PAGINATION_LIMIT
    }))
}

fn map_plan(r: &rusqlite::Row<'_>) -> rusqlite::Result<Value> {
    let split_actions = r.get::<_, String>(12)?;
    let target_folders = serde_json::from_str::<Value>(&split_actions)
        .ok()
        .and_then(|value| value.as_array().cloned())
        .unwrap_or_default()
        .into_iter()
        .filter_map(|action| action["target_folder"].as_str().map(str::to_string))
        .collect::<BTreeSet<_>>();
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
        "target_folders": target_folders,
        "split_action_count": serde_json::from_str::<Value>(&split_actions)
            .ok()
            .and_then(|value| value.as_array().map(Vec::len))
            .unwrap_or(0),
    }))
}

pub fn upsert_folder_rename_plans(
    conn: &Connection,
    artist_id: i64,
    plans: &[Value],
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let tx = conn.unchecked_transaction()?;
    let artist_exists = tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM artists WHERE id=?)",
        [artist_id],
        |row| row.get::<_, bool>(0),
    )?;
    if !artist_exists {
        bail!("invalid folder rename plan: artist does not exist");
    }
    let mut upserted = 0i64;
    for plan in plans {
        let source_raw = plan
            .get("source_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if source_raw.is_empty() {
            bail!("invalid folder rename plan: source_folder is required");
        }
        let source = validate_relative_folder(source_raw).with_context(|| {
            format!("invalid folder rename plan: invalid source_folder {source_raw:?}")
        })?;
        let target_raw = plan
            .get("target_folder")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let target = if target_raw.is_empty() {
            String::new()
        } else {
            validate_relative_folder(target_raw).with_context(|| {
                format!("invalid folder rename plan: invalid target_folder {target_raw:?}")
            })?
        };
        let status = plan
            .get("status")
            .and_then(|v| v.as_str())
            .unwrap_or("needs_tags");
        if !matches!(
            status,
            "draft" | "needs_tags" | "ready" | "manual_review" | "inconsistent_tags"
        ) {
            bail!("invalid folder rename plan: invalid editable status");
        }
        let existing_status: Option<String> = tx
            .query_row(
                "SELECT status FROM folder_rename_plans WHERE artist_id=? AND source_folder=?",
                params![artist_id, source],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(ref st) = existing_status {
            if st == "inconsistent_tags" && !target.is_empty() {
                bail!("invalid folder rename plan: folder tags are inconsistent, cannot modify target");
            }
        }
        let parsed_tags = match plan.get("selected_tag_ids") {
            Some(Value::String(value)) => serde_json::from_str(value).with_context(|| {
                "invalid folder rename plan: selected_tag_ids must be a JSON array"
            })?,
            Some(Value::Array(value)) => Value::Array(value.clone()),
            Some(_) => {
                bail!("invalid folder rename plan: selected_tag_ids must be an array")
            }
            None => Value::Array(Vec::new()),
        };
        let tag_values = parsed_tags.as_array().ok_or_else(|| {
            anyhow!("invalid folder rename plan: selected_tag_ids must be an array")
        })?;
        let mut tag_ids = Vec::with_capacity(tag_values.len());
        let mut seen = HashSet::new();
        for value in tag_values {
            let tag_id = value.as_i64().filter(|id| *id > 0).ok_or_else(|| {
                anyhow!(
                    "invalid folder rename plan: selected_tag_ids must contain positive integers"
                )
            })?;
            if !seen.insert(tag_id) {
                continue;
            }
            let owned = tx.query_row(
                "SELECT EXISTS(SELECT 1 FROM tags WHERE id=? AND artist_id=?)",
                params![tag_id, artist_id],
                |row| row.get::<_, bool>(0),
            )?;
            if !owned {
                bail!(
                    "invalid folder rename plan: tag {tag_id} does not belong to artist {artist_id}"
                );
            }
            tag_ids.push(tag_id);
        }
        let tags = serde_json::to_string(&tag_ids)?;
        upserted += tx.execute(
            "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status, selected_tag_ids, updated_at)
             VALUES (?,?,?,?,?,?)
             ON CONFLICT(artist_id, source_folder) DO UPDATE SET
               target_folder=excluded.target_folder,
               status=excluded.status,
               selected_tag_ids=excluded.selected_tag_ids,
               updated_at=excluded.updated_at
             WHERE folder_rename_plans.status NOT IN ('confirmed','executed')",
            params![artist_id, source, target, status, tags, now()],
        )? as i64;
    }
    tx.commit()?;
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

/// Read-only auto-archive toggle lookup.
///
/// The historical `folder_rename_auto` key is read as a fallback but never
/// rewritten here: GET responses must not perform settings migration or any
/// other DML. Canonicalization happens only in `set_folder_rename_auto`.
pub fn folder_rename_auto_enabled(conn: &Connection) -> Result<bool> {
    let table_exists = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='app_settings')",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !table_exists {
        return Ok(false);
    }
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
    Ok(legacy
        .as_deref()
        .map(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
        .unwrap_or(false))
}

/// Holds the §6.3 move intents a tidy execution registered, and finishes them
/// however the execution ends.
///
/// The drop is the point: a rename has several exit paths, and an intent left
/// `pending` by a crash or an early return would refuse every publish into that
/// group until somebody noticed. A failed intent is kept on purpose — the plan
/// asks for a recoverable one — so only a successful move clears it.
struct MoveIntentGuard<'a> {
    conn: &'a Connection,
    groups: Vec<String>,
    finished: bool,
    _operation_lock: Option<std::fs::File>,
}

impl MoveIntentGuard<'_> {
    fn finish(&mut self, state: &str, reason: &str) {
        if self.finished {
            return;
        }
        for group_id in &self.groups {
            let _ = crate::pawchive_groups::finish_group_move_intent(
                self.conn, group_id, state, reason,
            );
        }
        self.finished = true;
    }
}

impl Drop for MoveIntentGuard<'_> {
    fn drop(&mut self) {
        self.finish(
            crate::pawchive_groups::GROUP_MOVE_INTENT_FAILED,
            "整理未正常结束",
        );
    }
}

/// Register a move intent for every group whose root is `dir` or sits under it,
/// committed before the filesystem is touched.
///
/// Refuses the whole tidy when a publish holds a reservation for one of them:
/// §6.3 says 整理 must wait or explicitly block an active publish, and this call
/// cannot wait for another worker, so it blocks with the reason. Any intent
/// already registered here is rolled back before the error is returned.
fn register_move_intents<'a>(
    conn: &'a Connection,
    dir: &str,
    source_operation: &str,
) -> Result<MoveIntentGuard<'a>> {
    let operation_lock = crate::pawchive_groups::lock_group_operations(conn, true)?;
    let groups = crate::pawchive_groups::groups_under_path(conn, dir)?;
    let mut registered: Vec<String> = Vec::new();
    for group in &groups {
        // The lock above is what makes this reclaim honest: a pending intent
        // with nobody holding the lock cannot belong to a worker that is still
        // moving anything. Without it, a tidy killed between the rename and its
        // commit keeps its group fenced off for a full lease, and the recovery
        // this same function is asked to perform is refused until it lapses.
        let _ = crate::pawchive_groups::reclaim_orphaned_move_intent(conn, &group.group_id);
        if let Err(error) =
            crate::pawchive_groups::begin_group_move_intent(conn, &group.group_id, source_operation)
        {
            for group_id in &registered {
                let _ = crate::pawchive_groups::finish_group_move_intent(
                    conn,
                    group_id,
                    crate::pawchive_groups::GROUP_MOVE_INTENT_FAILED,
                    "同组整理登记未完成",
                );
            }
            return Err(error);
        }
        registered.push(group.group_id.clone());
    }
    Ok(MoveIntentGuard {
        conn,
        groups: registered,
        finished: false,
        _operation_lock: operation_lock,
    })
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
    let artist_name: String = conn.query_row(
        "SELECT name FROM artists WHERE id=?",
        params![artist_id],
        |r| r.get(0),
    )?;
    let artist_root = roots.map_to_real(&artist_path)?;
    let plans: Vec<(i64, String, String, String, String, i64)> = conn
        .prepare(
            "SELECT id, source_folder, target_folder, plan_kind, split_actions, file_count
             FROM folder_rename_plans
             WHERE artist_id=? AND status='confirmed'
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
        )?
        .query_map(params![artist_id], |r| {
            Ok((
                r.get(0)?,
                r.get(1)?,
                r.get(2)?,
                r.get(3)?,
                r.get(4)?,
                r.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<_>>()?;

    if !path_under_authorized_roots(&artist_root, roots) {
        let results = plans
            .iter()
            .map(|(id, source, target, _, _, _)| {
                if !dry_run {
                    record_plan_failure_or_log(conn, *id, "outside_artist", source, target, None);
                }
                json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": "outside_artist",
                    "source": source,
                    "target": target,
                })
            })
            .collect::<Vec<_>>();
        return Ok(json!({
            "ok": false,
            "dry_run": dry_run,
            "backup": "",
            "results": results,
        }));
    }

    let mut backup_path = backup_override.unwrap_or("").to_string();
    if !dry_run && !plans.is_empty() && backup_path.is_empty() {
        match create_db_backup(conn) {
            Ok(path) => backup_path = path,
            Err(error) => {
                for (id, source, target, _, _, _) in &plans {
                    record_plan_failure_or_log(
                        conn,
                        *id,
                        "backup_failed",
                        source,
                        target,
                        Some(json!({"error": error.to_string()})),
                    );
                }
                return Ok(json!({
                    "ok": false,
                    "dry_run": false,
                    "backup": "",
                    "results": plans.into_iter().map(|(id, source, target, _, _, _)| json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": "backup_failed",
                        "source": source,
                        "target": target,
                        "error": error.to_string()
                    })).collect::<Vec<_>>()
                }));
            }
        }
    }

    let mut executed = Vec::new();
    // Lazily built per-artist item-date grouping; executed plans rewrite item
    // paths, so the cache is dropped after every plan that moved files.
    let mut item_dates_cache: Option<ItemDatesByFolder> = None;
    for (id, source_raw, target_raw, plan_kind, split_actions, file_count) in plans {
        if plan_kind == "split_by_tag" {
            if dry_run {
                match prepare_split_file_moves(
                    conn,
                    roots,
                    artist_id,
                    &artist_path,
                    &source_raw,
                    &split_actions,
                ) {
                    Ok(files) => executed.push(json!({
                        "plan_id": id,
                        "status": "dry_run",
                        "kind": "split_by_tag",
                        "source": source_raw,
                        "targets": files
                            .iter()
                            .map(|file| file.target_folder.clone())
                            .collect::<BTreeSet<_>>(),
                        "file_count": files.len(),
                    })),
                    Err(error) => executed.push(json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": split_failure_reason(&error),
                        "source": source_raw,
                        "error": error.to_string(),
                    })),
                }
                continue;
            }
            match execute_split_plan(
                conn,
                roots,
                id,
                artist_id,
                &artist_path,
                &source_raw,
                &split_actions,
            ) {
                Ok(result) => {
                    item_dates_cache = None;
                    executed.push(result);
                }
                Err(error) => {
                    let reason = split_failure_reason(&error);
                    record_plan_failure_or_log(
                        conn,
                        id,
                        reason,
                        &source_raw,
                        "",
                        Some(json!({"error": error.to_string()})),
                    );
                    executed.push(json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": reason,
                        "source": source_raw,
                        "error": error.to_string(),
                    }));
                }
            }
            continue;
        }
        let source = match validate_relative_folder(&source_raw) {
            Ok(v) => v,
            Err(_) => {
                if !dry_run {
                    record_plan_failure_or_log(
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
                    record_plan_failure_or_log(
                        conn,
                        id,
                        "bad_folder_path",
                        &source,
                        &target_raw,
                        None,
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
        let src_logical = folder_db_path(&artist_path, &source);
        let dst_logical = folder_db_path(&artist_path, &target);

        // Revalidate targets produced by a saved format snapshot. Plans from
        // before custom naming had no snapshot and retain their confirmed path.
        // This block runs per-plan after earlier plans in the same artist may
        // already have executed and committed, so an error inside it must
        // demote this one plan — never abort the whole run mid-artist.
        if !dry_run {
            let recheck = (|| -> anyhow::Result<bool> {
                if item_dates_cache.is_none() {
                    item_dates_cache = Some(ItemDatesByFolder::build(conn, artist_id)?);
                }
                let mut dates = item_dates_cache
                    .as_ref()
                    .map(|cache| cache.get(&source_raw))
                    .unwrap_or_default();
                let (selected, format_snapshot, parsed_date): (String, String, String) = conn
                    .query_row(
                        "SELECT selected_tag_ids, format_snapshot, parsed_date
                         FROM folder_rename_plans WHERE id=?",
                        params![id],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )?;
                // Same fallback as `recompute_artist_plan_targets`: a plan whose
                // only date is the stored `parsed_date` (folder-name date) is
                // executable here too. Without this mirror the plan rendered
                // `ready` with a target and then bounced back to `needs_date`
                // on every execution attempt.
                if dates.is_empty() && !parsed_date.is_empty() {
                    if let Some(date_key) = effective_date_key(&parsed_date) {
                        dates.push(date_key);
                    }
                }
                if dates.is_empty() {
                    demote_plan_with_log(conn, id, "needs_date", "", "needs_date", &source_raw)?;
                    executed.push(json!({
                        "plan_id": id, "status": "error", "reason": "needs_date",
                        "source": source_raw, "target": target_raw,
                    }));
                    return Ok(false);
                }
                if dates.len() > 1 {
                    demote_plan_with_log(
                        conn,
                        id,
                        "date_conflict",
                        "",
                        "date_conflict",
                        &source_raw,
                    )?;
                    executed.push(json!({
                        "plan_id": id, "status": "error", "reason": "date_conflict",
                        "source": source_raw, "target": target_raw,
                    }));
                    return Ok(false);
                }
                let profile = serde_json::from_str::<Value>(&format_snapshot)
                    .ok()
                    .and_then(|snapshot| snapshot.get("profile").cloned());
                if let Some(profile) = profile {
                    let tags = plan_tag_names(conn, artist_id, &source_raw, &selected)?;
                    let original_title: String = conn.query_row(
                        "SELECT original_title FROM folder_rename_plans WHERE id=?",
                        params![id],
                        |row| row.get(0),
                    )?;
                    match render_archive_target(
                        &profile,
                        &ArchiveRenderParams {
                            artist: &artist_name,
                            date: &dates[0],
                            tags: &tags,
                            title: &original_title,
                            folder: &source_raw,
                            index: id as usize,
                            user_id: Some(artist_id),
                            id: Some(id),
                            ext: None,
                        },
                    ) {
                        Ok(derived)
                            if derived == target
                                || (profile["collision_strategy"].as_str() == Some("suffix")
                                    && is_suffixed_archive_target(&target, &derived)) => {}
                        Ok(derived) => {
                            demote_plan_with_log(
                                conn,
                                id,
                                "ready",
                                &derived,
                                "stale_target",
                                &source_raw,
                            )?;
                            executed.push(json!({
                                "plan_id": id, "status": "error", "reason": "stale_target",
                                "source": source_raw, "target": target_raw, "target_folder": derived,
                            }));
                            return Ok(false);
                        }
                        Err(error) => {
                            record_plan_execution_failure(
                                conn,
                                id,
                                "bad_target",
                                &source_raw,
                                &target_raw,
                                Some(json!({"error": error.to_string()})),
                            )?;
                            executed.push(json!({
                                "plan_id": id, "status": "error", "reason": "bad_target",
                                "source": source_raw, "target": target_raw, "error": error.to_string(),
                            }));
                            return Ok(false);
                        }
                    }
                }
                Ok(true)
            })();
            match recheck {
                Ok(true) => {}
                Ok(false) => continue,
                Err(error) => {
                    record_plan_failure_or_log(
                        conn,
                        id,
                        "revalidation_failed",
                        &source_raw,
                        &target_raw,
                        Some(json!({"error": error.to_string()})),
                    );
                    executed.push(json!({
                        "plan_id": id, "status": "error", "reason": "revalidation_failed",
                        "source": source_raw, "target": target_raw, "error": error.to_string(),
                    }));
                    continue;
                }
            }
        }

        // Revalidate: source exists, target free.
        //
        // A kill between the rename and the ledger commit leaves the opposite
        // shape — the source name is gone and the target one is already there —
        // and this used to answer it with `source_missing` and drop the plan:
        // the directory kept its new name while every item row, the group
        // location and the plan row still named the old one, so the next scan
        // read the whole folder as new and a later attachment was rendered into
        // a directory that no longer existed. The rename is already a fact on
        // disk, so the ledger is completed against it instead (V1).
        //
        // The adoption is not a guess about intent: it still has to pass the
        // same "this rewrite matched the rows this plan is about" check as a
        // normal execution further down, so a target directory that the ledger
        // never pointed at — one the user filled by hand and re-scanned — is
        // refused exactly as it was before.
        let adopted_rename = !dry_run && !src.is_dir() && dst.is_dir();
        if !src.is_dir() && !adopted_rename {
            if !dry_run {
                record_plan_failure_or_log(conn, id, "source_missing", &source, &target, None);
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "source_missing"}));
            continue;
        }
        if dst.exists() && !adopted_rename {
            if !dry_run {
                record_plan_failure_or_log(conn, id, "target_exists", &source, &target, None);
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "target_exists"}));
            continue;
        }
        if target == source || target.starts_with(&format!("{source}/")) {
            if !dry_run {
                record_plan_failure_or_log(
                    conn,
                    id,
                    "target_inside_source",
                    &source,
                    &target,
                    None,
                );
            }
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "target_inside_source"
            }));
            continue;
        }
        // Safety: stay under artist path.
        //
        // The source is not checked when it has already been renamed away:
        // `path_under_artist` canonicalizes both sides, and a path that no
        // longer exists cannot be canonicalized, which would read as "outside
        // the artist" for the one shape this branch exists to recover. The
        // target is the directory this plan is claiming either way, and it is
        // checked in both cases.
        if (!adopted_rename && !path_under_artist(&src, &artist_root))
            || !target_parent_under_artist(dst.parent().unwrap_or(&dst), &artist_root)
        {
            if !dry_run {
                record_plan_failure_or_log(conn, id, "outside_artist", &source, &target, None);
            }
            executed.push(json!({"plan_id": id, "status": "error", "reason": "outside_artist"}));
            continue;
        }
        if let Some(permission_path) = rename_permission_denied_parent(&src, &dst) {
            let permission_path = permission_path.to_string_lossy().replace('\\', "/");
            if !dry_run {
                record_plan_failure_or_log(
                    conn,
                    id,
                    "permission_denied",
                    &source,
                    &target,
                    Some(json!({"permission_path": permission_path})),
                );
            }
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "permission_denied",
                "permission_path": permission_path,
            }));
            continue;
        }
        if dry_run {
            executed.push(
                json!({"plan_id": id, "status": "dry_run", "source": source, "target": target}),
            );
            continue;
        }
        // An adopted rename needs its own evidence before the ledger follows it:
        // see `adoption_names_match_items`. Checked before the move intents are
        // registered, so a refusal leaves nothing fenced off.
        if adopted_rename && !adoption_names_match_items(conn, artist_id, &src, &dst)? {
            record_plan_failure_or_log(conn, id, "adoption_unverified", &source, &target, None);
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": "adoption_unverified"
            }));
            continue;
        }
        // §6.3: the tidy side's half of the fence, registered and committed
        // before anything is renamed. A publish that starts from here on is
        // refused until the intent finishes; a live publish blocks this rename
        // instead, with the reason rather than a silent no-op.
        let mut intent_guard =
            match register_move_intents(conn, &src.to_string_lossy(), "folder_rename") {
                Ok(guard) => guard,
                Err(error) => {
                    record_plan_failure_or_log(
                        conn,
                        id,
                        "publish_in_progress",
                        &source,
                        &target,
                        Some(json!({"error": error.to_string()})),
                    );
                    executed.push(json!({
                        "plan_id": id,
                        "status": "error",
                        "reason": "publish_in_progress",
                        "error": error.to_string(),
                    }));
                    continue;
                }
            };
        // Rename first, then DB in one transaction. On DB failure, restore folder
        // AND reverse any partial item path rewrite.
        //
        // Skipped when the rename is already a fact on disk: this run is the
        // recovery of a move another process finished and never recorded.
        if !adopted_rename {
            if let Err(error) =
                rename_artist_dir_no_overwrite(&artist_root, roots, &source, &target)
            {
                let reason = if error.kind() == std::io::ErrorKind::PermissionDenied {
                    "permission_denied"
                } else {
                    "execution_failed"
                };
                record_plan_failure_or_log(
                    conn,
                    id,
                    reason,
                    &source,
                    &target,
                    Some(json!({"error": error.to_string()})),
                );
                executed.push(json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": reason,
                    "error": error.to_string(),
                }));
                continue;
            }
        }
        let db_result = (|| -> Result<i64> {
            let tx = conn.unchecked_transaction()?;
            let updated_items = update_folder_item_paths(
                &tx,
                artist_id,
                &src_logical,
                &src_s,
                &dst_logical,
                &dst_s,
            )?;
            // The folder was already renamed on disk; if the stored item paths
            // match neither prefix, committing would leave every row pointing
            // at the old location (next scan: mass missing + duplicates).
            if updated_items == 0 && file_count > 0 {
                bail!("path rewrite matched no items for a non-empty plan");
            }
            // The work's location has to follow the rename in this same
            // transaction: the plan row is deleted right after, and without a
            // persistent location the group ledger would still name the old
            // directory, so a later attachment would be rendered into it.
            crate::pawchive_groups::relocate_groups_in_tx(
                &tx,
                &artist_root.to_string_lossy(),
                &[(src_s.clone(), dst_s.clone())],
                "folder_rename",
            )?;
            let changed = tx.execute(
                "DELETE FROM folder_rename_plans
                 WHERE id=? AND status='confirmed'
                   AND source_folder=? AND target_folder=?",
                params![id, source_raw, target_raw],
            )?;
            if changed != 1 {
                bail!("stale_state");
            }
            tx.commit()?;
            Ok(updated_items)
        })();
        if let Err(err) = db_result {
            let stale = err.to_string().contains("stale_state");
            // An adopted rename is not rolled back on disk. Moving the directory
            // back would rename a directory this run did not create, and the
            // state it leaves is the same recoverable one it was asked to
            // repair — the next pass adopts it again.
            if adopted_rename {
                record_plan_failure_or_log(
                    conn,
                    id,
                    "db_update_failed",
                    &source,
                    &target,
                    Some(json!({"error": err.to_string(), "adopted_rename": true})),
                );
                executed.push(json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": "db_update_failed",
                    "error": err.to_string(),
                }));
                continue;
            }
            if let Err(rollback_error) =
                rename_artist_dir_no_overwrite(&artist_root, roots, &target, &source)
            {
                let detail = json!({
                    "error": err.to_string(),
                    "rollback_error": rollback_error.to_string(),
                });
                record_plan_failure_or_log(
                    conn,
                    id,
                    "rollback_failed",
                    &source,
                    &target,
                    Some(detail.clone()),
                );
                executed.push(json!({
                    "plan_id": id,
                    "status": "error",
                    "reason": "rollback_failed",
                    "error": detail,
                }));
                continue;
            }
            if !stale {
                record_plan_failure_or_log(
                    conn,
                    id,
                    "db_update_failed",
                    &source,
                    &target,
                    Some(json!({"error": err.to_string()})),
                );
            }
            executed.push(json!({
                "plan_id": id,
                "status": "error",
                "reason": if stale { "stale_state" } else { "db_update_failed" },
                "error": err.to_string(),
            }));
            continue;
        }
        executed.push(json!(
            {"plan_id": id, "status": "executed", "source": source, "target": target,
             "adopted_rename": adopted_rename}
        ));
        intent_guard.finish(crate::pawchive_groups::GROUP_MOVE_INTENT_APPLIED, "");
        remove_empty_parents(Some(src), &artist_root);
        // The executed plan rewrote item paths; rebuild the grouping before
        // the next plan's recheck.
        item_dates_cache = None;
    }
    Ok(json!({
        "ok": true,
        "dry_run": dry_run,
        "backup": backup_path,
        "results": executed
    }))
}

pub fn undo_folder_rename_plan(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let plan: Option<(i64, String, String, String, String, String)> = conn
        .query_row(
            "SELECT artist_id, source_folder, target_folder, status, execution_log, plan_kind
             FROM folder_rename_plans WHERE id=?",
            params![plan_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?;
    let Some((artist_id, source_raw, target_raw, status, execution_log, plan_kind)) = plan else {
        bail!("plan_not_found");
    };
    if status != "executed" {
        bail!("plan_not_executed");
    }
    if plan_kind == "split_by_tag" {
        bail!("split_undo_not_supported");
    }
    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(artist_path) = artist_path else {
        bail!("artist_missing");
    };
    let artist_root = roots.map_to_real(&artist_path)?;
    if !artist_root.is_dir() {
        bail!("artist_missing");
    }
    if !path_under_authorized_roots(&artist_root, roots) {
        bail!("outside_artist");
    }
    let source = validate_relative_folder(&source_raw).map_err(|_| anyhow!("outside_artist"))?;
    let target = validate_relative_folder(&target_raw).map_err(|_| anyhow!("outside_artist"))?;
    let source_path = artist_root.join(&source);
    let target_path = artist_root.join(&target);
    if !target_path.exists() {
        bail!("target_missing");
    }
    if !target_path.is_dir() {
        bail!("target_not_directory");
    }
    if source_path.exists() {
        bail!("source_exists");
    }
    if !path_under_artist(&target_path, &artist_root)
        || !target_parent_under_artist(source_path.parent().unwrap_or(&source_path), &artist_root)
    {
        bail!("outside_artist");
    }

    let target_db = target_path.to_string_lossy().replace('\\', "/");
    let source_db = source_path.to_string_lossy().replace('\\', "/");
    let target_logical = folder_db_path(&artist_path, &target);
    let source_logical = folder_db_path(&artist_path, &source);
    // Precheck: the executed plan must still own items under the executed
    // target prefix. Reverting with zero matching rows would move the folder
    // on disk while no database row follows it back.
    let expected_items: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items
         WHERE artist_id=? AND (
           file_path=? OR instr(file_path, ?)=1 OR
           file_path=? OR instr(file_path, ?)=1
         )",
        params![
            artist_id,
            &target_db,
            format!("{target_db}/"),
            &target_logical,
            format!("{target_logical}/"),
        ],
        |row| row.get(0),
    )?;
    if expected_items == 0 {
        bail!("no_items_under_target");
    }

    let backup = create_db_backup(conn)?;
    // §6.3: an undo is a move like any other. The intent is registered against
    // the directory as it stands now, before the rename back.
    let mut intent_guard =
        register_move_intents(conn, &target_path.to_string_lossy(), "folder_rename_undo")?;
    rename_artist_dir_no_overwrite(&artist_root, roots, &target, &source).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            anyhow!("source_exists")
        } else {
            error.into()
        }
    })?;

    let db_result = (|| -> Result<i64> {
        let tx = conn.unchecked_transaction()?;
        let updated_items = update_folder_item_paths(
            &tx,
            artist_id,
            &target_logical,
            &target_db,
            &source_logical,
            &source_db,
        )?;
        // The folder was already renamed back on disk; committing zero rows
        // would leave the revert unrecorded. Roll the disk move back through
        // the shared failure path below.
        if updated_items == 0 {
            bail!("path rewrite matched no items for a non-empty plan");
        }
        // Undoing a rename is a move too, and the ledger has to follow it back:
        // recording only the forward direction would leave the group named at
        // the directory that no longer exists.
        crate::pawchive_groups::relocate_groups_in_tx(
            &tx,
            &artist_root.to_string_lossy(),
            &[(target_db.clone(), source_db.clone())],
            "folder_rename_undo",
        )?;
        let mut log = serde_json::from_str::<Value>(&execution_log)
            .ok()
            .and_then(|value| value.as_array().cloned())
            .unwrap_or_default();
        log.push(json!({
            "at": now(),
            "status": "reverted",
            "reason": "folder_rename_undo",
            "source": target_db,
            "target": source_db,
            "updated_items": updated_items,
            "backup": backup,
        }));
        let changed = tx.execute(
            "UPDATE folder_rename_plans
             SET status='reverted', executed_at=NULL, execution_log=?, updated_at=?
             WHERE id=? AND status='executed'
               AND source_folder=? AND target_folder=? AND execution_log=?",
            params![
                Value::Array(log).to_string(),
                now(),
                plan_id,
                source_raw,
                target_raw,
                execution_log,
            ],
        )?;
        if changed != 1 {
            bail!("stale_state");
        }
        tx.commit()?;
        Ok(updated_items)
    })();

    match db_result {
        Ok(updated_items) => {
            intent_guard.finish(crate::pawchive_groups::GROUP_MOVE_INTENT_APPLIED, "");
            Ok(json!({
                "ok": true,
                "status": "reverted",
                "reason": "folder_rename_undo",
                "plan_id": plan_id,
                "source": target,
                "target": source,
                "updated_items": updated_items,
                "backup": backup,
            }))
        }
        Err(error) => match rollback_undo_folder_rename(&artist_root, roots, &source, &target) {
            Ok(()) => Err(error),
            Err(rollback_error) => {
                record_undo_reconciliation_failure(
                    conn,
                    plan_id,
                    &target_db,
                    &source_db,
                    &backup,
                    &error,
                    &rollback_error,
                )
                .with_context(|| {
                    format!(
                        "reconciliation_required: database update failed: {error}; filesystem rollback failed: {rollback_error}; failure state could not be persisted"
                    )
                })?;
                bail!(
                    "reconciliation_required: database update failed: {error}; filesystem rollback failed: {rollback_error}"
                )
            }
        },
    }
}

/// Generate fixed archive targets for the draft plans of one or more artists
/// without requiring a manual "更新整理项" click, so a scan + auto execute
/// becomes fully automatic. The fixed target is derived from the current
/// effective item dates (`YYYY/YYYY-MM <tag>`); plans without dates, with
/// conflicting dates, or with physical conflicts stay manual. Shared by the
/// post-scan auto archive and the per-artist manual auto run.
pub fn auto_name_artist_draft_plans(
    conn: &Connection,
    roots: &MediaRoots,
    artists: &[i64],
) -> Result<usize> {
    let mut recomputed = 0usize;
    for artist_id in artists {
        auto_discover_artist_folder_plans(conn, *artist_id)?;
        recomputed += recompute_artist_plan_targets(conn, Some(roots), *artist_id)?;
    }
    Ok(recomputed)
}

/// Run automatic archive only after a successful full-library scan.
/// Returns an immediate summary for the caller; does not persist a last_run summary.
pub fn run_folder_rename_auto_after_full_scan(
    conn: &Connection,
    roots: &MediaRoots,
) -> Result<Value> {
    run_folder_rename_all(conn, roots, false)
}

/// Immediately run archive plans for every active artist, regardless of the
/// automatic post-scan setting. Failed plans are retried once on explicit user action.
pub fn run_folder_rename_all_now(conn: &Connection, roots: &MediaRoots) -> Result<Value> {
    run_folder_rename_all(conn, roots, true)
}

fn run_folder_rename_all(
    conn: &Connection,
    roots: &MediaRoots,
    retry_failed: bool,
) -> Result<Value> {
    ensure_folder_schema(conn)?;
    purge_folder_rename_auto_last_run(conn)?;
    if !retry_failed && !folder_rename_auto_enabled(conn)? {
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
    let retried_count = if retry_failed {
        // rollback_failed plans are excluded: disk and database already
        // disagree there and the log marks them as needing reconciliation;
        // blindly re-confirming would re-execute against a stale target.
        conn.execute(
            "UPDATE folder_rename_plans
             SET status='confirmed', confirmed_at=?, confirmation_source='manual', updated_at=?
             WHERE status='manual_review'
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))
               AND execution_log NOT LIKE '%\"reason\":\"rollback_failed\"%'
               AND EXISTS (
                 SELECT 1 FROM artists
                 WHERE artists.id=folder_rename_plans.artist_id
                   AND COALESCE(artists.missing, 0)=0
               )",
            params![now(), now()],
        )?
    } else {
        0
    };
    // Auto-archive must not require a manual "更新整理项" click: generate the
    // target folder names for discovered draft plans before confirming them,
    // so a scan + auto execute becomes fully automatic. Plans with conflicts
    // (target exists, unsafe paths, etc.) are skipped and stay manual.
    let auto_named = auto_name_artist_draft_plans(conn, roots, &artists)?;
    let mut executed_count = 0i64;
    let mut skipped_count = 0i64;
    let mut failed_count = 0i64;
    let mut actions = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut errors = Vec::new();
    let confirmation_source = if retry_failed { "manual" } else { "auto" };
    let auto_confirmed = conn.execute(
        "UPDATE folder_rename_plans
         SET status='confirmed', confirmed_at=?, confirmation_source=?, updated_at=?
         WHERE status='ready'
           AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))
           AND EXISTS (
             SELECT 1 FROM artists
             WHERE artists.id=folder_rename_plans.artist_id
               AND COALESCE(artists.missing, 0)=0
           )",
        params![now(), confirmation_source, now()],
    )?;
    let mut executable = Vec::new();
    for artist_id in artists {
        let plans: i64 = conn.query_row(
            "SELECT COUNT(*) FROM folder_rename_plans
             WHERE artist_id=? AND status='confirmed'
               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
            params![artist_id],
            |row| row.get(0),
        )?;
        if plans == 0 {
            skipped_count += 1;
            continue;
        }
        executable.push(artist_id);
    }
    let backup = if executable.is_empty() {
        String::new()
    } else {
        match create_db_backup(conn) {
            Ok(path) => path,
            Err(error) => {
                let error_text = error.to_string();
                let mut backup_failures = Vec::new();
                for artist_id in &executable {
                    let plans: Vec<(i64, String, String)> = conn
                        .prepare(
                            "SELECT id, source_folder, target_folder FROM folder_rename_plans
                             WHERE artist_id=? AND status='confirmed'
                               AND (target_folder != '' OR (plan_kind='split_by_tag' AND split_actions != '[]'))",
                        )?
                        .query_map(params![artist_id], |row| {
                            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                        })?
                        .collect::<rusqlite::Result<_>>()?;
                    for (plan_id, source, target) in plans {
                        record_plan_execution_failure(
                            conn,
                            plan_id,
                            "backup_failed",
                            &source,
                            &target,
                            Some(json!({"error": error_text})),
                        )?;
                        backup_failures.push(json!({
                            "artist_id": artist_id,
                            "plan_id": plan_id,
                            "status": "error",
                            "reason": "backup_failed",
                            "source": source,
                            "target": target,
                            "error": error_text,
                        }));
                    }
                }
                failed_count += backup_failures.len() as i64;
                errors.extend(backup_failures.iter().cloned());
                failed.extend(backup_failures);
                if skipped_count > 0 {
                    skipped.push(json!({
                        "reason": "no_confirmed_plans",
                        "count": skipped_count
                    }));
                }
                return Ok(json!({
                    "ok": true,
                    "status": "failed",
                    "scope": "full",
                    "artist_id": Value::Null,
                    "at": now(),
                    "backup": "",
                    "executed_count": executed_count,
                    "skipped_count": skipped_count,
                    "failed_count": failed_count,
                    "actions": actions,
                    "skipped": skipped,
                    "failed": failed,
                    "errors": errors
                }));
            }
        }
    };
    for artist_id in executable {
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
            }
            Err(error) => {
                failed_count += 1;
                let failure = json!({
                    "artist_id": artist_id,
                    "auto_confirmed": auto_confirmed,
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
        "auto_named": auto_named,
        "retried_count": retried_count,
        "executed_count": executed_count,
        "skipped_count": skipped_count,
        "failed_count": failed_count,
        "actions": actions,
        "skipped": skipped,
        "failed": failed,
        "errors": errors
    }))
}

pub(crate) struct PlanPathCheck {
    pub artist_id: i64,
    pub source_folder: String,
    pub target_folder: String,
    pub source_exists: bool,
    pub target_exists: bool,
    pub permission_path: Option<String>,
    pub reason: Option<String>,
}

pub(crate) fn evaluate_plan_paths(
    roots: &MediaRoots,
    artist_id: i64,
    artist_path: &str,
    source_raw: &str,
    target_raw: &str,
) -> Result<PlanPathCheck> {
    let source = match validate_relative_folder(source_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source_raw.to_string(),
                target_folder: target_raw.to_string(),
                source_exists: false,
                target_exists: false,
                permission_path: None,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let target = match validate_relative_folder(target_raw) {
        Ok(value) => value,
        Err(_) => {
            return Ok(PlanPathCheck {
                artist_id,
                source_folder: source,
                target_folder: target_raw.to_string(),
                source_exists: false,
                target_exists: false,
                permission_path: None,
                reason: Some("bad_folder_path".into()),
            })
        }
    };
    let artist_root = roots.map_to_real(artist_path)?;
    let source_path = artist_root.join(&source);
    let target_path = artist_root.join(&target);
    let source_exists = source_path.is_dir();
    let target_exists = target_path.exists();
    let permission_path = if source_exists
        && !target_exists
        && path_under_authorized_roots(&artist_root, roots)
        && path_under_artist(&source_path, &artist_root)
        && target_parent_under_artist(target_path.parent().unwrap_or(&target_path), &artist_root)
    {
        rename_permission_denied_parent(&source_path, &target_path)
            .map(|path| path.to_string_lossy().replace('\\', "/"))
    } else {
        None
    };
    let reason = if target == source || target.starts_with(&format!("{source}/")) {
        Some("target_inside_source".into())
    } else if !path_under_authorized_roots(&artist_root, roots)
        || !path_under_artist(&source_path, &artist_root)
        || !target_parent_under_artist(target_path.parent().unwrap_or(&target_path), &artist_root)
    {
        Some("outside_artist".into())
    } else if !source_exists {
        Some("source_missing".into())
    } else if target_exists {
        Some("target_exists".into())
    } else if permission_path.is_some() {
        Some("permission_denied".into())
    } else {
        None
    };
    Ok(PlanPathCheck {
        artist_id,
        source_folder: source,
        target_folder: target,
        source_exists,
        target_exists,
        permission_path,
        reason,
    })
}

pub(crate) fn check_plan_paths(
    conn: &Connection,
    roots: &MediaRoots,
    plan_id: i64,
) -> Result<PlanPathCheck> {
    let (artist_id, artist_path, source_raw, target_raw, plan_kind, split_actions): (
        i64,
        String,
        String,
        String,
        String,
        String,
    ) = conn
        .query_row(
            "SELECT p.artist_id, a.path, p.source_folder, p.target_folder,
                    p.plan_kind, p.split_actions
             FROM folder_rename_plans p JOIN artists a ON a.id=p.artist_id
             WHERE p.id=?",
            params![plan_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            },
        )
        .optional()?
        .ok_or_else(|| anyhow!("plan not found"))?;
    if plan_kind == "split_by_tag" {
        let result = prepare_split_file_moves(
            conn,
            roots,
            artist_id,
            &artist_path,
            &source_raw,
            &split_actions,
        );
        let reason = result
            .err()
            .map(|error| split_failure_reason(&error).to_string());
        return Ok(PlanPathCheck {
            artist_id,
            source_folder: source_raw,
            target_folder: String::new(),
            source_exists: reason.as_deref() != Some("source_missing"),
            target_exists: reason.as_deref() == Some("target_exists"),
            permission_path: None,
            reason,
        });
    }
    evaluate_plan_paths(roots, artist_id, &artist_path, &source_raw, &target_raw)
}

pub fn recheck_plan(conn: &Connection, roots: &MediaRoots, plan_id: i64) -> Result<Value> {
    ensure_folder_schema(conn)?;
    let check = check_plan_paths(conn, roots, plan_id)?;
    Ok(json!({
        "id": plan_id,
        "artist_id": check.artist_id,
        "status": if check.reason.is_none() { "ready" } else { "blocked" },
        "source_folder": check.source_folder,
        "target_folder": check.target_folder,
        "source_exists": check.source_exists,
        "target_exists": check.target_exists,
        "permission_path": check.permission_path,
        "error": check.reason,
        "rechecked": true
    }))
}

