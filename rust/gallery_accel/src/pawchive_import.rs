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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DbConfig, DbPool};
    struct Fixture {
        _dir: tempfile::TempDir,
        pool: Arc<DbPool>,
        roots: MediaRoots,
        target: PathBuf,
    }
    fn fixture() -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let media = dir.path().join("media");
        let artist = media.join("Artist");
        std::fs::create_dir_all(&artist).unwrap();
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
        conn.execute("INSERT INTO kemono_subscriptions(id,artist_id,service,user_id,target_dir,created_at,updated_at) VALUES(1,1,'patreon','123',?1,'t','t')",[artist.to_string_lossy().replace('\\',"/")]).unwrap();
        conn.execute("INSERT INTO kemono_posts(id,subscription_id,post_id,title,created_at,updated_at) VALUES(1,1,'remote','Bundle','t','t')",[]).unwrap();
        let source = dir.path().join("source.rar");
        std::fs::write(&source, b"verified archive bytes").unwrap();
        let target = artist.join("2026-07-23 Bundle/file.rar");
        let roots = MediaRoots::identical(
            vec![media.to_string_lossy().replace('\\', "/")],
            vec!["Media".into()],
        );
        let job = crate::ingest_publish::publish_completed_file(
            &conn,
            &crate::ingest_publish::IngestPublishRequest {
                engine: "pawchive",
                source_job_id: "test",
                manifest_version: 1,
                source_identity: "one",
                source_path: &source,
                expected_length: 22,
                expected_blake3: &blake3::hash(b"verified archive bytes").to_hex(),
                target_path: &target,
            },
            &roots,
        )
        .unwrap();
        conn.execute("INSERT INTO kemono_files(post_id,source_identity,remote_path,file_name,file_type,status,publish_job_id,created_at,updated_at) VALUES(1,'one','/file.rar','file.rar','archive','done',?1,'t','t')",[job.job_id]).unwrap();
        let mut settings = get_pawchive_settings(&conn).unwrap();
        settings.enabled = true;
        settings.auto_ingest = true;
        crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
        drop(conn);
        Fixture {
            _dir: dir,
            pool,
            roots,
            target,
        }
    }

    /// A subscription added with a folder of its own is never bound to a
    /// library artist. Its delivered files still have to reach the library, so
    /// the folder itself becomes the artist and the binding is remembered.
    #[test]
    fn an_unbound_subscription_registers_its_folder_as_the_artist() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let target: String = conn
            .query_row(
                "SELECT target_dir FROM kemono_subscriptions WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // What the download panel leaves behind when the user names a folder
        // instead of picking an artist.
        conn.execute("UPDATE kemono_subscriptions SET artist_id=NULL", [])
            .unwrap();
        conn.execute("DELETE FROM artists", []).unwrap();
        let control = Arc::new(ScanControl::new());

        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 1);

        let (id, name, path): (i64, String, String) = conn
            .query_row("SELECT id,name,path FROM artists", [], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })
            .unwrap();
        assert_eq!(path, target, "the folder the user named is the artist");
        assert_eq!(name, "Artist");
        let bound: Option<i64> = conn
            .query_row(
                "SELECT artist_id FROM kemono_subscriptions WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            bound,
            Some(id),
            "the binding is remembered for the next round"
        );
        assert_eq!(
            conn.query_row(
                "SELECT COUNT(*) FROM items WHERE artist_id=?1 AND missing=0",
                [id],
                |r| r.get::<_, i64>(0)
            )
            .unwrap(),
            1
        );

        // A second pass is a no-op, never a second artist for the same folder.
        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM artists", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn successful_retry_clears_import_error() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let control = Arc::new(ScanControl::new());
        std::fs::write(&f.target, b"changed").unwrap();
        assert!(import_post(&conn, 1, &f.roots, &control, true).is_err());
        std::fs::write(&f.target, b"verified archive bytes").unwrap();
        assert_eq!(import_post(&conn, 1, &f.roots, &control, false).unwrap(), 1);
        let (stage, error): (String, String) = conn
            .query_row(
                "SELECT stage,error_message FROM download_publish_jobs WHERE engine='pawchive'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(stage, "ingested");
        assert!(
            error.is_empty(),
            "successful import must not retain its resolved failure as a current error"
        );
    }

    #[test]
    fn automatic_import_honors_settings_busy_gate_and_restart() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let control = Arc::new(ScanControl::new());
        assert_eq!(pending_posts(&conn).unwrap(), [1]);
        let mut settings = get_pawchive_settings(&conn).unwrap();
        settings.auto_ingest = false;
        crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
        assert!(pending_posts(&conn).unwrap().is_empty());
        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        settings.auto_ingest = true;
        settings.enabled = false;
        crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
        assert!(pending_posts(&conn).unwrap().is_empty());
        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 0);
        settings.enabled = true;
        crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
        let guard = control.try_claim().unwrap();
        assert!(import_post(&conn, 1, &f.roots, &control, true).is_err());
        drop(guard);
        // A fresh connection/control resumes persisted published jobs.
        drop(conn);
        let conn = f.pool.get().unwrap();
        let control = Arc::new(ScanControl::new());
        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 1);
        assert!(pending_posts(&conn).unwrap().is_empty());
        assert_eq!(import_post(&conn, 1, &f.roots, &control, true).unwrap(), 0);
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM items WHERE missing=0", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        assert_eq!(std::fs::read(&f.target).unwrap(), b"verified archive bytes");
    }
    #[test]
    fn manual_import_works_with_automation_disabled() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        let mut settings = get_pawchive_settings(&conn).unwrap();
        settings.enabled = false;
        settings.auto_ingest = false;
        crate::pawchive::save_pawchive_settings(&conn, &settings).unwrap();
        assert_eq!(
            import_post(&conn, 1, &f.roots, &Arc::new(ScanControl::new()), false).unwrap(),
            1
        );
    }
    /// An unbound subscription whose folder is not under an authorized root has
    /// no artist to index into, so its delivery is a retryable failure rather
    /// than a file indexed somewhere the library does not point at.
    #[test]
    fn missing_artist_binding_reports_retryable_failure() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        conn.execute(
            "UPDATE kemono_subscriptions SET artist_id=NULL, target_dir='/outside/Elsewhere' WHERE id=1",
            [],
        )
        .unwrap();
        let error =
            import_post(&conn, 1, &f.roots, &Arc::new(ScanControl::new()), true).unwrap_err();
        assert!(error.to_string().contains("未绑定画师"));
        assert!(pending_posts(&conn).unwrap().is_empty());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM artists", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            1,
            "no artist is invented outside the authorized roots"
        );
    }

    #[test]
    fn modified_publication_is_not_indexed_and_failure_backs_off() {
        let f = fixture();
        let conn = f.pool.get().unwrap();
        std::fs::write(&f.target, b"changed").unwrap();
        assert!(import_post(&conn, 1, &f.roots, &Arc::new(ScanControl::new()), true).is_err());
        assert_eq!(
            conn.query_row("SELECT COUNT(*) FROM items", [], |r| r.get::<_, i64>(0))
                .unwrap(),
            0
        );
        assert!(pending_posts(&conn).unwrap().is_empty());
        conn.execute(
            "UPDATE download_publish_jobs SET updated_at=strftime('%s','now')-61",
            [],
        )
        .unwrap();
        assert_eq!(pending_posts(&conn).unwrap(), [1]);
        std::fs::write(&f.target, b"verified archive bytes").unwrap();
        assert_eq!(
            import_post(&conn, 1, &f.roots, &Arc::new(ScanControl::new()), true).unwrap(),
            1
        );
    }
}
