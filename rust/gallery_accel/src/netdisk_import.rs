//! Publish completed JD artifacts unchanged, preserving downloader sources.
use std::path::{Path, PathBuf};

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};

use crate::ingest_publish::{mark_job_ingested, publish_completed_file, IngestPublishRequest};
use crate::media_roots::{path_under_authorized_roots, MediaRoots};
use crate::netdisk::{
    load_netdisk_settings, queue_bridge_command, resolve_netdisk_staging_directory, BridgeTask,
    BRIDGE_TASK_SETTLED,
};
use crate::pawchive::{get_pawchive_settings, with_stable_suffix, FileCategory, PostNamingContext};
use crate::scan::{run_scan_inner, ScanControl};

pub fn ensure_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS netdisk_import_status (
        task_id TEXT PRIMARY KEY, error TEXT NOT NULL DEFAULT '', updated_at REAL NOT NULL, retry_count INTEGER NOT NULL DEFAULT 0
    )",
    )?;
    let _ = conn.execute(
        "ALTER TABLE netdisk_import_status ADD COLUMN retry_count INTEGER NOT NULL DEFAULT 0",
        [],
    );
    conn.execute_batch("CREATE TABLE IF NOT EXISTS netdisk_task_naming (
        task_id TEXT PRIMARY KEY REFERENCES netdisk_bridge_tasks(task_id) ON DELETE CASCADE, snapshot TEXT NOT NULL
    )")?;
    Ok(())
}

pub fn import_error(conn: &Connection, task_id: &str) -> Result<Option<String>> {
    Ok(conn
        .query_row(
            "SELECT NULLIF(error, '') FROM netdisk_import_status WHERE task_id=?1",
            [task_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn task_import_state(conn: &Connection, task: &BridgeTask) -> Result<(i64, i64)> {
    let linked: i64 = conn.query_row(
        "SELECT COUNT(*) FROM download_publish_jobs WHERE engine='netdisk'
         AND source_job_id=?1 AND manifest_version=?2 AND stage='ingested' AND item_id IS NOT NULL",
        params![task.task_id, task.manifest_version],
        |r| r.get(0),
    )?;
    Ok((linked, task.expected.len() as i64))
}

/// Return a bounded retry queue; one failed task cannot starve later tasks.
pub fn pending_auto_imports(conn: &Connection, bridge_id: &str) -> Result<Vec<String>> {
    ensure_schema(conn)?;
    let settings = load_netdisk_settings(conn)?;
    if !settings.enabled || !settings.auto_import {
        return Ok(Vec::new());
    }
    let mut stmt = conn.prepare(
        "SELECT t.task_id FROM netdisk_bridge_tasks t
        LEFT JOIN netdisk_import_status s ON s.task_id=t.task_id
        WHERE t.bridge_id=?1 AND t.state='confirmed'
          AND NOT EXISTS (
              SELECT 1 FROM netdisk_bridge_commands c
              WHERE c.job_id = t.task_id AND c.action = 'remove' AND c.status = 'confirmed'
          )
          AND (
            s.task_id IS NULL
            OR (
                s.error != ''
                AND COALESCE(s.retry_count, 0) < 5
                AND s.updated_at < strftime('%s','now') - CASE
                    WHEN COALESCE(s.retry_count, 0) <= 1 THEN 60
                    WHEN s.retry_count = 2 THEN 300
                    WHEN s.retry_count = 3 THEN 900
                    ELSE 3600 END
            )
            OR (
                COALESCE(json_array_length(t.expected), 0) > 0
                AND (SELECT COUNT(*) FROM download_publish_jobs WHERE engine='netdisk' AND source_job_id=t.task_id AND manifest_version=t.manifest_version AND stage='ingested' AND item_id IS NOT NULL) < json_array_length(t.expected)
                AND (
                    s.error = ''
                    AND COALESCE(s.retry_count, 0) < 5
                    AND s.updated_at < strftime('%s','now') - CASE
                        WHEN COALESCE(s.retry_count, 0) <= 1 THEN 60
                        WHEN s.retry_count = 2 THEN 300
                        WHEN s.retry_count = 3 THEN 900
                        ELSE 3600 END
                )
            )
          )
        ORDER BY COALESCE(s.updated_at,0), t.created_at LIMIT 4",
    )?;
    let ids = stmt
        .query_map([bridge_id], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<String>>>()?;
    Ok(ids)
}

struct ImportDestination {
    artist_id: i64,
    artist: PathBuf,
    folder: PathBuf,
    names: Vec<String>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct TaskNaming {
    artist_id: Option<i64>,
    artist_name: String,
    artist_path: String,
    target_dir: String,
    post_id: String,
    title: String,
    date: Option<String>,
    service: String,
    user_id: String,
    import_dir: String,
    templates: crate::pawchive::PawchiveSettings,
}

/// Capture before submission; a later settings save cannot rename this task.
pub(crate) fn freeze_task_naming(conn: &Connection, task_id: &str, post_id: i64) -> Result<()> {
    let snapshot = conn.query_row(
        "SELECT a.id,COALESCE(a.name,s.user_id),COALESCE(a.path,''),s.target_dir,
         p.post_id,COALESCE(p.title,''),p.published_at,s.service,s.user_id
         FROM kemono_posts p JOIN kemono_subscriptions s ON s.id=p.subscription_id
         LEFT JOIN artists a ON a.id=s.artist_id WHERE p.id=?1",
        [post_id],
        |r| {
            Ok((
                r.get::<_, Option<i64>>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, Option<String>>(6)?,
                r.get::<_, String>(7)?,
                r.get::<_, String>(8)?,
            ))
        },
    )?;
    let value = TaskNaming {
        artist_id: snapshot.0,
        artist_name: snapshot.1,
        artist_path: snapshot.2,
        target_dir: snapshot.3,
        post_id: snapshot.4,
        title: snapshot.5,
        date: snapshot.6,
        service: snapshot.7,
        user_id: snapshot.8,
        import_dir: load_netdisk_settings(conn)?.import_dir,
        templates: get_pawchive_settings(conn)?,
    };
    conn.execute(
        "INSERT INTO netdisk_task_naming(task_id,snapshot) VALUES(?1,?2)",
        params![task_id, serde_json::to_string(&value)?],
    )?;
    Ok(())
}

fn destination(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    sources: &[PathBuf],
) -> Result<ImportDestination> {
    let current: i64 = conn.query_row(
        "SELECT manifest_version FROM kemono_posts WHERE id=?1",
        [task.post_id],
        |r| r.get(0),
    )?;
    if current != task.manifest_version {
        bail!("作品清单已变化，请核对原任务");
    }
    // Legacy tasks retain their reserved target; capture their remaining naming
    // context once on first import rather than silently changing it on retries.
    let exists: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM netdisk_task_naming WHERE task_id=?1)",
        [&task.task_id],
        |r| r.get(0),
    )?;
    if !exists {
        freeze_task_naming(conn, &task.task_id, task.post_id)?;
    }
    let raw: String = conn.query_row(
        "SELECT snapshot FROM netdisk_task_naming WHERE task_id=?1",
        [&task.task_id],
        |r| r.get(0),
    )?;
    let TaskNaming {
        artist_id,
        artist_name,
        artist_path,
        target_dir,
        post_id,
        title,
        date,
        service,
        user_id,
        import_dir,
        templates,
    } = serde_json::from_str(&raw)?;
    let (artist_id, resolved_artist_name, resolved_artist_path, resolved_target_dir) =
        match artist_id {
            Some(id) => {
                let row: Option<(String, String)> = conn
                    .query_row(
                        "SELECT COALESCE(name, ''), COALESCE(path, '') FROM artists WHERE id=?1",
                        [id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .optional()?;
                let (name, path) = row.unwrap_or((artist_name.clone(), artist_path.clone()));
                let sub_target: Option<String> = conn
                    .query_row(
                        "SELECT s.target_dir FROM kemono_posts p JOIN kemono_subscriptions s ON s.id=p.subscription_id WHERE p.id=?1",
                        [task.post_id],
                        |r| r.get(0),
                    )
                    .optional()?;
                (id, name, path, sub_target.unwrap_or(target_dir))
            }
            None => {
                let bound: Option<(i64, String, String, String)> = conn
                    .query_row(
                        "SELECT a.id, COALESCE(a.name, s.user_id), COALESCE(a.path, ''), s.target_dir
                         FROM kemono_posts p JOIN kemono_subscriptions s ON s.id=p.subscription_id
                         JOIN artists a ON a.id=s.artist_id WHERE p.id=?1",
                        [task.post_id],
                        |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
                    )
                    .optional()?;
                if let Some((bound_id, bound_name, bound_path, bound_target)) = bound {
                    if let Ok(mut val) = serde_json::from_str::<serde_json::Value>(&raw) {
                        val["artist_id"] = serde_json::json!(bound_id);
                        val["artist_name"] = serde_json::json!(bound_name.clone());
                        val["artist_path"] = serde_json::json!(bound_path.clone());
                        val["target_dir"] = serde_json::json!(bound_target.clone());
                        let _ = conn.execute(
                            "UPDATE netdisk_task_naming SET snapshot=?1 WHERE task_id=?2",
                            params![serde_json::to_string(&val)?, task.task_id],
                        );
                    }
                    (bound_id, bound_name, bound_path, bound_target)
                } else {
                    bail!("任务创建时未绑定画师，无法确定入库目录");
                }
            }
        };
    let artist = roots.map_to_real(&resolved_artist_path)?;
    let base = if import_dir.trim().is_empty() {
        roots.map_to_real(&resolved_target_dir)?
    } else {
        roots.map_to_real(import_dir.trim())?
    };
    let naming = PostNamingContext::new(
        &resolved_artist_name,
        date.as_deref(),
        Some(&title),
        &post_id,
        Some(&service),
    )
    .with_identity(&user_id, &service);
    let rendered = naming.format_folder_path_with_template(&templates.folder_template);
    // Match subscription downloads: the complete user template is relative to
    // target_dir, including an explicitly requested artist directory component.
    let relative = Path::new(&rendered);
    if relative.as_os_str().is_empty()
        || relative
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        bail!("作品目录模板必须生成相对目录");
    }
    let reserved: Option<String> = conn
        .query_row(
            "SELECT target_path FROM download_publish_jobs WHERE engine='netdisk'
         AND source_job_id=?1 AND manifest_version=?2 ORDER BY id LIMIT 1",
            params![task.task_id, task.manifest_version],
            |r| r.get(0),
        )
        .optional()?;
    // A retry belongs to the already reserved folder, even after settings change.
    let folder = match reserved {
        Some(path) => PathBuf::from(path)
            .parent()
            .ok_or_else(|| anyhow!("入库记录缺少父目录"))?
            .to_path_buf(),
        None => base.join(relative),
    };
    if !folder.starts_with(&artist) || !path_under_authorized_roots(&folder, roots) {
        bail!("入库目录不在该作品的授权画师目录内");
    }
    let names = sources
        .iter()
        .enumerate()
        .map(|(index, source)| {
            let name = source
                .file_name()
                .and_then(|s| s.to_str())
                .ok_or_else(|| anyhow!("暂存输出缺少有效文件名"))?;
            Ok(
                if FileCategory::from_filename(name) == FileCategory::Image {
                    let ext = source.extension().and_then(|s| s.to_str()).unwrap_or("");
                    naming.format_image_filename_with_name(
                        index,
                        ext,
                        name,
                        &templates.image_template,
                    )
                } else {
                    naming.format_attachment_filename_with_index(
                        name,
                        index,
                        &templates.attachment_template,
                    )
                },
            )
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ImportDestination {
        artist_id,
        artist,
        folder,
        names,
    })
}

fn digest(path: &Path) -> Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut hash = blake3::Hasher::new();
    hash.update_reader(&mut file)?;
    Ok(hash.finalize().to_hex().to_string())
}

/// Uses the shared scan/file-operation gate; busy work is retried next heartbeat.
pub fn import_task(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    control: &std::sync::Arc<ScanControl>,
) -> Result<i64> {
    import_task_with_mode(conn, task, roots, control, true)
}

/// Explicit manual import triggered by user action, bypassing auto_import switch check.
pub fn import_task_manual(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    control: &std::sync::Arc<ScanControl>,
) -> Result<i64> {
    import_task_with_mode(conn, task, roots, control, false)
}

pub fn import_task_with_mode(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    control: &std::sync::Arc<ScanControl>,
    automatic: bool,
) -> Result<i64> {
    if task.state != BRIDGE_TASK_SETTLED {
        bail!("任务尚未结算，无法入库");
    }
    let _guard = control
        .try_claim()
        .ok_or_else(|| anyhow!("扫描或文件操作正在进行，稍后重试"))?;
    if automatic {
        let settings = load_netdisk_settings(conn)?;
        if !settings.enabled || !settings.auto_import {
            return Ok(task_import_state(conn, task)?.0);
        }
    }
    ensure_schema(conn)?;
    let result = import_inner(conn, task, roots, control, automatic);
    let message = result
        .as_ref()
        .err()
        .map(ToString::to_string)
        .unwrap_or_default();
    if message.is_empty() {
        let (linked, total) = task_import_state(conn, task).unwrap_or((0, 0));
        let all_done = total > 0 && linked == total;
        if all_done {
            conn.execute(
                "INSERT INTO netdisk_import_status(task_id,error,updated_at,retry_count) VALUES(?1,'',strftime('%s','now'),0)
                ON CONFLICT(task_id) DO UPDATE SET error='',updated_at=excluded.updated_at,retry_count=0",
                params![task.task_id],
            )?;
        } else {
            conn.execute(
                "INSERT INTO netdisk_import_status(task_id,error,updated_at,retry_count) VALUES(?1,'',strftime('%s','now'),1)
                ON CONFLICT(task_id) DO UPDATE SET error='',updated_at=excluded.updated_at,retry_count=COALESCE(netdisk_import_status.retry_count,0)+1",
                params![task.task_id],
            )?;
        }
    } else {
        conn.execute(
            "INSERT INTO netdisk_import_status(task_id,error,updated_at,retry_count) VALUES(?1,?2,strftime('%s','now'),1)
            ON CONFLICT(task_id) DO UPDATE SET error=excluded.error,updated_at=excluded.updated_at,retry_count=COALESCE(netdisk_import_status.retry_count,0)+1",
            params![task.task_id, message],
        )?;
    }
    result
}

fn import_inner(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    control: &ScanControl,
    automatic: bool,
) -> Result<i64> {
    let (linked, total) = task_import_state(conn, task)?;
    if total > 0 && linked == total {
        let settings = load_netdisk_settings(conn)?;
        if settings.cleanup_after_import {
            let _ = cleanup_imported_task(conn, task, roots);
        }
        return Ok(linked);
    }
    let raw: String = conn.query_row(
        "SELECT output_paths FROM pawchive_external_receipts
        WHERE task_id=?1 AND manifest_version=?2 AND settled=1 ORDER BY recorded_at DESC LIMIT 1",
        params![task.task_id, task.manifest_version],
        |r| r.get(0),
    )?;
    let mut paths: Vec<_> = raw
        .lines()
        .filter(|p| !p.is_empty())
        .map(PathBuf::from)
        .collect();
    if paths.len() != task.expected.len() || paths.is_empty() {
        bail!("完成回执的文件数量与任务不符");
    }
    paths.sort();
    let ImportDestination {
        artist_id,
        artist,
        folder,
        names,
    } = destination(conn, task, roots, &paths)?;
    let mut published = Vec::new();
    for (source, name) in paths.into_iter().zip(names) {
        if automatic {
            let settings = load_netdisk_settings(conn)?;
            if !settings.enabled || !settings.auto_import {
                break;
            }
        }
        let identity = source.to_string_lossy();
        let already_ingested: bool = conn.query_row(
            "SELECT EXISTS(SELECT 1 FROM download_publish_jobs WHERE engine='netdisk' AND source_job_id=?1 AND source_identity=?2 AND manifest_version=?3 AND stage='ingested' AND item_id IS NOT NULL)",
            params![task.task_id, identity, task.manifest_version],
            |r| r.get(0),
        )?;
        if already_ingested {
            continue;
        }
        if !path_under_authorized_roots(&source, roots) {
            bail!("暂存文件不在授权目录内");
        }
        let meta = std::fs::symlink_metadata(&source)?;
        if !meta.is_file() || meta.file_type().is_symlink() || meta.len() == 0 {
            bail!("暂存输出不是有效普通文件");
        }
        // Once chosen, a job's target is durable across retries/settings changes.
        let existing: Option<String> = conn
            .query_row(
                "SELECT target_path FROM download_publish_jobs
            WHERE engine='netdisk' AND source_job_id=?1 AND source_identity=?2 AND manifest_version=?3",
                params![task.task_id, identity, task.manifest_version],
                |r| r.get(0),
            )
            .optional()?;
        let target = match existing {
            Some(path) => PathBuf::from(path),
            None => {
                let desired = folder.join(name);
                let occupied: bool = conn.query_row(
                    "SELECT EXISTS(SELECT 1 FROM download_publish_jobs WHERE target_path=?1)",
                    [desired.to_string_lossy().as_ref()],
                    |r| r.get(0),
                )?;
                if occupied {
                    let suffix =
                        blake3::hash(format!("{}\n{}", task.task_id, identity).as_bytes()).to_hex();
                    with_stable_suffix(&desired, &suffix[..8])
                } else {
                    desired
                }
            }
        };
        if target.parent() != Some(folder.as_path()) {
            bail!("入库目标已变化，请核对原发布记录");
        }
        let hash = digest(&source)?;
        let result = publish_completed_file(
            conn,
            &IngestPublishRequest {
                engine: "netdisk",
                source_job_id: &task.task_id,
                manifest_version: task.manifest_version,
                source_identity: &identity,
                source_path: &source,
                expected_length: meta.len(),
                expected_blake3: &hash,
                target_path: &target,
            },
            roots,
        )?;
        published.push((result.job_id, target, hash));
    }
    if !published.is_empty() {
        index_published_files(conn, roots, control, artist_id, &artist, &folder, published)?;
    }
    let (linked, total) = task_import_state(conn, task)?;
    if total > 0 && linked == total {
        let settings = load_netdisk_settings(conn)?;
        if settings.cleanup_after_import {
            let _ = cleanup_imported_task(conn, task, roots);
        }
    }
    Ok(linked)
}

/// Index a verified publication through the real folder scan and candidate API.
pub(crate) fn index_published_files(
    conn: &Connection,
    roots: &MediaRoots,
    control: &ScanControl,
    artist_id: i64,
    artist: &Path,
    folder: &Path,
    published: Vec<(i64, PathBuf, String)>,
) -> Result<()> {
    // Refuse modified publications before scanning can index them.
    for (_, path, hash) in &published {
        if digest(path)? != *hash {
            bail!("归位文件校验失败");
        }
    }
    let relative = folder
        .strip_prefix(artist)?
        .to_string_lossy()
        .replace('\\', "/");
    let scan = run_scan_inner(conn, roots, control, Some(artist_id), Some(&relative))?;
    if scan
        .get("phase")
        .and_then(|v| v.as_str())
        .is_some_and(|p| p != "complete")
    {
        bail!("文件已归位，目录扫描尚未完成");
    }
    for (job_id, path, hash) in published {
        let target = path.to_string_lossy().replace('\\', "/");
        if digest(&path)? != hash {
            bail!("归位文件校验失败");
        }
        let mut item: Option<i64> = conn
            .query_row(
                "SELECT id FROM items WHERE file_path=?1 AND missing=0",
                [&target],
                |r| r.get(0),
            )
            .optional()?;
        if item.is_none() {
            let candidate: Option<i64> = conn
                .query_row(
                    "SELECT id FROM scan_candidates WHERE file_path=?1
                AND status IN ('pending','candidate') ORDER BY id DESC LIMIT 1",
                    [&target],
                    |r| r.get(0),
                )
                .optional()?;
            if let Some(candidate) = candidate {
                let response = crate::scan_candidates_write::create_new_item_response_with_roots(
                    conn, roots, candidate,
                )?;
                item = response.get("item_id").and_then(|v| v.as_i64());
            }
        }
        let item = item.ok_or_else(|| anyhow!("文件已归位，等待媒体索引：{target}"))?;
        mark_job_ingested(conn, job_id, item)?;
    }
    Ok(())
}

/// Clean up staging files and issue remove command for a fully imported task.
pub fn cleanup_imported_task(
    conn: &Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
) -> Result<()> {
    let (linked, total) = task_import_state(conn, task)?;
    if total == 0 || linked < total {
        return Ok(());
    }

    // 1. Enqueue remove command to JDownloader via bridge if not already present.
    let existing_remove: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM netdisk_bridge_commands WHERE job_id = ?1 AND action = 'remove')",
        params![task.task_id],
        |r| r.get(0),
    )?;
    if !existing_remove {
        let _ = queue_bridge_command(
            conn,
            &task.bridge_id,
            &task.task_id,
            "remove",
            &serde_json::json!({ "link_ids": [] }),
        );
    }

    // 2. Resolve and canonicalize staging directory.
    let settings = load_netdisk_settings(conn)?;
    let staging_dir = if !settings.staging_dir.trim().is_empty() {
        PathBuf::from(settings.staging_dir.trim())
    } else if let Some(default_staging) = resolve_netdisk_staging_directory(roots) {
        default_staging
    } else {
        bail!("无法确定暂存目录");
    };
    let staging_dir_canon = match crate::fs_util::safe_canonicalize(&staging_dir) {
        Ok(c) => c,
        Err(_) => return Ok(()),
    };

    // 3. Query all successfully ingested files for this task.
    let mut stmt = conn.prepare(
        "SELECT source_identity, target_path FROM download_publish_jobs
         WHERE engine='netdisk' AND source_job_id=?1 AND manifest_version=?2
           AND stage='ingested' AND item_id IS NOT NULL",
    )?;
    let jobs = stmt
        .query_map(params![task.task_id, task.manifest_version], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut parent_dirs = std::collections::HashSet::new();
    for (source_identity, target_path) in jobs {
        let source = PathBuf::from(&source_identity);
        let target = PathBuf::from(&target_path);

        if !source.exists() {
            continue;
        }

        let source_sym_meta = match std::fs::symlink_metadata(&source) {
            Ok(m) => m,
            Err(_) => continue,
        };
        // Safety: strictly reject symlinks and non-files
        if source_sym_meta.file_type().is_symlink() || !source_sym_meta.is_file() {
            continue;
        }

        // Safety: source must reside strictly within the staging directory
        let source_canon = match crate::fs_util::safe_canonicalize(&source) {
            Ok(c) => c,
            Err(_) => continue,
        };
        if !source_canon.starts_with(&staging_dir_canon) {
            continue;
        }

        // Safety: target must exist in the library as a regular file
        if !target.is_file() {
            continue;
        }

        // Safety: source and target must not be the same file or share an inode
        if let Ok(target_canon) = crate::fs_util::safe_canonicalize(&target) {
            if source_canon == target_canon {
                continue;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::MetadataExt;
                if let Ok(target_meta) = std::fs::metadata(&target) {
                    if target_meta.dev() == source_sym_meta.dev()
                        && target_meta.ino() == source_sym_meta.ino()
                    {
                        continue;
                    }
                }
            }
        }

        if let Some(parent) = source.parent() {
            parent_dirs.insert(parent.to_path_buf());
        }

        let _ = std::fs::remove_file(&source);
    }

    // Safely attempt to remove empty task subdirectories inside staging
    for parent in parent_dirs {
        if let Ok(parent_canon) = crate::fs_util::safe_canonicalize(&parent) {
            if parent_canon.starts_with(&staging_dir_canon) && parent_canon != staging_dir_canon {
                let _ = std::fs::remove_dir(&parent);
            }
        }
    }

    Ok(())
}
