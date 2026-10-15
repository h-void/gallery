use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::Value;

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
    // Housekeeping (every hash tick) asks "are there expired missing items
    // with no pending move candidate", which needs these two: the existing
    // items index leads with artist_id and the existing move_candidates
    // indexes lead with status or scan_candidate_id, so the probe and its
    // NOT EXISTS subquery both fell back to a full scan of items plus one
    // lookups per row. They must be created on every writable startup, not
    // only fresh databases: a library migrated from the legacy backend (or
    // from an older package) never takes the fresh-database path again, and
    // without this batch its probe degrades to a per-tick full items scan.
    // Both are partial/narrow indexes, so the one-time build on a migrated
    // library stays cheap.
    conn.execute_batch(
        r#"
        CREATE INDEX IF NOT EXISTS idx_items_missing_at
            ON items(missing, missing_at) WHERE missing_at IS NOT NULL;
        CREATE INDEX IF NOT EXISTS idx_move_candidates_item_status
            ON move_candidates(item_id, status);
        "#,
    )?;
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

