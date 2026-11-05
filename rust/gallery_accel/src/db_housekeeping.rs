use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde::Serialize;

use crate::folder_archive::create_housekeeping_db_backup;

const TERMINAL_SCAN_CANDIDATE_STATUSES: &str = "'resolved','new','superseded'";

#[derive(Debug, Default, Serialize)]
pub struct HousekeepingResult {
    pub missing_items_deleted: usize,
    pub scan_seen_deleted: usize,
    pub scan_candidates_deleted: usize,
    /// Online backup taken before this batch permanently deleted expired
    /// missing items. `None` when the batch deleted no missing items.
    pub missing_items_backup: Option<String>,
}

pub fn cleanup_scan_seen(conn: &Connection, scan_id: &str) -> Result<usize> {
    crate::scan::delete_scan_seen_chunked(conn, Some(scan_id), None)
}

/// Missing items expiring out of the 90-day window are permanently deleted
/// (cascading their tags and favorites). Every batch that would delete at
/// least one such row first takes a SQLite online backup; a failed backup
/// aborts the whole housekeeping batch without deleting anything.
pub fn run_housekeeping_batch(
    conn: &Connection,
    missing_item_cutoff: f64,
    scan_seen_cutoff: f64,
    scan_candidate_cutoff: f64,
    batch_size: i64,
) -> Result<HousekeepingResult> {
    run_housekeeping_batch_inner(
        conn,
        missing_item_cutoff,
        scan_seen_cutoff,
        scan_candidate_cutoff,
        batch_size,
        &create_housekeeping_db_backup,
        missing_sweep_due(),
    )
}

/// The expired-missing sweep takes a whole online database backup before it
/// deletes anything, and the hash loop calls this every `HASH_INTERVAL` (30s by
/// default). A backlog of N expired rows therefore produced ceil(N/10 000)
/// full copies of a multi-gigabyte database within minutes, keeping
/// `DB_BACKUP_RETENTION` of them. Throttled to one sweep per hour: the backlog
/// still drains (24 × batch per day) and the backup count stays bounded.
const MISSING_SWEEP_MIN_INTERVAL: Duration = Duration::from_secs(3600);

static LAST_MISSING_SWEEP: Mutex<Option<Instant>> = Mutex::new(None);

fn missing_sweep_due() -> bool {
    let mut last = LAST_MISSING_SWEEP
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    match *last {
        Some(at) if at.elapsed() < MISSING_SWEEP_MIN_INTERVAL => false,
        _ => {
            *last = Some(Instant::now());
            true
        }
    }
}


fn run_housekeeping_batch_inner(
    conn: &Connection,
    missing_item_cutoff: f64,
    scan_seen_cutoff: f64,
    scan_candidate_cutoff: f64,
    batch_size: i64,
    backup: &dyn Fn(&Connection) -> Result<String>,
    missing_sweep: bool,
) -> Result<HousekeepingResult> {
    let batch_size = batch_size.clamp(1, 50_000);
    // One bounded probe, because this runs on every hash tick. The predicate
    // asks for expired missing items with no pending move candidate, and the
    // old form counted all of them before deciding anything: a full `items`
    // scan per tick, on a table that only grows. Nothing needs the number —
    // the count fed the backup decision and nothing else — and `LIMIT 1` stops
    // as soon as `idx_items_missing_at` finds a row, so an empty backlog costs
    // a lookup and a non-empty one costs one row. `idx_move_candidates_item_status`
    // covers the `NOT EXISTS` subquery, which the delete below repeats.
    let anything_expired = missing_sweep && {
        let mut probe = conn.prepare_cached(
            "SELECT 1 FROM items i
             WHERE i.missing=1
               AND i.missing_at IS NOT NULL
               AND i.missing_at <= ?
               AND NOT EXISTS (
                   SELECT 1 FROM move_candidates mc
                   WHERE mc.item_id=i.id AND mc.status='pending'
               )
             LIMIT 1",
        )?;
        probe.exists(params![missing_item_cutoff])?
    };
    let mut missing_items_backup = None;
    if anything_expired {
        missing_items_backup = Some(
            backup(conn)
                .context("backup before expired missing-item deletion failed; batch skipped")?,
        );
    }
    let tx = conn.unchecked_transaction()?;
    // The delete is gated on the same flag as the backup: deleting expired rows
    // without the safety snapshot this function promises would be worse than
    // waiting for the next throttled sweep.
    let missing_items_deleted = if missing_sweep {
        tx.execute(
            "DELETE FROM items
             WHERE id IN (
                 SELECT i.id
                 FROM items i
                 WHERE i.missing=1
                   AND i.missing_at IS NOT NULL
                   AND i.missing_at <= ?
                   AND NOT EXISTS (
                       SELECT 1 FROM move_candidates mc
                       WHERE mc.item_id=i.id AND mc.status='pending'
                   )
                 ORDER BY i.id
                 LIMIT ?
             )",
            params![missing_item_cutoff, batch_size],
        )?
    } else {
        0
    };
    let scan_seen_deleted = tx.execute(
        "DELETE FROM scan_seen
             WHERE id IN (
                 SELECT ss.id
                 FROM scan_seen ss
                 WHERE ss.created_at <= ?
                 ORDER BY ss.id
                 LIMIT ?
             )",
        params![scan_seen_cutoff, batch_size],
    )?;
    let scan_candidates_deleted = tx.execute(
        &format!(
            "DELETE FROM scan_candidates
             WHERE id IN (
                 SELECT id
                 FROM scan_candidates
                 WHERE status IN ({TERMINAL_SCAN_CANDIDATE_STATUSES})
                   AND COALESCE(resolved_at, created_at) <= ?
                   -- Only a *pending* candidate move is still owed an answer.
                   -- Nothing deletes move_candidates rows, so excluding every
                   -- reference stranded terminal candidates past the 30-day
                   -- retention forever — the same over-broad predicate
                   -- hash_run's queue had to narrow.
                   AND NOT EXISTS (
                       SELECT 1 FROM move_candidates mc
                       WHERE mc.scan_candidate_id=scan_candidates.id
                         AND mc.status='pending'
                   )
                 ORDER BY id
                 LIMIT ?
             )"
        ),
        params![scan_candidate_cutoff, batch_size],
    )?;
    tx.commit()?;
    if missing_sweep {
        if let Err(error) = maybe_vacuum_database(conn) {
            crate::log_error!("housekeeping vacuum failed: {error}");
        }
    }
    Ok(HousekeepingResult {
        missing_items_deleted,
        scan_seen_deleted,
        scan_candidates_deleted,
        missing_items_backup,
    })
}

const VACUUM_MIN_INTERVAL: Duration = Duration::from_secs(24 * 3600);
static LAST_VACUUM: Mutex<Option<Instant>> = Mutex::new(None);

/// Run VACUUM when the freelist has grown past 100 MB and accounts for at least 20%
/// of total database pages. Throttled to at most once per 24 hours.
pub fn maybe_vacuum_database(conn: &Connection) -> Result<bool> {
    let mut last = LAST_VACUUM
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if let Some(at) = *last {
        if at.elapsed() < VACUUM_MIN_INTERVAL {
            return Ok(false);
        }
    }

    let free_pages: i64 = conn
        .query_row("PRAGMA freelist_count", [], |r| r.get(0))
        .unwrap_or(0);
    let page_count: i64 = conn
        .query_row("PRAGMA page_count", [], |r| r.get(0))
        .unwrap_or(0);
    let page_size: i64 = conn
        .query_row("PRAGMA page_size", [], |r| r.get(0))
        .unwrap_or(4096);
    let reclaimable = free_pages.saturating_mul(page_size);

    if reclaimable >= 100 * 1024 * 1024
        && page_count > 0
        && (free_pages as f64 / page_count as f64) >= 0.20
    {
        *last = Some(Instant::now());
        drop(last);
        log_info!(
            "housekeeping: reclaimable space {} MB ({}/{} pages); running VACUUM",
            reclaimable / (1024 * 1024),
            free_pages,
            page_count
        );
        conn.execute_batch("VACUUM;")?;
        log_info!("housekeeping: VACUUM completed successfully");
        return Ok(true);
    }
    Ok(false)
}
