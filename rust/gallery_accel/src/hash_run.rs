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
/// Reference single-phase batch ceiling under default configuration (`HASH_BATCH_SIZE` clamped to 500 in `workers.rs`).
/// Used as a baseline sizing heuristic in invariant tests. In production, `items` and `scan_candidates` share the same
/// ledger while each drawing up to `limit` rows per tick.
#[cfg(test)]
const ERROR_ROTATION_BATCH: u64 = 500;

/// `HASH_INTERVAL`'s documented default, in seconds. Note that `HASH_INTERVAL` is user-configurable (can be set to 0
/// or a custom duration), so the rotation period below serves as a design dimensioning heuristic under default parameters
/// rather than an invariant that holds universally across all configurations.
#[cfg(test)]
const ERROR_ROTATION_INTERVAL_SECS: u64 = 30;

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

/// Test helper: runs hash batch with empty media roots.
/// Production routes and workers use `run_hash_batch_with_roots`.
#[cfg(test)]
pub fn run_hash_batch(conn: &Connection, limit: i64) -> Result<Value> {
    let roots = MediaRoots {
        roots: Vec::new(),
        labels: Vec::new(),
        real_paths: Vec::new(),
    };
    run_hash_batch_with_roots(conn, &roots, limit)
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
    let limit = limit.clamp(1, 500);
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

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tempfile::tempdir;

    fn file_state(path: &Path) -> (i64, f64) {
        let metadata = std::fs::metadata(path).unwrap();
        let mtime = metadata
            .modified()
            .unwrap()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        (metadata.len() as i64, mtime)
    }

    fn race_schema() -> &'static str {
        "
        CREATE TABLE items (
          id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
          file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
          date TEXT DEFAULT '', auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
          missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
          content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
          media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER,
          width INTEGER DEFAULT 0, height INTEGER DEFAULT 0
        );
        CREATE TABLE scan_candidates (
          id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
          file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
          file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
          is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
          content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
          created_at REAL DEFAULT 0, resolved_at REAL
        );
        CREATE TABLE scan_seen (
          id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
          created_at REAL DEFAULT 0
        );
        CREATE TABLE move_candidates (
          id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
          artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
          reason TEXT DEFAULT '', status TEXT, resolved_at REAL
        );
        CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
        CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT NOT NULL, path TEXT NOT NULL);
        INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures/artist');
        "
    }

    /// R3 regression: a concurrent rescan that refreshes the row between hashing
    /// and the final update must prevent a stale `done` digest, and the refreshed
    /// row must keep its newer metadata for a future hash batch.
    #[test]
    fn concurrent_rescan_prevents_stale_done_on_items() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("item.bin");
        std::fs::write(&file, b"item-content").unwrap();
        let db_path = dir.path().join("race.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, hash_status)
             VALUES (1, 1, ?, 'item.bin', ?, ?, 'pending')",
            params![path, size, mtime],
        )
        .unwrap();

        // A second connection plays the concurrent rescan: it refreshes the
        // row exactly when the hash worker finalizes it.
        let rescan = Connection::open(&db_path).unwrap();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "items",
                    ..
                }
            ) {
                rescan
                    .execute(
                        "UPDATE items SET file_size=999, file_mtime=424242.0, hash_status='pending',
                         content_hash='' WHERE id=1",
                        [],
                    )
                    .unwrap();
            }
            Authorization::Allow
        }));

        let out = run_hash_batch(&conn, 10).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(out["ok"], true);
        let (hash_status, content_hash, file_size, file_mtime): (String, String, i64, f64) = conn
            .query_row(
                "SELECT hash_status, content_hash, file_size, file_mtime FROM items WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash_status, "pending");
        assert_eq!(content_hash, "");
        assert_eq!(file_size, 999);
        assert_eq!(file_mtime, 424242.0);
    }

    /// §7.2 regression: a tick that finds nothing to do must not re-scan the
    /// whole library to compute counts nobody reads. The trace hook records the
    /// SQL of every statement that actually *executes*, so this asserts the
    /// absence of the unbounded `GROUP BY` over `items` rather than its cost,
    /// which is invisible on a test-sized database.
    #[test]
    fn idle_batch_skips_the_full_library_status_aggregate() {
        use rusqlite::trace::{TraceEvent, TraceEventCodes};
        use std::sync::Mutex;

        // `trace_v2` takes a plain `fn`, so the collected SQL has to live in a
        // static. No other test installs a tracer, so there is no cross-talk.
        static TRACED: Mutex<Vec<String>> = Mutex::new(Vec::new());
        fn tracer(event: TraceEvent<'_>) {
            if let TraceEvent::Stmt(_, sql) = event {
                TRACED.lock().unwrap().push(sql.to_string());
            }
        }

        // The removed aggregate is the only statement that groups the whole
        // `items` table: `SELECT hash_status, COUNT(*) FROM items WHERE
        // missing=0 GROUP BY hash_status`. Every other `items` read in a tick
        // is a per-row lookup or a queue select bounded by `LIMIT`.
        fn groups_whole_items_table(sql: &str) -> bool {
            let sql = sql.to_ascii_lowercase();
            sql.contains("from items")
                && sql.contains("group by")
                && sql.contains("count(")
                && !sql.contains("limit")
        }

        let dir = tempdir().unwrap();
        let db_path = dir.path().join("idle.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(race_schema()).unwrap();
        // A fully hashed, fully resolved library: the tick has nothing to do,
        // which is exactly the round that used to pay for a full-library scan.
        conn.execute_batch(
            "
            WITH RECURSIVE seq(value) AS (
                SELECT 1 UNION ALL SELECT value + 1 FROM seq WHERE value < 300
            )
            INSERT INTO items (id, artist_id, file_path, file_name, missing, hash_status)
            SELECT 1000 + value, 1, '/none/' || value, 'f' || value, 0, 'done' FROM seq;
            ",
        )
        .unwrap();

        // Warm-up: the first tick only prepares the cached queue statements,
        // which is not what this test is about.
        run_hash_batch(&conn, 10).unwrap();

        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(tracer));
        let out = run_hash_batch(&conn, 10).unwrap();
        let traced = {
            conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
            std::mem::take(&mut *TRACED.lock().unwrap())
        };

        assert_eq!(out["ok"], true);
        assert_eq!(out["message"], "hash_batch_idle");
        assert!(
            out.get("status").is_none(),
            "the per-tick status aggregate must stay out of the batch payload: {out}"
        );
        let offender = traced.iter().find(|sql| groups_whole_items_table(sql));
        assert!(
            offender.is_none(),
            "an idle tick must not group the whole items table, but ran: {}\n\
             every statement the tick ran:\n{}",
            offender.map(String::as_str).unwrap_or_default(),
            traced.join("\n")
        );

        // Guard against the assertion above passing only because the predicate
        // matches nothing: the on-demand endpoint still runs the aggregate, and
        // the same predicate must recognise it there.
        conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, Some(tracer));
        crate::hash_status::hash_status_response(&conn).unwrap();
        let traced = {
            conn.trace_v2(TraceEventCodes::SQLITE_TRACE_STMT, None);
            std::mem::take(&mut *TRACED.lock().unwrap())
        };
        assert!(
            traced.iter().any(|sql| groups_whole_items_table(sql)),
            "the predicate no longer recognises the aggregate, so the idle-tick \
             assertion above proves nothing. Statements seen:\n{}",
            traced.join("\n")
        );
    }

    #[test]
    fn concurrent_rescan_prevents_stale_done_on_scan_candidate() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("cand.bin");
        std::fs::write(&file, b"candidate-content").unwrap();
        let db_path = dir.path().join("race.db");
        let conn = Connection::open(&db_path).unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (1, 'pending', 'pending', ?, 'cand.bin', ?, ?)",
            params![path, size, mtime],
        )
        .unwrap();

        let rescan = Connection::open(&db_path).unwrap();
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "scan_candidates",
                    ..
                }
            ) {
                rescan
                    .execute(
                        "UPDATE scan_candidates SET file_size=777, file_mtime=131313.0,
                         hash_status='pending', content_hash='' WHERE id=1",
                        [],
                    )
                    .unwrap();
            }
            Authorization::Allow
        }));

        let out = run_hash_batch(&conn, 10).unwrap();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert_eq!(out["ok"], true);
        let (hash_status, content_hash, file_size, status): (String, String, i64, String) = conn
            .query_row(
                "SELECT hash_status, content_hash, file_size, status FROM scan_candidates WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .unwrap();
        assert_eq!(hash_status, "pending");
        assert_eq!(content_hash, "");
        assert_eq!(file_size, 777);
        assert_eq!(status, "pending");
    }

    #[test]
    fn replaced_file_does_not_finalize_a_stale_snapshot() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("changed.bin");
        std::fs::write(&file, b"old").unwrap();
        let (size, mtime) = file_state(&file);
        let path = file.to_string_lossy().to_string();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, hash_status)
             VALUES (1, 1, ?, 'changed.bin', ?, ?, 'pending')",
            params![&path, size, mtime],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (1, 'pending', 'pending', ?, 'changed.bin', ?, ?)",
            params![&path, size, mtime],
        )
        .unwrap();

        std::fs::write(&file, b"replacement with a different size").unwrap();
        run_hash_batch(&conn, 10).unwrap();

        for table in ["items", "scan_candidates"] {
            let state: (String, String) = conn
                .query_row(
                    &format!("SELECT hash_status, content_hash FROM {table} WHERE id=1"),
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, ("pending".into(), "".into()));
        }
    }

    /// R4 regression: with configured media roots, an item or candidate path
    /// outside the roots is never read and is conservatively marked `error`.
    #[test]
    fn unauthorized_paths_are_not_read_and_marked_error() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let outside_dir = dir.path().join("outside");
        std::fs::create_dir_all(&outside_dir).unwrap();
        let secret = outside_dir.join("secret.bin");
        std::fs::write(&secret, b"secret-bytes").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let outside_s = secret.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (1, 1, ?, 'secret.bin', 'pending')",
            params![outside_s],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'secret.bin')",
            params![outside_s],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let out = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        assert_eq!(out["ok"], true);
        let item_status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(item_status, "error");
        let cand_status: String = conn
            .query_row(
                "SELECT hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(cand_status, "error");
        // The unauthorized file was never consumed or moved.
        assert_eq!(std::fs::read(&secret).unwrap(), b"secret-bytes");
    }

    /// R5 regression: a scan candidate whose source file was deleted but whose
    /// path is still authorized must be dropped (not retried forever as
    /// `error`), and a missing item file must be flagged `missing=1` instead of
    /// left in a perpetual `error` state.
    #[test]
    fn missing_file_scan_candidate_is_dropped_and_item_marked_missing() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let gone = media.join("gone.bin");
        let gone_s = gone.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (1, 1, ?, 'gone.bin', 'pending')",
            params![gone_s],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'gone.bin')",
            params![gone_s],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let out = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        assert_eq!(out["ok"], true);

        // The stale candidate row is gone, not stuck in 'error'.
        let cand_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(cand_count, 0);

        // The item is flagged missing rather than left in 'error'.
        let (missing, hash_status): (i64, String) = conn
            .query_row(
                "SELECT missing, hash_status FROM items WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(missing, 1);
        assert_eq!(hash_status, "pending");
    }

    fn single_media_root(media: &Path) -> MediaRoots {
        let root = media.to_string_lossy().replace('\\', "/");
        MediaRoots {
            roots: vec![root.clone()],
            labels: vec!["p1".into()],
            real_paths: vec![root],
        }
    }

    /// The DELETE guard must match the queue guard. A *resolved* move-candidate
    /// reference used to block the delete, and nothing deletes
    /// `move_candidates` rows, so the candidate was re-picked at the head of
    /// every batch forever and inflated the maintenance `remaining` count.
    #[test]
    fn resolved_move_reference_does_not_strand_a_missing_candidate() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let gone = media.join("gone.bin").to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'gone.bin')",
            params![gone],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO move_candidates (id, scan_candidate_id, status)
             VALUES (1, 1, 'resolved')",
            [],
        )
        .unwrap();

        let out = run_hash_batch_with_roots(&conn, &single_media_root(&media), 10).unwrap();
        assert_eq!(out["ok"], true);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            remaining, 0,
            "a finished move must not strand its scan candidate"
        );
    }

    /// A *pending* move still owns the row, so the delete guard skips it and the
    /// queue never offers it in the first place.
    #[test]
    fn pending_move_reference_keeps_the_scan_candidate_row() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        let gone = media.join("gone.bin").to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'gone.bin')",
            params![gone],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO move_candidates (id, scan_candidate_id, status)
             VALUES (1, 1, 'pending')",
            [],
        )
        .unwrap();

        let out = run_hash_batch_with_roots(&conn, &single_media_root(&media), 10).unwrap();
        assert_eq!(out["ok"], true);

        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM scan_candidates WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(remaining, 1, "a pending move still owns the row");
    }

    /// Pins the rule the pipeline relies on: a failed `metadata` call only means
    /// "gone" when the OS said NotFound. Permission errors and transient I/O
    /// failures must keep the row retryable.
    #[test]
    fn source_is_missing_only_trusts_not_found() {
        let file = tempdir().unwrap().path().join("probe.bin");
        let present = std::fs::metadata(&file);
        assert!(
            matches!(present, Err(error) if error.kind() == std::io::ErrorKind::NotFound),
            "a missing probe file must report NotFound"
        );

        let denied = Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            "probe",
        ));
        assert!(
            !source_is_missing(&denied),
            "a permission error is not a deleted file"
        );
        let flapping = Err(std::io::Error::new(std::io::ErrorKind::TimedOut, "probe"));
        assert!(
            !source_is_missing(&flapping),
            "a mount flap is not a deletion"
        );

        let stale = std::fs::File::create(tempdir().unwrap().path().join("live.bin")).unwrap();
        let live = stale.metadata();
        assert!(!source_is_missing(&live), "a readable file is not missing");
    }

    /// End-to-end companion: an existing source that cannot be hashed keeps the
    /// item retryable instead of being flagged missing.
    #[test]
    fn unhashable_source_is_retried_instead_of_marked_missing() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        // Exists, so `metadata` succeeds, but `stable_file_hash` returns
        // Ok(None): the same shape as an EACCES or a flapping mount.
        let not_a_file = media.join("replaced-by-directory");
        std::fs::create_dir_all(&not_a_file).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (1, 1, ?, 'replaced-by-directory', 'pending')",
            params![not_a_file.to_string_lossy().to_string()],
        )
        .unwrap();

        let out = run_hash_batch_with_roots(&conn, &single_media_root(&media), 10).unwrap();
        assert_eq!(out["ok"], true);

        let (missing, hash_status): (i64, String) = conn
            .query_row(
                "SELECT missing, hash_status FROM items WHERE id=1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(missing, 0, "an existing source is not missing");
        assert_eq!(hash_status, "error", "an unreadable source stays retryable");
    }

    /// A row that fails on every tick used to rewrite the same `error` value on
    /// every tick: the retry is necessary, the write is not. The retry must
    /// still happen, so the row is expected to recover as soon as the source
    /// becomes hashable.
    #[test]
    fn repeated_error_retry_does_not_rewrite_the_same_state() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        // A directory where a file is expected: `metadata` succeeds, so the row
        // is not missing, but hashing fails, so it becomes `error`.
        let unhashable = media.join("not-a-file");
        std::fs::create_dir_all(&unhashable).unwrap();
        let unhashable_path = unhashable.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (1, 1, ?, 'not-a-file', 'pending')",
            params![unhashable_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'pending', ?, 'not-a-file')",
            params![unhashable_path],
        )
        .unwrap();
        let roots = single_media_root(&media);

        run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        let item_status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        let cand_status: String = conn
            .query_row(
                "SELECT hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(item_status, "error");
        assert_eq!(cand_status, "error");

        // Second tick, same failure. Nothing about the rows changed, so the
        // batch must not write anything for them.
        let before = conn.total_changes();
        let out = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        assert_eq!(out["message"], "hash_batch_idle");
        assert_eq!(
            conn.total_changes(),
            before,
            "re-failing a row that is already 'error' must not rewrite the same state"
        );

        // The retry itself must still be happening: replace the directory with a
        // real file and the very next tick has to hash it.
        std::fs::remove_dir_all(&unhashable).unwrap();
        std::fs::write(&unhashable, b"now-hashable").unwrap();
        run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        let recovered: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            recovered, "done",
            "an errored row must still be retried once its source is readable"
        );
    }

    /// §7.4 acceptance: with more failing rows than a batch can hold, a
    /// recovered high id must still be reached. The old queue took the lowest
    /// `limit` error ids on every tick, so every id above that window was never
    /// retried at all — the pending-first ordering only protects pending rows.
    #[test]
    fn recovered_high_id_error_is_retried_behind_a_wall_of_low_id_failures() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        // File-backed, because the retry ledger is keyed by database file;
        // in-memory databases deliberately share no ledger.
        let conn = Connection::open(dir.path().join("queue.db")).unwrap();
        conn.execute_batch(race_schema()).unwrap();

        // Twenty permanently broken rows...
        for id in 1..=20i64 {
            let broken = media.join(format!("broken-{id}"));
            std::fs::create_dir_all(&broken).unwrap();
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
                 VALUES (?1, 1, ?2, ?3, 'error')",
                params![
                    id,
                    broken.to_string_lossy().to_string(),
                    format!("broken-{id}")
                ],
            )
            .unwrap();
        }
        // ...and one good row with a higher id that must not be starved.
        let good = media.join("good.bin");
        std::fs::write(&good, b"good-content").unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status)
             VALUES (21, 1, ?, 'good.bin', 'error')",
            params![good.to_string_lossy().to_string()],
        )
        .unwrap();

        let roots = single_media_root(&media);
        let mut ticks = 0;
        let mut status = String::new();
        for _ in 0..40 {
            run_hash_batch_with_roots(&conn, &roots, 5).unwrap();
            ticks += 1;
            status = conn
                .query_row("SELECT hash_status FROM items WHERE id=21", [], |row| {
                    row.get(0)
                })
                .unwrap();
            if status == "done" {
                break;
            }
        }
        assert_eq!(
            status, "done",
            "a recovered high-id error row must be retried within a bounded number of ticks (took {ticks})"
        );
    }

    #[test]
    fn error_backoff_grows_then_saturates_at_the_ceiling() {
        // Below the threshold a failing row is retried on the very next tick.
        assert_eq!(backoff_for(0), Duration::ZERO);
        assert_eq!(
            backoff_for(ERROR_BACKOFF_AFTER_FAILURES - 1),
            Duration::ZERO
        );
        // At the threshold the base applies, then it doubles per further failure.
        assert_eq!(
            backoff_for(ERROR_BACKOFF_AFTER_FAILURES),
            ERROR_BACKOFF_BASE
        );
        assert_eq!(
            backoff_for(ERROR_BACKOFF_AFTER_FAILURES + 1),
            ERROR_BACKOFF_BASE * 2
        );
        // It saturates at the ceiling rather than shifting past it.
        assert_eq!(
            backoff_for(ERROR_BACKOFF_AFTER_FAILURES + 16),
            ERROR_BACKOFF_MAX
        );
        assert_eq!(backoff_for(u32::MAX), ERROR_BACKOFF_MAX);
    }

    /// A failing set that fits inside the ledger must end up fully backed off.
    ///
    /// This is the property the capacity exists for, and the one the 4096-entry
    /// ledger did not have. The error phase attempts a bounded slice per tick and
    /// the cursor rotates, so between two attempts of one row every other failing
    /// row is attempted once; a count therefore survives only while the failing
    /// set is smaller than the capacity. Past that the entry is evicted before
    /// the row comes round again, the count restarts at 1, and the row is
    /// re-read on every sweep forever.
    #[test]
    fn a_failing_set_below_capacity_is_fully_backed_off() {
        let ledger = ErrorRetryLedger::default();
        // One offline mount's worth of rows, attempted `ERROR_ROTATION_BATCH` at
        // a time the way the hash loop does.
        let rows = 5_000i64;
        let batch = ERROR_ROTATION_BATCH as i64;
        for _round in 0..ERROR_BACKOFF_AFTER_FAILURES {
            let mut id = 0;
            while id < rows {
                for offset in 0..batch.min(rows - id) {
                    ledger.note_failure("items", id + offset);
                }
                id += batch;
            }
        }

        let missing: Vec<i64> = (0..rows)
            .filter(|id| !ledger.is_backed_off("items", *id))
            .collect();
        assert!(
            missing.is_empty(),
            "every row of a {rows}-row failing set must earn a backoff, but {} did not \
             (first: {:?})",
            missing.len(),
            missing.first()
        );
    }

    /// The entry a failure just recorded is never the one its own insertion
    /// discards. The cap is enforced by dropping entries whose backoff has
    /// expired, and a freshly recorded entry is always one of those, so it has to
    /// be exempt: otherwise the count that was just earned is thrown away by the
    /// very call that earned it.
    #[test]
    fn error_ledger_keeps_the_row_it_just_recorded() {
        let ledger = ErrorRetryLedger::with_capacity(2);
        // Fill the ledger with rows that have each earned a live backoff.
        for id in 0..2i64 {
            for _ in 0..ERROR_BACKOFF_AFTER_FAILURES {
                ledger.note_failure("items", id);
            }
        }
        assert_eq!(ledger.lock().failures.len(), 2);
        assert!(
            ledger.is_backed_off("items", 0),
            "a filled ledger still backs its resident rows off"
        );

        // The insertion that pushes the map over its capacity must not be the
        // one that discards the row it recorded.
        ledger.note_failure("items", 7);
        assert_eq!(
            ledger
                .lock()
                .failures
                .get(&("items", 7))
                .map(|(count, _)| *count),
            Some(1),
            "the count just recorded must survive its own insertion"
        );
        assert_eq!(
            ledger.lock().failures.len(),
            2,
            "the ledger strictly respects its hard capacity"
        );
    }

    /// Under baseline sizing parameters (500-batch single-phase, 30s interval), a full ledger sweep
    /// takes longer than the maximum backoff window. In production, actual retries depend on
    /// configured HASH_INTERVAL and multi-queue concurrency.
    #[test]
    fn the_rotation_over_a_full_ledger_outspaces_the_backoff_ceiling() {
        let period = Duration::from_secs(
            ERROR_LEDGER_CAPACITY as u64 / ERROR_ROTATION_BATCH * ERROR_ROTATION_INTERVAL_SECS,
        );
        assert!(
            period >= ERROR_BACKOFF_MAX,
            "under default baseline parameters (500 batch, 30s interval), rotation period ({period:?}) \
             must be at least {ERROR_BACKOFF_MAX:?}"
        );
    }

    #[test]
    fn error_ledger_strictly_enforces_hard_capacity() {
        let capacity = 2usize;
        let ledger = ErrorRetryLedger::with_capacity(capacity);
        for id in 1..=2 {
            for _ in 0..ERROR_BACKOFF_AFTER_FAILURES {
                ledger.note_failure("items", id);
            }
        }
        ledger.note_failure("items", 3);
        assert_eq!(
            ledger.lock().failures.len(),
            capacity,
            "ledger must strictly enforce hard capacity even when backoffs are active"
        );
    }

    #[test]
    fn select_error_ids_advances_cursor_when_merged_results_have_remaining() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scan_candidates(id INTEGER PRIMARY KEY, status TEXT, hash_status TEXT);
             INSERT INTO scan_candidates VALUES (1,'pending','error'),(2,'pending','error'),
                 (3,'candidate','error'),(4,'candidate','error');",
        )
        .unwrap();
        let ledger = Box::leak(Box::new(ErrorRetryLedger::default()));

        // Tick 1: 4 candidates exist across 2 filters; limit is 3.
        let ids1 = select_error_ids(
            &conn,
            "scan_candidates",
            "",
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            3,
            Some(ledger),
        )
        .unwrap();
        assert_eq!(ids1, vec![1, 2, 3]);
        // Cursor must advance to 3 because candidate 4 remains.
        assert_eq!(ledger.cursor("scan_candidates"), 3);

        // Put ids 1..=3 into backoff so they are skipped on subsequent attempts.
        for id in &ids1 {
            for _ in 0..ERROR_BACKOFF_AFTER_FAILURES {
                ledger.note_failure("scan_candidates", *id);
            }
        }

        // Tick 2: continuing sweep past cursor 3 must reach candidate 4 instead of starving it.
        let ids2 = select_error_ids(
            &conn,
            "scan_candidates",
            "",
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            3,
            Some(ledger),
        )
        .unwrap();
        assert_eq!(
            ids2,
            vec![4],
            "candidate 4 must not be starved by backed-off low ids"
        );
        // After candidate 4, the sweep has exhausted the table, so cursor resets to start.
        assert_eq!(ledger.cursor("scan_candidates"), ERROR_CURSOR_START);
    }

    #[test]
    fn cursor_rotates_across_multiple_error_states() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE scan_candidates(id INTEGER PRIMARY KEY, status TEXT, hash_status TEXT);
             INSERT INTO scan_candidates VALUES
                 (10, 'pending', 'error'),
                 (20, 'pending', 'error'),
                 (30, 'candidate', 'error'),
                 (40, 'candidate', 'error');",
        )
        .unwrap();
        let ledger = Box::leak(Box::new(ErrorRetryLedger::default()));

        // Sweep 1: limit 2 selects lowest ids [10, 20] from pending filter.
        let ids1 = select_error_ids(
            &conn,
            "scan_candidates",
            "",
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            2,
            Some(ledger),
        )
        .unwrap();
        assert_eq!(ids1, vec![10, 20]);
        assert_eq!(ledger.cursor("scan_candidates"), 20);

        // Sweep 2: continuing past cursor 20, selects [30, 40] from candidate filter.
        let ids2 = select_error_ids(
            &conn,
            "scan_candidates",
            "",
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            2,
            Some(ledger),
        )
        .unwrap();
        assert_eq!(ids2, vec![30, 40]);
        // Exhausted: cursor wraps back to ERROR_CURSOR_START.
        assert_eq!(ledger.cursor("scan_candidates"), ERROR_CURSOR_START);

        // Sweep 3: next tick restarts from lowest id and rotates back to [10, 20].
        let ids3 = select_error_ids(
            &conn,
            "scan_candidates",
            "",
            &[
                "status='pending' AND hash_status='error'",
                "status='candidate' AND hash_status='error'",
            ],
            2,
            Some(ledger),
        )
        .unwrap();
        assert_eq!(ids3, vec![10, 20]);
        assert_eq!(ledger.cursor("scan_candidates"), 20);
    }

    /// The other half of the cap: entries whose backoff has expired are dropped
    /// once the map is over capacity, so a burst of one-off failures does not
    /// accumulate.
    #[test]
    fn error_ledger_prunes_expired_backoffs_once_over_capacity() {
        let capacity = 64usize;
        let ledger = ErrorRetryLedger::with_capacity(capacity);
        for id in 0..(capacity as i64 + 512) {
            ledger.note_failure("items", id);
        }
        let len = ledger.lock().failures.len();
        assert!(
            len <= capacity + 1,
            "expired entries must be pruned once the map exceeds capacity: {len}"
        );
    }

    #[test]
    fn hashes_pending_item() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("a.bin");
        std::fs::write(&file, b"hello-hash").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER,
              width INTEGER DEFAULT 0, height INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_seen (id, scan_id, artist_id, file_path, created_at)
             VALUES (1, 'stale-scan', 1, '/stale.jpg', 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status) VALUES (1,1,?, 'a.bin','pending')",
            params![path],
        )
        .unwrap();
        let out = run_hash_batch(&conn, 10).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["housekeeping"]["scan_seen_expired_deleted"], 1);
        let status: String = conn
            .query_row("SELECT hash_status FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(status, "done");
        let hash: String = conn
            .query_row("SELECT content_hash FROM items WHERE id=1", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert!(!hash.is_empty());
    }

    #[test]
    fn resolves_historical_done_scan_candidate_without_rehashing() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("historical.jpg");
        std::fs::write(&file, b"historical-hash").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER,
              width INTEGER DEFAULT 0, height INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/library/Artist');
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        let (file_size, file_mtime) = file_state(&file);
        let content_hash = hash_file(&file, 1024 * 1024).unwrap();
        conn.execute(
            "INSERT INTO scan_candidates
             (id, scan_id, status, hash_status, file_path, file_name, file_size,
              file_mtime, media_type, content_hash, artist_id)
             VALUES (1, 'old-scan', 'pending', 'done', ?, 'historical.jpg', ?, ?,
                     'image', ?, 1)",
            params![path, file_size, file_mtime, content_hash],
        )
        .unwrap();

        let result = run_hash_batch(&conn, 10).unwrap();

        assert_eq!(result["message"], "hash_batch_progress");
        assert_eq!(result["scan_candidates"]["done"], 0);
        let (status, count): (String, i64) = conn
            .query_row(
                "SELECT status, (SELECT COUNT(*) FROM items) FROM scan_candidates WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "new");
        assert_eq!(count, 1);
    }

    #[test]
    fn indexes_links_after_text_candidate_is_auto_imported() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("links.txt");
        std::fs::write(&file, "https://pan.quark.cn/s/example 提取码: A123").unwrap();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "
            CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              file_size INTEGER DEFAULT 0, file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '',
              date TEXT DEFAULT '', detected_date TEXT DEFAULT '', manual_date TEXT,
              auto_role TEXT DEFAULT '', tags TEXT DEFAULT '[]',
              missing INTEGER DEFAULT 0, missing_at REAL, scanned_at REAL,
              content_hash TEXT DEFAULT '', hash_status TEXT DEFAULT 'pending', hash_updated_at REAL,
              media_type TEXT DEFAULT 'image', is_archive INTEGER DEFAULT 0, st_dev INTEGER, st_ino INTEGER,
              width INTEGER DEFAULT 0, height INTEGER DEFAULT 0
            );
            CREATE TABLE scan_candidates (
              id INTEGER PRIMARY KEY, scan_id TEXT DEFAULT '', status TEXT, hash_status TEXT,
              file_path TEXT, file_name TEXT DEFAULT '', file_size INTEGER DEFAULT 0,
              file_mtime REAL DEFAULT 0, folder_name TEXT DEFAULT '', date TEXT DEFAULT '',
              is_archive INTEGER DEFAULT 0, media_type TEXT DEFAULT 'image',
              content_hash TEXT DEFAULT '', artist_id INTEGER DEFAULT 1, st_dev INTEGER, st_ino INTEGER,
              created_at REAL DEFAULT 0, resolved_at REAL
            );
            CREATE TABLE scan_seen (
              id INTEGER PRIMARY KEY, scan_id TEXT, artist_id INTEGER, file_path TEXT,
              created_at REAL DEFAULT 0
            );
            CREATE TABLE move_candidates (
              id INTEGER PRIMARY KEY, scan_candidate_id INTEGER, item_id INTEGER,
              artist_id INTEGER, old_path TEXT DEFAULT '', new_path TEXT DEFAULT '',
              reason TEXT DEFAULT '', status TEXT, resolved_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL);
            INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/library/Artist');
            ",
        )
        .unwrap();
        let path = file.to_string_lossy().to_string();
        let (file_size, file_mtime) = file_state(&file);
        conn.execute(
            "INSERT INTO scan_candidates
             (id, scan_id, status, hash_status, file_path, file_name, file_size,
              file_mtime, media_type, artist_id)
             VALUES (1, 'scan', 'pending', 'pending', ?, 'links.txt', ?, ?, 'text', 1)",
            params![path, file_size, file_mtime],
        )
        .unwrap();
        let root = dir.path().to_string_lossy().to_string();
        let roots = MediaRoots::identical(vec![root], vec!["library".into()]);

        let result = run_hash_batch_with_roots(&conn, &roots, 10).unwrap();
        let response = crate::link_index::artist_links_response(&conn, 1).unwrap();

        assert_eq!(result["links"]["indexed_documents"], 1);
        assert_eq!(response["summary"]["links"], 1);
        assert_eq!(response["summary"]["documents"], 1);
        assert_eq!(response["links"][0]["provider_name"], "夸克网盘");
    }

    /// L3 cross-check: `file_matches_snapshot` must reject a sub-second mtime
    /// change (no 1-second grace window). The cached hash is only valid when the
    /// live file mtime still matches the stored value within float epsilon.
    #[test]
    fn file_matches_snapshot_rejects_subsecond_mtime_change() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("snap.bin");
        std::fs::write(&file, b"snapshot-content").unwrap();
        let meta0 = std::fs::metadata(&file).unwrap();
        let m0 = meta0
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();

        // Unchanged snapshot matches.
        assert!(
            file_matches_snapshot(&meta0, meta0.len() as i64, m0, None, None),
            "unchanged metadata must match the snapshot"
        );

        // Bump mtime by 500ms. The live metadata must no longer match the stored mtime.
        let new_mtime = m0 + 0.5;
        std::fs::OpenOptions::new()
            .write(true)
            .open(&file)
            .unwrap()
            .set_modified(std::time::UNIX_EPOCH + std::time::Duration::from_secs_f64(new_mtime))
            .unwrap();
        let meta1 = std::fs::metadata(&file).unwrap();
        let live = meta1
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs_f64();
        assert!(
            (live - m0).abs() >= 0.4,
            "test setup: stored mtime should advance by ~0.5s, got delta {}",
            live - m0
        );
        assert!(
            !file_matches_snapshot(&meta1, meta1.len() as i64, m0, None, None),
            "a 500ms mtime change must invalidate the cached snapshot"
        );
    }

    /// L5: already-hashed scan candidates needing resolution must keep making
    /// progress even when the candidate-hashing queue is full of error rows that
    /// would otherwise consume the entire tick budget.
    #[test]
    fn history_phase_resolves_done_candidates_when_candidate_queue_full() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let file = media.join("real.bin");
        std::fs::write(&file, b"real-content-bytes").unwrap();
        let (size, mtime) = file_state(&file);
        let digest = crate::content_hash::hash_file(&file, 1024 * 1024).unwrap();
        let real_path = file.to_string_lossy().to_string();
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();

        // A present item already lives at the file path, so the done candidate
        // resolves via resolve_existing (action "existing") and is marked resolved.
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, file_size, file_mtime, hash_status, missing)
             VALUES (1, 1, ?, 'real.bin', ?, ?, 'done', 0)",
            params![real_path, size, mtime],
        )
        .unwrap();

        // Error candidate: a directory path keeps it 'error' and consumes the
        // candidate-queue slot every tick. Without a reserved resolution budget
        // the done candidate behind it would never be processed.
        let sub = media.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();
        let dir_path = sub.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name)
             VALUES (1, 'pending', 'error', ?, 'subdir')",
            params![dir_path],
        )
        .unwrap();

        // Done candidate awaiting resolution.
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime, content_hash)
             VALUES (2, 'pending', 'done', ?, 'real.bin', ?, ?, ?)",
            params![real_path, size, mtime, digest],
        )
        .unwrap();

        // limit=1: the error candidate fills the candidate queue; before the fix
        // the history phase received 0 budget and the done candidate was never
        // resolved.
        let out = run_hash_batch_with_roots(&conn, &single_media_root(&media), 1).unwrap();

        let status: String = conn
            .query_row("SELECT status FROM scan_candidates WHERE id=2", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(
            status, "resolved",
            "history phase must resolve the done candidate even when the candidate queue is full: {out}"
        );
    }

    /// L5: a scan candidate whose stored snapshot drifted (file changed after the
    /// scan) must be refreshed in place rather than blocking every later file;
    /// both the stale head and the file behind it make progress within a few ticks.
    #[test]
    fn stale_pending_candidate_refreshes_snapshot_and_unblocks_later_files() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let stale = media.join("stale.bin");
        let normal = media.join("normal.bin");
        std::fs::write(&stale, b"stale-v1").unwrap();
        std::fs::write(&normal, b"normal-v1").unwrap();
        let roots = single_media_root(&media);

        // Record the stale candidate with a STALE snapshot, then rewrite the file
        // so the stored size/mtime no longer matches the live file.
        let (stale_size, stale_mtime) = file_state(&stale);
        std::fs::write(&stale, b"stale-version-two-longer").unwrap();
        let (normal_size, normal_mtime) = file_state(&normal);
        let stale_path = stale.to_string_lossy().to_string();
        let normal_path = normal.to_string_lossy().to_string();

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        // Present items at both paths so each candidate resolves via the safe
        // "existing" path after its snapshot is refreshed and it is re-hashed.
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, hash_status, missing)
             VALUES (1, 1, ?, 'stale.bin', 'done', 0), (2, 1, ?, 'normal.bin', 'done', 0)",
            params![stale_path, normal_path],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (1, 'pending', 'pending', ?, 'stale.bin', ?, ?)",
            params![stale_path, stale_size, stale_mtime],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO scan_candidates (id, status, hash_status, file_path, file_name, file_size, file_mtime)
             VALUES (2, 'pending', 'pending', ?, 'normal.bin', ?, ?)",
            params![normal_path, normal_size, normal_mtime],
        )
        .unwrap();

        // limit=1: with the stale candidate at the head, the old code blocked the
        // normal file forever; the refresh keeps both progressing across ticks.
        for _ in 0..3 {
            run_hash_batch_with_roots(&conn, &roots, 1).unwrap();
        }

        let stale_status: String = conn
            .query_row(
                "SELECT hash_status FROM scan_candidates WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        let normal_status: String = conn
            .query_row(
                "SELECT hash_status FROM scan_candidates WHERE id=2",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stale_status, "done",
            "stale candidate should be re-hashed after its snapshot is refreshed"
        );
        assert_eq!(
            normal_status, "done",
            "the normal candidate must make progress behind a stale head"
        );
    }

    // --------------------------------------------------------- section 4: batching
    //
    // Item hash results are buffered and committed in short transactions
    // instead of one autocommit statement per file. These cover the acceptance
    // list in docs/PLAN_SCAN_QUERY_REVIEW_2026-09-12.md section 4.

    /// Real temp files in a media root, each with a pending item row.
    fn pending_items(media: &Path, count: i64) -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(race_schema()).unwrap();
        for id in 1..=count {
            let name = format!("f{id}.bin");
            let path = media.join(&name);
            std::fs::write(&path, format!("content-{id}")).unwrap();
            let (size, mtime) = file_state(&path);
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, file_size,
                 file_mtime, hash_status)
                 VALUES (?, 1, ?, ?, ?, ?, 'pending')",
                params![id, path.to_string_lossy().to_string(), name, size, mtime],
            )
            .unwrap();
        }
        conn
    }

    /// A budget that never flushes on its own, so only the end-of-loop flush
    /// runs and the row/byte/wait thresholds stay out of the way.
    fn one_shot_budget() -> HashCommitBudget {
        HashCommitBudget {
            max_rows: 64,
            max_bytes: i64::MAX,
            max_wait: Duration::from_secs(3600),
            large_file_bytes: i64::MAX,
        }
    }

    fn count_commits(conn: &Connection) -> std::sync::Arc<AtomicUsize> {
        let counter = std::sync::Arc::new(AtomicUsize::new(0));
        let handle = std::sync::Arc::clone(&counter);
        // rusqlite's commit hook returns `true` to *roll back*, so this returns
        // false to let every commit through.
        conn.commit_hook(Some(move || {
            handle.fetch_add(1, Ordering::SeqCst);
            false
        }));
        counter
    }

    fn item_status(conn: &Connection, id: i64) -> (String, String) {
        conn.query_row(
            "SELECT hash_status, content_hash FROM items WHERE id=?",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
    }

    /// The point of the change: several rows commit together, and every row
    /// still ends up finalized. One commit per row is the behaviour this
    /// replaces, so the batched run must strictly beat it.
    #[test]
    fn batched_item_hashes_commit_together() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 4);
        let roots = single_media_root(&media);

        let commits = count_commits(&conn);
        let per_row_budget = HashCommitBudget {
            max_rows: 1,
            ..one_shot_budget()
        };
        let out = run_hash_batch_with_budget(&conn, &roots, 10, per_row_budget).unwrap();
        assert_eq!(out["items"]["done"], 4);
        let per_row = commits.load(Ordering::SeqCst);

        // Re-queue the same four rows and hash them again in one commit.
        conn.execute(
            "UPDATE items SET hash_status='pending', content_hash=''",
            [],
        )
        .unwrap();
        let before = commits.load(Ordering::SeqCst);
        let out = run_hash_batch_with_budget(&conn, &roots, 10, one_shot_budget()).unwrap();
        assert_eq!(out["items"]["done"], 4);
        let batched = commits.load(Ordering::SeqCst) - before;

        assert!(
            per_row >= 4,
            "one row per commit must cost at least one commit per row, saw {per_row}"
        );
        assert!(
            batched < per_row,
            "batching four rows must commit less often: {batched} vs {per_row}"
        );
        for id in 1..=4 {
            let (status, hash) = item_status(&conn, id);
            assert_eq!(status, "done", "item {id} must still be finalized");
            assert!(!hash.is_empty(), "item {id} must keep its digest");
        }
    }

    /// The byte budget is a real boundary on its own: with a tiny budget the
    /// rows commit one at a time even though the row limit is far away.
    #[test]
    fn byte_budget_flushes_without_the_row_limit() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 3);
        let roots = single_media_root(&media);

        let commits = count_commits(&conn);
        let byte_budget = HashCommitBudget {
            max_bytes: 1,
            ..one_shot_budget()
        };
        let out = run_hash_batch_with_budget(&conn, &roots, 10, byte_budget).unwrap();
        assert_eq!(out["items"]["done"], 3);
        assert!(
            commits.load(Ordering::SeqCst) >= 3,
            "a one-byte budget must flush per row, not once per batch"
        );
    }

    /// A large file must not hold already-hashed results across its read: the
    /// wait threshold is only checked after a hash finishes, so the flush has
    /// to happen before the read starts.
    #[test]
    fn large_file_flushes_pending_rows_before_hashing() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 2);
        let roots = single_media_root(&media);

        let commits = count_commits(&conn);
        let large_file_budget = HashCommitBudget {
            large_file_bytes: 1,
            ..one_shot_budget()
        };
        let out = run_hash_batch_with_budget(&conn, &roots, 10, large_file_budget).unwrap();
        assert_eq!(out["items"]["done"], 2);
        assert!(
            commits.load(Ordering::SeqCst) >= 2,
            "a large file must flush the pending buffer before it is read"
        );
        for id in 1..=2 {
            assert_eq!(item_status(&conn, id).0, "done");
        }
    }

    /// A failed commit must roll the whole buffer back. Nothing is reported as
    /// done and no row is finalized, so the next tick simply re-hashes them.
    #[test]
    fn batched_item_hashes_roll_back_together_on_failure() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 3);
        let roots = single_media_root(&media);

        // Deny the second item update so the commit fails part-way through.
        // The authorizer fires once per updated *column*, so the second call is
        // the second column of the first statement: the batch's first update
        // fails and the whole buffer must roll back.
        let seen = std::sync::Arc::new(AtomicUsize::new(0));
        let handle = std::sync::Arc::clone(&seen);
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "items",
                    ..
                }
            ) && handle.fetch_add(1, Ordering::SeqCst) >= 1
            {
                return Authorization::Deny;
            }
            Authorization::Allow
        }));

        let result = run_hash_batch_with_budget(&conn, &roots, 10, one_shot_budget());
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(result.is_err(), "a denied update must fail the batch");

        let done: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE hash_status='done'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            done, 0,
            "a failed transaction must not leave a partially committed buffer"
        );
    }

    /// Crash-equivalent recovery: results committed by an earlier flush must
    /// survive a later failure. Only the uncommitted tail is allowed to be lost
    /// and re-hashed.
    #[test]
    fn committed_item_hashes_survive_a_later_flush_failure() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 3);
        let roots = single_media_root(&media);

        // Deny every item update once a hash result has already been committed.
        // The authorizer fires once per updated *column*, not once per
        // statement, so counting its calls would deny the first row part-way
        // through. Gating on the commit itself says exactly what is meant: the
        // first row lands, and every write after it fails.
        let commits = std::sync::Arc::new(AtomicUsize::new(0));
        let committed = std::sync::Arc::clone(&commits);
        conn.commit_hook(Some(move || {
            committed.fetch_add(1, Ordering::SeqCst);
            false
        }));
        let gate = std::sync::Arc::clone(&commits);
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Update {
                    table_name: "items",
                    ..
                }
            ) && gate.load(Ordering::SeqCst) >= 1
            {
                return Authorization::Deny;
            }
            Authorization::Allow
        }));

        let per_row_budget = HashCommitBudget {
            max_rows: 1,
            ..one_shot_budget()
        };
        let result = run_hash_batch_with_budget(&conn, &roots, 10, per_row_budget);
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);
        assert!(result.is_err(), "the second row's commit must fail");
        assert!(
            commits.load(Ordering::SeqCst) >= 1,
            "the first row must have committed before the failure"
        );

        assert_eq!(
            item_status(&conn, 1).0,
            "done",
            "the row committed before the failure must keep its result"
        );
        let pending: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM items WHERE id>1 AND hash_status='pending'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending, 2,
            "uncommitted rows must stay queued for the next batch"
        );
    }

    /// Hashing the same unchanged file again is a no-op, and re-queuing it
    /// produces the same digest: batching must not turn a retry into a rewrite
    /// or a different value.
    #[test]
    fn rehashing_the_same_file_is_idempotent() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let conn = pending_items(&media, 1);
        let roots = single_media_root(&media);

        let out = run_hash_batch_with_budget(&conn, &roots, 10, one_shot_budget()).unwrap();
        assert_eq!(out["items"]["done"], 1);
        let (_, first_hash) = item_status(&conn, 1);
        let first_stamp: f64 = conn
            .query_row("SELECT hash_updated_at FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();

        // Nothing is pending, so an unchanged library must not rewrite the row.
        // A commit count cannot show this: housekeeping and move resolution
        // share the tick and may commit on their own. The row's own timestamp
        // can, because a rewrite would move it.
        let out = run_hash_batch_with_budget(&conn, &roots, 10, one_shot_budget()).unwrap();
        assert_eq!(
            out["items"]["done"], 0,
            "a fully hashed library has no work"
        );
        let idle_stamp: f64 = conn
            .query_row("SELECT hash_updated_at FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(
            idle_stamp, first_stamp,
            "an idle tick must not rewrite an already-hashed row"
        );

        // Re-queueing the same file must produce the same digest.
        conn.execute("UPDATE items SET hash_status='pending'", [])
            .unwrap();
        let out = run_hash_batch_with_budget(&conn, &roots, 10, one_shot_budget()).unwrap();
        assert_eq!(out["items"]["done"], 1);
        let (status, second_hash) = item_status(&conn, 1);
        assert_eq!(status, "done");
        assert_eq!(
            second_hash, first_hash,
            "the same file must hash to the same digest"
        );
        let second_stamp: f64 = conn
            .query_row("SELECT hash_updated_at FROM items WHERE id=1", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(
            second_stamp >= first_stamp,
            "a re-hash must refresh the timestamp, not move it backwards"
        );
    }
}
