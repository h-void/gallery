use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

#[cfg(test)]
use crate::media_roots::MediaRoots;
use crate::natural_sort::natural_compare;

/// Hard cap for SQLite connection pool size (plan: limit pool).
const MAX_POOL_SIZE: usize = 32;

#[derive(Debug, Clone, Copy)]
pub struct DbConfig {
    pub read_only: bool,
    pub pool_size: usize,
}

#[derive(Debug)]
pub struct DbPool {
    db_path: PathBuf,
    config: DbConfig,
    conns: Mutex<PoolState>,
    available: Condvar,
}

#[derive(Debug)]
struct PoolState {
    idle: Vec<Connection>,
    /// Total live connections (idle + checked out). Never exceeds
    /// `config.pool_size`, so a request burst cannot grow the pool without
    /// bound and re-run WAL PRAGMAs per open.
    live: usize,
}

fn pool_acquire_timeout() -> Duration {
    env::var("DB_POOL_ACQUIRE_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .map(Duration::from_millis)
        .unwrap_or(Duration::from_secs(10))
}

pub struct PooledConn {
    pool: Arc<DbPool>,
    conn: Option<Connection>,
}
pub fn env_db_path() -> PathBuf {
    if let Ok(path) = env::var("GALLERY_ACCEL_DB_PATH") {
        if !path.trim().is_empty() {
            return PathBuf::from(path);
        }
    }
    let data_dir = env::var("DATA_DIR").unwrap_or_else(|_| "data".to_string());
    Path::new(&data_dir).join("gallery.db")
}

impl DbPool {
    pub fn new(db_path: PathBuf, size: usize) -> Result<Self> {
        Self::with_config(
            db_path,
            DbConfig {
                read_only: true,
                pool_size: size,
            },
        )
    }

    pub fn with_config(db_path: PathBuf, config: DbConfig) -> Result<Self> {
        let size = config.pool_size.clamp(1, MAX_POOL_SIZE);
        let fresh_database = !db_path.is_file()
            || std::fs::metadata(&db_path)
                .map(|metadata| metadata.len() == 0)
                .unwrap_or(true);
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(open_db(&db_path, config.read_only)?);
        }
        // Writable primary process must ensure schema exists (fail closed).
        if !config.read_only {
            ensure_product_schema(&conns[0], fresh_database)?;
            crate::artist_profile_links::ensure_artist_profile_links_schema(&conns[0])?;
            crate::link_index::ensure_link_schema(&conns[0])?;
            crate::ingest_publish::ensure_ingest_publish_schema(&conns[0])?;
            crate::netdisk::ensure_netdisk_bridge_schema(&conns[0])?;
            crate::pawchive::ensure_pawchive_schema(&conns[0])?;
            // The content-group ledger is read on the read-only path (the panel
            // and the pairing preview), so the tables have to exist before any
            // reader looks for them. The writers self-ensure as well; this is
            // what makes a read of a never-grouped library an empty answer
            // instead of "no such table".
            crate::pawchive_groups::ensure_content_group_schema(&conns[0])?;
            // A legacy database keeps its old scan_state shape (no stable
            // per-run scan_id) until some scan happens to write it. Migrate
            // at writable startup so health, /api/scan/state, WebSocket
            // polling, and idle workers can read the schema immediately.
            crate::scan::ensure_scan_state(&conns[0])
                .context("writable startup scan_state migration failed")?;
            // Statistics have to be collected before the first slow query runs,
            // and refreshed as the library grows. This also covers a fresh
            // install: an empty database analyzes in 0.17ms, and without this
            // the database would grow to full size before its first restart and
            // serve the mis-planned tag search in the meantime. A failure must
            // not block startup: the data stays correct, just slowly queried.
            if let Err(error) = ensure_query_planner_stats(&conns[0]) {
                log_error!("query planner statistics bootstrap failed: {error}");
            }
        } else {
            // Read-only: require at least artists table so empty files fail early.
            require_core_schema(&conns[0])?;
        }
        Ok(Self {
            db_path,
            config: DbConfig {
                read_only: config.read_only,
                pool_size: size,
            },
            conns: Mutex::new(PoolState {
                idle: conns,
                live: size,
            }),
            available: Condvar::new(),
        })
    }

    pub fn config(&self) -> DbConfig {
        self.config
    }

    pub fn get(self: &Arc<Self>) -> Result<PooledConn> {
        let deadline = Instant::now() + pool_acquire_timeout();
        let mut state = self.conns.lock().unwrap_or_else(|e| e.into_inner());
        loop {
            if let Some(conn) = state.idle.pop() {
                return Ok(PooledConn {
                    pool: Arc::clone(self),
                    conn: Some(conn),
                });
            }
            // Pool fully checked out: wait for a return instead of opening an
            // unbounded extra connection (each writable open re-runs WAL
            // PRAGMAs and competes with the scanner for locks).
            if state.live >= self.config.pool_size {
                let now = Instant::now();
                if now >= deadline {
                    anyhow::bail!(
                        "database connection pool busy: {} live connections",
                        state.live
                    );
                }
                let (next, wait) = self
                    .available
                    .wait_timeout(state, deadline - now)
                    .unwrap_or_else(|e| e.into_inner());
                state = next;
                if wait.timed_out() && state.idle.is_empty() {
                    anyhow::bail!(
                        "database connection pool busy: {} live connections",
                        state.live
                    );
                }
                continue;
            }
            let conn = open_db(&self.db_path, self.config.read_only)?;
            state.live += 1;
            return Ok(PooledConn {
                pool: Arc::clone(self),
                conn: Some(conn),
            });
        }
    }
}
impl std::ops::Deref for PooledConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        // Invariant: `conn` is only taken by `Drop`, so it is always `Some`
        // while a pooled connection is borrowable. If this ever fires, the
        // pool was borrowed after drop; a loud panic beats handing back a
        // missing connection.
        self.conn.as_ref().expect("pooled connection missing")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            // A connection returned mid-transaction would leak its locks and
            // uncommitted state into the next borrower (a failed COMMIT on a
            // call site leaves the transaction open). Roll any open
            // transaction back before re-pooling; if even the rollback fails,
            // discard the connection entirely.
            if !conn.is_autocommit() && conn.execute_batch("ROLLBACK").is_err() {
                if let Ok(mut state) = self.pool.conns.lock() {
                    state.live = state.live.saturating_sub(1);
                }
                self.pool.available.notify_one();
                return;
            }
            if let Ok(mut state) = self.pool.conns.lock() {
                // Do not grow the idle list beyond configured size; a surplus
                // connection is closed instead (and stops counting as live).
                if state.idle.len() < self.pool.config.pool_size {
                    state.idle.push(conn);
                } else {
                    state.live = state.live.saturating_sub(1);
                }
            }
            self.pool.available.notify_one();
        }
    }
}

fn open_db(path: &Path, read_only: bool) -> Result<Connection> {
    if read_only {
        open_readonly_db(path)
    } else {
        open_writable_db(path)
    }
}

fn open_readonly_db(path: &Path) -> Result<Connection> {
    let flags = OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI;
    let immutable = env::var("GALLERY_ACCEL_SQLITE_IMMUTABLE")
        .map(|value| {
            matches!(
                value.trim().to_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false);
    let conn = if immutable {
        let uri = sqlite_immutable_uri(path);
        Connection::open_with_flags(&uri, flags)
            .with_context(|| format!("open immutable sqlite database {}", path.display()))?
    } else {
        Connection::open_with_flags(path, flags)
            .with_context(|| format!("open read-only sqlite database {}", path.display()))?
    };
    configure_connection(&conn, true)?;
    Ok(conn)
}

pub fn open_writable_db(path: &Path) -> Result<Connection> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create data dir {}", parent.display()))?;
    }
    let flags = OpenFlags::SQLITE_OPEN_READ_WRITE
        | OpenFlags::SQLITE_OPEN_CREATE
        | OpenFlags::SQLITE_OPEN_URI;
    let conn = Connection::open_with_flags(path, flags)
        .with_context(|| format!("open read-write sqlite database {}", path.display()))?;
    configure_connection(&conn, false)?;
    Ok(conn)
}

fn require_core_schema(conn: &Connection) -> Result<()> {
    let has_artists: i64 = conn
        .query_row(
            "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='artists'",
            [],
            |r| r.get(0),
        )
        .unwrap_or(0);
    if has_artists == 0 {
        return Err(anyhow!(
            "database has no schema (missing artists); refuse empty sqlite file"
        ));
    }
    Ok(())
}

/// Minimal product schema for pure-Rust first boot (mirrors Python init_db core).
fn ensure_product_schema(conn: &Connection, create_indexes: bool) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS artists (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL,
            path TEXT UNIQUE NOT NULL,
            missing INTEGER NOT NULL DEFAULT 0,
            missing_at REAL,
            created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE TABLE IF NOT EXISTS items (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            file_path TEXT UNIQUE NOT NULL,
            file_name TEXT NOT NULL,
            file_size INTEGER NOT NULL DEFAULT 0,
            file_mtime REAL NOT NULL DEFAULT 0,
            folder_name TEXT NOT NULL DEFAULT '',
            date TEXT NOT NULL DEFAULT '',
            detected_date TEXT NOT NULL DEFAULT '',
            manual_date TEXT DEFAULT NULL,
            auto_role TEXT NOT NULL DEFAULT '',
            manual_role TEXT DEFAULT NULL,
            tags TEXT NOT NULL DEFAULT '[]',
            is_archive INTEGER NOT NULL DEFAULT 0,
            media_type TEXT NOT NULL DEFAULT 'image',
            content_hash TEXT NOT NULL DEFAULT '',
            hash_status TEXT NOT NULL DEFAULT 'pending',
            hash_updated_at REAL,
            st_dev INTEGER,
            st_ino INTEGER,
            missing INTEGER NOT NULL DEFAULT 0,
            missing_at REAL,
            scanned_at INTEGER NOT NULL DEFAULT (strftime('%s','now')),
            width INTEGER NOT NULL DEFAULT 0,
            height INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS tags (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            name TEXT NOT NULL,
            sort_order INTEGER NOT NULL DEFAULT 0,
            UNIQUE(artist_id, name)
        );
        CREATE TABLE IF NOT EXISTS item_tags (
            item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
            tag_id INTEGER NOT NULL REFERENCES tags(id) ON DELETE CASCADE,
            PRIMARY KEY(item_id, tag_id)
        );
        CREATE TABLE IF NOT EXISTS item_favorites (
            item_id INTEGER PRIMARY KEY REFERENCES items(id) ON DELETE CASCADE,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE TABLE IF NOT EXISTS recycle_entries (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            original_item_id INTEGER NOT NULL,
            artist_id INTEGER NOT NULL,
            original_path TEXT NOT NULL,
            recycled_path TEXT NOT NULL,
            item_snapshot TEXT NOT NULL,
            tag_ids_snapshot TEXT NOT NULL DEFAULT '[]',
            tag_single_refs_snapshot TEXT NOT NULL DEFAULT '[]',
            non_tag_single_ref_ids TEXT NOT NULL DEFAULT '[]',
            status TEXT NOT NULL DEFAULT 'recycled',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            restored_at REAL,
            restore_path TEXT NOT NULL DEFAULT '',
            last_error TEXT NOT NULL DEFAULT ''
        );
        CREATE INDEX IF NOT EXISTS idx_recycle_entries_status_created
            ON recycle_entries(status, created_at DESC, id DESC);
        CREATE TABLE IF NOT EXISTS app_settings (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL,
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE TABLE IF NOT EXISTS folder_rename_plans (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
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
            format_snapshot TEXT NOT NULL DEFAULT '{}',
            plan_kind TEXT NOT NULL DEFAULT 'rename_folder',
            split_actions TEXT NOT NULL DEFAULT '[]',
            UNIQUE(artist_id, source_folder)
        );
        CREATE TABLE IF NOT EXISTS characters (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            name TEXT NOT NULL UNIQUE,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE TABLE IF NOT EXISTS character_references (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            character_id INTEGER NOT NULL REFERENCES characters(id) ON DELETE CASCADE,
            embedding BLOB NOT NULL,
            embedding_dim INTEGER NOT NULL,
            embedding_model_repo_id TEXT NOT NULL DEFAULT '',
            embedding_model_variant TEXT NOT NULL DEFAULT '',
            embedding_model_file TEXT NOT NULL DEFAULT '',
            embedding_updated_at REAL,
            source_type TEXT NOT NULL DEFAULT 'gallery_item',
            item_id INTEGER REFERENCES items(id) ON DELETE SET NULL,
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE TABLE IF NOT EXISTS scan_seen (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scan_id TEXT NOT NULL,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            media_type TEXT NOT NULL DEFAULT 'image',
            file_size INTEGER NOT NULL DEFAULT 0,
            file_mtime REAL NOT NULL DEFAULT 0,
            st_dev INTEGER,
            st_ino INTEGER,
            content_hash TEXT NOT NULL DEFAULT '',
            hash_status TEXT NOT NULL DEFAULT 'pending',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_scan_seen_scan_artist
            ON scan_seen(scan_id, artist_id);
        CREATE INDEX IF NOT EXISTS idx_scan_seen_path
            ON scan_seen(scan_id, file_path);
        CREATE TABLE IF NOT EXISTS scan_candidates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scan_id TEXT NOT NULL,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_size INTEGER NOT NULL DEFAULT 0,
            file_mtime REAL NOT NULL DEFAULT 0,
            folder_name TEXT NOT NULL DEFAULT '',
            date TEXT NOT NULL DEFAULT '',
            is_archive INTEGER NOT NULL DEFAULT 0,
            media_type TEXT NOT NULL DEFAULT 'image',
            content_hash TEXT NOT NULL DEFAULT '',
            hash_status TEXT NOT NULL DEFAULT 'pending',
            st_dev INTEGER,
            st_ino INTEGER,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            resolved_at REAL
        );
        CREATE TABLE IF NOT EXISTS move_candidates (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            scan_candidate_id INTEGER REFERENCES scan_candidates(id) ON DELETE SET NULL,
            item_id INTEGER REFERENCES items(id) ON DELETE CASCADE,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            old_path TEXT NOT NULL,
            new_path TEXT NOT NULL,
            reason TEXT NOT NULL,
            content_hash TEXT NOT NULL DEFAULT '',
            st_dev INTEGER,
            st_ino INTEGER,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            resolved_at REAL
        );
        CREATE TABLE IF NOT EXISTS move_history (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            old_path TEXT NOT NULL,
            new_path TEXT NOT NULL,
            reason TEXT NOT NULL,
            status TEXT NOT NULL,
            details TEXT NOT NULL DEFAULT '{}',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            applied_at REAL,
            reverted_at REAL
        );
        "#,
    )
    .context("initialize product schema")?;
    ensure_character_reference_columns(conn)?;
    ensure_item_date_columns(conn)?;
    ensure_item_dimension_columns(conn)?;
    // Folder-archive schema (including its repair UPDATE) belongs to writable
    // startup so list endpoints never perform DDL/DML on read paths.
    crate::folder_archive::ensure_folder_schema(conn)?;
    if create_indexes {
        conn.execute_batch(
            r#"
            CREATE INDEX IF NOT EXISTS idx_artists_missing ON artists(missing);
            CREATE INDEX IF NOT EXISTS idx_items_artist ON items(artist_id);
            CREATE INDEX IF NOT EXISTS idx_items_role ON items(artist_id, manual_role);
            CREATE INDEX IF NOT EXISTS idx_items_auto_role ON items(artist_id, auto_role);
            CREATE INDEX IF NOT EXISTS idx_items_date ON items(artist_id, date);
            CREATE INDEX IF NOT EXISTS idx_items_archive ON items(artist_id, is_archive);
            CREATE INDEX IF NOT EXISTS idx_items_path ON items(file_path);
            CREATE INDEX IF NOT EXISTS idx_items_missing ON items(artist_id, missing);
            CREATE INDEX IF NOT EXISTS idx_items_hash_missing
                ON items(artist_id, content_hash, missing);
            CREATE INDEX IF NOT EXISTS idx_items_inode_missing
                ON items(artist_id, st_dev, st_ino, missing);
            CREATE INDEX IF NOT EXISTS idx_items_media
                ON items(artist_id, media_type, missing);
            CREATE INDEX IF NOT EXISTS idx_items_hash_queue
                ON items(missing, hash_status, id);
            CREATE INDEX IF NOT EXISTS idx_tags_artist ON tags(artist_id);
            CREATE INDEX IF NOT EXISTS idx_item_tags_item ON item_tags(item_id);
            CREATE INDEX IF NOT EXISTS idx_item_tags_tag ON item_tags(tag_id);
            CREATE INDEX IF NOT EXISTS idx_character_references_character
                ON character_references(character_id);
            CREATE INDEX IF NOT EXISTS idx_character_references_item
                ON character_references(item_id);
            CREATE INDEX IF NOT EXISTS idx_character_references_source
                ON character_references(source_type);
            CREATE INDEX IF NOT EXISTS idx_character_references_model
                ON character_references(
                    embedding_model_repo_id,
                    embedding_model_variant,
                    embedding_model_file,
                    embedding_dim
                );
            CREATE INDEX IF NOT EXISTS idx_scan_seen_scan_artist
                ON scan_seen(scan_id, artist_id);
            CREATE INDEX IF NOT EXISTS idx_scan_seen_hash
                ON scan_seen(scan_id, artist_id, content_hash);
            CREATE INDEX IF NOT EXISTS idx_scan_seen_path
                ON scan_seen(scan_id, file_path);
            CREATE INDEX IF NOT EXISTS idx_scan_candidates_status
                ON scan_candidates(status, artist_id);
            CREATE INDEX IF NOT EXISTS idx_scan_candidates_scan_status
                ON scan_candidates(scan_id, status);
            CREATE INDEX IF NOT EXISTS idx_scan_candidates_path
                ON scan_candidates(file_path);
            CREATE INDEX IF NOT EXISTS idx_scan_candidates_hash
                ON scan_candidates(artist_id, content_hash, status);
            CREATE INDEX IF NOT EXISTS idx_scan_candidates_hash_queue
                ON scan_candidates(status, hash_status, id);
            CREATE INDEX IF NOT EXISTS idx_move_candidates_status
                ON move_candidates(status, artist_id);
            CREATE INDEX IF NOT EXISTS idx_move_candidates_scan_candidate_status
                ON move_candidates(scan_candidate_id, status);
            -- Housekeeping (every hash tick) asks "are there expired missing
            -- items with no pending move candidate", which needs these two: the
            -- existing items index leads with artist_id and the existing
            -- move_candidates indexes lead with status or scan_candidate_id, so
            -- the probe and its NOT EXISTS subquery both fell back to a full
            -- scan of items plus one lookups per row.
            CREATE INDEX IF NOT EXISTS idx_items_missing_at
                ON items(missing, missing_at) WHERE missing_at IS NOT NULL;
            CREATE INDEX IF NOT EXISTS idx_move_candidates_item_status
                ON move_candidates(item_id, status);
            CREATE INDEX IF NOT EXISTS idx_move_candidates_new_path
                ON move_candidates(new_path);
            CREATE INDEX IF NOT EXISTS idx_move_history_item ON move_history(item_id);
            CREATE INDEX IF NOT EXISTS idx_move_history_status ON move_history(status);
            CREATE INDEX IF NOT EXISTS idx_folder_rename_artist_status
                ON folder_rename_plans(artist_id, status);
            "#,
        )?;
    }
    cleanup_legacy_missing_archive_plans(conn)?;
    require_core_schema(conn)?;
    Ok(())
}

/// Collect and refresh the SQLite query-planner statistics for the library.
///
/// SQLite only learns real cardinalities from `sqlite_stat1`, which is written
/// exclusively by `ANALYZE`. Without it the planner falls back to estimates
/// that are far off for this database's shape (750k+ `items`, a few thousand
/// `item_tags`), and it then drives the tag-count aggregate from the wrong side
/// of the join: the same SQL over the same rows returned 6.9s in production,
/// 10.3s with a cold cache locally and 249ms warm, against 1.4-13ms once
/// statistics existed.
///
/// Statistics are collected at writable startup and re-collected once the
/// library has changed scale (see [`STATS_REFRESH_FACTOR`]). The same check runs
/// from the scan and hash loops through [`StatsRefreshGate`], because a startup
/// bootstrap alone only covers "the library grew while the service was down":
/// a fresh install analyzes an empty database and then imports the whole
/// library without ever reaching another startup. Measured on a 260MB
/// same-shape database: 0.17ms on an empty database, 447ms bootstrap on a
/// populated one, and a no-op check costs one covering-index `COUNT(*)`
/// (~29ms at 755k items).
///
/// `PRAGMA optimize` is deliberately *not* the refresh mechanism. Measured on
/// the bundled SQLite: it is a no-op after a 1000x row growth (0.01ms, stat
/// unchanged), and it only re-analyzes a table the planner has already read
/// statistics for — and then still skipped a 10x growth. It would have been a
/// no-op in exactly the case this refresh exists for.
pub fn ensure_query_planner_stats(conn: &Connection) -> Result<()> {
    refresh_query_planner_stats(conn).map(|_| ())
}

/// Returns `true` when `ANALYZE` actually ran.
fn refresh_query_planner_stats(conn: &Connection) -> Result<bool> {
    let live_items = count_items(conn)?;
    let live_item_tags = count_item_tags(conn)?;
    if query_planner_stats_are_current(conn, live_items, live_item_tags)? {
        return Ok(false);
    }
    let started = Instant::now();
    conn.execute_batch("ANALYZE")
        .context("refresh query planner statistics failed")?;
    record_analyzed_items(conn, live_items, live_item_tags)?;
    // Only report work that was actually done. A fresh install analyzes an
    // empty database in well under a millisecond, and logging it would create
    // `gallery.log` on a brand-new install before anything has happened.
    if live_items > 0 || live_item_tags > 0 {
        log_info!(
            "query planner statistics collected with ANALYZE in {} ms ({} items, {} item_tags)",
            started.elapsed().as_millis(),
            live_items,
            live_item_tags
        );
    }
    Ok(true)
}

/// True when the recorded `ANALYZE` baseline still represents the library.
///
/// A recorded baseline of `0` means "this empty library has already been
/// analyzed", not "statistics were never collected". Without that distinction an
/// install that stays empty re-runs `ANALYZE` and rewrites the setting on every
/// single startup, which is a write per boot for no benefit. A *missing*
/// baseline is never current, so a database whose statistics predate this
/// setting is analyzed once and then adopted.
fn query_planner_stats_are_current(
    conn: &Connection,
    live_items: i64,
    live_item_tags: i64,
) -> Result<bool> {
    let Some(analyzed_items) = recorded_analyzed_items(conn)? else {
        return Ok(false);
    };
    // A database written before the tag baseline existed reads as "never
    // collected" and re-analyzes once, then records both keys.
    let Some(analyzed_item_tags) = recorded_analyzed_item_tags(conn)? else {
        return Ok(false);
    };
    Ok(cardinality_is_current(live_items, analyzed_items)
        && cardinality_is_current(live_item_tags, analyzed_item_tags))
}

/// `true` while `live` is close enough to `analyzed` for the recorded statistics
/// to still describe the table.
///
/// Growth *and* shrinkage both invalidate: a table that shrank to a quarter is
/// as mis-estimated as one that quadrupled. A recorded `0` means "this table was
/// already empty when we last looked", so it stays current only while it still
/// is — treating `0` as "within 4x of everything" would make an empty library
/// re-analyze on every single start.
fn cardinality_is_current(live: i64, analyzed: i64) -> bool {
    if analyzed == 0 {
        return live == 0;
    }
    live < analyzed.saturating_mul(STATS_REFRESH_FACTOR)
        && live.saturating_mul(STATS_REFRESH_FACTOR) > analyzed
}

/// Row-count change, in either direction, that makes recorded statistics
/// unrepresentative enough to re-collect. `PRAGMA optimize` tolerates 10x of
/// growth before it even considers a table; acting at 4x keeps the planner on
/// current cardinalities without re-reading a 1.2GiB database on every start.
const STATS_REFRESH_FACTOR: i64 = 4;

/// Default ceiling on how often the runtime refresh may look at the library.
const DEFAULT_STATS_REFRESH_INTERVAL: Duration = Duration::from_secs(60);

/// `app_settings.key` holding the item count observed by the last `ANALYZE`.
const ANALYZED_ITEMS_SETTING: &str = "query_planner_analyzed_items";

/// `app_settings.key` holding the `item_tags` count observed by the last
/// `ANALYZE`. A database written before this key existed simply reads as
/// "never collected" once, re-analyzes, and records both keys.
const ANALYZED_ITEM_TAGS_SETTING: &str = "query_planner_analyzed_item_tags";

/// Bounded-rate gate for the runtime statistics refresh.
///
/// The startup bootstrap is unconditional — it has to run before the first slow
/// query. The runtime refresh is called from inside the scan and hash loops,
/// where an ungated check would cost one library-wide `COUNT(*)` per file and
/// per batch. The gate puts a fixed ceiling on that cost per process, no matter
/// how much work the loops do.
#[derive(Debug)]
pub struct StatsRefreshGate {
    interval: Duration,
    last: Mutex<Option<Instant>>,
}

impl StatsRefreshGate {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            last: Mutex::new(None),
        }
    }

    /// Interval from `GALLERY_STATS_REFRESH_INTERVAL` seconds. `0` disables the
    /// runtime refresh entirely; an unparsable value falls back to the default
    /// rather than silently disabling it.
    pub fn from_env() -> Self {
        let interval = env::var("GALLERY_STATS_REFRESH_INTERVAL")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_STATS_REFRESH_INTERVAL);
        Self::new(interval)
    }

    pub fn interval(&self) -> Duration {
        self.interval
    }

    /// Returns `true` when the gate opened and the refresh ran.
    ///
    /// The gate closes for a full interval *before* the work is attempted, so a
    /// refresh that fails — lock contention, a read-only connection — gets a
    /// bounded retry on the next interval instead of a retry storm. The error is
    /// returned for the caller to log; it must never fail the scan or hash batch
    /// that triggered it, because the data stays correct either way.
    pub fn maybe_refresh(&self, conn: &Connection) -> Result<bool> {
        if self.interval.is_zero() {
            return Ok(false);
        }
        {
            let mut last = self
                .last
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            let now = Instant::now();
            if let Some(previous) = *last {
                if now.duration_since(previous) < self.interval {
                    return Ok(false);
                }
            }
            *last = Some(now);
        }
        refresh_query_planner_stats(conn)?;
        Ok(true)
    }

    /// Refresh inside the gate and swallow the error.
    ///
    /// A failed `ANALYZE` — lock contention, a full disk — must not turn a
    /// successful scan or hash batch into a failed one: the data stays correct and
    /// the queries keep the old plan until the gate opens again. The gate is
    /// closed before the attempt, so the retry is bounded rather than per file.
    ///
    /// Called from the two worker loops and from the manual scan and hash routes:
    /// the manual routes do the same work, so they must close the same gap.
    pub fn refresh_after_work(&self, conn: &Connection) {
        if let Err(error) = self.maybe_refresh(conn) {
            log_error!("query planner statistics refresh failed: {error:#}");
        }
    }
}

fn count_items(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM items", [], |row| row.get(0))?)
}

/// `item_tags` cardinality, the other half of the tag aggregate's join.
///
/// The mis-plan this whole module exists to prevent is the planner driving the
/// tag-count aggregate from the wrong side of the `items`/`item_tags` join, so
/// it is the *ratio* between the two tables that matters. Measuring on the
/// 755k-item reference library: this costs 0.02ms against 17.2ms for the
/// `items` count, because `item_tags` is small and the count rides a covering
/// index. It is only ever read from inside the rate-limited gate, never per
/// poll, so it does not reintroduce a per-round full-table count.
fn count_item_tags(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM item_tags", [], |row| row.get(0))?)
}

fn recorded_analyzed_items(conn: &Connection) -> Result<Option<i64>> {
    recorded_setting_count(conn, ANALYZED_ITEMS_SETTING)
}

fn recorded_analyzed_item_tags(conn: &Connection) -> Result<Option<i64>> {
    recorded_setting_count(conn, ANALYZED_ITEM_TAGS_SETTING)
}

fn recorded_setting_count(conn: &Connection, key: &str) -> Result<Option<i64>> {
    Ok(conn
        .query_row(
            "SELECT value FROM app_settings WHERE key=?1",
            [key],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .and_then(|value| value.trim().parse::<i64>().ok()))
}

/// Persist the cardinalities `ANALYZE` was run against.
///
/// The `WHERE value <> excluded.value` guard matters because this runs on the
/// startup path: without it a caller that reached here for any reason rewrites
/// the row and bumps `updated_at`, which turns a no-op startup into a write and
/// makes the setting useless for detecting whether anything actually changed.
fn record_analyzed_items(conn: &Connection, items: i64, item_tags: i64) -> Result<()> {
    record_setting_count(conn, ANALYZED_ITEMS_SETTING, items)?;
    record_setting_count(conn, ANALYZED_ITEM_TAGS_SETTING, item_tags)?;
    Ok(())
}

fn record_setting_count(conn: &Connection, key: &str, value: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at
         WHERE app_settings.value <> excluded.value",
        rusqlite::params![key, value.to_string()],
    )
    .context("record analyzed cardinality")?;
    Ok(())
}

fn ensure_character_reference_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(character_references)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in [
        ("embedding_model_repo_id", "TEXT NOT NULL DEFAULT ''"),
        ("embedding_model_variant", "TEXT NOT NULL DEFAULT ''"),
        ("embedding_model_file", "TEXT NOT NULL DEFAULT ''"),
        ("embedding_updated_at", "REAL"),
        // Server-side path of a manually uploaded reference photo. NULL for
        // tag_single rows, which preview through their library item instead.
        ("image_path", "TEXT"),
    ] {
        if !columns.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE character_references ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn ensure_item_date_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(items)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in [
        ("detected_date", "TEXT NOT NULL DEFAULT ''"),
        ("manual_date", "TEXT"),
    ] {
        if !columns.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE items ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    backfill_item_detected_dates(conn)?;
    Ok(())
}

/// Adds the intrinsic pixel-size columns used by the justified grid.
///
/// Legacy databases predate these columns, and the item page/detail queries
/// select them unconditionally, so they must exist before any read path runs.
/// `0` means "unknown": the grid falls back to a 4:3 aspect until the
/// dimension backfill fills the row in.
fn ensure_item_dimension_columns(conn: &Connection) -> Result<()> {
    let columns = conn
        .prepare("PRAGMA table_info(items)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (name, definition) in [
        ("width", "INTEGER NOT NULL DEFAULT 0"),
        ("height", "INTEGER NOT NULL DEFAULT 0"),
    ] {
        if !columns.iter().any(|column| column == name) {
            conn.execute(
                &format!("ALTER TABLE items ADD COLUMN {name} {definition}"),
                [],
            )?;
        }
    }
    Ok(())
}

fn backfill_item_detected_dates(conn: &Connection) -> Result<()> {
    let marker: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='item_date_columns_v1'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| "read item_date_columns_v1 marker")?;
    if marker.as_deref() == Some("1") {
        return Ok(());
    }
    let artist_path_cols = conn
        .prepare("PRAGMA table_info(artists)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    if !artist_path_cols.iter().any(|column| column == "path") {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT i.id, a.path, i.file_path, i.date FROM items i
         JOIN artists a ON a.id=i.artist_id
         WHERE i.detected_date='' AND i.date != ''",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut updates: Vec<(String, i64)> = Vec::with_capacity(rows.len());
    for (id, artist_path, file_path, legacy_date) in rows {
        updates.push((
            backfill_detected_from_path(&artist_path, &file_path, &legacy_date),
            id,
        ));
    }
    let tx = conn.unchecked_transaction()?;
    for (detected, id) in updates {
        tx.execute(
            "UPDATE items SET detected_date=? WHERE id=?",
            [detected, id.to_string()],
        )?;
    }
    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at)
         VALUES ('item_date_columns_v1', '1', strftime('%s','now'))",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// Derive the raw detected date for a legacy item from its stored path chain
/// relative to the artist root, falling back to the historical canonical
/// `date` when the old metadata cannot reveal its original precision.
fn backfill_detected_from_path(artist_path: &str, file_path: &str, legacy_date: &str) -> String {
    let artist = artist_path.trim_end_matches('/');
    let full = file_path.trim_end_matches('/');
    if artist.is_empty() || full.is_empty() {
        return legacy_date.to_string();
    }
    let chain = if full == artist {
        ""
    } else if let Some(rest) = full.strip_prefix(&format!("{artist}/")) {
        rest
    } else {
        full
    };
    let date_folder = chain.rsplit_once('/').map(|(dir, _)| dir).unwrap_or("");
    match crate::media_type::extract_date_value_from_folder(date_folder) {
        Some(value) if value.canonical == legacy_date => value.raw,
        _ => legacy_date.to_string(),
    }
}

fn cleanup_legacy_missing_archive_plans(conn: &Connection) -> Result<()> {
    let marker: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='legacy_missing_archive_cleanup_v2'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .with_context(|| "read legacy_missing_archive_cleanup_v2 marker")?;
    if marker.as_deref() == Some("1") {
        return Ok(());
    }

    let mut stmt = conn.prepare(
        "SELECT id, target_folder, execution_log FROM folder_rename_plans WHERE status='manual_review'",
    )?;
    let candidates = stmt
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<rusqlite::Result<Vec<(i64, String, String)>>>()?;
    let tx = conn.unchecked_transaction()?;
    for (id, target, raw_log) in candidates {
        let name = target.rsplit('/').next().unwrap_or("");
        let bytes = name.as_bytes();
        let year_month = bytes.len() >= 8
            && bytes[4] == b'-'
            && bytes[7] == b'-'
            && bytes[..4].iter().all(u8::is_ascii_digit)
            && bytes[5..7].iter().all(u8::is_ascii_digit);
        let year_month_day =
            bytes.len() >= 11 && bytes[10] == b'-' && bytes[8..10].iter().all(u8::is_ascii_digit);
        let legacy_format = year_month && (year_month_day || bytes.get(8) != Some(&b' '));
        let source_missing = serde_json::from_str::<Value>(&raw_log)
            .ok()
            .and_then(|value| value.as_array().and_then(|rows| rows.last()).cloned())
            .and_then(|row| row.get("reason").and_then(Value::as_str).map(str::to_owned))
            .as_deref()
            == Some("source_missing");
        if legacy_format && source_missing {
            tx.execute("DELETE FROM folder_rename_plans WHERE id=?", [id])?;
        }
    }
    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('legacy_missing_archive_cleanup_v2', '1', strftime('%s','now'))",
        [],
    )?;
    tx.commit()?;
    Ok(())
}

/// The media-path migration lives in `db_identity` (plan M10). Re-exported
/// here because `db` is the schema-upgrade surface callers already use.
pub use crate::db_identity::normalize_configured_media_paths;

pub const DEFAULT_SQLITE_BUSY_TIMEOUT_MS: u64 = 30_000;

/// Mirror of `app/database.py:_configure_connection` so the Rust side behaves
/// identically to the Python side (WAL, busy_timeout, foreign_keys, NATURAL_NOCASE).
fn configure_connection(conn: &Connection, read_only: bool) -> Result<()> {
    let busy_timeout = env::var("SQLITE_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_SQLITE_BUSY_TIMEOUT_MS);
    let journal_size_limit = env::var("SQLITE_JOURNAL_SIZE_LIMIT")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(67_108_864);
    let mmap_size = env::var("SQLITE_MMAP_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(268_435_456);

    conn.create_collation("NATURAL_NOCASE", |left: &str, right: &str| {
        natural_compare(left, right)
    })
    .context("register NATURAL_NOCASE collation")?;

    if read_only {
        conn.pragma_update(None, "query_only", "ON")?;
    } else {
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "NORMAL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.pragma_update(None, "journal_size_limit", journal_size_limit)?;
    }
    conn.pragma_update(None, "busy_timeout", busy_timeout as i64)?;
    conn.pragma_update(None, "mmap_size", mmap_size)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;
    Ok(())
}

/// SQL identifiers interpolated into statement text must come from internal
/// constants only — never from user input. Fail loudly if that invariant is
/// ever violated instead of letting a future refactor become an injection.
pub(crate) fn sql_ident(name: &str) -> &str {
    let valid = !name.is_empty()
        && name.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    assert!(
        valid,
        "SQL identifier must be an internal constant: {name:?}"
    );
    name
}

fn sqlite_immutable_uri(path: &Path) -> String {
    let path = path.to_string_lossy().replace('\\', "/");
    let mut encoded = String::with_capacity(path.len());
    for byte in path.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'/' | b'.' | b'_' | b'-' | b':' => {
                encoded.push(byte as char);
            }
            other => encoded.push_str(&format!("%{other:02X}")),
        }
    }
    format!("file:{encoded}?mode=ro&immutable=1")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Row count ANALYZE recorded for `table`, or `None` without statistics.
    /// Prefers the table-level row (`idx IS NULL`); every row's leading token is
    /// the row count, index rows just append distinct-value counts.
    fn table_stat(conn: &Connection, table: &str) -> Option<i64> {
        let stat: Option<String> = conn
            .query_row(
                "SELECT stat FROM sqlite_stat1 WHERE tbl=?1
                 ORDER BY (idx IS NOT NULL), idx LIMIT 1",
                [table],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        stat.and_then(|value| value.split_whitespace().next()?.parse().ok())
    }

    fn seed_items(conn: &Connection, id: i64) {
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (?1, 1, ?2, ?3)",
            rusqlite::params![
                id,
                format!("/pictures/Artist/{id}.jpg"),
                format!("{id}.jpg")
            ],
        )
        .unwrap();
    }

    /// Insert `id` in `from..=to` in one statement, so a test can reach a real
    /// cardinality without a loop.
    fn seed_item_range(conn: &Connection, from: i64, to: i64) {
        conn.execute_batch(&format!(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             WITH RECURSIVE n(i) AS (SELECT {from} UNION ALL SELECT i+1 FROM n WHERE i<{to})
             SELECT i, 1, '/pictures/Artist/' || i || '.jpg', i || '.jpg' FROM n;"
        ))
        .unwrap();
    }

    /// A writable database with one artist and the product schema, plus the
    /// `app_settings` table the statistics baseline is recorded in.
    fn seeded_database(path: &Path, artist: bool) {
        let conn = open_writable_db(path).unwrap();
        ensure_product_schema(&conn, true).unwrap();
        if artist {
            conn.execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist')",
                [],
            )
            .unwrap();
        }
    }

    #[test]
    fn sql_ident_rejects_non_internal_identifiers() {
        assert_eq!(sql_ident("move_candidates"), "move_candidates");
        assert_eq!(sql_ident("_legacy"), "_legacy");
        for bad in ["", "Move", "move;drop", "move-table", "1table", "t able"] {
            let rejected = std::panic::catch_unwind(|| sql_ident(bad)).is_err();
            assert!(rejected, "sql_ident must reject {bad:?}");
        }
    }

    #[test]
    fn schema_initialization_does_not_run_media_path_migration() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();

        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |row| row.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "schema initialization must not migrate media paths before HTTP bind"
        );
    }

    #[test]
    fn legacy_items_gain_detected_and_manual_date_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (
                id INTEGER PRIMARY KEY,
                name TEXT NOT NULL,
                path TEXT NOT NULL,
                missing INTEGER NOT NULL DEFAULT 0,
                missing_at REAL,
                created_at INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                artist_id INTEGER NOT NULL,
                file_path TEXT NOT NULL,
                file_name TEXT NOT NULL,
                file_size INTEGER NOT NULL DEFAULT 0,
                file_mtime REAL NOT NULL DEFAULT 0,
                folder_name TEXT NOT NULL DEFAULT '',
                date TEXT NOT NULL DEFAULT '',
                auto_role TEXT NOT NULL DEFAULT '',
                manual_role TEXT DEFAULT NULL,
                tags TEXT NOT NULL DEFAULT '[]',
                is_archive INTEGER NOT NULL DEFAULT 0,
                media_type TEXT NOT NULL DEFAULT 'image',
                content_hash TEXT NOT NULL DEFAULT '',
                hash_status TEXT NOT NULL DEFAULT 'pending',
                hash_updated_at REAL,
                st_dev INTEGER,
                st_ino INTEGER,
                missing INTEGER NOT NULL DEFAULT 0,
                missing_at REAL,
                scanned_at INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist');
             INSERT INTO items (id, artist_id, file_path, file_name, date) VALUES
                (1, 1, '/pictures/Artist/2026/202607 foo/x.jpg', 'x.jpg', '2026-07-01'),
                (2, 1, '/pictures/Artist/2026-05-01_title/y.jpg', 'y.jpg', '2026-05-01'),
                (3, 1, '/pictures/Artist/no_date_dir/z.jpg', 'z.jpg', '2026-08-15'),
                (4, 1, '/pictures/Artist/202508/01_1536_title/w.jpg', 'w.jpg', '2025-08-01');",
        )
        .unwrap();

        ensure_product_schema(&conn, false).unwrap();

        let columns = conn
            .prepare("PRAGMA table_info(items)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for column in ["detected_date", "manual_date"] {
            assert!(columns.iter().any(|candidate| candidate == column));
        }

        let rows: Vec<(String, String, Option<String>)> = conn
            .prepare("SELECT date, detected_date, manual_date FROM items ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            rows[0],
            ("2026-07-01".to_string(), "2026-07".to_string(), None),
            "month-only folder must expose month precision without changing the canonical date"
        );
        assert_eq!(
            rows[1],
            ("2026-05-01".to_string(), "2026-05-01".to_string(), None),
            "full-day folder keeps its day in detected_date"
        );
        assert_eq!(
            rows[2],
            ("2026-08-15".to_string(), "2026-08-15".to_string(), None),
            "unparseable folder falls back exactly to the historical date"
        );
        assert_eq!(
            rows[3],
            ("2025-08-01".to_string(), "2025-08-01".to_string(), None),
            "compact folder keeps its full date in detected_date"
        );

        ensure_product_schema(&conn, false).unwrap();
        let again: Vec<(String, String)> = conn
            .prepare("SELECT date, detected_date FROM items ORDER BY id")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(again[0], ("2026-07-01".to_string(), "2026-07".to_string()));
    }

    /// Pre-scan_id production database: artists plus the legacy scan_state
    /// shape that lacks the immutable per-run scan_id column.
    fn legacy_scan_state_fixture(db_path: &std::path::Path) {
        let conn = Connection::open(db_path).unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                name TEXT NOT NULL,
                path TEXT UNIQUE NOT NULL,
                missing INTEGER NOT NULL DEFAULT 0,
                missing_at REAL,
                created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
             );
             INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist');
             CREATE TABLE scan_state (
                id INTEGER PRIMARY KEY CHECK (id = 1),
                artist_id INTEGER,
                status TEXT NOT NULL DEFAULT 'idle',
                phase TEXT NOT NULL DEFAULT '',
                scanned_count INTEGER NOT NULL DEFAULT 0,
                total_estimate INTEGER NOT NULL DEFAULT 0,
                current_path TEXT NOT NULL DEFAULT '',
                started_at REAL,
                updated_at REAL
             );
             INSERT INTO scan_state (id, status) VALUES (1, 'idle');",
        )
        .unwrap();
    }

    #[test]
    fn writable_startup_migrates_legacy_scan_state_before_any_reader() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        legacy_scan_state_fixture(&db_path);

        // Writable with_config must run the migration itself; the test never
        // calls ensure_scan_state() manually.
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();

        let has_scan_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('scan_state') WHERE name='scan_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            has_scan_id, 1,
            "legacy scan_state gains scan_id at writable startup"
        );
        let row: (String, String) = conn
            .query_row(
                "SELECT status, scan_id FROM scan_state WHERE id=1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(row.0, "idle", "existing scan_state row is preserved");
        assert_eq!(row.1, "", "the migrated scan_id starts empty");

        let state = crate::scan::get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["scan_id"], "");
        drop(conn);
        drop(pool);
    }

    #[test]
    fn writable_startup_scan_state_migration_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        legacy_scan_state_fixture(&db_path);

        for _ in 0..2 {
            let pool = std::sync::Arc::new(
                DbPool::with_config(
                    db_path.clone(),
                    DbConfig {
                        read_only: false,
                        pool_size: 1,
                    },
                )
                .unwrap(),
            );
            let conn = pool.get().unwrap();
            let state = crate::scan::get_scan_state(&conn).unwrap();
            assert_eq!(state["scan_id"], "");
            let shape: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM pragma_table_info('scan_state')
                     WHERE name='scan_id' AND type='TEXT' AND \"notnull\"=1 AND dflt_value IS NOT NULL",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(shape, 1, "scan_id keeps its TEXT NOT NULL DEFAULT '' shape");
            drop(conn);
            drop(pool);
        }
    }

    #[test]
    fn writable_startup_bootstraps_query_planner_statistics_once() {
        // Comparing the whole table beats asserting one row's shape: ANALYZE
        // may record a table-level row or fold the count into its index rows.
        fn stats_snapshot(conn: &Connection) -> Option<Vec<(String, Option<String>, String)>> {
            let exists: i64 = conn
                .query_row(
                    "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='sqlite_stat1'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            if exists == 0 {
                return None;
            }
            let mut stmt = conn
                .prepare("SELECT tbl, idx, stat FROM sqlite_stat1 ORDER BY tbl, idx")
                .unwrap();
            Some(
                stmt.query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
                    .unwrap()
                    .collect::<rusqlite::Result<Vec<_>>>()
                    .unwrap(),
            )
        }

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        // Build the file by hand so the startup under test is the upgrade path
        // an existing install takes, not the schema-creating first start.
        {
            let conn = open_writable_db(&db_path).unwrap();
            ensure_product_schema(&conn, true).unwrap();
            conn.execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist')",
                [],
            )
            .unwrap();
            seed_items(&conn, 1);
        }

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        let first = stats_snapshot(&conn)
            .expect("a database without statistics must be analyzed at writable startup");
        assert!(
            !first.is_empty(),
            "the bootstrap ANALYZE must record something"
        );

        // The library itself is unchanged, so a later startup must leave the
        // recorded statistics alone rather than re-reading the whole database.
        drop(conn);
        drop(pool);

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        assert_eq!(
            stats_snapshot(&conn),
            Some(first),
            "statistics within the refresh factor must not be rebuilt on every startup"
        );
    }

    #[test]
    fn fresh_writable_startup_still_collects_statistics() {
        // A brand-new database has no statistics either, and it can grow to full
        // library size long before the next restart. Skip the bootstrap on a
        // fresh path and that first library serves the mis-planned tag search.
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path,
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        let stats_table_exists: i64 = conn
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name='sqlite_stat1'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            stats_table_exists, 1,
            "a fresh database must still run the bootstrap ANALYZE"
        );
    }

    #[test]
    fn statistics_are_refreshed_once_the_library_outgrows_them() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        {
            let conn = open_writable_db(&db_path).unwrap();
            ensure_product_schema(&conn, true).unwrap();
            conn.execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist')",
                [],
            )
            .unwrap();
            seed_items(&conn, 1);
        }
        let open = || {
            std::sync::Arc::new(
                DbPool::with_config(
                    db_path.clone(),
                    DbConfig {
                        read_only: false,
                        pool_size: 1,
                    },
                )
                .unwrap(),
            )
        };

        let pool = open();
        let conn = pool.get().unwrap();
        let bootstrapped = table_stat(&conn, "items").expect("bootstrap must record items");
        assert_eq!(bootstrapped, 1);
        // Grow the library far past the factor that makes the recorded
        // statistics unrepresentative.
        conn.execute_batch(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i<300)
             SELECT i, 1, '/pictures/Artist/' || i || '.jpg', i || '.jpg' FROM n;",
        )
        .unwrap();
        drop(conn);
        drop(pool);

        let pool = open();
        let conn = pool.get().unwrap();
        assert_eq!(
            table_stat(&conn, "items"),
            Some(300),
            "a startup after material growth must refresh the statistics"
        );
    }

    /// The gap this closes: the startup bootstrap analyzes the library as it is
    /// at boot, so an install that starts empty and then imports everything has
    /// no statistics until its next restart. The runtime gate has to notice the
    /// growth inside the same pool lifetime.
    #[test]
    fn runtime_refresh_collects_statistics_after_ingest_without_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        seeded_database(&db_path, true);
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path,
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        assert_eq!(
            recorded_analyzed_items(&conn).unwrap(),
            Some(0),
            "the startup bootstrap must record that it analyzed the empty library"
        );
        // An empty table has no row count to learn, so `ANALYZE` leaves it
        // without a statistic — but it does record a `0` for an index over that
        // empty table, and `idx_items_missing_at` is one. Either answer means
        // the same thing here: nothing has been measured about `items` yet.
        assert!(
            matches!(table_stat(&conn, "items"), None | Some(0)),
            "an empty library has no item count to analyze, got {:?}",
            table_stat(&conn, "items")
        );

        // Ingest through the same connection, with no restart in between.
        seed_item_range(&conn, 1, 300);

        // Any non-zero interval opens the gate on its first call.
        let gate = StatsRefreshGate::new(Duration::from_millis(1));
        assert!(
            gate.maybe_refresh(&conn).unwrap(),
            "the first runtime check must run"
        );
        assert_eq!(
            table_stat(&conn, "items"),
            Some(300),
            "a runtime ingest must produce items statistics without a restart"
        );
        assert_eq!(recorded_analyzed_items(&conn).unwrap(), Some(300));
    }

    /// An install that stays empty must not re-`ANALYZE` and rewrite the
    /// baseline on every single startup. The recorded `0` means "this empty
    /// library has been analyzed", not "statistics were never collected".
    #[test]
    fn empty_library_startups_analyze_once() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        let open = || {
            std::sync::Arc::new(
                DbPool::with_config(
                    db_path.clone(),
                    DbConfig {
                        read_only: false,
                        pool_size: 1,
                    },
                )
                .unwrap(),
            )
        };

        let pool = open();
        {
            let conn = pool.get().unwrap();
            assert_eq!(
                recorded_analyzed_items(&conn).unwrap(),
                Some(0),
                "the first startup must record the empty-library baseline"
            );
        }
        drop(pool);

        for attempt in 0..2 {
            let pool = open();
            let conn = pool.get().unwrap();
            let value: String = conn
                .query_row(
                    "SELECT value FROM app_settings WHERE key=?1",
                    [ANALYZED_ITEMS_SETTING],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(value, "0");
            // `total_changes` counts rows written on this connection, so a
            // re-run of ANALYZE plus a baseline rewrite cannot hide behind
            // "the setting still says 0".
            assert_eq!(
                conn.total_changes(),
                0,
                "startup #{attempt} must not write anything on an unchanged empty library"
            );
        }
    }

    /// The gate is what keeps the runtime refresh from becoming a per-file cost:
    /// inside its interval it must not even run the library-wide `COUNT(*)`.
    #[test]
    fn runtime_gate_skips_checks_within_its_interval() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        seeded_database(&db_path, true);
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path,
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();

        let gate = StatsRefreshGate::new(Duration::from_secs(3600));
        assert!(gate.maybe_refresh(&conn).unwrap());
        assert!(
            !gate.maybe_refresh(&conn).unwrap(),
            "a second check inside the interval must be skipped"
        );
        assert!(
            !gate.maybe_refresh(&conn).unwrap(),
            "the gate must stay closed for the whole interval"
        );

        // `0` is the documented way to turn the runtime refresh off entirely.
        let disabled = StatsRefreshGate::new(Duration::ZERO);
        assert!(!disabled.maybe_refresh(&conn).unwrap());
    }

    /// Shrinkage is as misleading as growth: a library that dropped to a quarter
    /// of its size has statistics describing four times the rows.
    #[test]
    fn statistics_are_refreshed_when_the_library_shrinks_to_a_quarter() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        seeded_database(&db_path, true);
        {
            let conn = open_writable_db(&db_path).unwrap();
            seed_item_range(&conn, 1, 400);
        }

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        {
            let conn = pool.get().unwrap();
            assert_eq!(table_stat(&conn, "items"), Some(400));
            conn.execute("DELETE FROM items WHERE id > 100", [])
                .unwrap();
        }
        drop(pool);

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path,
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        assert_eq!(
            table_stat(&conn, "items"),
            Some(100),
            "a startup after the library shrank to a quarter must refresh the statistics"
        );
    }

    /// Plan section 10 step 2: the refresh trigger must not look at `items`
    /// alone.
    ///
    /// The mis-plan this module exists to prevent is the planner driving the
    /// tag-count aggregate from the wrong side of the `items`/`item_tags` join,
    /// which is decided by the *ratio* between the two tables. A library whose
    /// item count is frozen while `item_tags` changes by orders of magnitude
    /// leaves the planner estimating that join from a cardinality that no longer
    /// exists -- so the gate has to watch the tag side too, without adding a
    /// per-poll count of every table (section 10 asks specifically for that).
    #[test]
    fn statistics_are_refreshed_when_only_item_tags_changes_scale() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        seeded_database(&db_path, true);
        {
            let conn = open_writable_db(&db_path).unwrap();
            seed_item_range(&conn, 1, 200);
            conn.execute_batch(
                "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'tag1');
                 INSERT INTO item_tags (item_id, tag_id) SELECT id, 1 FROM items;",
            )
            .unwrap();
        }

        let open = || {
            std::sync::Arc::new(
                DbPool::with_config(
                    db_path.clone(),
                    DbConfig {
                        read_only: false,
                        pool_size: 1,
                    },
                )
                .unwrap(),
            )
        };

        let pool = open();
        {
            let conn = pool.get().unwrap();
            // First writable startup records both baselines.
            assert_eq!(table_stat(&conn, "items"), Some(200));
            assert_eq!(table_stat(&conn, "item_tags"), Some(200));

            // 12 more tags, each applied to every item: item_tags 200 -> 2600,
            // while `items` stays at exactly 200.
            conn.execute_batch(
                "INSERT INTO tags (id, artist_id, name)
                 WITH RECURSIVE n(i) AS (SELECT 2 UNION ALL SELECT i+1 FROM n WHERE i < 13)
                 SELECT i, 1, 'tag' || i FROM n;
                 INSERT INTO item_tags (item_id, tag_id)
                 SELECT it.item_id, t.id FROM item_tags it JOIN tags t ON t.id BETWEEN 2 AND 13;",
            )
            .unwrap();
        }
        drop(pool);

        let pool = open();
        let conn = pool.get().unwrap();
        assert_eq!(
            table_stat(&conn, "items"),
            Some(200),
            "the item count must be untouched, so the item rule alone cannot account for a refresh"
        );
        assert_eq!(
            table_stat(&conn, "item_tags"),
            Some(2600),
            "a startup after only item_tags changed scale must refresh the statistics"
        );
    }

    /// A refresh runs on whichever pooled connection the worker holds. Statements
    /// prepared later on the *other* connections must plan against the new
    /// statistics, which is a different question from "did sqlite_stat1 change".
    #[test]
    fn refreshed_statistics_are_visible_to_other_pooled_connections() {
        fn plan(conn: &Connection, sql: &str) -> Vec<String> {
            conn.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
                .unwrap()
                .query_map([], |row| row.get::<_, String>(3))
                .unwrap()
                .collect::<rusqlite::Result<Vec<_>>>()
                .unwrap()
        }

        const QUERY: &str =
            "SELECT t.id, (SELECT COUNT(*) FROM item_tags it WHERE it.tag_id = t.id) FROM tags t";

        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        seeded_database(&db_path, true);
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 2,
                },
            )
            .unwrap(),
        );

        let warm = pool.get().unwrap();
        // Prepare and step on the second connection *before* the refresh, so a
        // cached plan or an unseen schema change would show up as a difference.
        let _ = plan(&warm, QUERY);

        let worker = pool.get().unwrap();
        seed_item_range(&worker, 1, 500);
        worker.execute_batch("ANALYZE").unwrap();
        drop(worker);

        let after = plan(&warm, QUERY);
        // A connection opened now can only see the statistics on disk, so it is
        // the reference for "what the plan should be".
        let fresh = open_writable_db(&db_path).unwrap();
        assert_eq!(
            after,
            plan(&fresh, QUERY),
            "a pooled connection must plan against the refreshed statistics"
        );
    }

    #[test]
    fn migrated_scan_state_reads_from_a_read_only_pool() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        legacy_scan_state_fixture(&db_path);

        {
            let pool = std::sync::Arc::new(
                DbPool::with_config(
                    db_path.clone(),
                    DbConfig {
                        read_only: false,
                        pool_size: 1,
                    },
                )
                .unwrap(),
            );
            drop(pool);
        }

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: true,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        let state = crate::scan::get_scan_state(&conn).unwrap();
        assert_eq!(state["status"], "idle");
        assert_eq!(state["scan_id"], "");
    }

    #[test]
    fn read_only_startup_never_performs_scan_state_ddl() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        legacy_scan_state_fixture(&db_path);

        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path.clone(),
                DbConfig {
                    read_only: true,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        let has_scan_id: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_info('scan_state') WHERE name='scan_id'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            has_scan_id, 0,
            "read-only startup must not perform scan-state DDL"
        );
        let err = crate::scan::get_scan_state(&conn).unwrap_err();
        assert!(
            format!("{err}").contains("no such column"),
            "reads still surface the legacy schema instead of mutating it"
        );
    }

    #[test]
    fn writable_startup_creates_the_content_group_ledger() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        let pool = std::sync::Arc::new(
            DbPool::with_config(
                db_path,
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
            )
            .unwrap(),
        );
        let conn = pool.get().unwrap();
        // The reconciliation reads run on this schema, and a library that was
        // never grouped must answer "no groups" rather than "no such table".
        for table in [
            "content_groups",
            "content_group_members",
            "content_group_locations",
        ] {
            let present: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                    rusqlite::params![table],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(present, 1, "{table} is created at writable startup");
        }
        let groups = crate::pawchive_groups::list_content_groups(&conn, None, false).unwrap();
        assert!(groups.is_empty(), "a never-grouped library has no groups");
    }

    #[test]
    fn writable_startup_fails_closed_when_scan_state_migration_fails() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        {
            let conn = Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE artists (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    path TEXT UNIQUE NOT NULL
                 );
                 CREATE VIEW scan_state AS SELECT 1 AS id;",
            )
            .unwrap();
        }

        let result = DbPool::with_config(
            db_path,
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
        );

        assert!(
            result.is_err(),
            "pool must not be returned on migration failure"
        );
        let err = result.err().unwrap();
        let chain = format!("{err:#}");
        assert!(
            chain.contains("writable startup scan_state migration"),
            "error chain must identify writable startup scan_state migration: {chain}"
        );
    }

    #[test]
    fn legacy_missing_archive_cleanup_is_scoped_and_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute(
            "DELETE FROM app_settings WHERE key='legacy_missing_archive_cleanup_v2'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/artist')",
            [],
        )
        .unwrap();
        let log = r#"[{"status":"failed","reason":"source_missing"}]"#;
        for (target, status, reason_log) in [
            ("2024-01-02-tag", "manual_review", log),
            ("2024-01-tag", "manual_review", log),
            // Regression: an 8-byte "YYYY-MM-" basename must not index past the
            // buffer during startup cleanup.
            ("2024-01-", "manual_review", log),
            ("2024-01-02 tag", "manual_review", log),
            (
                "2024-01-02-other",
                "manual_review",
                r#"[{"reason":"target_exists"}]"#,
            ),
            ("2024-01-02-tag", "ready", log),
            ("", "manual_review", log),
            ("2024-01-02-invalid", "manual_review", "not-json"),
            (
                "2024/2024-01-02-latest",
                "manual_review",
                r#"[{"reason":"source_missing"},{"reason":"target_exists"}]"#,
            ),
        ] {
            conn.execute(
                "INSERT INTO folder_rename_plans (artist_id, source_folder, target_folder, status, execution_log) VALUES (1, ?, ?, ?, ?)",
                rusqlite::params![format!("source-{target}-{status}"), target, status, reason_log],
            ).unwrap();
        }
        cleanup_legacy_missing_archive_plans(&conn).unwrap();
        let remaining: i64 = conn
            .query_row("SELECT COUNT(*) FROM folder_rename_plans", [], |r| r.get(0))
            .unwrap();
        assert_eq!(remaining, 5);
        cleanup_legacy_missing_archive_plans(&conn).unwrap();
        let marker: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='legacy_missing_archive_cleanup_v2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(marker, "1");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM folder_rename_plans", [], |r| r
                .get(0))
                .unwrap(),
            5
        );
    }

    #[test]
    fn fresh_schema_creates_performance_indexes() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();

        for (table, index) in [
            ("artists", "idx_artists_missing"),
            ("items", "idx_items_hash_queue"),
            ("items", "idx_items_missing_at"),
            ("move_candidates", "idx_move_candidates_item_status"),
            ("scan_seen", "idx_scan_seen_hash"),
            ("scan_candidates", "idx_scan_candidates_hash_queue"),
            (
                "move_candidates",
                "idx_move_candidates_scan_candidate_status",
            ),
            ("move_history", "idx_move_history_status"),
            ("folder_rename_plans", "idx_folder_rename_artist_status"),
            ("tags", "idx_tags_artist"),
            ("item_tags", "idx_item_tags_tag"),
            ("character_references", "idx_character_references_character"),
        ] {
            let found: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name=?",
                    [index],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(found, 1, "missing {table}.{index}");
        }
    }

    /// The housekeeping probe is the one query that runs on every hash tick, so
    /// its plan has to be an index lookup rather than a scan of `items`.
    ///
    /// `EXPLAIN QUERY PLAN` is the check that actually distinguishes the two:
    /// the index existing is not the same as SQLite choosing it, and a scan here
    /// costs one pass over the whole library every 30 seconds.
    #[test]
    fn housekeeping_probe_uses_the_missing_item_index() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();

        let plan: Vec<String> = conn
            .prepare(
                "EXPLAIN QUERY PLAN
                 SELECT 1 FROM items i
                 WHERE i.missing=1
                   AND i.missing_at IS NOT NULL
                   AND i.missing_at <= ?
                   AND NOT EXISTS (
                       SELECT 1 FROM move_candidates mc
                       WHERE mc.item_id=i.id AND mc.status='pending'
                   )
                 LIMIT 1",
            )
            .unwrap()
            .query_map([1.0f64], |row| row.get::<_, String>(3))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        let joined = plan.join(" | ");
        assert!(
            joined.contains("idx_items_missing_at"),
            "the expired-missing probe must be answered by idx_items_missing_at: {joined}"
        );
        assert!(
            !joined.contains("SCAN items"),
            "the expired-missing probe must not scan items: {joined}"
        );
    }

    #[test]
    fn existing_character_references_gain_model_metadata_columns() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY);
             CREATE TABLE character_references (
               id INTEGER PRIMARY KEY, character_id INTEGER, embedding BLOB,
               embedding_dim INTEGER, source_type TEXT, item_id INTEGER, created_at REAL
             );",
        )
        .unwrap();

        ensure_product_schema(&conn, false).unwrap();

        let columns = conn
            .prepare("PRAGMA table_info(character_references)")
            .unwrap()
            .query_map([], |row| row.get::<_, String>(1))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        for column in [
            "embedding_model_repo_id",
            "embedding_model_variant",
            "embedding_model_file",
            "embedding_updated_at",
        ] {
            assert!(columns.iter().any(|candidate| candidate == column));
        }
    }

    #[test]
    fn fresh_schema_creates_cascading_item_favorites() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute("INSERT INTO artists (id,name,path) VALUES (1,'a','/a')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO items (id,artist_id,file_path,file_name) VALUES (1,1,'/a/1.jpg','1.jpg')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO item_favorites (item_id) VALUES (1)", [])
            .unwrap();
        conn.execute("DELETE FROM items WHERE id=1", []).unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_favorites", [], |row| row.get(0))
            .unwrap();
        assert_eq!(count, 0);
    }

    #[test]
    fn media_path_migration_merges_same_name_tags_across_virtual_and_real_artists() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (3, 1, 'rin')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (2, 1, '/real/pictures/artist/b.jpg', 'b.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (3, 2, '/real/pictures/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 3)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (3, 2)", [])
            .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        assert_eq!(result["merged_items"], 1);
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            1,
            "only the real-path artist row remains"
        );
        let tags: Vec<(i64, String)> = conn
            .prepare("SELECT id, name FROM tags ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tags,
            vec![(2, "miku".to_string()), (3, "rin".to_string())],
            "the union of tag names survives the same-name collision"
        );
        let links: Vec<(i64, i64)> = conn
            .prepare("SELECT item_id, tag_id FROM item_tags ORDER BY item_id, tag_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            links,
            vec![(3, 2), (3, 3)],
            "kept item carries both the target miku link and the merged rin link"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM items WHERE artist_id=1", [], |r| r
                .get(0),)
                .unwrap(),
            0,
            "no item stays on the dropped alias artist"
        );
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            !signature.is_empty(),
            "successful migration commits its signature"
        );
    }

    /// Regression: a configured root containing `_` (or differing in case)
    /// must not let prefix matching rewrite sibling directories.
    #[test]
    fn media_path_migration_underscore_root_leaves_sibling_paths_untouched() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/vol/my_pictures".into()],
            labels: vec!["my_pictures".into()],
            real_paths: vec!["/real/my_pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'a', '/vol/my_pictures/a')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/vol/my_pictures/a/x.jpg', 'x.jpg')",
            [],
        )
        .unwrap();
        // A `LIKE '/vol/my_pictures/%'` pattern would wrongly match these:
        // `_` is a single-char wildcard and LIKE folds ASCII case.
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 's', '/vol/myXpictures/s')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (2, 2, '/vol/myXpictures/s/y.jpg', 'y.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (3, 'c', '/vol/MY_PICTURES/c')",
            [],
        )
        .unwrap();

        normalize_configured_media_paths(&conn, &roots).unwrap();

        let paths: Vec<String> = conn
            .prepare("SELECT path FROM artists ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(paths[0], "/real/my_pictures/a", "in-root artist migrated");
        assert_eq!(paths[1], "/vol/myXpictures/s", "wildcard sibling untouched");
        assert_eq!(
            paths[2], "/vol/MY_PICTURES/c",
            "case-similar sibling untouched"
        );
        let item_path: String = conn
            .query_row("SELECT file_path FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(item_path, "/real/my_pictures/a/x.jpg");
        let sibling_item: String = conn
            .query_row("SELECT file_path FROM items WHERE id=2", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            sibling_item, "/vol/myXpictures/s/y.jpg",
            "sibling item untouched"
        );
    }

    #[test]
    fn media_path_migration_rolls_back_all_rows_when_reassignment_fails() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER inject_reassignment_failure
             BEFORE UPDATE ON tags
             BEGIN SELECT RAISE(ABORT, 'injected tag reassignment failure'); END;",
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();

        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        assert!(
            format!("{error:#}").contains("injected tag reassignment failure"),
            "error surfaces the failing reassignment: {error:#}"
        );

        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            2,
            "failure rolls back the alias artist deletion"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT artist_id FROM tags WHERE id=1", [], |r| r.get(0))
                .unwrap(),
            1,
            "failure rolls back the tag reassignment"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT artist_id FROM items WHERE id=1", [], |r| r.get(0))
                .unwrap(),
            1,
            "failure rolls back the item reassignment"
        );
        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "failed migration must not advance its signature"
        );
    }

    #[test]
    fn media_path_migration_preserves_artist_and_item_relations_on_merge() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE artist_references (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
                item_id INTEGER REFERENCES items(id) ON DELETE SET NULL,
                style_group TEXT NOT NULL DEFAULT '',
                dino_embedding BLOB,
                dino_embedding_dim INTEGER,
                wd14_embedding BLOB,
                wd14_embedding_dim INTEGER,
                embedding_model_variant TEXT NOT NULL DEFAULT '',
                embedding_updated_at REAL,
                created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
            );
            CREATE UNIQUE INDEX idx_artist_references_artist_item
                ON artist_references(artist_id, item_id);
            CREATE TABLE artist_suggestions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                artist_id INTEGER REFERENCES artists(id) ON DELETE SET NULL,
                status TEXT NOT NULL DEFAULT 'suggested',
                dino_score REAL,
                wd14_score REAL,
                fused_score REAL,
                matched_ref_id INTEGER,
                reason TEXT NOT NULL DEFAULT '',
                created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
                confirmed_at REAL,
                UNIQUE(item_id, artist_id)
            );
            CREATE TABLE artist_profile_links (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
                kind TEXT NOT NULL CHECK(kind IN ('social', 'subscription')),
                platform TEXT NOT NULL DEFAULT '',
                url TEXT NOT NULL,
                host TEXT NOT NULL DEFAULT '',
                created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
                updated_at REAL NOT NULL DEFAULT (strftime('%s','now')),
                UNIQUE(artist_id, kind, url)
            );
            CREATE TABLE character_recognition_results (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id INTEGER NOT NULL UNIQUE,
                character_id INTEGER,
                status TEXT NOT NULL,
                top_score REAL,
                second_score REAL,
                gap REAL,
                threshold REAL,
                reference_count INTEGER NOT NULL DEFAULT 0,
                checked_at REAL NOT NULL DEFAULT (strftime('%s','now')),
                error TEXT NOT NULL DEFAULT ''
            );
            "#,
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (3, 1, 'rin')",
            [],
        )
        .unwrap();
        for (id, artist_id, path, name) in [
            (1, 1, "/pictures1/artist/a.jpg", "a.jpg"),
            (2, 1, "/real/pictures/artist/b.jpg", "b.jpg"),
            (3, 2, "/real/pictures/artist/a.jpg", "a.jpg"),
            (4, 2, "/real/pictures/artist/c.jpg", "c.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (?, ?, ?, ?)",
                rusqlite::params![id, artist_id, path, name],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 3)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (3, 2)", [])
            .unwrap();
        conn.execute("INSERT INTO item_favorites (item_id) VALUES (1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_favorites (item_id) VALUES (3)", [])
            .unwrap();
        conn.execute("INSERT INTO characters (id, name) VALUES (10, 'miku')", [])
            .unwrap();
        conn.execute(
            "INSERT INTO character_references
             (id, character_id, embedding, embedding_dim, item_id)
             VALUES (1, 10, X'00', 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_references
             (id, character_id, embedding, embedding_dim, item_id)
             VALUES (2, 10, X'00', 1, 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_recognition_results
             (item_id, character_id, status, checked_at)
             VALUES (1, 10, 'matched', 100)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_recognition_results
             (item_id, character_id, status, checked_at)
             VALUES (3, 10, 'matched', 200)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO move_candidates
             (item_id, artist_id, old_path, new_path, reason)
             VALUES (1, 1, '/pictures1/artist/a.jpg', '/real/pictures/artist/a.jpg', 'test')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO move_history
             (item_id, artist_id, old_path, new_path, reason, status)
             VALUES (1, 1, '/pictures1/artist/a.jpg', '/real/pictures/artist/a.jpg', 'test', 'ok')",
            [],
        )
        .unwrap();
        // References: ar1's item merges onto item 3, then its artist merges.
        // ar2 collides with the target's ar2b on the artist merge; newest wins.
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (1, 1, 1, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (2, 1, 2, 2)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (3, 2, 2, 3)",
            [],
        )
        .unwrap();
        // Suggestions: s1 follows the merged item; s3's confirmed status
        // outranks the target's rejected s4 on the artist merge.
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status) VALUES (1, 1, 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status) VALUES (3, 2, 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status) VALUES (2, 1, 'confirmed')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status) VALUES (2, 2, 'rejected')",
            [],
        )
        .unwrap();
        // Profile links: p1 repoints, p2 duplicates the target's p3 and drops.
        conn.execute(
            "INSERT INTO artist_profile_links (artist_id, kind, url)
             VALUES (1, 'social', 'https://pixiv.net/users/1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_profile_links (artist_id, kind, url)
             VALUES (1, 'social', 'https://pixiv.net/users/2')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_profile_links (artist_id, kind, url)
             VALUES (2, 'social', 'https://pixiv.net/users/2')",
            [],
        )
        .unwrap();
        // Folder plans: fp1 duplicates fp2 on the same source folder and is a
        // proven duplicate once its tag selection remaps '[1]' to '[2]', so it
        // may be removed; fp3 repoints.
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (1, 1, '2024', '2024-01-02', 'ready', '[1]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (2, 2, '2024', '2024-01-02', 'ready', '[2]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (3, 1, '2025', '2025-06-07', 'ready', '[3]')",
            [],
        )
        .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        assert_eq!(result["merged_items"], 1);
        let artists: Vec<(i64, String)> = conn
            .prepare("SELECT id, path FROM artists ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(artists, vec![(2, "/real/pictures/artist".to_string())]);
        let tags: Vec<(i64, i64, String)> = conn
            .prepare("SELECT id, artist_id, name FROM tags ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            tags,
            vec![(2, 2, "miku".to_string()), (3, 2, "rin".to_string())]
        );
        let links: Vec<(i64, i64)> = conn
            .prepare("SELECT item_id, tag_id FROM item_tags ORDER BY item_id, tag_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(links, vec![(3, 2), (3, 3)]);
        let items: Vec<(i64, i64, String)> = conn
            .prepare("SELECT id, artist_id, file_path FROM items ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items,
            vec![
                (2, 2, "/real/pictures/artist/b.jpg".to_string()),
                (3, 2, "/real/pictures/artist/a.jpg".to_string()),
                (4, 2, "/real/pictures/artist/c.jpg".to_string()),
            ]
        );
        let favorite_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM item_favorites", [], |r| r.get(0))
            .unwrap();
        assert_eq!(favorite_count, 1, "favorites merged onto the kept item");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT item_id FROM item_favorites", [], |r| r.get(0),)
                .unwrap(),
            3
        );
        let refs: Vec<(i64, i64)> = conn
            .prepare("SELECT id, item_id FROM character_references ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            refs,
            vec![(1, 3), (2, 3)],
            "references repointed to the kept item"
        );
        let recognition: Vec<(i64, i64)> = conn
            .prepare("SELECT item_id, character_id FROM character_recognition_results")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            recognition,
            vec![(3, 10)],
            "the kept item's recognition result survives"
        );
        for table in ["move_candidates", "move_history"] {
            let table = sql_ident(table);
            let item_id: i64 = conn
                .query_row(&format!("SELECT item_id FROM {table}"), [], |r| r.get(0))
                .unwrap();
            assert_eq!(item_id, 3, "{table} repointed to the kept item");
        }
        let profile_links: Vec<(i64, String)> = conn
            .prepare("SELECT artist_id, url FROM artist_profile_links ORDER BY url")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            profile_links,
            vec![
                (2, "https://pixiv.net/users/1".to_string()),
                (2, "https://pixiv.net/users/2".to_string()),
            ],
            "unique profile links union onto the survivor, duplicates drop"
        );
        let references: Vec<(i64, i64)> = conn
            .prepare("SELECT artist_id, item_id FROM artist_references ORDER BY item_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            references,
            vec![(2, 2), (2, 3)],
            "references follow the kept item/artist; newest wins collisions"
        );
        let suggestions: Vec<(i64, i64, String)> = conn
            .prepare("SELECT item_id, artist_id, status FROM artist_suggestions ORDER BY item_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            suggestions,
            vec![
                (2, 2, "confirmed".to_string()),
                (3, 2, "pending".to_string()),
            ],
            "higher-status suggestion wins the collision"
        );
        let plans: Vec<(i64, String, String)> = conn
            .prepare("SELECT artist_id, source_folder, selected_tag_ids FROM folder_rename_plans ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            plans,
            vec![
                (2, "2024".to_string(), "[2]".to_string()),
                (2, "2025".to_string(), "[3]".to_string()),
            ],
            "proven duplicate folder plans collapse, the rest repoint"
        );
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }

    #[test]
    fn read_probe_failure_is_propagated_and_signature_does_not_advance() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Read {
                    table_name: "artists",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);

        assert!(
            format!("{error:#}").contains("count virtual paths in artists.path"),
            "probe failure must surface with context: {error:#}"
        );
        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "failed probe must not advance the signature"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            1,
            "no migration writes happen before the transaction"
        );
    }

    #[test]
    fn in_transaction_read_failure_rolls_back_merges_without_advancing_signature() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (2, 2, '/real/pictures/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (2, 2)", [])
            .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        // Deny reading item rows: the merge branch has already rewritten tags
        // when the item repoint suddenly fails, so rollback must restore them.
        conn.authorizer(Some(move |ctx: AuthContext<'_>| {
            if matches!(
                ctx.action,
                AuthAction::Read {
                    table_name: "items",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }));
        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        conn.authorizer(None::<fn(AuthContext<'_>) -> Authorization>);

        assert!(
            !error.to_string().is_empty(),
            "the denied in-transaction read must surface as a real error"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            2,
            "rollback keeps both artists"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT artist_id FROM tags WHERE id=1", [], |r| r.get(0))
                .unwrap(),
            1,
            "rollback restores the alias tag onto the alias artist"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT artist_id FROM items WHERE id=1", [], |r| r.get(0))
                .unwrap(),
            1,
            "rollback keeps the alias item on the alias artist"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM item_tags", [], |r| r.get(0))
                .unwrap(),
            2,
            "rollback keeps item_tags untouched"
        );
        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "failed in-transaction migration must not advance its signature"
        );
    }

    #[test]
    fn wrong_typed_migration_marker_errors_instead_of_advancing() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at)
             VALUES ('media_path_real_migration_signature', X'2A', strftime('%s','now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (2, 2, '/real/pictures/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };

        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        assert!(
            format!("{error:#}").contains("read media_path_real_migration_signature marker"),
            "wrong-typed marker read must surface: {error:#}"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            2,
            "a wrong-typed marker must not trigger any migration writes"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM items", [], |r| r.get(0))
                .unwrap(),
            2,
            "items stay untouched"
        );
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            vec![42],
            "the wrong-typed marker row itself is untouched"
        );
    }

    #[test]
    fn wrong_typed_cleanup_marker_errors_without_deleting_plans() {
        let conn = Connection::open_in_memory().unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute(
            "DELETE FROM app_settings WHERE key='legacy_missing_archive_cleanup_v2'",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at)
             VALUES ('legacy_missing_archive_cleanup_v2', X'2A', strftime('%s','now'))",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans
             (artist_id, source_folder, target_folder, status, execution_log)
             VALUES (1, 'source', '2024-01-02-tag', 'manual_review',
                     '[{\"status\":\"failed\",\"reason\":\"source_missing\"}]')",
            [],
        )
        .unwrap();

        let error = cleanup_legacy_missing_archive_plans(&conn).unwrap_err();
        assert!(
            format!("{error:#}").contains("read legacy_missing_archive_cleanup_v2 marker"),
            "wrong-typed cleanup marker read must surface: {error:#}"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM folder_rename_plans", [], |r| r
                .get(0))
                .unwrap(),
            1,
            "no legacy plan is deleted when the marker read fails"
        );
        let stored: Vec<u8> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='legacy_missing_archive_cleanup_v2'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            stored,
            vec![42],
            "the wrong-typed cleanup marker row is untouched"
        );
    }

    #[test]
    fn media_path_migration_resolves_bulk_item_conflicts_and_keeps_foreign_keys_clean() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'other', '/real/pictures/other')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (10, 2, 'tag')",
            [],
        )
        .unwrap();
        for (id, artist_id, path, missing) in [
            (1, 1, "/pictures1/artist/dup.jpg", 0),
            (2, 1, "/pictures1/artist/unique.jpg", 0),
            (3, 2, "/real/pictures/artist/dup.jpg", 1),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name, missing)
                 VALUES (?, ?, ?, ?, ?)",
                rusqlite::params![
                    id,
                    artist_id,
                    path,
                    path.rsplit('/').next().unwrap(),
                    missing
                ],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 10)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (3, 10)", [])
            .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 0);
        assert_eq!(result["merged_items"], 1);
        let items: Vec<(i64, i64, String, i64)> = conn
            .prepare("SELECT id, artist_id, file_path, missing FROM items ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items,
            vec![
                (2, 1, "/real/pictures/artist/unique.jpg".to_string(), 0),
                (3, 2, "/real/pictures/artist/dup.jpg".to_string(), 0),
            ],
            "the virtual duplicate merges into the real row; a missing target becomes active"
        );
        let links: Vec<(i64, i64)> = conn
            .prepare("SELECT item_id, tag_id FROM item_tags ORDER BY item_id, tag_id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            links,
            vec![(3, 10)],
            "kept item carries the merged tag link"
        );
        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0, "merge leaves no foreign key violations");
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }

    #[test]
    fn media_path_migration_works_with_optional_tables_absent() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (
                id INTEGER PRIMARY KEY AUTOINCREMENT, name TEXT NOT NULL, path TEXT UNIQUE NOT NULL
             );
             CREATE TABLE items (
                id INTEGER PRIMARY KEY AUTOINCREMENT, artist_id INTEGER NOT NULL,
                file_path TEXT UNIQUE NOT NULL, file_name TEXT NOT NULL
             );
             CREATE TABLE tags (
                id INTEGER PRIMARY KEY AUTOINCREMENT, artist_id INTEGER NOT NULL,
                name TEXT NOT NULL, UNIQUE(artist_id, name)
             );
             CREATE TABLE item_tags (
                item_id INTEGER NOT NULL, tag_id INTEGER NOT NULL,
                PRIMARY KEY(item_id, tag_id)
             );
             CREATE TABLE app_settings (
                key TEXT PRIMARY KEY, value TEXT NOT NULL,
                updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
             );",
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (3, 1, 'rin')",
            [],
        )
        .unwrap();
        for (id, artist_id, path, name) in [
            (1, 1, "/pictures1/artist/a.jpg", "a.jpg"),
            (2, 2, "/real/pictures/artist/a.jpg", "a.jpg"),
            (3, 1, "/pictures1/artist/d.jpg", "d.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (?, ?, ?, ?)",
                rusqlite::params![id, artist_id, path, name],
            )
            .unwrap();
        }
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 3)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (2, 2)", [])
            .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        assert_eq!(result["merged_items"], 1);
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            1,
            "a minimal legacy schema still survives the merge"
        );
        let items: Vec<(i64, i64, String)> = conn
            .prepare("SELECT id, artist_id, file_path FROM items ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items,
            vec![
                (2, 2, "/real/pictures/artist/a.jpg".to_string()),
                (3, 2, "/real/pictures/artist/d.jpg".to_string()),
            ]
        );
        let tags: Vec<(i64, i64)> = conn
            .prepare("SELECT id, artist_id FROM tags ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(tags, vec![(2, 2), (3, 2)]);
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }

    #[test]
    fn media_path_migration_preserves_null_references_and_suggestions_with_matched_ref_remap() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        conn.execute_batch(
            r#"
            CREATE TABLE artist_references (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
                item_id INTEGER REFERENCES items(id) ON DELETE SET NULL,
                style_group TEXT NOT NULL DEFAULT '',
                dino_embedding BLOB,
                dino_embedding_dim INTEGER,
                wd14_embedding BLOB,
                wd14_embedding_dim INTEGER,
                embedding_model_variant TEXT NOT NULL DEFAULT '',
                embedding_updated_at REAL,
                created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
            );
            CREATE UNIQUE INDEX idx_artist_references_artist_item
                ON artist_references(artist_id, item_id);
            CREATE TABLE artist_suggestions (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                item_id INTEGER NOT NULL REFERENCES items(id) ON DELETE CASCADE,
                artist_id INTEGER REFERENCES artists(id) ON DELETE SET NULL,
                status TEXT NOT NULL DEFAULT 'suggested',
                dino_score REAL,
                wd14_score REAL,
                fused_score REAL,
                matched_ref_id INTEGER,
                reason TEXT NOT NULL DEFAULT '',
                created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
                confirmed_at REAL,
                UNIQUE(item_id, artist_id)
            );
            "#,
        )
        .unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        for (id, artist_id, path, name) in [
            (1, 1, "/pictures1/artist/a.jpg", "a.jpg"),
            (2, 2, "/real/pictures/artist/a.jpg", "a.jpg"),
            (3, 1, "/pictures1/artist/d.jpg", "d.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (?, ?, ?, ?)",
                rusqlite::params![id, artist_id, path, name],
            )
            .unwrap();
        }
        // r1/r2 are detached references: NULL item_id is not part of the unique
        // pair, so they must survive the artist merge by plain reassignment.
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (1, 1, NULL, 1)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (2, 2, NULL, 2)",
            [],
        )
        .unwrap();
        // r3 loses to r5 on the item merge (newest wins), r5 then beats r6 on
        // the artist merge and copies created_at onto the surviving row.
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (3, 1, 1, 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (5, 1, 2, 7)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (6, 2, 2, 5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_references (id, artist_id, item_id, created_at)
             VALUES (7, 2, 3, 8)",
            [],
        )
        .unwrap();
        // su1 is an orphaned NULL-artist suggestion on the merged item: it must
        // follow the item and then the artist, and its matched_ref chains
        // r3 -> r5 -> r6 as each loser disappears. su2 (NULL artist on the kept
        // item) and su4 (target coordinate) must stay.
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status, matched_ref_id)
             VALUES (1, NULL, 'pending', 3)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status)
             VALUES (2, NULL, 'pending')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status, matched_ref_id)
             VALUES (2, 1, 'confirmed', 5)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_suggestions (item_id, artist_id, status)
             VALUES (2, 2, 'pending')",
            [],
        )
        .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        assert_eq!(result["merged_items"], 1);
        let references: Vec<(i64, i64, Option<i64>, f64)> = conn
            .prepare("SELECT id, artist_id, item_id, created_at FROM artist_references ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            references,
            vec![
                (1, 2, None, 1.0),
                (2, 2, None, 2.0),
                (6, 2, Some(2), 7.0),
                (7, 2, Some(3), 8.0),
            ],
            "detached NULL-item references survive; the newest winner copies created_at"
        );
        #[allow(clippy::type_complexity)]
        let suggestions: Vec<(i64, i64, Option<i64>, String, Option<i64>)> = conn
            .prepare(
                "SELECT id, item_id, artist_id, status, matched_ref_id
                 FROM artist_suggestions ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            suggestions,
            vec![
                (1, 2, None, "pending".to_string(), Some(6)),
                (2, 2, None, "pending".to_string(), None),
                (4, 2, Some(2), "confirmed".to_string(), Some(6)),
            ],
            "NULL-artist suggestions follow the merged item; matched_ref follows winners"
        );
        let dangling: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artist_suggestions
                 WHERE matched_ref_id IS NOT NULL
                   AND matched_ref_id NOT IN (SELECT id FROM artist_references)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            dangling, 0,
            "every surviving matched_ref points at a live reference"
        );
        let items: Vec<(i64, i64, String)> = conn
            .prepare("SELECT id, artist_id, file_path FROM items ORDER BY id")
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            items,
            vec![
                (2, 2, "/real/pictures/artist/a.jpg".to_string()),
                (3, 2, "/real/pictures/artist/d.jpg".to_string()),
            ]
        );
    }

    #[test]
    fn media_path_migration_resolves_link_documents_and_occurrences_on_merge() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        crate::link_index::ensure_link_schema(&conn).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        for (id, artist_id, path, name) in [
            (1, 1, "/pictures1/artist/a.jpg", "a.jpg"),
            (2, 2, "/real/pictures/artist/a.jpg", "a.jpg"),
            (3, 1, "/pictures1/artist/b.jpg", "b.jpg"),
            (6, 1, "/pictures1/artist/g.jpg", "g.jpg"),
            (7, 2, "/real/pictures/artist/g.jpg", "g.jpg"),
        ] {
            conn.execute(
                "INSERT INTO items (id, artist_id, file_path, file_name) VALUES (?, ?, ?, ?)",
                rusqlite::params![id, artist_id, path, name],
            )
            .unwrap();
        }
        // doc1 loses to doc2 when item 1 merges into item 2 (occurrences
        // cascade); doc4 has no target document so it repoints onto item 7;
        // doc3 follows the artist-only move of item 3 and its path is bulk
        // rewritten.
        for (doc_id, artist_id, item_id, path) in [
            (1, 1, 1, "/pictures1/artist/a.jpg"),
            (2, 2, 2, "/real/pictures/artist/a.jpg"),
            (3, 1, 3, "/pictures1/artist/b.jpg"),
            (4, 1, 6, "/pictures1/artist/g.jpg"),
        ] {
            conn.execute(
                "INSERT INTO artist_link_documents
                 (id, artist_id, item_id, file_path, file_name, file_kind, parse_status, link_count)
                 VALUES (?, ?, ?, ?, ?, 'html', 'done', 1)",
                rusqlite::params![
                    doc_id,
                    artist_id,
                    item_id,
                    path,
                    path.rsplit('/').next().unwrap()
                ],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO artist_link_occurrences (document_id, normalized_url, raw_url, host)
                 VALUES (?, 'https://example.com/x', 'https://example.com/x', 'example.com')",
                [doc_id],
            )
            .unwrap();
        }

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_items"], 2);
        let documents: Vec<(i64, i64, i64, String)> = conn
            .prepare(
                "SELECT id, artist_id, item_id, file_path FROM artist_link_documents ORDER BY id",
            )
            .unwrap()
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            documents,
            vec![
                (2, 2, 2, "/real/pictures/artist/a.jpg".to_string()),
                (3, 2, 3, "/real/pictures/artist/b.jpg".to_string()),
                (4, 2, 7, "/real/pictures/artist/g.jpg".to_string()),
            ],
            "superseded documents collapse, survivors follow their items"
        );
        let occurrences: Vec<i64> = conn
            .prepare("SELECT document_id FROM artist_link_occurrences ORDER BY document_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            occurrences,
            vec![2, 3, 4],
            "occurrences cascade with their document"
        );
        let disagreeing: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM artist_link_documents d
                 JOIN items i ON i.id = d.item_id
                 WHERE d.artist_id <> i.artist_id OR d.file_path <> i.file_path",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            disagreeing, 0,
            "document (item, artist, path) never disagrees"
        );
    }

    #[test]
    fn media_path_migration_refuses_conflicting_folder_plans_and_records_stay_touched() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (1, 1, '2024', '2024-01-02', 'ready', '[1]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, target_folder, status, selected_tag_ids)
             VALUES (2, 2, '2024', '2024-01-02', 'executed', '[2]')",
            [],
        )
        .unwrap();

        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        assert!(
            format!("{error:#}").contains("folder plan conflict"),
            "conflicting plans must abort the migration: {error:#}"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            2,
            "conflict rolls back the alias artist deletion"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT artist_id FROM tags WHERE id=1", [], |r| r.get(0))
                .unwrap(),
            1,
            "conflict rolls back the tag fold"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>(
                "SELECT artist_id FROM folder_rename_plans WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            1,
            "conflict rolls back the plan remap"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT selected_tag_ids FROM folder_rename_plans WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            "[1]",
            "conflict rolls back the selected_tag_ids remap"
        );
        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "conflicted migration must not advance its signature"
        );
    }

    #[test]
    fn media_path_migration_refuses_malformed_selection_on_affected_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, selected_tag_ids)
             VALUES (1, 1, '2024', 'not-json')",
            [],
        )
        .unwrap();

        let error = normalize_configured_media_paths(&conn, &roots).unwrap_err();
        assert!(
            format!("{error:#}").contains("parse selected_tag_ids of folder plan 1"),
            "malformed selection on an affected plan must abort: {error:#}"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT selected_tag_ids FROM folder_rename_plans WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            "not-json",
            "the malformed plan row itself is untouched"
        );
        let signature: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .ok();
        assert!(
            signature.is_none(),
            "malformed plan must not advance the signature"
        );
    }

    #[test]
    fn media_path_migration_ignores_malformed_json_on_unrelated_plan() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/pictures1/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (3, 'other', '/pictures1/other')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, selected_tag_ids)
             VALUES (1, 1, '2024', '[1]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO folder_rename_plans (id, artist_id, source_folder, selected_tag_ids)
             VALUES (2, 3, '2025', 'bad-json')",
            [],
        )
        .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        let plan_artists: Vec<i64> = conn
            .prepare("SELECT artist_id FROM folder_rename_plans ORDER BY id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            plan_artists,
            vec![2, 3],
            "the affected plan repoints; the unrelated plan is never read"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT selected_tag_ids FROM folder_rename_plans WHERE id=2",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            "bad-json",
            "unrelated malformed JSON stays untouched"
        );
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }

    #[test]
    fn media_path_migration_keeps_recycle_restore_path_valid_under_merged_artist() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("pictures");
        let media_dir = real_root.join("Artist");
        std::fs::create_dir_all(&media_dir).unwrap();
        let original = media_dir.join("a.jpg");
        std::fs::write(&original, b"original").unwrap();
        let real_forward = real_root.to_string_lossy().replace('\\', "/");
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec![real_forward.clone()],
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        crate::recycle::ensure_recycle_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures1/Artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (2, 'Artist', ?)",
            [format!("{real_forward}/Artist")],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (1, 1, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (2, 2, 'miku')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO tags (id, artist_id, name) VALUES (3, 1, 'rin')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/Artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 3)", [])
            .unwrap();
        conn.execute("INSERT INTO item_favorites (item_id) VALUES (1)", [])
            .unwrap();

        crate::delete_item_to_recycle(&conn, "/pictures1/Artist/a.jpg", &roots).unwrap();
        let entry_id: i64 = conn
            .query_row("SELECT id FROM recycle_entries ORDER BY id DESC", [], |r| {
                r.get(0)
            })
            .unwrap();
        normalize_configured_media_paths(&conn, &roots).unwrap();
        crate::recycle::restore_recycle_entry(&conn, &roots, entry_id).unwrap();

        assert_eq!(std::fs::read(&original).unwrap(), b"original");
        let (status, restored_at): (String, Option<f64>) = conn
            .query_row(
                "SELECT status, restored_at FROM recycle_entries WHERE id=?",
                [entry_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "restored");
        assert!(restored_at.is_some(), "restore stamps restored_at");
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM items", [], |r| r.get(0))
                .unwrap(),
            1,
            "the recycled item comes back"
        );
        let (artist_id, file_path): (i64, String) = conn
            .query_row("SELECT artist_id, file_path FROM items", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(artist_id, 2, "the item restores under the surviving artist");
        assert_eq!(
            file_path,
            format!("{real_forward}/Artist/a.jpg"),
            "the restored row stores the resolved real authorized path"
        );
        let linked_tags: Vec<i64> = conn
            .prepare("SELECT tag_id FROM item_tags ORDER BY tag_id")
            .unwrap()
            .query_map([], |r| r.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            linked_tags,
            vec![2, 3],
            "tag snapshot remaps through the folded tag alias"
        );
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM item_favorites", [], |r| r.get(0))
                .unwrap(),
            1,
            "favorite snapshot restores"
        );
    }

    #[test]
    fn media_path_migration_activates_missing_survivors_and_keeps_item_on_surviving_artist() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        crate::link_index::ensure_link_schema(&conn).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path, missing, missing_at)
             VALUES (1, 'artist', '/pictures1/artist', 0, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing, missing_at)
             VALUES (2, 'artist', '/real/pictures/artist', 1, 111)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path, missing, missing_at)
             VALUES (3, 'third', '/real/pictures/third', 0, NULL)",
            [],
        )
        .unwrap();
        // The kept real-path row lives under a third artist; the artist-scoped
        // merge must move it onto the surviving artist before link-document
        // coordinates are derived from it.
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing, missing_at)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg', 0, NULL)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing, missing_at)
             VALUES (2, 3, '/real/pictures/artist/a.jpg', 'a.jpg', 1, 222)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_link_documents
             (artist_id, item_id, file_path, file_name, file_kind, parse_status, link_count)
             VALUES (1, 1, '/pictures1/artist/a.jpg', 'a.jpg', 'html', 'done', 1)",
            [],
        )
        .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert_eq!(result["merged_artists"], 1);
        assert_eq!(result["merged_items"], 1);
        assert_eq!(
            conn.query_row::<i64, _, _>("SELECT COUNT(*) FROM artists", [], |r| r.get(0))
                .unwrap(),
            2,
            "the alias artist is gone"
        );
        let (missing, missing_at): (i64, Option<f64>) = conn
            .query_row(
                "SELECT missing, missing_at FROM artists WHERE id=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (missing, missing_at),
            (0, None),
            "the active source artist makes the surviving artist active"
        );
        let (artist_id, file_path, missing, missing_at): (i64, String, i64, Option<f64>) = conn
            .query_row(
                "SELECT artist_id, file_path, missing, missing_at FROM items WHERE id=2",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(
            (artist_id, file_path.as_str(), missing, missing_at),
            (2, "/real/pictures/artist/a.jpg", 0, None,),
            "the kept item belongs to the surviving artist and is active"
        );
        let (doc_artist, doc_item, doc_path): (i64, i64, String) = conn
            .query_row(
                "SELECT artist_id, item_id, file_path FROM artist_link_documents",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .unwrap();
        assert_eq!(
            (doc_artist, doc_item, doc_path.as_str()),
            (2, 2, "/real/pictures/artist/a.jpg"),
            "the repointed link document agrees with the kept item"
        );
        let violations: i64 = conn
            .query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(violations, 0, "merge leaves no foreign key violations");
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }

    #[test]
    fn restore_recycle_entry_normalizes_legacy_virtual_snapshot_path_without_collision() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let real_root = dir.path().join("pictures");
        let media_dir = real_root.join("Artist");
        std::fs::create_dir_all(&media_dir).unwrap();
        let original = media_dir.join("a.jpg");
        std::fs::write(&original, b"original").unwrap();
        let real_forward = real_root.to_string_lossy().replace('\\', "/");
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec![real_forward.clone()],
        };
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        crate::recycle::ensure_recycle_schema(&conn).unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures1/Artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/pictures1/Artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();

        crate::delete_item_to_recycle(&conn, "/pictures1/Artist/a.jpg", &roots).unwrap();
        let entry_id: i64 = conn
            .query_row("SELECT id FROM recycle_entries", [], |row| row.get(0))
            .unwrap();
        crate::recycle::restore_recycle_entry(&conn, &roots, entry_id).unwrap();

        let (artist_id, file_path): (i64, String) = conn
            .query_row("SELECT artist_id, file_path FROM items", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert_eq!(artist_id, 1);
        assert_eq!(
            file_path,
            format!("{real_forward}/Artist/a.jpg"),
            "restore itself normalizes a legacy virtual snapshot path"
        );
        let (status, restore_path): (String, String) = conn
            .query_row(
                "SELECT status, restore_path FROM recycle_entries WHERE id=?",
                [entry_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(status, "restored");
        assert_eq!(
            restore_path,
            format!("{real_forward}/Artist/a.jpg"),
            "the recorded restore path is the real authorized path"
        );
    }

    #[test]
    fn media_path_migration_rewrites_stale_virtual_link_document_paths() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch("PRAGMA foreign_keys=ON;").unwrap();
        ensure_product_schema(&conn, true).unwrap();
        crate::link_index::ensure_link_schema(&conn).unwrap();
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["pictures1".into()],
            real_paths: vec!["/real/pictures".into()],
        };
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'artist', '/real/pictures/artist')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name)
             VALUES (1, 1, '/real/pictures/artist/a.jpg', 'a.jpg')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artist_link_documents
             (artist_id, item_id, file_path, file_name, file_kind, parse_status, link_count)
             VALUES (1, 1, '/pictures1/artist/stale.html', 'stale.html', 'html', 'done', 0)",
            [],
        )
        .unwrap();

        let result = normalize_configured_media_paths(&conn, &roots).unwrap();

        assert!(
            result["updated"].as_i64().unwrap_or(0) >= 1,
            "the stale virtual document alone must trigger the migration"
        );
        assert_eq!(
            conn.query_row::<String, _, _>(
                "SELECT file_path FROM artist_link_documents WHERE id=1",
                [],
                |r| r.get(0),
            )
            .unwrap(),
            "/real/pictures/artist/stale.html",
            "the link document path is rewritten before the signature commits"
        );
        let signature: String = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(!signature.is_empty(), "migration commits its signature");
    }
}
