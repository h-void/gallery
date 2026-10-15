//! In-process hash batch (replaces residual `hash_worker.run_hash_batch`).

use anyhow::Result;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use crate::content_hash::hash_file;
use crate::db_housekeeping::run_housekeeping_batch;
use crate::link_index::{is_link_source_file, reindex_scanned_items_links};
use crate::media_roots::{authorized_media_path, MediaRoots};
use crate::product_ui::auto_resolve_move_candidates_with_roots;
use crate::scan_candidates_write::resolve_scan_candidate_response_with_roots;

/// The queue used to select its batch with
/// `ORDER BY (hash_status = 'error') ASC, id LIMIT ?`. That expression cannot be
/// served by an index, so SQLite materialised and sorted the entire waiting set
/// before applying `LIMIT`: on a 100k-row pending backlog a `LIMIT 1` cost
/// 1,200,048 VM instructions and 11ms, against 13 instructions for a plain
/// `hash_status = 'pending' ORDER BY id LIMIT 1` over the same index.
///
/// The batch is now assembled from bounded, index-ordered lookups that are
/// merged in memory. One lookup per leading-index-column value keeps every scan
/// a range scan on `idx_items_hash_queue(missing, hash_status, id)` /
/// `idx_scan_candidates_hash_queue(status, hash_status, id)`, and the merge
/// reproduces the old order exactly because each stream is already sorted by id.
///
/// `filters` are complete equality fragments covering the leading index
/// columns. They are built from string literals in this module only; nothing
/// from a request or a row ever reaches them.
fn select_queue_ids(
    conn: &Connection,
    table: &str,
    extra: &str,
    filters: &[&str],
    limit: i64,
) -> Result<Vec<i64>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let mut ids: Vec<i64> = Vec::new();
    for filter in filters {
        let sql = format!("SELECT id FROM {table} WHERE {filter} {extra} ORDER BY id LIMIT ?1");
        let mut stmt = conn.prepare_cached(&sql)?;
        ids.extend(
            stmt.query_map(params![limit], |row| row.get::<_, i64>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?,
        );
    }
    ids.sort_unstable();
    ids.dedup();
    ids.truncate(limit as usize);
    Ok(ids)
}

/// Cursor value that starts a sweep from the lowest id.
const ERROR_CURSOR_START: i64 = i64::MIN;

/// Consecutive failures a row may accumulate before its retries are spaced out.
/// The first attempts stay immediate, so a transient I/O error is not delayed;
/// only a row that keeps failing is backed off.
const ERROR_BACKOFF_AFTER_FAILURES: u32 = 3;
const ERROR_BACKOFF_BASE: Duration = Duration::from_secs(30);
const ERROR_BACKOFF_MAX: Duration = Duration::from_secs(3600);


/// Upper bound on the ledger's size.
///
/// A row keeps its failure count while fewer than `ERROR_LEDGER_CAPACITY` other rows fail
/// between attempts. The ledger enforces a strict hard cap: expired backoffs are pruned first,
/// and any remaining excess entries are evicted by earliest retry time (`next_at`), keeping
/// memory strictly bounded.
///
/// Under baseline defaults (500 items per batch, 30s interval), a single-phase sweep of
/// 65,536 entries corresponds to approximately
/// `ERROR_LEDGER_CAPACITY / ERROR_ROTATION_BATCH * ERROR_ROTATION_INTERVAL_SECS`
/// (65,536 / 500 * 30s ≈ 66 minutes), which exceeds `ERROR_BACKOFF_MAX` (60 minutes).
/// In production, `HASH_INTERVAL` is configurable and `items`/`scan_candidates` share this ledger
/// while querying batches independently, so this rotation duration is a baseline dimensioning heuristic.
///
/// Resident memory is strictly bounded by this many entries, about 4 MB at ~56 bytes each.
const ERROR_LEDGER_CAPACITY: usize = 65_536;

#[derive(Default)]
struct ErrorRetryState {
    /// `(table, id)` -> consecutive failures and the earliest next attempt.
    failures: HashMap<(&'static str, i64), (u32, Instant)>,
    /// `table` -> id the error phase stopped at. `ERROR_CURSOR_START` means the
    /// next sweep starts from the lowest id.
    cursors: HashMap<&'static str, i64>,
}

/// In-process retry ledger for the error phase of the hash queue.
///
/// The error phase used to take the lowest `limit` ids on every tick. When more
/// rows were failing than a batch could hold, those low ids occupied the phase
/// forever and a high id that had since recovered was never reached — the
/// pending-first ordering only stops errors from blocking *pending* rows, not
/// other errors. The ledger fixes that without persisting anything:
///
/// * a cursor continues the error phase after the last id it selected, and wraps
///   back to the lowest id once the sweep reaches the end, so every error row is
///   reached within `ceil(error_rows / limit)` ticks;
/// * a bounded exponential backoff spaces out the retries of rows that keep
///   failing, so a permanently broken file stops being re-read every tick.
///
/// The two mechanisms overlap deliberately. A row's backoff only survives while
/// the failing set fits inside [`ERROR_LEDGER_CAPACITY`]; past that the sweep is
/// what spaces the retries, and the capacity is sized so that the sweep over a
/// full ledger is already slower than the backoff ceiling. See the capacity's
/// own documentation for the arithmetic.
///
/// Nothing here survives a restart, and that is the intended behaviour: after a
/// restart every error row is retried once more.
struct ErrorRetryLedger {
    /// Entries the ledger may hold. A field rather than a bare constant so the
    /// boundary can be driven directly by tests, the same way
    /// `HashCommitBudget` is a value.
    capacity: usize,
    state: Mutex<ErrorRetryState>,
}

impl Default for ErrorRetryLedger {
    fn default() -> Self {
        Self::with_capacity(ERROR_LEDGER_CAPACITY)
    }
}

impl ErrorRetryLedger {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity,
            state: Mutex::new(ErrorRetryState::default()),
        }
    }

    fn cursor(&self, table: &'static str) -> i64 {
        self.lock()
            .cursors
            .get(table)
            .copied()
            .unwrap_or(ERROR_CURSOR_START)
    }

    fn advance_cursor(&self, table: &'static str, id: i64) {
        self.lock().cursors.insert(table, id);
    }

    fn is_backed_off(&self, table: &'static str, id: i64) -> bool {
        match self.lock().failures.get(&(table, id)) {
            Some((_, next_at)) => *next_at > Instant::now(),
            None => false,
        }
    }

    fn note_failure(&self, table: &'static str, id: i64) {
        let mut state = self.lock();
        let entry = state
            .failures
            .entry((table, id))
            .or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now() + backoff_for(entry.0);
        if state.failures.len() > self.capacity {
            // Entries whose backoff has already expired carry no future
            // information, so they are the ones to drop first. The row this call just
            // recorded is exempt: its entry is the one that pushed the map over
            // the bound, and dropping it would throw away a failure count that
            // was earned a moment ago — a row could then never accumulate the
            // `ERROR_BACKOFF_AFTER_FAILURES` it needs to earn a backoff at all.
            let keep = (table, id);
            let now = Instant::now();
            state
                .failures
                .retain(|key, (_, next_at)| *key == keep || *next_at > now);
            if state.failures.len() > self.capacity {
                if self.capacity == 0 {
                    state.failures.clear();
                } else {
                    let excess = state.failures.len() - self.capacity;
                    let mut evict_candidates: Vec<((&'static str, i64), Instant)> = state
                        .failures
                        .iter()
                        .filter(|(&key, _)| key != keep)
                        .map(|(&key, (_, next_at))| (key, *next_at))
                        .collect();
                    evict_candidates.sort_by_key(|&(_, next_at)| next_at);
                    for (key, _) in evict_candidates.into_iter().take(excess) {
                        state.failures.remove(&key);
                    }
                }
            }
        }
    }

    /// A row that stopped failing must not carry a stale backoff into its next
    /// real failure, so the entry is dropped rather than merely reset.
    fn note_success(&self, table: &'static str, id: i64) {
        self.lock().failures.remove(&(table, id));
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ErrorRetryState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

fn backoff_for(failures: u32) -> Duration {
    if failures < ERROR_BACKOFF_AFTER_FAILURES {
        return Duration::ZERO;
    }
    let steps = (failures - ERROR_BACKOFF_AFTER_FAILURES).min(16);
    ERROR_BACKOFF_BASE
        .saturating_mul(1u32 << steps)
        .min(ERROR_BACKOFF_MAX)
}

/// The ledger for this connection's database, or `None` for an in-memory one.
///
/// The ledger is keyed by database file because backoff and rotation are
/// per-library state. In-memory databases have no file and are only used by
/// tests; giving them one shared ledger would let one test's backoff leak into
/// another's, so they keep the plain lowest-id behaviour.
fn error_retry_ledger(conn: &Connection) -> Option<&'static ErrorRetryLedger> {
    static LEDGERS: OnceLock<Mutex<HashMap<String, &'static ErrorRetryLedger>>> = OnceLock::new();
    let file: String = conn
        .query_row("PRAGMA database_list", [], |row| row.get(2))
        .ok()?;
    if file.is_empty() {
        return None;
    }
    let ledgers = LEDGERS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut ledgers = ledgers
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    Some(
        ledgers
            .entry(file)
            .or_insert_with(|| Box::leak(Box::new(ErrorRetryLedger::default()))),
    )
}

/// Pick error rows for this batch, continuing the sweep and skipping rows that
/// are still inside their backoff window.
///
/// The sweep is what guarantees progress for a high id behind a wall of failing
/// low ids. The budget is a maximum rather than a quota: a tick whose whole
/// window is backed off simply does no error work and lets the pending phase
/// keep the batch.
fn select_error_ids(
    conn: &Connection,
    table: &'static str,
    extra: &str,
    filters: &[&str],
    limit: i64,
    ledger: Option<&'static ErrorRetryLedger>,
) -> Result<Vec<i64>> {
    if limit <= 0 {
        return Ok(Vec::new());
    }
    let cursor = ledger.map_or(ERROR_CURSOR_START, |ledger| ledger.cursor(table));
    let mut ids: Vec<i64> = Vec::new();
    // "Exhausted" means every lookup ran out of rows, which is the signal that
    // the sweep has reached the end of the error range and must restart.
    let mut exhausted = true;
    for filter in filters {
        let sql = format!(
            "SELECT id FROM {table} WHERE {filter} {extra} AND id > ?1 ORDER BY id LIMIT ?2"
        );
        let mut stmt = conn.prepare_cached(&sql)?;
        let stream: Vec<i64> = stmt
            .query_map(params![cursor, limit + 1], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        if stream.len() as i64 > limit {
            exhausted = false;
        }
        ids.extend(stream);
    }
    ids.sort_unstable();
    ids.dedup();
    let has_remaining = ids.len() > limit as usize || !exhausted;
    ids.truncate(limit as usize);

    let selected = match ledger {
        None => ids,
        Some(ledger) => {
            let selected: Vec<i64> = ids
                .iter()
                .copied()
                .filter(|id| !ledger.is_backed_off(table, *id))
                .collect();
            match ids.last() {
                Some(last) if has_remaining => ledger.advance_cursor(table, *last),
                // The sweep reached the end of the range: restart from the
                // lowest id so a low-id failure cannot own the phase forever.
                _ => ledger.advance_cursor(table, ERROR_CURSOR_START),
            }
            selected
        }
    };
    Ok(selected)
}

fn stable_file_hash(path: &Path) -> Result<Option<String>> {
    let before = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    let digest = hash_file(path, 1024 * 1024)?;
    let after = match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => metadata,
        _ => return Ok(None),
    };
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return Ok(None);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if before.dev() != after.dev() || before.ino() != after.ino() {
            return Ok(None);
        }
    }
    Ok(Some(digest))
}

/// True only when the source is genuinely absent.
///
/// The hash pipeline used to read every metadata failure as "the file was
/// deleted", so a permission error or a flapping mount mass-marked live rows
/// missing (or dropped live scan candidates). Only `NotFound` means gone;
/// anything else keeps the row retryable, matching the scan pipeline's
/// fails-closed check on unreachable media roots.
fn source_is_missing(metadata: &std::io::Result<std::fs::Metadata>) -> bool {
    matches!(metadata, Err(error) if error.kind() == std::io::ErrorKind::NotFound)
}

/// The guarded write that finalizes one item's hash.
///
/// Every snapshot column is part of the predicate on purpose: the hash was
/// computed outside any transaction, so between reading the row and committing
/// the result the file may have been replaced, moved, or the row may have been
/// re-queued by a rescan. A row that no longer matches is rejected here and
/// stays queued for the next batch instead of being finalized against a stale
/// snapshot.
const ITEM_HASH_SUCCESS_UPDATE: &str = "
    UPDATE items SET content_hash=?, hash_status='done',
     hash_updated_at=strftime('%s','now') WHERE id=? AND missing=0
     AND file_path=?
     AND ((file_size=? AND file_mtime=?) OR (file_size=0 AND file_mtime=0))
     AND (st_dev IS ? OR st_dev=?) AND (st_ino IS ? OR st_ino=?)
     AND content_hash=? AND hash_status=?";

/// Flush thresholds for buffered item hash results.
///
/// These are a value rather than constants so the batch budget can be measured
/// and tested on its own. The whole 500-file batch is deliberately *not* the
/// only boundary: the write lock is held for the duration of a commit, so a
/// batch that always ran to the caller's limit would hold it for as long as a
/// full batch takes. `max_rows: 1` reproduces the per-row autocommit behaviour
/// this replaces, which is what the section 4 measurement compares against.
#[derive(Debug, Clone, Copy)]
pub struct HashCommitBudget {
    /// Buffered rows that trigger a commit.
    pub max_rows: usize,
    /// Media bytes hashed that trigger a commit.
    pub max_bytes: i64,
    /// Age of the oldest buffered row that triggers a commit.
    pub max_wait: Duration,
    /// Flush pending rows before hashing a file at least this large.
    pub large_file_bytes: i64,
}

impl Default for HashCommitBudget {
    fn default() -> Self {
        Self {
            max_rows: 64,
            max_bytes: 1 << 30,
            max_wait: Duration::from_millis(500),
            large_file_bytes: 64 << 20,
        }
    }
}

/// One successful item hash waiting to be persisted.
struct PendingItemHash {
    id: i64,
    digest: String,
    path: String,
    size: i64,
    mtime: f64,
    st_dev: Option<i64>,
    st_ino: Option<i64>,
    previous_hash: String,
    previous_status: String,
}

/// Buffers successful item hash results and persists them in one short
/// transaction, instead of one autocommit statement per file.
///
/// The statement itself is unchanged, so the drift guard behaves exactly as it
/// did per row. Only a committed transaction contributes to the done count, and
/// a failed transaction rolls the whole buffer back: that can cost a re-hash,
/// which is allowed, but it can never lose an already-committed result or
/// report a row as done that was not written.
struct ItemHashBatch {
    rows: Vec<PendingItemHash>,
    bytes: i64,
    opened_at: Option<Instant>,
}

impl ItemHashBatch {
    fn new() -> Self {
        Self {
            rows: Vec::new(),
            bytes: 0,
            opened_at: None,
        }
    }

    fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    fn push(&mut self, row: PendingItemHash, size: i64) {
        if self.opened_at.is_none() {
            self.opened_at = Some(Instant::now());
        }
        self.bytes += size.max(0);
        self.rows.push(row);
    }

    fn budget_reached(&self, budget: &HashCommitBudget) -> bool {
        self.rows.len() >= budget.max_rows
            || self.bytes >= budget.max_bytes
            || self
                .opened_at
                .map(|opened| opened.elapsed() >= budget.max_wait)
                .unwrap_or(false)
    }

    /// Persist every buffered row in one short transaction and return how many
    /// rows the guarded update actually matched.
    ///
    /// The transaction is `DEFERRED` on purpose. Its first statement is the
    /// update, so the write lock is taken at the same moment the per-row
    /// autocommit version took it, and it is simply held until the commit
    /// instead of being released after every file. That keeps the concurrency
    /// contract identical: a rescan can still refresh a row at any point before
    /// its update runs, and the guard then rejects it. `IMMEDIATE` would take
    /// the lock one step earlier, at `BEGIN`, which closes part of that window
    /// and made a concurrent writer fail outright rather than being rejected by
    /// the guard. A deferred transaction whose first access is a write cannot
    /// hit the WAL upgrade failure, because it never reads before writing.
    ///
    /// The buffer is taken before the transaction starts, so a failure leaves
    /// nothing pending and nothing counted.
    fn flush(&mut self, conn: &Connection) -> Result<usize> {
        if self.rows.is_empty() {
            return Ok(0);
        }
        let rows = std::mem::take(&mut self.rows);
        self.bytes = 0;
        self.opened_at = None;

        let tx = Transaction::new_unchecked(conn, TransactionBehavior::Deferred)
            .map_err(|error| anyhow::anyhow!("begin hash result commit: {error}"))?;
        let mut applied = 0usize;
        for row in &rows {
            let changed = tx.execute(
                ITEM_HASH_SUCCESS_UPDATE,
                params![
                    row.digest,
                    row.id,
                    row.path,
                    row.size,
                    row.mtime,
                    row.st_dev,
                    row.st_dev,
                    row.st_ino,
                    row.st_ino,
                    row.previous_hash,
                    row.previous_status
                ],
            )?;
            if changed == 1 {
                applied += 1;
            }
        }
        tx.commit()?;
        Ok(applied)
    }
}

pub fn run_hash_batch_with_roots(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
) -> Result<Value> {
    run_hash_batch_with_budget(conn, roots, limit, HashCommitBudget::default())
}

/// Runs one hash tick with explicit flush thresholds.
///
/// Production callers use `run_hash_batch_with_roots`, which applies
/// `HashCommitBudget::default()`. This entry point exists so the commit
/// granularity can be driven directly by tests and by the section 4
/// measurement; nothing else should need it.
pub fn run_hash_batch_with_budget(
    conn: &Connection,
    roots: &MediaRoots,
    limit: i64,
    budget: HashCommitBudget,
) -> Result<Value> {
    let limit = limit.clamp(1, crate::MAX_PAGINATION_LIMIT);
    let mut items_done = 0i64;
    let mut cand_done = 0i64;
    let mut resolved = 0i64;
    let mut link_items: BTreeMap<i64, BTreeSet<i64>> = BTreeMap::new();

    // Hash pending scan candidates first. Errored rows retry only after the
    // pending backlog, so permanently failing low-id rows cannot starve the
    // rest of the queue.
    let ledger = error_retry_ledger(conn);
    const CANDIDATE_MOVE_GUARD: &str = "
        AND NOT EXISTS (
            SELECT 1 FROM move_candidates mc
            WHERE mc.scan_candidate_id = scan_candidates.id
              AND mc.status = 'pending'
        )";
    let mut cand_ids = select_queue_ids(
        conn,
        "scan_candidates",
        CANDIDATE_MOVE_GUARD,
        &[
            "status='pending' AND hash_status='pending'",
            "status='pending' AND hash_status=''",
            "status='candidate' AND hash_status='pending'",
            "status='candidate' AND hash_status=''",
        ],
        limit,
    )?;
    if (cand_ids.len() as i64) < limit {
        cand_ids.extend(select_error_ids(
            conn,
            "scan_candidates",
            CANDIDATE_MOVE_GUARD,
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            limit - cand_ids.len() as i64,
            ledger,
        )?);
    }

    for id in cand_ids {
        let candidate_state: (String, i64, f64, Option<i64>, Option<i64>, String, String) = conn
            .query_row(
                "SELECT file_path, file_size, file_mtime, st_dev, st_ino, content_hash, hash_status
             FROM scan_candidates WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
        let Ok(path) = authorized_media_path(roots, &candidate_state.0) else {
            // Only write when the status actually changes. A candidate whose path
            // stays unauthorized fails on every single tick; without the guard
            // each retry rewrote the same value, appended a WAL frame and bumped
            // the row's page for no state change at all.
            conn.execute(
                "UPDATE scan_candidates SET hash_status='error'
                 WHERE id=? AND status IN ('pending','candidate')
                   AND COALESCE(hash_status,'') <> 'error'",
                params![id],
            )?;
            if let Some(ledger) = ledger {
                ledger.note_failure("scan_candidates", id);
            }
            continue;
        };
        // Only a genuinely absent source means "gone". A permission or
        // transient I/O error must keep the row retryable: the scan pipeline
        // fails closed on an unreachable root, and the hash pipeline must not
        // mass-mark rows missing while a mount is flapping.
        let before_meta = std::fs::metadata(&path);
        let source_missing = source_is_missing(&before_meta);
        let before = before_meta.ok();
        match stable_file_hash(&path) {
            Ok(Some(digest)) => {
                let after = std::fs::metadata(&path).ok();
                let identity_matches = match (before.as_ref(), after.as_ref()) {
                    (Some(before), Some(after)) => {
                        file_metadata_matches(before, after)
                            && file_matches_snapshot(
                                before,
                                candidate_state.1,
                                candidate_state.2,
                                candidate_state.3,
                                candidate_state.4,
                            )
                    }
                    _ => false,
                };
                if !identity_matches {
                    // The stored snapshot no longer matches the live file: the file
                    // changed after the scan that recorded the candidate. Refresh the
                    // snapshot from the live metadata so the next batch can re-hash it,
                    // instead of leaving the row as a stuck `pending` head that blocks
                    // every later file until a full rescan happens. A genuinely absent
                    // source is handled by the `Ok(None) | Err(_)` branch below; here
                    // the file is present but its identity drifted.
                    let Some(live) = after.as_ref().or(before.as_ref()) else {
                        continue;
                    };
                    let (live_size, live_mtime, live_dev, live_ino) = live_snapshot(live);
                    conn.execute(
                        "UPDATE scan_candidates
                         SET file_size=?, file_mtime=?, st_dev=?, st_ino=?
                         WHERE id=? AND status IN ('pending','candidate') AND file_path=?
                           AND content_hash=? AND hash_status IN ('pending','error','')",
                        params![
                            live_size,
                            live_mtime,
                            live_dev,
                            live_ino,
                            id,
                            candidate_state.0,
                            candidate_state.5
                        ],
                    )?;
                    if let Some(ledger) = ledger {
                        ledger.note_success("scan_candidates", id);
                    }
                    continue;
                }
                let changed = conn.execute(
                    "UPDATE scan_candidates SET content_hash=?, hash_status='done'
                     WHERE id=? AND status IN ('pending','candidate')
                       AND file_path=?
                       AND ((file_size=? AND file_mtime=?) OR (file_size=0 AND file_mtime=0))
                       AND (st_dev IS ? OR st_dev=?) AND (st_ino IS ? OR st_ino=?)
                       AND content_hash=? AND hash_status=?",
                    params![
                        digest,
                        id,
                        candidate_state.0,
                        candidate_state.1,
                        candidate_state.2,
                        candidate_state.3,
                        candidate_state.3,
                        candidate_state.4,
                        candidate_state.4,
                        candidate_state.5,
                        candidate_state.6
                    ],
                )?;
                if changed != 1 {
                    if let Some(ledger) = ledger {
                        ledger.note_success("scan_candidates", id);
                    }
                    continue;
                }
                cand_done += 1;
                if let Some(ledger) = ledger {
                    ledger.note_success("scan_candidates", id);
                }
                match resolve_scan_candidate_response_with_roots(conn, roots, id) {
                    Ok(v) => {
                        if record_resolution(conn, &v, &mut link_items)? {
                            resolved += 1;
                        }
                    }
                    Err(error) => {
                        log_error!("hash: resolve scan candidate {id} failed: {error:#}");
                    }
                }
            }
            Ok(None) | Err(_) => {
                if source_missing {
                    // Source file is gone: this is a stale scan-candidate
                    // reference, not a transient hash failure. Drop it so it
                    // stops being retried forever as 'error'.
                    // The guard must match the queue's: only a *pending*
                    // move candidate still needs the row. Excluding every
                    // move-candidate reference stranded rows forever, because
                    // nothing deletes move_candidates rows, so a resolved
                    // reference never clears and the row kept the head slot of
                    // every batch while inverting `remaining` on maintenance.
                    conn.execute(
                        "DELETE FROM scan_candidates
                         WHERE id=?
                           AND status IN ('pending','candidate')
                           AND NOT EXISTS (
                               SELECT 1 FROM move_candidates mc
                               WHERE mc.scan_candidate_id = scan_candidates.id
                                 AND mc.status = 'pending'
                           )",
                        params![id],
                    )?;
                } else {
                    conn.execute(
                        "UPDATE scan_candidates SET hash_status='error'
                         WHERE id=? AND COALESCE(hash_status,'') <> 'error'",
                        params![id],
                    )?;
                    if let Some(ledger) = ledger {
                        ledger.note_failure("scan_candidates", id);
                    }
                }
            }
        }
    }

    // Upgrade backlog: candidates that were hashed before the native resolver
    // existed still need the same safety pass. Keep the total candidate work
    // bounded by the caller's batch limit.
    // The candidate queue only selects rows still needing a hash, while the
    // history phase below only selects already-hashed rows needing resolution.
    // They touch disjoint rows, so resolution must keep a *guaranteed* share of
    // the tick instead of being squeezed to zero whenever the candidate backlog
    // fills the budget (which used to starve every done candidate behind a full
    // batch). The two phases still share the caller's batch bound.
    let history_limit = (limit / 2).max(1);
    if history_limit > 0 {
        let ready_ids: Vec<i64> = conn
            .prepare(
                "
                SELECT sc.id FROM scan_candidates sc
                WHERE sc.status IN ('pending','candidate')
                  AND sc.hash_status = 'done'
                  AND NOT EXISTS (
                      SELECT 1 FROM move_candidates mc
                      WHERE mc.scan_candidate_id = sc.id AND mc.status = 'pending'
                  )
                ORDER BY sc.id LIMIT ?
                ",
            )?
            .query_map(params![history_limit], |row| row.get(0))?
            .collect::<rusqlite::Result<_>>()?;
        for id in ready_ids {
            match resolve_scan_candidate_response_with_roots(conn, roots, id) {
                Ok(v) => {
                    if record_resolution(conn, &v, &mut link_items)? {
                        resolved += 1;
                    }
                }
                Err(error) => {
                    log_error!("hash: resolve historical scan candidate {id} failed: {error:#}");
                }
            }
        }
    }

    // Successful item hash results are buffered and committed in short
    // transactions; see `ItemHashBatch`. Scan candidates keep their per-row
    // writes because their resolution step is not part of this change.
    let mut batch = ItemHashBatch::new();

    let mut item_ids = select_queue_ids(
        conn,
        "items",
        "AND missing=0",
        &["hash_status='pending'", "hash_status=''"],
        limit,
    )?;
    if (item_ids.len() as i64) < limit {
        item_ids.extend(select_error_ids(
            conn,
            "items",
            "AND missing=0",
            &["hash_status='error'"],
            limit - item_ids.len() as i64,
            ledger,
        )?);
    }

    for id in item_ids {
        let item_state: (String, i64, f64, Option<i64>, Option<i64>, String, String) = conn
            .query_row(
                "SELECT file_path, file_size, file_mtime, st_dev, st_ino, content_hash, hash_status
             FROM items WHERE id=?",
                params![id],
                |r| {
                    Ok((
                        r.get(0)?,
                        r.get(1)?,
                        r.get(2)?,
                        r.get(3)?,
                        r.get(4)?,
                        r.get(5)?,
                        r.get(6)?,
                    ))
                },
            )?;
        let Ok(path) = authorized_media_path(roots, &item_state.0) else {
            conn.execute(
                "UPDATE items SET hash_status='error'
                 WHERE id=? AND missing=0 AND COALESCE(hash_status,'') <> 'error'",
                params![id],
            )?;
            if let Some(ledger) = ledger {
                ledger.note_failure("items", id);
            }
            continue;
        };
        let before_meta = std::fs::metadata(&path);
        let source_missing = source_is_missing(&before_meta);
        let before = before_meta.ok();
        // The wait threshold is only consulted after a hash completes, so a
        // single slow file would otherwise stretch a batch far past it. Commit
        // what is already buffered before starting a large read.
        if !batch.is_empty()
            && before.as_ref().map(|meta| meta.len() as i64).unwrap_or(0) >= budget.large_file_bytes
        {
            items_done += batch.flush(conn)? as i64;
        }
        match stable_file_hash(&path) {
            Ok(Some(digest)) => {
                let after = std::fs::metadata(&path).ok();
                let identity_matches = match (before.as_ref(), after.as_ref()) {
                    (Some(before), Some(after)) => {
                        file_metadata_matches(before, after)
                            && file_matches_snapshot(
                                before,
                                item_state.1,
                                item_state.2,
                                item_state.3,
                                item_state.4,
                            )
                    }
                    _ => false,
                };
                if !identity_matches {
                    // Same drift handling as the scan-candidate loop: refresh the
                    // stale snapshot in place so the row is not a perpetual `pending`
                    // head blocking later files. A missing source is handled below.
                    let Some(live) = after.as_ref().or(before.as_ref()) else {
                        continue;
                    };
                    let (live_size, live_mtime, live_dev, live_ino) = live_snapshot(live);
                    conn.execute(
                        "UPDATE items
                         SET file_size=?, file_mtime=?, st_dev=?, st_ino=?
                         WHERE id=? AND missing=0 AND file_path=?
                           AND content_hash=? AND hash_status IN ('pending','error','')",
                        params![
                            live_size,
                            live_mtime,
                            live_dev,
                            live_ino,
                            id,
                            item_state.0,
                            item_state.5
                        ],
                    )?;
                    if let Some(ledger) = ledger {
                        ledger.note_success("items", id);
                    }
                    continue;
                }
                batch.push(
                    PendingItemHash {
                        id,
                        digest,
                        path: item_state.0,
                        size: item_state.1,
                        mtime: item_state.2,
                        st_dev: item_state.3,
                        st_ino: item_state.4,
                        previous_hash: item_state.5,
                        previous_status: item_state.6,
                    },
                    item_state.1,
                );
                if batch.budget_reached(&budget) {
                    items_done += batch.flush(conn)? as i64;
                }
                if let Some(ledger) = ledger {
                    ledger.note_success("items", id);
                }
            }
            Ok(None) | Err(_) => {
                if source_missing {
                    // File is gone: mark the item missing instead of looping on
                    // 'error' forever. The scan/missing pipeline reconciles it.
                    conn.execute(
                        "UPDATE items SET missing=1, missing_at=strftime('%s','now')
                         WHERE id=? AND missing=0",
                        params![id],
                    )?;
                    if let Some(ledger) = ledger {
                        ledger.note_success("items", id);
                    }
                } else {
                    conn.execute(
                        "UPDATE items SET hash_status='error'
                         WHERE id=? AND COALESCE(hash_status,'') <> 'error'",
                        params![id],
                    )?;
                    if let Some(ledger) = ledger {
                        ledger.note_failure("items", id);
                    }
                }
            }
        }
    }

    // Commit the tail of the buffer before anything downstream reads item hash
    // state: move-candidate resolution and link reindexing both do.
    items_done += batch.flush(conn)? as i64;

    let move_candidates = auto_resolve_move_candidates_with_roots(conn, roots, limit)?;
    let moves_applied = move_candidates["applied"].as_i64().unwrap_or(0);
    let links = if link_items.is_empty() {
        json!({"ok": true, "artists": 0, "skipped": "no_resolved_text_items"})
    } else {
        reindex_scanned_items_links(conn, roots, &link_items)
            .unwrap_or_else(|error| json!({"ok": false, "error": error.to_string()}))
    };

    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    let housekeeping = run_housekeeping_batch(
        conn,
        now - 90.0 * 24.0 * 60.0 * 60.0,
        now - 7.0 * 24.0 * 60.0 * 60.0,
        now - 30.0 * 24.0 * 60.0 * 60.0,
        10_000,
    )?;
    // The full-library `hash_status_response` aggregate is deliberately *not*
    // included here. It used to run on every tick including idle ones, where it
    // cost 97-98% of the tick on a 755k-item library, and nothing consumed the
    // counts: the frontend reads them from `GET /api/hash/status`, nothing
    // requests `/api/workers`, and `/api/health` only reads `last.error` while
    // cloning the whole payload into every response. Callers that need real
    // library totals should ask `/api/hash/status` on demand.
    let progress = items_done + cand_done + resolved + moves_applied;
    Ok(json!({
        "ok": true,
        "message": if progress > 0 { "hash_batch_progress" } else { "hash_batch_idle" },
        "items": {"done": items_done},
        "scan_candidates": {"done": cand_done},
        "resolved": resolved,
        "move_candidates": move_candidates,
        "links": links,
        "housekeeping": {
            "missing_items_expired_deleted": housekeeping.missing_items_deleted,
            "missing_items_backup": housekeeping.missing_items_backup,
            "scan_seen_expired_deleted": housekeeping.scan_seen_deleted,
            "scan_candidates_terminal_deleted": housekeeping.scan_candidates_deleted,
        },
    }))
}

fn file_metadata_matches(before: &std::fs::Metadata, after: &std::fs::Metadata) -> bool {
    if before.len() != after.len() || before.modified().ok() != after.modified().ok() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        return before.dev() == after.dev() && before.ino() == after.ino();
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Extract a fresh identity (size, mtime, dev, ino) from live metadata so a row
/// whose stored snapshot has drifted can be refreshed in place instead of being
/// left as a stuck `pending` head that blocks every later file.
fn live_snapshot(meta: &std::fs::Metadata) -> (i64, f64, Option<i64>, Option<i64>) {
    let mtime = meta
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map(|value| value.as_secs_f64())
        .unwrap_or(0.0);
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        (
            meta.len() as i64,
            mtime,
            Some(meta.dev() as i64),
            Some(meta.ino() as i64),
        )
    }
    #[cfg(not(unix))]
    {
        (meta.len() as i64, mtime, None, None)
    }
}

/// A hash belongs to the row snapshot only when the file still has the same
/// observed metadata. Legacy rows with neither size nor mtime remain eligible
/// for their first hash; every populated identity field is authoritative.
///
/// The mtime comparison uses only a floating-point/round-trip epsilon
/// (`MTIME_REUSE_EPSILON`), never a 1-second or 1-millisecond grace window: a real sub-millisecond
/// modification (e.g. an in-place equal-length rewrite bumping mtime by 0.5ms)
/// must invalidate the cached hash rather than be reused.
use crate::scan::MTIME_REUSE_EPSILON;

fn file_matches_snapshot(
    metadata: &std::fs::Metadata,
    file_size: i64,
    file_mtime: f64,
    _st_dev: Option<i64>,
    _st_ino: Option<i64>,
) -> bool {
    if (file_size != 0 || file_mtime != 0.0)
        && (metadata.len() as i64 != file_size
            || metadata
                .modified()
                .ok()
                .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
                .is_none_or(|value| {
                    (value.as_secs_f64() - file_mtime).abs() >= MTIME_REUSE_EPSILON
                }))
    {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if _st_dev.is_some_and(|value| value != metadata.dev() as i64)
            || _st_ino.is_some_and(|value| value != metadata.ino() as i64)
        {
            return false;
        }
    }
    true
}

fn record_resolution(
    conn: &Connection,
    response: &Value,
    link_items: &mut BTreeMap<i64, BTreeSet<i64>>,
) -> Result<bool> {
    if matches!(
        response.get("action").and_then(|action| action.as_str()),
        Some("missing") | Some("waiting_hash") | Some("no_match")
    ) {
        return Ok(false);
    }
    let Some(item_id) = response.get("item_id").and_then(Value::as_i64) else {
        return Ok(true);
    };
    let item = conn
        .query_row(
            "SELECT artist_id, file_name FROM items WHERE id=? AND missing=0",
            params![item_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((artist_id, file_name)) = item {
        if is_link_source_file(&file_name) {
            link_items.entry(artist_id).or_default().insert(item_id);
        }
    }
    Ok(true)
}
