//! Scan already-published subscription files, independently of download rounds.
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};

use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use rusqlite::{params, Connection};

use crate::{media_roots::MediaRoots, pawchive::get_pawchive_settings, scan::ScanControl};

pub fn pending_posts(conn: &Connection) -> Result<Vec<i64>> {
    let settings = get_pawchive_settings(conn)?;
    if !settings.enabled || !settings.auto_ingest {
        return Ok(Vec::new());
    }
    let mut q = conn.prepare(
        "SELECT f.post_id FROM download_publish_jobs j
        JOIN kemono_files f ON f.publish_job_id=j.id
        WHERE j.engine='pawchive' AND j.stage='published'
          AND (COALESCE(j.error_message,'')='' OR j.updated_at < strftime('%s','now')-60)
        GROUP BY f.post_id ORDER BY MIN(j.updated_at),f.post_id LIMIT 4",
    )?;
    let posts = q
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(posts)
}

pub fn import_post(
    conn: &Connection,
    post_id: i64,
    roots: &MediaRoots,
    control: &Arc<ScanControl>,
    automatic: bool,
) -> Result<usize> {
    if automatic {
        let settings = get_pawchive_settings(conn)?;
        if !settings.enabled || !settings.auto_ingest {
            return Ok(0);
        }
    }
    let _guard = control
        .try_claim()
        .ok_or_else(|| anyhow!("扫描或文件操作正在进行，稍后重试"))?;
    let result = import_inner(conn, post_id, roots, control, automatic);
    if let Err(error) = &result {
        conn.execute(
            "UPDATE download_publish_jobs SET error_message=?1,updated_at=strftime('%s','now')
            WHERE stage='published' AND engine='pawchive'
              AND id IN (SELECT publish_job_id FROM kemono_files WHERE post_id=?2)",
            params![error.to_string(), post_id],
        )?;
    }
    result
}

struct PublishedFile {
    id: i64,
    path: String,
    hash: String,
    artist_id: Option<i64>,
    artist_path: Option<String>,
    subscription_id: i64,
    subscription_target_dir: String,
    subscription_user_id: String,
}

fn import_inner(
    conn: &Connection,
    post_id: i64,
    roots: &MediaRoots,
    control: &ScanControl,
    automatic: bool,
) -> Result<usize> {
    let jobs: Vec<PublishedFile> = {
        let mut q = conn.prepare(
            "SELECT j.id,j.target_path,j.expected_blake3,a.id,a.path,s.id,s.target_dir,s.user_id
            FROM download_publish_jobs j JOIN kemono_files f ON f.publish_job_id=j.id
            JOIN kemono_posts p ON p.id=f.post_id
            JOIN kemono_subscriptions s ON s.id=p.subscription_id
            LEFT JOIN artists a ON a.id=s.artist_id
            WHERE j.engine='pawchive' AND j.stage='published' AND p.id=?1 ORDER BY j.id LIMIT 500",
        )?;
        let rows = q
            .query_map([post_id], |r| {
                Ok(PublishedFile {
                    id: r.get(0)?,
                    path: r.get(1)?,
                    hash: r.get(2)?,
                    artist_id: r.get(3)?,
                    artist_path: r.get(4)?,
                    subscription_id: r.get(5)?,
                    subscription_target_dir: r.get(6)?,
                    subscription_user_id: r.get(7)?,
                })
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    let mut folders = BTreeMap::<(i64, PathBuf, PathBuf), Vec<(i64, PathBuf, String)>>::new();
    for PublishedFile {
        id,
        path,
        hash,
        artist_id,
        artist_path,
        subscription_id,
        subscription_target_dir,
        subscription_user_id,
    } in jobs
    {
        // A subscription added with a folder of its own was never bound to a
        // library artist, so the LEFT JOIN above gives no row. The delivered
        // folder is that artist: register it, and remember the binding so the
        // next round reads the column instead of deriving it again.
        let (artist_id, artist_path) = match artist_id.zip(artist_path) {
            Some(bound) => bound,
            None => {
                let (artist_id, artist_path) = crate::scan::ensure_artist_for_folder(
                    conn,
                    roots,
                    &subscription_target_dir,
                    &subscription_user_id,
                )?
                .ok_or_else(|| anyhow!("作品未绑定画师，且下载目录不在授权媒体目录内"))?;
                conn.execute(
                    "UPDATE kemono_subscriptions SET artist_id = ?1, updated_at = ?2
                     WHERE id = ?3 AND artist_id IS NULL",
                    params![artist_id, Utc::now().to_rfc3339(), subscription_id],
                )?;
                (artist_id, artist_path)
            }
        };
        let artist = roots.map_to_real(&artist_path)?;
        let target = roots.map_to_real(&path)?;
        if !target.starts_with(&artist)
            || !crate::media_roots::path_under_authorized_roots(&target, roots)
        {
            bail!("已发布文件不在该画师授权目录内");
        }
        let folder = target
            .parent()
            .ok_or_else(|| anyhow!("已发布文件缺少父目录"))?
            .to_path_buf();
        folders
            .entry((artist_id, artist, folder))
            .or_default()
            .push((id, target, hash));
    }
    let mut count = 0;
    for ((artist_id, artist, folder), jobs) in folders {
        if automatic {
            let settings = get_pawchive_settings(conn)?;
            if !settings.enabled || !settings.auto_ingest {
                break;
            }
        }
        let size = jobs.len();
        crate::netdisk_import::index_published_files(
            conn, roots, control, artist_id, &artist, &folder, jobs,
        )?;
        count += size;
    }
    crate::pawchive::link_evidence_to_items(conn)?;
    Ok(count)
}
