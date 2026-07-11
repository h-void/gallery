use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    create_db_backup, run_full_library_scan, run_hash_batch, DbPool, MediaRoots, ScanControl,
};

#[derive(Clone, Default)]
pub struct WorkerStatus {
    inner: Arc<Mutex<BTreeMap<String, Value>>>,
}

impl WorkerStatus {
    pub fn record(&self, name: &str, running: bool, last: Value, next_at: Option<f64>) {
        self.inner.lock().unwrap().insert(
            name.to_string(),
            json!({"running": running, "last": last, "next_at": next_at}),
        );
    }

    pub fn snapshot(&self) -> Value {
        json!(self.inner.lock().unwrap().clone())
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

fn interval_env(name: &str) -> Option<Duration> {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0)
        .map(Duration::from_secs)
}

fn enabled_env(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn backup_retention() -> usize {
    std::env::var("DB_BACKUP_RETENTION")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(10)
}

fn prune_backup_root(root: &Path, retention: usize) -> Result<usize> {
    std::fs::create_dir_all(root)?;
    let root = root.canonicalize()?;
    let mut entries = std::fs::read_dir(&root)?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();
            path.is_dir().then_some(path)
        })
        .filter_map(|path| path.canonicalize().ok())
        .filter(|path| path.starts_with(&root))
        .collect::<Vec<_>>();
    entries.sort();
    let remove_count = entries.len().saturating_sub(retention);
    for path in entries.into_iter().take(remove_count) {
        if path.starts_with(&root) {
            std::fs::remove_dir_all(path)?;
        }
    }
    Ok(remove_count)
}

fn run_backup(pool: &Arc<DbPool>) -> Result<Value> {
    let conn = pool.get()?;
    let backup = create_db_backup(&conn)?;
    let root = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    let pruned = prune_backup_root(&Path::new(&root).join("db-backups"), backup_retention())?;
    Ok(json!({"ok": true, "backup": backup, "pruned": pruned}))
}

pub fn spawn_configured_workers(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
) {
    if let Some(interval) = interval_env("SCAN_INTERVAL") {
        spawn_scan_loop(
            pool.clone(),
            roots.clone(),
            scan.clone(),
            status.clone(),
            interval,
        );
    } else {
        status.record("scan", false, json!({"status": "disabled"}), None);
    }

    if let Some(interval) = interval_env("HASH_INTERVAL") {
        let batch_size = std::env::var("HASH_BATCH_SIZE")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(500)
            .clamp(1, 500);
        spawn_hash_loop(pool.clone(), scan, status.clone(), interval, batch_size);
    } else {
        status.record("hash", false, json!({"status": "disabled"}), None);
    }

    let backup_interval = interval_env("DB_BACKUP_INTERVAL");
    let backup_on_start = enabled_env("DB_BACKUP_ON_START");
    if backup_on_start || backup_interval.is_some() {
        let start_delay = std::env::var("DB_BACKUP_START_DELAY")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or_default();
        spawn_backup_loop(pool, status, backup_interval, backup_on_start, start_delay);
    } else {
        status.record("backup", false, json!({"status": "disabled"}), None);
    }
}

fn spawn_scan_loop(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
    interval: Duration,
) {
    status.record(
        "scan",
        true,
        json!({"status": "waiting"}),
        Some(now() + interval.as_secs_f64()),
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let next_at = now() + interval.as_secs_f64();
            let result = if !scan.try_start() {
                Ok(json!({"ok": true, "skipped": "scan_active"}))
            } else {
                let pool = pool.clone();
                let roots = roots.clone();
                let scan = scan.clone();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let conn = pool.get()?;
                    run_full_library_scan(&conn, &roots, &scan)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            };
            status.record(
                "scan",
                true,
                result.unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                Some(next_at),
            );
        }
    });
}

fn spawn_hash_loop(
    pool: Arc<DbPool>,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
    interval: Duration,
    batch_size: i64,
) {
    status.record(
        "hash",
        true,
        json!({"status": "waiting"}),
        Some(now() + interval.as_secs_f64()),
    );
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let next_at = now() + interval.as_secs_f64();
            let result = if scan.is_running() {
                Ok(json!({"ok": true, "skipped": "scan_active"}))
            } else {
                let pool = pool.clone();
                tokio::task::spawn_blocking(move || -> Result<Value> {
                    let conn = pool.get()?;
                    run_hash_batch(&conn, batch_size)
                })
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result)
            };
            status.record(
                "hash",
                true,
                result.unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                Some(next_at),
            );
        }
    });
}

fn spawn_backup_loop(
    pool: Arc<DbPool>,
    status: WorkerStatus,
    interval: Option<Duration>,
    on_start: bool,
    start_delay: Duration,
) {
    status.record(
        "backup",
        true,
        json!({"status": "waiting"}),
        if on_start {
            Some(now() + start_delay.as_secs_f64())
        } else {
            interval.map(|value| now() + value.as_secs_f64())
        },
    );
    tokio::spawn(async move {
        if on_start {
            if !start_delay.is_zero() {
                tokio::time::sleep(start_delay).await;
            }
            let pool = pool.clone();
            let result = tokio::task::spawn_blocking(move || run_backup(&pool))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            status.record(
                "backup",
                true,
                result.unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                interval.map(|value| now() + value.as_secs_f64()),
            );
        }
        let Some(interval) = interval else {
            return;
        };
        loop {
            tokio::time::sleep(interval).await;
            let next_at = now() + interval.as_secs_f64();
            let pool = pool.clone();
            let result = tokio::task::spawn_blocking(move || run_backup(&pool))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            status.record(
                "backup",
                true,
                result.unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()})),
                Some(next_at),
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn zero_intervals_disable_all_workers() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let previous = [
            ("SCAN_INTERVAL", std::env::var("SCAN_INTERVAL").ok()),
            ("HASH_INTERVAL", std::env::var("HASH_INTERVAL").ok()),
            (
                "DB_BACKUP_INTERVAL",
                std::env::var("DB_BACKUP_INTERVAL").ok(),
            ),
            (
                "DB_BACKUP_ON_START",
                std::env::var("DB_BACKUP_ON_START").ok(),
            ),
        ];
        std::env::set_var("SCAN_INTERVAL", "0");
        std::env::set_var("HASH_INTERVAL", "0");
        std::env::set_var("DB_BACKUP_INTERVAL", "0");
        std::env::set_var("DB_BACKUP_ON_START", "0");

        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            DbPool::with_config(
                dir.path().join("gallery.db"),
                crate::DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let status = WorkerStatus::default();
        spawn_configured_workers(
            pool,
            MediaRoots {
                roots: Vec::new(),
                labels: Vec::new(),
                real_paths: Vec::new().clone(),
            },
            Arc::new(ScanControl::new()),
            status.clone(),
        );

        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }

        let snapshot = status.snapshot();
        for name in ["scan", "hash", "backup"] {
            assert_eq!(snapshot[name]["running"], false);
            assert_eq!(snapshot[name]["last"]["status"], "disabled");
        }
    }

    #[tokio::test]
    async fn enabled_workers_publish_waiting_status_before_first_interval() {
        let _env_lock = ENV_LOCK.lock().unwrap();
        let previous = [
            ("SCAN_INTERVAL", std::env::var("SCAN_INTERVAL").ok()),
            ("HASH_INTERVAL", std::env::var("HASH_INTERVAL").ok()),
            (
                "DB_BACKUP_INTERVAL",
                std::env::var("DB_BACKUP_INTERVAL").ok(),
            ),
            (
                "DB_BACKUP_ON_START",
                std::env::var("DB_BACKUP_ON_START").ok(),
            ),
        ];
        std::env::set_var("SCAN_INTERVAL", "60");
        std::env::set_var("HASH_INTERVAL", "60");
        std::env::set_var("DB_BACKUP_INTERVAL", "0");
        std::env::set_var("DB_BACKUP_ON_START", "0");

        let dir = tempfile::tempdir().unwrap();
        let pool = Arc::new(
            DbPool::with_config(
                dir.path().join("gallery.db"),
                crate::DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let status = WorkerStatus::default();
        spawn_configured_workers(
            pool,
            MediaRoots {
                roots: Vec::new(),
                labels: Vec::new(),
                real_paths: Vec::new().clone(),
            },
            Arc::new(ScanControl::new()),
            status.clone(),
        );

        for (key, value) in previous {
            if let Some(value) = value {
                std::env::set_var(key, value);
            } else {
                std::env::remove_var(key);
            }
        }

        let snapshot = status.snapshot();
        assert_eq!(snapshot["scan"]["running"], true);
        assert_eq!(snapshot["hash"]["running"], true);
        assert!(snapshot["scan"]["next_at"].as_f64().is_some());
        assert!(snapshot["hash"]["next_at"].as_f64().is_some());
    }

    #[test]
    fn backup_retention_stays_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("db-backups");
        std::fs::create_dir_all(root.join("20240101")).unwrap();
        std::fs::create_dir_all(root.join("20240102")).unwrap();
        assert_eq!(prune_backup_root(&root, 1).unwrap(), 1);
        assert!(!root.join("20240101").exists());
        assert!(root.join("20240102").exists());
    }
}
