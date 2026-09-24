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
        task_id TEXT PRIMARY KEY, error TEXT NOT NULL DEFAULT '', updated_at REAL NOT NULL
    )",
    )?;
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
          AND (
            s.task_id IS NULL
            OR (s.error != '' AND s.updated_at < strftime('%s','now')-60)
            OR (
                COALESCE(json_array_length(t.expected), 0) > 0
                AND (SELECT COUNT(*) FROM download_publish_jobs WHERE engine='netdisk' AND source_job_id=t.task_id AND manifest_version=t.manifest_version AND stage='ingested' AND item_id IS NOT NULL) < json_array_length(t.expected)
                AND (s.error = '' AND s.updated_at < strftime('%s','now')-60)
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
    conn.execute(
        "INSERT INTO netdisk_import_status(task_id,error,updated_at) VALUES(?1,?2,strftime('%s','now'))
        ON CONFLICT(task_id) DO UPDATE SET error=excluded.error,updated_at=excluded.updated_at",
        params![task.task_id, message],
    )?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::{DbConfig, DbPool};
    use crate::netdisk::{
        create_bridge_task, load_bridge_task, process_bridge_exchange, BridgeExchangePayload,
    };
    use std::sync::Arc;

    struct Fixture {
        _dir: tempfile::TempDir,
        pool: Arc<DbPool>,
        roots: MediaRoots,
        task: BridgeTask,
        source: PathBuf,
        target: PathBuf,
    }
    fn fixture() -> Fixture {
        fixture_configured(|_| {})
    }

    fn fixture_configured(configure: impl FnOnce(&Connection)) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let artist = media.join("Artist");
        let staging = media.join(".staging");
        std::fs::create_dir_all(&artist).unwrap();
        std::fs::create_dir_all(&staging).unwrap();
        let source = staging.join("bundle.rar");
        std::fs::write(&source, b"RAR archive bytes preserved unchanged").unwrap();
        let pool = Arc::new(
            DbPool::with_config(
                dir.path().join("gallery.db"),
                DbConfig {
                    read_only: false,
                    pool_size: 2,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        conn.execute(
            "INSERT INTO artists(id,name,path) VALUES(1,'Artist',?1)",
            [artist.to_string_lossy().replace('\\', "/")],
        )
        .unwrap();
        conn.execute("INSERT INTO kemono_subscriptions(id,artist_id,service,user_id,target_dir,created_at,updated_at)
            VALUES(1,1,'patreon','123',?1,'t','t')",[artist.to_string_lossy().replace('\\',"/")]).unwrap();
        conn.execute("INSERT INTO kemono_posts(id,subscription_id,post_id,title,published_at,external_links,created_at,updated_at)
            VALUES(1,1,'remote','Bundle','2026-07-23T09:00:00Z','[\"https://drive.google.com/file/d/example/view\"]','t','t')",[]).unwrap();
        crate::pawchive::record_work_observation(
            &conn,
            1,
            crate::pawchive::ManifestCompleteness::Complete,
        )
        .unwrap();
        conn.execute("INSERT INTO app_settings(key,value) VALUES('netdisk_enabled','1'),('netdisk_auto_import','1')",[]).unwrap();
        let roots = MediaRoots::identical(
            vec![media.to_string_lossy().replace('\\', "/")],
            vec!["Media".into()],
        );
        let handshake: BridgeExchangePayload = serde_json::from_value(serde_json::json!({
            "version":"2","bridge_id":"jd-local","session_id":"test","seq":1,"ack":0,
            "capabilities":{"script_version":"2.2","protocol_version":"2","supports_commands":true,
                "supports_pagination":true,"supports_download_path":true,"supports_snapshot":true,
                "max_page_size":100,"linkgrabber_auto_start_enabled":false},"pages":[]
        }))
        .unwrap();
        process_bridge_exchange(&conn, &handshake, &roots).unwrap();
        configure(&conn);
        let task = create_bridge_task(
            &conn,
            "jd-local",
            "test",
            1,
            &["https://drive.google.com/file/d/example/view".into()],
            None,
        )
        .unwrap();
        let mut payload = serde_json::to_value(&handshake).unwrap();
        payload["seq"] = 2.into();
        payload["ack"] = 1.into();
        payload["pages"] = serde_json::json!([{"job_id":task.task_id,"manifest_version":task.manifest_version,
            "snapshot_id":"pkg","page_index":0,"total_count":1,"complete":true,"expected":["link1"],
            "records":[{"link_id":"link1","name":"bundle.rar","finished":true,"status":"FINISHED",
                "bytes_total":source.metadata().unwrap().len(),"download_path":source.to_string_lossy().replace('\\',"/")}]}]);
        let response =
            process_bridge_exchange(&conn, &serde_json::from_value(payload).unwrap(), &roots)
                .unwrap();
        assert_eq!(response.settled_receipts, 1);
        let task = load_bridge_task(&conn, &task.task_id).unwrap().unwrap();
        drop(conn);
        Fixture {
            _dir: dir,
            pool,
            roots,
            task,
            source,
            target: artist.join("Artist/2026-07-23 Bundle/2026-07-23 bundle.rar"),
        }
    }

    #[test]
    fn import_after_real_artist_move() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let _data = crate::test_support::EnvVar::set("DATA_DIR", f._dir.path().join("data"));
        let moved =
            crate::artist_folder_move::execute_artist_folder_move(&conn, &f.roots, 1, 0, "Moved")
                .unwrap();
        assert_eq!(moved["ok"], true);
        let result = import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new()));
        assert!(result.is_ok());
        assert!(
            !f.target.exists(),
            "moving the artist must not recreate its abandoned source path on import"
        );
    }

    #[test]
    fn auto_import_stops_after_disabled_first_file() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let other = f.source.with_file_name("other.rar");
        std::fs::write(&other, b"second unchanged archive").unwrap();
        let mut task = f.task.clone();
        task.expected.push("link2".into());
        conn.execute(
            "UPDATE netdisk_bridge_tasks SET expected=?1 WHERE task_id=?2",
            params![serde_json::to_string(&task.expected).unwrap(), task.task_id],
        )
        .unwrap();
        conn.execute(
            "UPDATE pawchive_external_receipts SET output_paths=?1 WHERE task_id=?2",
            params![
                format!("{}\n{}", f.source.display(), other.display()),
                task.task_id
            ],
        )
        .unwrap();
        conn.execute_batch("CREATE TRIGGER review_disable_after_first_publish AFTER INSERT ON download_publish_jobs
            WHEN NEW.engine='netdisk' BEGIN UPDATE app_settings SET value='0' WHERE key='netdisk_auto_import'; END;").unwrap();
        assert!(pending_auto_imports(&conn, "jd-local")
            .unwrap()
            .contains(&task.task_id));
        assert!(load_netdisk_settings(&conn).unwrap().auto_import);
        let result = import_task(&conn, &task, &f.roots, &Arc::new(ScanControl::new()));
        assert!(result.is_ok());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM download_publish_jobs WHERE engine='netdisk'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            count, 1,
            "auto-import must not claim the second file after the setting is disabled"
        );
    }

    #[test]
    fn late_artist_binding_can_recover() {
        let f = fixture_configured(|conn| {
            conn.execute(
                "UPDATE kemono_subscriptions SET artist_id=NULL WHERE id=1",
                [],
            )
            .unwrap();
        });
        let conn = f.pool.get().unwrap();
        conn.execute("UPDATE kemono_subscriptions SET artist_id=1 WHERE id=1", [])
            .unwrap();
        let result = import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new()));
        assert!(
            result.is_ok(),
            "an accepted task must recover after its missing artist is bound"
        );
    }

    #[test]
    fn netdisk_freezes_naming_before_first_import() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let sources = vec![f.source.clone()];
        let original = destination(&conn, &f.task, &f.roots, &sources).unwrap();
        let mut templates = get_pawchive_settings(&conn).unwrap();
        templates.folder_template = "{date}/{title}".into();
        templates.attachment_template = "changed_{name}".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        let mut settings = load_netdisk_settings(&conn).unwrap();
        settings.import_dir = original.artist.join("NewBase").to_string_lossy().into();
        crate::netdisk::save_netdisk_settings(&conn, &settings).unwrap();
        conn.execute("UPDATE kemono_posts SET title='NewTitle',published_at='2026-08-21T09:00:00Z' WHERE id=1", []).unwrap();
        let frozen = destination(&conn, &f.task, &f.roots, &sources).unwrap();
        assert_eq!(frozen.folder, original.folder);
        assert_eq!(frozen.names, original.names);
        assert_eq!(
            import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).unwrap(),
            1
        );
        assert!(f.target.is_file());
        let next = create_bridge_task(&conn, "jd-local", "test", 1, &f.task.links, None).unwrap();
        let next = destination(&conn, &next, &f.roots, &sources).unwrap();
        assert_eq!(
            next.folder,
            original.artist.join("NewBase/2026-08-21/NewTitle")
        );
        assert_eq!(next.names, ["changed_bundle.rar"]);
        // Pre-upgrade tasks with a reserved publication retain that path.
        conn.execute(
            "DELETE FROM netdisk_task_naming WHERE task_id=?1",
            [&f.task.task_id],
        )
        .unwrap();
        let legacy = destination(&conn, &f.task, &f.roots, &sources).unwrap();
        assert_eq!(legacy.folder, original.folder);
        assert_eq!(
            import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).unwrap(),
            1
        );
    }

    #[test]
    fn netdisk_destination_preserves_custom_folder_components() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let mut settings = get_pawchive_settings(&conn).unwrap();
        for (template, expected) in [
            ("{user}/{date} {title}/", "Artist/2026-07-23 Bundle"),
            ("{date} {title}/", "2026-07-23 Bundle"),
            ("{user}/{user}/{title}/", "Artist/Artist/Bundle"),
            ("{service}/{userID}/{id}", "patreon/123/remote"),
            ("{year}/{month}/{title}", "2026/07/Bundle"),
            ("固定目录", "固定目录"),
            ("  ", "Artist/2026-07-23 Bundle"),
        ] {
            settings.folder_template = template.to_string();
            crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
            conn.execute(
                "DELETE FROM netdisk_task_naming WHERE task_id=?1",
                [&f.task.task_id],
            )
            .unwrap();
            freeze_task_naming(&conn, &f.task.task_id, f.task.post_id).unwrap();
            let actual = destination(&conn, &f.task, &f.roots, &[]).unwrap();
            assert_eq!(actual.folder, actual.artist.join(expected));
        }
    }

    #[test]
    fn netdisk_destination_uses_both_file_templates_and_checks_custom_base() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let mut templates = get_pawchive_settings(&conn).unwrap();
        templates.image_template = "{userID}_{index}.{ext}".into();
        templates.attachment_template = "{service}_{index}_{name}".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        let sources = [PathBuf::from("cover.PNG"), PathBuf::from("archive.rar")];
        conn.execute(
            "DELETE FROM netdisk_task_naming WHERE task_id=?1",
            [&f.task.task_id],
        )
        .unwrap();
        freeze_task_naming(&conn, &f.task.task_id, f.task.post_id).unwrap();
        let plan = destination(&conn, &f.task, &f.roots, &sources).unwrap();
        assert_eq!(plan.names, ["123_0.PNG", "patreon_1_archive.rar"]);
        let mut settings = load_netdisk_settings(&conn).unwrap();
        settings.import_dir = plan.artist.parent().unwrap().to_string_lossy().into();
        crate::netdisk::save_netdisk_settings(&conn, &settings).unwrap();
        conn.execute(
            "DELETE FROM netdisk_task_naming WHERE task_id=?1",
            [&f.task.task_id],
        )
        .unwrap();
        freeze_task_naming(&conn, &f.task.task_id, f.task.post_id).unwrap();
        let parent_base = destination(&conn, &f.task, &f.roots, &sources).unwrap();
        assert_eq!(parent_base.folder, plan.artist.join("2026-07-23 Bundle"));
        templates.folder_template = "Other/{title}".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        conn.execute(
            "DELETE FROM netdisk_task_naming WHERE task_id=?1",
            [&f.task.task_id],
        )
        .unwrap();
        freeze_task_naming(&conn, &f.task.task_id, f.task.post_id).unwrap();
        assert!(destination(&conn, &f.task, &f.roots, &sources).is_err());
    }

    #[test]
    fn netdisk_constant_filename_keeps_two_outputs_distinct_and_retry_stable() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let other = f.source.with_file_name("other.rar");
        std::fs::write(&other, b"second archive, different bytes").unwrap();
        let mut task = f.task.clone();
        task.expected.push("link2".into());
        conn.execute(
            "UPDATE pawchive_external_receipts SET output_paths=?1 WHERE task_id=?2",
            params![
                format!("{}\n{}", other.display(), f.source.display()),
                task.task_id
            ],
        )
        .unwrap();
        let mut templates = get_pawchive_settings(&conn).unwrap();
        templates.attachment_template = "same.rar".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        conn.execute(
            "DELETE FROM netdisk_task_naming WHERE task_id=?1",
            [&f.task.task_id],
        )
        .unwrap();
        freeze_task_naming(&conn, &f.task.task_id, f.task.post_id).unwrap();
        let control = Arc::new(ScanControl::new());
        assert_eq!(import_task(&conn, &task, &f.roots, &control).unwrap(), 2);
        let mut q = conn
            .prepare(
                "SELECT target_path FROM download_publish_jobs WHERE source_job_id=?1 ORDER BY id",
            )
            .unwrap();
        let paths = q
            .query_map([&task.task_id], |r| r.get::<_, String>(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(paths.len(), 2);
        assert_ne!(paths[0], paths[1]);
        assert_eq!(
            std::fs::read(&paths[0]).unwrap(),
            std::fs::read(&f.source).unwrap()
        );
        assert_eq!(
            std::fs::read(&paths[1]).unwrap(),
            std::fs::read(&other).unwrap()
        );
        templates.folder_template = "changed".into();
        templates.attachment_template = "renamed.rar".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        assert_eq!(import_task(&conn, &task, &f.roots, &control).unwrap(), 2);
        assert_eq!(
            std::fs::read_dir(f.target.parent().unwrap())
                .unwrap()
                .count(),
            2
        );
    }

    #[test]
    fn netdisk_finished_receipt_automatically_publishes_and_indexes_archive_once() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        assert_eq!(
            pending_auto_imports(&conn, "jd-local").unwrap(),
            vec![f.task.task_id.clone()]
        );
        let control = Arc::new(ScanControl::new());
        assert_eq!(import_task(&conn, &f.task, &f.roots, &control).unwrap(), 1);
        assert!(!control.is_running());
        assert_eq!(
            std::fs::read(&f.target).unwrap(),
            std::fs::read(&f.source).unwrap()
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            assert_ne!(
                f.target.metadata().unwrap().ino(),
                f.source.metadata().unwrap().ino()
            );
        }
        let (kind, archive): (String, i64) = conn
            .query_row(
                "SELECT media_type,is_archive FROM items WHERE file_name='2026-07-23 bundle.rar'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(kind, "archive");
        assert_eq!(archive, 1);
        assert_eq!(import_task(&conn, &f.task, &f.roots, &control).unwrap(), 1);
        assert!(pending_auto_imports(&conn, "jd-local").unwrap().is_empty());
        assert_eq!(
            crate::netdisk::bridge_task_view(&conn, &f.task).unwrap()["state_label"],
            "已入库"
        );
        assert_eq!(
            std::fs::read_dir(f.target.parent().unwrap())
                .unwrap()
                .count(),
            1,
            "no extraction or duplicate files"
        );
    }

    #[test]
    fn netdisk_collision_never_overwrites_and_retry_recovers() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let control = Arc::new(ScanControl::new());
        std::fs::create_dir_all(f.target.parent().unwrap()).unwrap();
        std::fs::write(&f.target, b"user file").unwrap();
        assert!(import_task(&conn, &f.task, &f.roots, &control).is_err());
        assert_eq!(std::fs::read(&f.target).unwrap(), b"user file");
        assert!(f.source.is_file());
        assert!(!control.is_running());
        assert!(import_error(&conn, &f.task.task_id).unwrap().is_some());
        assert!(
            pending_auto_imports(&conn, "jd-local").unwrap().is_empty(),
            "failed work backs off"
        );
        std::fs::rename(&f.target, f.target.with_extension("kept")).unwrap();
        assert_eq!(import_task(&conn, &f.task, &f.roots, &control).unwrap(), 1);
    }

    #[test]
    fn netdisk_retry_preserves_reserved_target_after_template_changes() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let identity = f.source.to_string_lossy().replace('\\', "/");
        let request = IngestPublishRequest {
            engine: "netdisk",
            source_job_id: &f.task.task_id,
            manifest_version: f.task.manifest_version,
            source_identity: &identity,
            source_path: &f.source,
            expected_length: f.source.metadata().unwrap().len(),
            expected_blake3: "deliberately incorrect digest",
            target_path: &f.target,
        };
        assert!(publish_completed_file(&conn, &request, &f.roots).is_err());
        assert!(!f.target.exists());
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM download_publish_jobs WHERE source_job_id=?1",
                [&f.task.task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(count, 1, "a failed staging copy keeps its reservation");
        let mut templates = get_pawchive_settings(&conn).unwrap();
        templates.folder_template = "changed/{title}".into();
        templates.attachment_template = "renamed.rar".into();
        crate::pawchive::save_pawchive_settings(&conn, &templates).unwrap();
        assert_eq!(
            import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).unwrap(),
            1
        );
        assert_eq!(
            std::fs::read(&f.target).unwrap(),
            std::fs::read(&f.source).unwrap()
        );
    }

    #[test]
    fn netdisk_off_and_busy_leave_media_untouched() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        conn.execute(
            "UPDATE app_settings SET value='0' WHERE key='netdisk_auto_import'",
            [],
        )
        .unwrap();
        assert!(pending_auto_imports(&conn, "jd-local").unwrap().is_empty());
        let control = Arc::new(ScanControl::new());
        let guard = control.try_claim().unwrap();
        assert!(import_task(&conn, &f.task, &f.roots, &control).is_err());
        assert!(!f.target.exists());
        assert!(f.source.exists());
        drop(guard);
    }

    #[test]
    fn netdisk_stale_manifest_refuses_publication() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        conn.execute(
            "UPDATE kemono_posts SET manifest_version=manifest_version+1 WHERE id=1",
            [],
        )
        .unwrap();
        assert!(import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).is_err());
        assert!(!f.target.exists());
    }

    #[test]
    fn netdisk_cleanup_after_import_deletes_staging_and_queues_remove() {
        let f = fixture_configured(|conn| {
            conn.execute(
                "INSERT INTO app_settings(key, value) VALUES('netdisk_cleanup_after_import', '1')
                 ON CONFLICT(key) DO UPDATE SET value='1'",
                [],
            )
            .unwrap();
        });
        let conn = f.pool.get().unwrap();
        let result = import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).unwrap();
        assert_eq!(result, 1);
        assert!(f.target.exists());
        assert!(!f.source.exists());

        let remove_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM netdisk_bridge_commands WHERE job_id = ?1 AND action = 'remove'",
                params![f.task.task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remove_count, 1);
    }

    #[test]
    fn netdisk_cleanup_disabled_preserves_staging() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let result = import_task(&conn, &f.task, &f.roots, &Arc::new(ScanControl::new())).unwrap();
        assert_eq!(result, 1);
        assert!(f.target.exists());
        assert!(f.source.exists());

        let remove_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM netdisk_bridge_commands WHERE job_id = ?1 AND action = 'remove'",
                params![f.task.task_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(remove_count, 0);
    }
}
