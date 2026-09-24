use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::Result;
use serde_json::{json, Value};

use crate::{
    create_db_backup, finish_pawchive_sync, get_pawchive_settings, pawchive_round_due,
    run_full_library_scan_claimed, run_hash_batch_with_roots, run_pawchive_sync,
    try_begin_pawchive_sync, DbPool, MediaRoots, ScanControl, StatsRefreshGate, SyncTrigger,
};

#[derive(Clone, Default)]
pub struct WorkerStatus {
    inner: Arc<Mutex<BTreeMap<String, Value>>>,
    /// Set once when the status mutex is found poisoned. A poisoned lock must
    /// never kill the background loops: recover and keep recording, but keep
    /// health degraded until restart.
    poisoned: Arc<AtomicBool>,
}

impl WorkerStatus {
    fn lock(&self) -> MutexGuard<'_, BTreeMap<String, Value>> {
        match self.inner.lock() {
            Ok(guard) => guard,
            Err(poisoned) => {
                if !self.poisoned.swap(true, Ordering::SeqCst) {
                    log_error!(
                        "workers: status mutex was poisoned; recovered, health stays degraded"
                    );
                }
                poisoned.into_inner()
            }
        }
    }

    pub fn record(&self, name: &str, running: bool, last: Value, next_at: Option<f64>) {
        self.lock().insert(
            name.to_string(),
            json!({"running": running, "last": last, "next_at": next_at}),
        );
    }

    pub fn snapshot(&self) -> Value {
        json!(self.lock().clone())
    }

    /// True when a panic poisoned the status mutex and the worker recovered.
    pub fn recovered_from_poison(&self) -> bool {
        self.poisoned.load(Ordering::SeqCst)
    }
}

fn now() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0)
}

fn interval_env(name: &str) -> Option<Duration> {
    match std::env::var(name) {
        Ok(value) => {
            let trimmed = value.trim();
            match trimmed.parse::<u64>() {
                // `0` and an empty value mean "disabled"; anything else that
                // fails to parse is a misconfiguration worth surfacing (e.g.
                // "60s" would otherwise silently disable the loop).
                Ok(_) if trimmed.is_empty() => None,
                Ok(seconds) if seconds > 0 => Some(Duration::from_secs(seconds)),
                Ok(_) => None,
                Err(_) => {
                    if !trimmed.is_empty() {
                        log_error!(
                            "workers: ignoring invalid {name}={value:?}; expected seconds, loop disabled"
                        );
                    }
                    None
                }
            }
        }
        Err(_) => None,
    }
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

fn run_backup(pool: &Arc<DbPool>) -> Result<Value> {
    let conn = pool.get()?;
    let backup = create_db_backup(&conn)?;
    Ok(json!({"ok": true, "backup": backup}))
}

pub fn spawn_configured_workers(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
    stats_gate: Arc<StatsRefreshGate>,
) {
    // The branches below consume `pool`, `roots` and `status`; the Pawchive
    // loop is spawned last, so it keeps its own handles.
    let pawchive_pool = Arc::clone(&pool);
    let pawchive_roots = roots.clone();
    let pawchive_status = status.clone();
    spawn_pawchive_import_loop(pool.clone(), roots.clone(), scan.clone(), status.clone());
    // One gate for the whole process: the scan and the hash batch can each be the
    // thing that grows the library, and a per-loop gate would let two library
    // wide counts run per interval instead of one. The manual scan and hash
    // routes share this same gate for the same reason — a manual run is the work
    // the loop would otherwise have done, so it must refresh on the same budget
    // rather than analyze again on every click.
    if let Some(interval) = interval_env("SCAN_INTERVAL") {
        spawn_scan_loop(
            pool.clone(),
            roots.clone(),
            scan.clone(),
            status.clone(),
            interval,
            stats_gate.clone(),
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
        spawn_hash_loop(
            pool.clone(),
            roots,
            scan,
            status.clone(),
            interval,
            batch_size,
            stats_gate,
        );
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

    // The Pawchive loop always runs: whether it does anything is decided per
    // tick from the stored settings, so the master switch can be toggled in the
    // UI without a service restart.
    spawn_pawchive_loop(pawchive_pool, pawchive_roots, pawchive_status);
}

/// How often the loop asks whether a round is due. The configured interval is
/// measured in hours, so a minute-granularity check is plenty and keeps the
/// disabled case nearly free.
const PAWCHIVE_TICK: Duration = Duration::from_secs(60);

fn spawn_pawchive_import_loop(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
) {
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(Duration::from_secs(10)).await;
            if scan.is_shutting_down() {
                break;
            }
            let pool = Arc::clone(&pool);
            let roots = roots.clone();
            let scan = Arc::clone(&scan);
            let result = tokio::task::spawn_blocking(move || -> anyhow::Result<usize> {
                let conn = pool.get()?;
                let mut imported = 0;
                for post in crate::pawchive_import::pending_posts(&conn)? {
                    if scan.is_shutting_down() {
                        break;
                    }
                    imported +=
                        crate::pawchive_import::import_post(&conn, post, &roots, &scan, true)?;
                }
                Ok(imported)
            })
            .await;
            let last = match result {
                Ok(Ok(count)) => json!({"status":"idle","imported":count}),
                Ok(Err(error)) => json!({"status":"waiting","error":error.to_string()}),
                Err(error) => json!({"status":"error","error":error.to_string()}),
            };
            status.record("pawchive_import", true, last, None);
        }
        status.record("pawchive_import", false, json!({"status":"stopped"}), None);
    });
}

/// Background subscription rounds for Pawchive.
///
/// The master switch and the interval are read from `app_settings` on every
/// tick; the single-flight slot is shared with the manual route, so a manual
/// round in progress simply makes the scheduled one wait for the next tick.
fn spawn_pawchive_loop(pool: Arc<DbPool>, roots: MediaRoots, status: WorkerStatus) {
    status.record("pawchive", true, json!({"status": "idle"}), None);
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(PAWCHIVE_TICK).await;
            let due = {
                let conn = match pool.get() {
                    Ok(conn) => conn,
                    Err(_) => continue,
                };
                match get_pawchive_settings(&conn) {
                    Ok(settings) => settings.enabled && pawchive_round_due(settings.interval_hours),
                    Err(_) => false,
                }
            };
            if !due || !try_begin_pawchive_sync(SyncTrigger::Scheduled) {
                continue;
            }
            let outcome = run_pawchive_sync(
                Arc::clone(&pool),
                roots.clone(),
                SyncTrigger::Scheduled,
                None,
            )
            .await;
            finish_pawchive_sync(outcome);
        }
    });
}

fn spawn_scan_loop(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
    interval: Duration,
    stats_gate: Arc<StatsRefreshGate>,
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
            if scan.is_shutting_down() {
                break;
            }
            let result = match scan.try_claim() {
                None => Ok(json!({"ok": true, "skipped": "scan_active"})),
                Some(guard) => {
                    let pool = pool.clone();
                    let roots = roots.clone();
                    let scan = scan.clone();
                    let gate = stats_gate.clone();
                    tokio::task::spawn_blocking(move || -> Result<Value> {
                        let _slot = guard;
                        let conn = pool.get()?;
                        let result = run_full_library_scan_claimed(&conn, &roots, &scan);
                        // A scan is the main way a library grows, so this is the
                        // trigger that closes the "fresh install imports 750k
                        // rows before its next restart" gap. It runs inside the
                        // blocking task, never on an async worker.
                        gate.refresh_after_work(&conn);
                        result
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                }
            };
            let next_at = now() + interval.as_secs_f64();
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
    roots: MediaRoots,
    scan: Arc<ScanControl>,
    status: WorkerStatus,
    interval: Duration,
    batch_size: i64,
    stats_gate: Arc<StatsRefreshGate>,
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
            if scan.is_shutting_down() {
                break;
            }
            let result = match scan.try_claim() {
                None => {
                    // Claim the operation slot: hashing while a scan or folder move
                    // rewrites paths reads stale files. Skip this tick instead.
                    Ok(json!({"ok": true, "skipped": "scan_active"}))
                }
                Some(guard) => {
                    let pool = pool.clone();
                    let roots = roots.clone();
                    let gate = stats_gate.clone();
                    tokio::task::spawn_blocking(move || -> Result<Value> {
                        let _slot = guard;
                        let conn = pool.get()?;
                        let result = run_hash_batch_with_roots(&conn, &roots, batch_size);
                        // Candidates that the scan discovered are only promoted
                        // into `items` here, so the hash loop is the other half
                        // of the ingestion path the startup bootstrap misses.
                        gate.refresh_after_work(&conn);
                        result
                    })
                    .await
                    .map_err(anyhow::Error::from)
                    .and_then(|result| result)
                }
            };
            let next_at = now() + interval.as_secs_f64();
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
            let pool = pool.clone();
            let result = tokio::task::spawn_blocking(move || run_backup(&pool))
                .await
                .map_err(anyhow::Error::from)
                .and_then(|result| result);
            let next_at = now() + interval.as_secs_f64();
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
    use crate::folder_archive::prune_backup_root;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    // `spawn_configured_workers` now also starts the Pawchive subscription
    // loop, which is runtime-driven rather than interval-gated (it decides per
    // tick from the stored settings), so this needs a Tokio context even when
    // every interval is 0.
    #[tokio::test]
    async fn zero_intervals_disable_all_workers() {
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
            Arc::new(StatsRefreshGate::new(Duration::from_secs(60))),
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
            Arc::new(StatsRefreshGate::new(Duration::from_secs(60))),
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

    #[test]
    fn poisoned_status_mutex_recovers_and_reports_degraded() {
        let status = WorkerStatus::default();
        // Poison the mutex: a thread panics while holding the lock.
        let poacher = {
            let inner = std::sync::Arc::clone(&status.inner);
            std::thread::spawn(move || {
                let _guard = inner.lock().unwrap();
                panic!("poison the status mutex");
            })
        };
        let _ = poacher.join();
        // record/snapshot must survive the poisoned lock and keep serving.
        status.record("scan", true, json!({"status": "waiting"}), None);
        let snapshot = status.snapshot();
        assert_eq!(snapshot["scan"]["last"]["status"], "waiting");
        assert!(
            status.recovered_from_poison(),
            "poison recovery must surface as degraded"
        );
    }

    #[tokio::test]
    async fn scan_worker_next_at_computed_after_task_run() {
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
        let scan = Arc::new(ScanControl::new());
        let roots = MediaRoots {
            roots: Vec::new(),
            labels: Vec::new(),
            real_paths: Vec::new(),
        };
        let interval = Duration::from_millis(50);
        let start_time = now();
        spawn_scan_loop(
            pool,
            roots,
            scan,
            status.clone(),
            interval,
            Arc::new(StatsRefreshGate::new(Duration::from_secs(60))),
        );

        tokio::time::sleep(Duration::from_millis(150)).await;
        let snapshot = status.snapshot();
        assert_eq!(snapshot["scan"]["running"], true);
        let next_at = snapshot["scan"]["next_at"]
            .as_f64()
            .expect("next_at must be present");
        assert!(
            next_at >= start_time + 0.08,
            "next_at ({next_at}) must reflect post-run time, start_time={start_time}"
        );
    }
}
