use std::collections::HashMap;
use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use serde_json::{json, Value};

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
    conns: Mutex<Vec<Connection>>,
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
        let size = config.pool_size.max(1).min(MAX_POOL_SIZE);
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
            // A legacy database keeps its old scan_state shape (no stable
            // per-run scan_id) until some scan happens to write it. Migrate
            // at writable startup so health, /api/scan/state, WebSocket
            // polling, and idle workers can read the schema immediately.
            crate::scan::ensure_scan_state(&conns[0])
                .context("writable startup scan_state migration failed")?;
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
            conns: Mutex::new(conns),
        })
    }

    pub fn config(&self) -> DbConfig {
        self.config
    }

    pub fn get(self: &Arc<Self>) -> Result<PooledConn> {
        let conn = self
            .conns
            .lock()
            .map_err(|_| anyhow!("db pool mutex poisoned"))?
            .pop();
        let conn = match conn {
            Some(conn) => conn,
            None => open_db(&self.db_path, self.config.read_only)?,
        };
        Ok(PooledConn {
            pool: Arc::clone(self),
            conn: Some(conn),
        })
    }
}
impl std::ops::Deref for PooledConn {
    type Target = Connection;

    fn deref(&self) -> &Self::Target {
        self.conn.as_ref().expect("pooled connection missing")
    }
}

impl Drop for PooledConn {
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            if let Ok(mut conns) = self.pool.conns.lock() {
                // Do not grow the pool unbounded beyond configured size.
                if conns.len() < self.pool.config.pool_size {
                    conns.push(conn);
                }
            }
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

fn open_writable_db(path: &Path) -> Result<Connection> {
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
            scanned_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
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
        let legacy_format = year_month && (year_month_day || bytes[8] != b' ');
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

fn media_path_migration_signature(roots: &MediaRoots) -> String {
    let roots_n: Vec<String> = roots
        .roots
        .iter()
        .map(|r| r.replace('\\', "/").trim_end_matches('/').to_string())
        .collect();
    let reals_n: Vec<String> = roots
        .real_paths
        .iter()
        .map(|r| r.replace('\\', "/").trim_end_matches('/').to_string())
        .collect();
    json!({"roots": roots_n, "real_paths": reals_n}).to_string()
}

fn has_virtual_paths(conn: &Connection, roots: &MediaRoots) -> Result<bool> {
    let columns = [
        ("artists", "path"),
        ("items", "file_path"),
        ("artist_link_documents", "file_path"),
        ("scan_seen", "file_path"),
        ("scan_candidates", "file_path"),
        ("move_candidates", "old_path"),
        ("move_candidates", "new_path"),
        ("move_history", "old_path"),
        ("move_history", "new_path"),
    ];
    for (table, column) in columns {
        let exists: i64 = conn
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .with_context(|| format!("probe presence of table {table}"))?;
        if exists == 0 {
            continue;
        }
        for root in &roots.roots {
            let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
            if root_n.is_empty() {
                continue;
            }
            // Skip when virtual root already equals real root (no alias).
            let idx = roots.roots.iter().position(|r| r == root).unwrap_or(0);
            if roots
                .real_root_at(idx)
                .map(|r| r.replace('\\', "/").trim_end_matches('/') == root_n.as_str())
                == Some(true)
            {
                continue;
            }
            let hit: i64 = conn
                .query_row(
                    &format!(
                        "SELECT COUNT(1) FROM {table} WHERE {column}=? OR {column} LIKE ? LIMIT 1"
                    ),
                    rusqlite::params![&root_n, format!("{root_n}/%")],
                    |r| r.get(0),
                )
                .with_context(|| format!("count virtual paths in {table}.{column}"))?;
            if hit > 0 {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn set_migration_signature(conn: &Connection, signature: &str) -> Result<()> {
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at)
         VALUES ('media_path_real_migration_signature', ?, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value=excluded.value, updated_at=excluded.updated_at",
        rusqlite::params![signature],
    )?;
    Ok(())
}

/// Probe whether a table exists without creating it. A failed probe is an
/// error, distinct from a genuinely absent optional table.
fn table_exists(conn: &Connection, name: &str) -> Result<bool> {
    let count: i64 = conn
        .query_row(
            "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
            rusqlite::params![name],
            |row| row.get(0),
        )
        .with_context(|| format!("probe schema table {name}"))?;
    Ok(count > 0)
}

#[derive(Clone, Debug)]
struct ArtistReferenceMergeRow {
    id: i64,
    artist_id: i64,
    item_id: Option<i64>,
    style_group: String,
    dino_embedding: Option<Vec<u8>>,
    dino_embedding_dim: Option<i64>,
    wd14_embedding: Option<Vec<u8>>,
    wd14_embedding_dim: Option<i64>,
    embedding_model_variant: String,
    embedding_updated_at: Option<f64>,
    created_at: f64,
}

const ARTIST_REFERENCE_COLUMNS: &str = "id, artist_id, item_id, style_group, dino_embedding, \
     dino_embedding_dim, wd14_embedding, wd14_embedding_dim, embedding_model_variant, \
     embedding_updated_at, created_at";

/// Column names of a table that is known to exist (PRAGMA table_info).
fn table_columns(conn: &Connection, name: &str) -> Result<Vec<String>> {
    let columns = conn
        .prepare(&format!("PRAGMA table_info({name})"))?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .with_context(|| format!("probe columns of {name}"))?;
    Ok(columns)
}

/// Whether a known table carries the given column; missing table yields false.
fn column_exists(conn: &Connection, table: &str, column: &str) -> Result<bool> {
    Ok(table_columns(conn, table)?
        .iter()
        .any(|candidate| candidate == column))
}

fn write_artist_reference_row(
    conn: &Connection,
    target_row_id: i64,
    row: &ArtistReferenceMergeRow,
) -> Result<()> {
    conn.execute(
        "UPDATE artist_references
         SET style_group=?, dino_embedding=?, dino_embedding_dim=?, wd14_embedding=?,
             wd14_embedding_dim=?, embedding_model_variant=?, embedding_updated_at=?,
             created_at=?
         WHERE id=?",
        rusqlite::params![
            row.style_group,
            row.dino_embedding,
            row.dino_embedding_dim,
            row.wd14_embedding,
            row.wd14_embedding_dim,
            row.embedding_model_variant,
            row.embedding_updated_at,
            row.created_at,
            target_row_id
        ],
    )
    .with_context(|| {
        format!(
            "merge artist reference {} onto reference {target_row_id}",
            row.id
        )
    })?;
    Ok(())
}

/// Before a losing reference is deleted, remap every
/// `artist_suggestions.matched_ref_id` pointing at it to the surviving
/// reference so restore/regeneration stays valid (optional column).
fn remap_artist_suggestion_matched_refs(
    conn: &Connection,
    loser_ref_id: i64,
    surviving_ref_id: i64,
) -> Result<()> {
    if table_exists(conn, "artist_suggestions")?
        && column_exists(conn, "artist_suggestions", "matched_ref_id")?
    {
        conn.execute(
            "UPDATE artist_suggestions SET matched_ref_id=? WHERE matched_ref_id=?",
            rusqlite::params![surviving_ref_id, loser_ref_id],
        )
        .with_context(|| {
            format!(
                "repoint suggestion matched_ref_id from reference {loser_ref_id} to {surviving_ref_id}"
            )
        })?;
    }
    Ok(())
}

/// Move `artist_references` rows so one coordinate (item or artist) changes to
/// a value that already exists elsewhere; deterministic newest row wins on
/// UNIQUE(artist_id, item_id) collisions, the loser is removed.
fn repoint_artist_references(
    conn: &Connection,
    move_by_item: Option<(i64, i64)>,
    move_by_artist: Option<(i64, i64)>,
) -> Result<()> {
    let (source_id, target_id, by_item) = match (move_by_item, move_by_artist) {
        (Some((source_item, keep_item)), None) => (source_item, keep_item, true),
        (None, Some((source_artist, target_artist))) => (source_artist, target_artist, false),
        _ => unreachable!("repoint_artist_references takes exactly one coordinate"),
    };
    let column = if by_item { "item_id" } else { "artist_id" };
    let mut stmt = conn.prepare(&format!(
        "SELECT {ARTIST_REFERENCE_COLUMNS} FROM artist_references WHERE {column}=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_id], |row| {
            Ok(ArtistReferenceMergeRow {
                id: row.get(0)?,
                artist_id: row.get(1)?,
                item_id: row.get(2)?,
                style_group: row.get(3)?,
                dino_embedding: row.get(4)?,
                dino_embedding_dim: row.get(5)?,
                wd14_embedding: row.get(6)?,
                wd14_embedding_dim: row.get(7)?,
                embedding_model_variant: row.get(8)?,
                embedding_updated_at: row.get(9)?,
                created_at: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for row in rows {
        if !by_item && row.item_id.is_none() {
            // Detached references are not unique per artist (NULL item_id),
            // so they can never collide: plain artist reassignment.
            conn.execute(
                "UPDATE artist_references SET artist_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint detached artist reference {} artist", row.id))?;
            continue;
        }
        let collision: Option<(i64, f64)> = if by_item {
            conn.query_row(
                "SELECT id, created_at FROM artist_references
                 WHERE artist_id=? AND item_id=? AND id<>?",
                rusqlite::params![row.artist_id, target_id, row.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist reference collision for item {target_id}"))?
        } else {
            conn.query_row(
                "SELECT id, created_at FROM artist_references
                 WHERE artist_id=? AND item_id=? AND id<>?",
                rusqlite::params![target_id, row.item_id.unwrap(), row.id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist reference collision for artist {target_id}"))?
        };
        if let Some((target_row_id, target_created)) = collision {
            if (row.created_at, row.id) > (target_created, target_row_id) {
                write_artist_reference_row(conn, target_row_id, &row)?;
            }
            remap_artist_suggestion_matched_refs(conn, row.id, target_row_id)?;
            conn.execute(
                "DELETE FROM artist_references WHERE id=?",
                rusqlite::params![row.id],
            )
            .with_context(|| format!("drop losing artist reference {}", row.id))?;
        } else if by_item {
            conn.execute(
                "UPDATE artist_references SET item_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint artist reference {} item", row.id))?;
        } else {
            conn.execute(
                "UPDATE artist_references SET artist_id=? WHERE id=?",
                rusqlite::params![target_id, row.id],
            )
            .with_context(|| format!("repoint artist reference {} artist", row.id))?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct ArtistSuggestionMergeRow {
    artist_id: Option<i64>,
    item_id: Option<i64>,
    status: String,
    dino_score: Option<f64>,
    wd14_score: Option<f64>,
    fused_score: Option<f64>,
    matched_ref_id: Option<i64>,
    reason: String,
    confirmed_at: Option<f64>,
}

fn suggestion_status_priority(status: &str) -> i64 {
    // Explicit user decisions outrank regenerated suggestions.
    match status {
        "confirmed" => 3,
        "rejected" => 2,
        "pending" | "suggested" => 1,
        _ => 0,
    }
}

/// Optional score columns of `artist_suggestions` (the Rust-created minimal
/// table lacks them while the legacy Python table has all of them), probed so
/// merges never fail on either schema.
fn artist_suggestion_optional_columns(conn: &Connection) -> Result<Vec<String>> {
    let columns = table_columns(conn, "artist_suggestions")?;
    Ok(
        ["dino_score", "wd14_score", "fused_score", "matched_ref_id"]
            .iter()
            .filter(|name| columns.iter().any(|column| column.as_str() == **name))
            .map(|name| name.to_string())
            .collect(),
    )
}

/// Rows whose item or artist equals `source_id`; the row identity is the
/// UNIQUE(item_id, artist_id) pair, so no `id` column is required (the
/// Rust-created minimal table has none).
fn read_artist_suggestion_merge_rows(
    conn: &Connection,
    column: &str,
    source_id: i64,
) -> Result<Vec<ArtistSuggestionMergeRow>> {
    let optional = artist_suggestion_optional_columns(conn)?;
    let optional_sql = if optional.is_empty() {
        String::new()
    } else {
        format!(", {}", optional.join(", "))
    };
    let mut stmt = conn.prepare(&format!(
        "SELECT item_id, artist_id, status, reason, confirmed_at{optional_sql}
         FROM artist_suggestions WHERE {column}=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_id], |row| {
            let mut index = 5usize;
            let mut dino_score = None;
            let mut wd14_score = None;
            let mut fused_score = None;
            let mut matched_ref_id = None;
            for name in &optional {
                match name.as_str() {
                    "dino_score" => dino_score = row.get(index)?,
                    "wd14_score" => wd14_score = row.get(index)?,
                    "fused_score" => fused_score = row.get(index)?,
                    "matched_ref_id" => matched_ref_id = row.get(index)?,
                    _ => {}
                }
                index += 1;
            }
            Ok(ArtistSuggestionMergeRow {
                artist_id: row.get(1)?,
                item_id: row.get(0)?,
                status: row.get(2)?,
                reason: row.get(3)?,
                confirmed_at: row.get(4)?,
                dino_score,
                wd14_score,
                fused_score,
                matched_ref_id,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn sql_value_real(value: Option<f64>) -> rusqlite::types::Value {
    match value {
        Some(v) => rusqlite::types::Value::Real(v),
        None => rusqlite::types::Value::Null,
    }
}

fn write_artist_suggestion_row(
    conn: &Connection,
    row: &ArtistSuggestionMergeRow,
    target_item: i64,
    target_artist: i64,
) -> Result<()> {
    let optional = artist_suggestion_optional_columns(conn)?;
    let mut set: Vec<String> = vec![
        "status=?".to_string(),
        "reason=?".to_string(),
        "confirmed_at=?".to_string(),
    ];
    set.extend(optional.iter().map(|name| format!("{name}=?")));
    let sql = format!(
        "UPDATE artist_suggestions SET {} WHERE item_id=? AND artist_id=?",
        set.join(", ")
    );
    let mut values: Vec<rusqlite::types::Value> = vec![
        rusqlite::types::Value::Text(row.status.clone()),
        rusqlite::types::Value::Text(row.reason.clone()),
        sql_value_real(row.confirmed_at),
    ];
    for name in &optional {
        values.push(match name.as_str() {
            "dino_score" => sql_value_real(row.dino_score),
            "wd14_score" => sql_value_real(row.wd14_score),
            "fused_score" => sql_value_real(row.fused_score),
            "matched_ref_id" => match row.matched_ref_id {
                Some(v) => rusqlite::types::Value::Integer(v),
                None => rusqlite::types::Value::Null,
            },
            _ => rusqlite::types::Value::Null,
        });
    }
    values.push(rusqlite::types::Value::Integer(target_item));
    values.push(rusqlite::types::Value::Integer(target_artist));
    conn.execute(&sql, rusqlite::params_from_iter(values.iter()))
        .with_context(|| {
            format!(
                "merge artist suggestion of item {} onto artist {target_artist}",
                row.item_id.unwrap_or(0)
            )
        })?;
    Ok(())
}

/// Move `artist_suggestions` rows so one coordinate changes to a value that
/// already exists elsewhere; the higher-status winner survives on
/// UNIQUE(item_id, artist_id) collisions, ties keep the existing target row.
/// The (item_id, artist_id) pair identifies rows, so this also works on the
/// minimal Rust-created table without an `id` column.
fn repoint_artist_suggestions(
    conn: &Connection,
    move_by_item: Option<(i64, i64)>,
    move_by_artist: Option<(i64, i64)>,
) -> Result<()> {
    let (source_id, target_id, by_item) = match (move_by_item, move_by_artist) {
        (Some((source_item, keep_item)), None) => (source_item, keep_item, true),
        (None, Some((source_artist, target_artist))) => (source_artist, target_artist, false),
        _ => unreachable!("repoint_artist_suggestions takes exactly one coordinate"),
    };
    let rows = read_artist_suggestion_merge_rows(
        conn,
        if by_item { "item_id" } else { "artist_id" },
        source_id,
    )?;
    for row in rows {
        let (source_item, source_artist) = (row.item_id, row.artist_id);
        if (by_item && source_artist.is_none()) || (!by_item && source_item.is_none()) {
            // A NULL coordinate is not part of the UNIQUE(item_id, artist_id)
            // pair, so it can never collide: bulk-repoint it plainly.
            if by_item {
                conn.execute(
                    "UPDATE artist_suggestions SET item_id=?
                     WHERE item_id=? AND artist_id IS NULL",
                    rusqlite::params![target_id, source_id],
                )
                .with_context(|| {
                    format!("repoint NULL-artist suggestion of item {source_id} onto {target_id}")
                })?;
            } else {
                conn.execute(
                    "UPDATE artist_suggestions SET artist_id=?
                     WHERE item_id IS NULL AND artist_id=?",
                    rusqlite::params![target_id, source_id],
                )
                .with_context(|| {
                    format!("repoint NULL-item suggestion of artist {source_id} onto {target_id}")
                })?;
            }
            continue;
        }
        let collision: Option<(i64, i64)> = if by_item {
            conn.query_row(
                "SELECT item_id, artist_id FROM artist_suggestions
                 WHERE item_id=? AND artist_id IS ?",
                rusqlite::params![target_id, row.artist_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist suggestion collision for item {target_id}"))?
        } else {
            conn.query_row(
                "SELECT item_id, artist_id FROM artist_suggestions
                 WHERE item_id IS ? AND artist_id=?",
                rusqlite::params![row.item_id, target_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()
            .with_context(|| format!("probe artist suggestion collision for artist {target_id}"))?
        };
        let (source_item, source_artist) = (row.item_id, row.artist_id);
        if let Some((target_item, target_artist)) = collision {
            let target_status: String = conn.query_row(
                "SELECT status FROM artist_suggestions
                 WHERE item_id=? AND artist_id=?",
                rusqlite::params![target_item, target_artist],
                |r| r.get(0),
            )?;
            if suggestion_status_priority(&row.status) > suggestion_status_priority(&target_status)
            {
                write_artist_suggestion_row(conn, &row, target_item, target_artist)?;
            }
            conn.execute(
                "DELETE FROM artist_suggestions WHERE item_id IS ? AND artist_id IS ?",
                rusqlite::params![source_item, source_artist],
            )
            .with_context(|| format!("drop losing artist suggestion for item {source_item:?}"))?;
        } else if by_item {
            conn.execute(
                "UPDATE artist_suggestions SET item_id=? WHERE item_id=? AND artist_id IS ?",
                rusqlite::params![target_id, source_item, source_artist],
            )
            .with_context(|| {
                format!("repoint artist suggestion item for artist {source_artist:?}")
            })?;
        } else {
            conn.execute(
                "UPDATE artist_suggestions SET artist_id=? WHERE item_id IS ? AND artist_id=?",
                rusqlite::params![target_id, source_item, source_artist],
            )
            .with_context(|| {
                format!("repoint artist suggestion artist for item {source_item:?}")
            })?;
        }
    }
    Ok(())
}

/// Remap `selected_tag_ids` of the merged artist's folder plans through a tag
/// alias map after its tags were folded into the target artist's tags. Strict
/// JSON: a malformed selection on an affected plan is a migration error, not
/// an empty default, and unrelated plans are never read.
fn remap_folder_plan_tag_ids(
    conn: &Connection,
    tag_map: &HashMap<i64, i64>,
    artist_ids: &[i64],
) -> Result<()> {
    if tag_map.is_empty()
        || artist_ids.is_empty()
        || !table_exists(conn, "folder_rename_plans")?
        || !column_exists(conn, "folder_rename_plans", "selected_tag_ids")?
    {
        return Ok(());
    }
    let placeholders = std::iter::repeat("?")
        .take(artist_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut stmt = conn.prepare(&format!(
        "SELECT id, selected_tag_ids FROM folder_rename_plans
         WHERE artist_id IN ({placeholders})"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params_from_iter(artist_ids.iter()), |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (plan_id, raw) in rows {
        let ids = serde_json::from_str::<Vec<i64>>(&raw)
            .with_context(|| format!("parse selected_tag_ids of folder plan {plan_id}"))?;
        let mut mapped: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            let remapped = tag_map.get(&id).copied().unwrap_or(id);
            if !mapped.contains(&remapped) {
                mapped.push(remapped);
            }
        }
        let encoded = serde_json::to_string(&mapped)
            .with_context(|| format!("encode selected_tag_ids of folder plan {plan_id}"))?;
        if encoded != raw {
            conn.execute(
                "UPDATE folder_rename_plans SET selected_tag_ids=? WHERE id=?",
                rusqlite::params![encoded, plan_id],
            )
            .with_context(|| format!("remap selected_tag_ids of folder plan {plan_id}"))?;
        }
    }
    Ok(())
}

/// Semantic identity fields of a folder plan, compared across an artist merge;
/// legacy tables may lack some, and identity is judged on what is available.
const FOLDER_PLAN_SEMANTIC_COLUMNS: &[&str] = &[
    "original_folder_name",
    "original_title",
    "parsed_date",
    "selected_tag_ids",
    "status",
    "file_count",
    "total_size",
    "max_mtime",
    "created_at",
    "updated_at",
    "confirmed_at",
    "confirmation_source",
    "target_folder",
    "executed_at",
    "execution_log",
    "format_snapshot",
    "plan_kind",
    "split_actions",
];

/// The folder-plan columns actually present in the connected schema; legacy
/// tables may lack some, and identity is judged on what is available.
fn folder_plan_semantic_columns(conn: &Connection) -> Result<Vec<String>> {
    let mut present = Vec::new();
    for name in FOLDER_PLAN_SEMANTIC_COLUMNS {
        if column_exists(conn, "folder_rename_plans", name)? {
            present.push((*name).to_string());
        }
    }
    Ok(present)
}

/// Reassign folder plans of a merged alias artist. A plan whose source folder
/// already exists for the target artist is removed only when every available
/// semantic field matches after tag remapping; otherwise the migration aborts
/// and rolls back instead of silently dropping an executed/distinct plan.
fn merge_artist_folder_plans(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if !table_exists(conn, "folder_rename_plans")? {
        return Ok(());
    }
    let semantic = folder_plan_semantic_columns(conn)?;
    if semantic.is_empty() {
        conn.execute(
            "UPDATE folder_rename_plans SET artist_id=? WHERE artist_id=?",
            rusqlite::params![target_artist, source_artist],
        )
        .with_context(|| format!("reassign folder plans of artist {source_artist}"))?;
        return Ok(());
    }
    let select = format!("id, source_folder, {}", semantic.join(", "));
    let mut stmt = conn.prepare(&format!(
        "SELECT {select} FROM folder_rename_plans WHERE artist_id=?"
    ))?;
    let rows = stmt
        .query_map(rusqlite::params![source_artist], |row| {
            let mut values: Vec<Value> = Vec::with_capacity(semantic.len() + 2);
            values.push(json!(row.get::<_, i64>(0)?));
            values.push(json!(row.get::<_, String>(1)?));
            for index in 0..semantic.len() {
                values.push(column_value_json(&row, 2 + index)?);
            }
            Ok(values)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for source in rows {
        let plan_id = source[0].as_i64().unwrap();
        let source_folder = source[1].as_str().unwrap();
        let target: Option<Vec<Value>> = conn
            .query_row(
                &format!(
                    "SELECT {select} FROM folder_rename_plans
                     WHERE artist_id=? AND source_folder=?"
                ),
                rusqlite::params![target_artist, source_folder],
                |row| {
                    let mut values: Vec<Value> = Vec::with_capacity(semantic.len() + 2);
                    values.push(json!(row.get::<_, i64>(0)?));
                    values.push(json!(row.get::<_, String>(1)?));
                    for index in 0..semantic.len() {
                        values.push(column_value_json(&row, 2 + index)?);
                    }
                    Ok(values)
                },
            )
            .optional()
            .with_context(|| {
                format!(
                    "probe folder plan conflict of artist {target_artist} for folder {source_folder}"
                )
            })?;
        if let Some(target) = target {
            let target_plan_id = target[0].as_i64().unwrap();
            if source[2..] == target[2..] {
                conn.execute(
                    "DELETE FROM folder_rename_plans WHERE id=?",
                    rusqlite::params![plan_id],
                )
                .with_context(|| format!("drop duplicate folder plan {plan_id}"))?;
            } else {
                return Err(anyhow!(
                    "folder plan conflict: plan {plan_id} of artist {source_artist} and \
                     plan {target_plan_id} of artist {target_artist} share source folder \
                     {source_folder} but differ in semantic fields; refusing to merge"
                ));
            }
        } else {
            conn.execute(
                "UPDATE folder_rename_plans SET artist_id=? WHERE id=?",
                rusqlite::params![target_artist, plan_id],
            )
            .with_context(|| format!("reassign folder plan {plan_id} to artist {target_artist}"))?;
        }
    }
    Ok(())
}

/// Lossless JSON copy of one row column for identity comparisons.
fn column_value_json(row: &rusqlite::Row, index: usize) -> rusqlite::Result<Value> {
    use rusqlite::types::ValueRef;
    Ok(match row.get_ref(index)? {
        ValueRef::Null => Value::Null,
        ValueRef::Integer(value) => json!(value),
        ValueRef::Real(value) => json!(value),
        ValueRef::Text(value) => json!(String::from_utf8_lossy(value)),
        ValueRef::Blob(value) => json!(value.to_vec()),
    })
}

/// Merge every authoritative relationship from a source item onto a kept item
/// before the source row is deleted. Optional tables are probed (missing
/// allowed, probe failure fatal) and merged or repointed deterministically.
fn merge_item_into(
    conn: &Connection,
    source_id: i64,
    keep_id: i64,
    target_artist: Option<i64>,
) -> Result<()> {
    if let Some(target_artist) = target_artist {
        // Artifact of the artist-scoped merge: the kept item must belong to
        // the surviving artist before any derived document coordinates are
        // read from it (its own row may live under a different artist).
        conn.execute(
            "UPDATE items SET artist_id=? WHERE id=?",
            rusqlite::params![target_artist, keep_id],
        )
        .with_context(|| format!("assign kept item {keep_id} to artist {target_artist}"))?;
    }
    if column_exists(conn, "items", "missing")? {
        let (source_missing, keep_missing): (i64, i64) = conn
            .query_row(
                "SELECT
                   (SELECT missing FROM items WHERE id=?),
                   (SELECT missing FROM items WHERE id=?)",
                rusqlite::params![source_id, keep_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .with_context(|| format!("probe missing state of items {source_id}/{keep_id}"))?;
        if source_missing == 0 || keep_missing == 0 {
            let sql = if column_exists(conn, "items", "missing_at")? {
                "UPDATE items SET missing=0, missing_at=NULL WHERE id=?"
            } else {
                "UPDATE items SET missing=0 WHERE id=?"
            };
            conn.execute(sql, rusqlite::params![keep_id])
                .with_context(|| format!("keep merged item {keep_id} active"))?;
        }
    }
    conn.execute(
        "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
         SELECT ?, tag_id FROM item_tags WHERE item_id=?",
        rusqlite::params![keep_id, source_id],
    )
    .with_context(|| format!("merge item_tags from item {source_id} onto item {keep_id}"))?;
    conn.execute(
        "DELETE FROM item_tags WHERE item_id=?",
        rusqlite::params![source_id],
    )
    .with_context(|| format!("drop item_tags links of merged item {source_id}"))?;
    if table_exists(conn, "item_favorites")? {
        conn.execute(
            "INSERT OR IGNORE INTO item_favorites (item_id, created_at)
             SELECT ?, created_at FROM item_favorites WHERE item_id=?",
            rusqlite::params![keep_id, source_id],
        )
        .with_context(|| format!("merge favorites of item {source_id} onto item {keep_id}"))?;
        conn.execute(
            "DELETE FROM item_favorites WHERE item_id=?",
            rusqlite::params![source_id],
        )
        .with_context(|| format!("drop favorites of merged item {source_id}"))?;
    }
    if table_exists(conn, "character_references")?
        && column_exists(conn, "character_references", "item_id")?
    {
        conn.execute(
            "UPDATE character_references SET item_id=? WHERE item_id=?",
            rusqlite::params![keep_id, source_id],
        )
        .with_context(|| {
            format!("repoint character references of item {source_id} onto {keep_id}")
        })?;
    }
    if table_exists(conn, "character_recognition_results")? {
        let source_has: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM character_recognition_results WHERE item_id=?)",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .with_context(|| format!("probe recognition result of item {source_id}"))?;
        if source_has {
            let target_has: bool = conn
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM character_recognition_results WHERE item_id=?)",
                    rusqlite::params![keep_id],
                    |row| row.get(0),
                )
                .with_context(|| format!("probe recognition result of item {keep_id}"))?;
            if target_has {
                // The kept item's computation survives; the source row drops.
                conn.execute(
                    "DELETE FROM character_recognition_results WHERE item_id=?",
                    rusqlite::params![source_id],
                )
                .with_context(|| {
                    format!("drop superseded recognition result of item {source_id}")
                })?;
            } else {
                conn.execute(
                    "UPDATE character_recognition_results SET item_id=? WHERE item_id=?",
                    rusqlite::params![keep_id, source_id],
                )
                .with_context(|| {
                    format!("repoint recognition result of item {source_id} onto {keep_id}")
                })?;
            }
        }
    }
    for table in ["move_candidates", "move_history"] {
        if table_exists(conn, table)? && column_exists(conn, table, "item_id")? {
            conn.execute(
                &format!("UPDATE {table} SET item_id=? WHERE item_id=?"),
                rusqlite::params![keep_id, source_id],
            )
            .with_context(|| format!("repoint {table} rows of item {source_id} onto {keep_id}"))?;
        }
    }
    if table_exists(conn, "artist_references")? {
        repoint_artist_references(conn, Some((source_id, keep_id)), None)?;
    }
    if table_exists(conn, "artist_suggestions")? {
        repoint_artist_suggestions(conn, Some((source_id, keep_id)), None)?;
    }
    if table_exists(conn, "artist_link_documents")?
        && column_exists(conn, "artist_link_documents", "item_id")?
    {
        let source_doc: Option<i64> = conn
            .query_row(
                "SELECT id FROM artist_link_documents WHERE item_id=?",
                rusqlite::params![source_id],
                |row| row.get(0),
            )
            .optional()
            .with_context(|| format!("probe link document of item {source_id}"))?;
        if let Some(source_doc_id) = source_doc {
            let target_doc: Option<i64> = conn
                .query_row(
                    "SELECT id FROM artist_link_documents WHERE item_id=?",
                    rusqlite::params![keep_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| format!("probe link document of item {keep_id}"))?;
            if let Some(_target_doc_id) = target_doc {
                // Both items were indexed: the kept item's valid document
                // survives, the derived source document is invalidated for
                // reindexing (occurrences cascade only with it).
                conn.execute(
                    "DELETE FROM artist_link_documents WHERE id=?",
                    rusqlite::params![source_doc_id],
                )
                .with_context(|| format!("invalidate link document {source_doc_id}"))?;
            } else {
                // Repoint every document coordinate to the kept item so item,
                // artist, and path never disagree.
                let (keep_artist, keep_path): (i64, String) = conn
                    .query_row(
                        "SELECT artist_id, file_path FROM items WHERE id=?",
                        rusqlite::params![keep_id],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )
                    .with_context(|| format!("probe kept item {keep_id} coordinates"))?;
                conn.execute(
                    "UPDATE artist_link_documents SET item_id=?, artist_id=?, file_path=?
                     WHERE id=?",
                    rusqlite::params![keep_id, keep_artist, keep_path, source_doc_id],
                )
                .with_context(|| {
                    format!("repoint link document {source_doc_id} onto item {keep_id}")
                })?;
            }
        }
    }
    Ok(())
}

/// Remap `tag_ids_snapshot` JSON of the merged artist's active recycle entries
/// through the tag alias map before duplicate tag rows are deleted. A
/// malformed active snapshot is a migration error, not an empty default.
fn remap_recycle_tag_ids(
    conn: &Connection,
    tag_map: &HashMap<i64, i64>,
    artist_id: i64,
) -> Result<()> {
    if tag_map.is_empty()
        || !table_exists(conn, "recycle_entries")?
        || !column_exists(conn, "recycle_entries", "tag_ids_snapshot")?
    {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id, tag_ids_snapshot FROM recycle_entries
         WHERE artist_id=? AND status='recycled'",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![artist_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (entry_id, raw) in rows {
        let ids = serde_json::from_str::<Vec<i64>>(&raw)
            .with_context(|| format!("parse tag_ids_snapshot of recycle entry {entry_id}"))?;
        let mut mapped: Vec<i64> = Vec::with_capacity(ids.len());
        for id in ids {
            let remapped = tag_map.get(&id).copied().unwrap_or(id);
            if !mapped.contains(&remapped) {
                mapped.push(remapped);
            }
        }
        let encoded = serde_json::to_string(&mapped)
            .with_context(|| format!("encode tag_ids_snapshot of recycle entry {entry_id}"))?;
        if encoded != raw {
            conn.execute(
                "UPDATE recycle_entries SET tag_ids_snapshot=? WHERE id=?",
                rusqlite::params![encoded, entry_id],
            )
            .with_context(|| format!("remap tag_ids_snapshot of recycle entry {entry_id}"))?;
        }
    }
    Ok(())
}

/// Repoint active recycle entries of a merged alias artist to the target: the
/// record `artist_id` and the strict `item_snapshot` JSON artist must agree
/// and move together, or restore would reject the entry. Historical restored
/// records stay unchanged.
fn remap_recycle_entries_artist(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if !table_exists(conn, "recycle_entries")? {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT id, item_snapshot FROM recycle_entries
         WHERE artist_id=? AND status='recycled'",
    )?;
    let rows = stmt
        .query_map(rusqlite::params![source_artist], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (entry_id, snapshot_raw) in rows {
        let mut snapshot: Value = serde_json::from_str(&snapshot_raw)
            .with_context(|| format!("parse item_snapshot of recycle entry {entry_id}"))?;
        if snapshot.get("artist_id").and_then(Value::as_i64) != Some(source_artist) {
            return Err(anyhow!(
                "recycle entry {entry_id} item_snapshot artist does not match its record"
            ));
        }
        snapshot["artist_id"] = json!(target_artist);
        let encoded = serde_json::to_string(&snapshot)
            .with_context(|| format!("encode item_snapshot of recycle entry {entry_id}"))?;
        conn.execute(
            "UPDATE recycle_entries SET artist_id=?, item_snapshot=? WHERE id=?",
            rusqlite::params![target_artist, encoded, entry_id],
        )
        .with_context(|| format!("repoint recycle entry {entry_id} to artist {target_artist}"))?;
    }
    Ok(())
}

/// Merge every artist-scoped relationship from a merged alias artist onto the
/// target before the alias row is deleted: union profile links, repoint
/// references/suggestions/documents deterministically, repoint scan_state.
fn merge_artist_relationships(
    conn: &Connection,
    source_artist: i64,
    target_artist: i64,
) -> Result<()> {
    if table_exists(conn, "artist_profile_links")? {
        let mut stmt = conn.prepare("SELECT id FROM artist_profile_links WHERE artist_id=?")?;
        let rows = stmt
            .query_map(rusqlite::params![source_artist], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for link_id in rows {
            let duplicate: Option<i64> = conn
                .query_row(
                    "SELECT l2.id
                     FROM artist_profile_links l1
                     JOIN artist_profile_links l2
                       ON l2.artist_id=? AND l2.kind=l1.kind AND l2.url=l1.url
                     WHERE l1.id=?",
                    rusqlite::params![target_artist, link_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| {
                    format!("probe profile link duplicate for artist {target_artist}")
                })?;
            if let Some(_duplicate_id) = duplicate {
                conn.execute(
                    "DELETE FROM artist_profile_links WHERE id=?",
                    rusqlite::params![link_id],
                )
                .with_context(|| format!("drop duplicate profile link {link_id}"))?;
            } else {
                conn.execute(
                    "UPDATE artist_profile_links SET artist_id=? WHERE id=?",
                    rusqlite::params![target_artist, link_id],
                )
                .with_context(|| {
                    format!("repoint profile link {link_id} to artist {target_artist}")
                })?;
            }
        }
    }
    if table_exists(conn, "artist_references")? {
        repoint_artist_references(conn, None, Some((source_artist, target_artist)))?;
    }
    if table_exists(conn, "artist_suggestions")? {
        repoint_artist_suggestions(conn, None, Some((source_artist, target_artist)))?;
    }
    if table_exists(conn, "artist_link_documents")? {
        let mut stmt =
            conn.prepare("SELECT id, file_path FROM artist_link_documents WHERE artist_id=?")?;
        let rows = stmt
            .query_map(rusqlite::params![source_artist], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (doc_id, file_path) in rows {
            let duplicate: Option<i64> = conn
                .query_row(
                    "SELECT id FROM artist_link_documents
                     WHERE artist_id=? AND file_path=? AND id<>?",
                    rusqlite::params![target_artist, file_path, doc_id],
                    |row| row.get(0),
                )
                .optional()
                .with_context(|| {
                    format!("probe link document duplicate for artist {target_artist}")
                })?;
            if duplicate.is_some() {
                conn.execute(
                    "DELETE FROM artist_link_documents WHERE id=?",
                    rusqlite::params![doc_id],
                )
                .with_context(|| format!("invalidate superseded link document {doc_id}"))?;
            } else {
                conn.execute(
                    "UPDATE artist_link_documents SET artist_id=? WHERE id=?",
                    rusqlite::params![target_artist, doc_id],
                )
                .with_context(|| {
                    format!("repoint link document {doc_id} to artist {target_artist}")
                })?;
            }
        }
    }
    if table_exists(conn, "scan_state")? && column_exists(conn, "scan_state", "artist_id")? {
        conn.execute(
            "UPDATE scan_state SET artist_id=? WHERE artist_id=?",
            rusqlite::params![target_artist, source_artist],
        )
        .with_context(|| {
            format!("repoint scan_state from artist {source_artist} to {target_artist}")
        })?;
    }
    remap_recycle_entries_artist(conn, source_artist, target_artist)?;
    Ok(())
}

/// Rewrite legacy virtual media-root aliases in path columns to real authorized paths.
///
/// Signature-gated so the same root mapping runs only once. Conflicts reuse simple merge:
/// keep the target path row, reassign foreign keys from the source artist/item.
pub fn normalize_configured_media_paths(conn: &Connection, roots: &MediaRoots) -> Result<Value> {
    let signature = media_path_migration_signature(roots);
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
            [],
            |r| r.get(0),
        )
        .optional()
        .with_context(|| "read media_path_real_migration_signature marker")?;
    if existing.as_deref() == Some(signature.as_str()) {
        return Ok(json!({"updated": 0, "skipped": "already_applied"}));
    }
    if !has_virtual_paths(conn, roots)? {
        set_migration_signature(conn, &signature)?;
        return Ok(json!({"updated": 0, "skipped": "no_virtual_paths"}));
    }

    let pairs: Vec<(String, String)> = roots
        .roots
        .iter()
        .enumerate()
        .filter_map(|(i, root)| {
            let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
            let real = roots.real_root_at(i)?;
            let real_n = real.replace('\\', "/").trim_end_matches('/').to_string();
            if root_n.is_empty() || real_n.is_empty() || root_n == real_n {
                None
            } else {
                Some((root_n, real_n))
            }
        })
        .collect();
    if pairs.is_empty() {
        set_migration_signature(conn, &signature)?;
        return Ok(json!({"updated": 0, "skipped": "no_pairs"}));
    }

    let tx = conn.unchecked_transaction()?;
    let mut updated = 0i64;
    let mut merged_artists = 0i64;
    let mut merged_items = 0i64;

    // Artists first: resolve unique path conflicts by merging source into target.
    for (root_n, real_n) in &pairs {
        let rows = tx
            .prepare("SELECT id, path FROM artists WHERE path=? OR path LIKE ? ORDER BY id")?
            .query_map(rusqlite::params![root_n, format!("{root_n}/%")], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (artist_id, old_path) in rows {
            let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
            if new_path == old_path {
                continue;
            }
            let existing_id: Option<i64> = tx
                .query_row(
                    "SELECT id FROM artists WHERE path=? AND id<>?",
                    rusqlite::params![&new_path, artist_id],
                    |r| r.get(0),
                )
                .optional()
                .with_context(|| format!("probe artist path conflict for {new_path}"))?;
            if let Some(target_id) = existing_id {
                // Merge tags onto the real-path artist. A source tag sharing a
                // name with a target tag cannot be reassigned because of
                // UNIQUE(artist_id, name): attach the source tag's item links
                // to the target tag first, then delete the alias tag row only
                // after every link is moved.
                let mut tag_map: HashMap<i64, i64> = HashMap::new();
                let source_tags = tx
                    .prepare("SELECT id, name FROM tags WHERE artist_id=? ORDER BY id")?
                    .query_map(rusqlite::params![artist_id], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (tag_id, tag_name) in source_tags {
                    let target_tag_id: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM tags WHERE artist_id=? AND name=?",
                            rusqlite::params![target_id, tag_name],
                            |r| r.get(0),
                        )
                        .optional()
                        .with_context(|| format!("probe tag {tag_name} of artist {target_id}"))?;
                    if let Some(target_tag_id) = target_tag_id {
                        tx.execute(
                            "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
                             SELECT item_id, ? FROM item_tags WHERE tag_id=?",
                            rusqlite::params![target_tag_id, tag_id],
                        )
                        .with_context(|| {
                            format!("merge item_tags from tag {tag_id} onto tag {target_tag_id}")
                        })?;
                        tx.execute("DELETE FROM tags WHERE id=?", rusqlite::params![tag_id])
                            .with_context(|| format!("drop duplicate source tag {tag_id}"))?;
                        tag_map.insert(tag_id, target_tag_id);
                    } else {
                        tx.execute(
                            "UPDATE tags SET artist_id=? WHERE id=?",
                            rusqlite::params![target_id, tag_id],
                        )
                        .with_context(|| format!("reassign tag {tag_id} to artist {target_id}"))?;
                        tag_map.insert(tag_id, tag_id);
                    }
                }
                remap_folder_plan_tag_ids(&tx, &tag_map, &[artist_id])?;
                remap_recycle_tag_ids(&tx, &tag_map, artist_id)?;
                // Items: reassign or merge on path conflict.
                let items = tx
                    .prepare("SELECT id, file_path FROM items WHERE artist_id=?")?
                    .query_map(rusqlite::params![artist_id], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (item_id, file_path) in items {
                    let mapped =
                        if file_path == *root_n || file_path.starts_with(&format!("{root_n}/")) {
                            format!("{real_n}{}", &file_path[root_n.len()..])
                        } else {
                            roots.normalize_db_path(&file_path)
                        };
                    let conflict: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM items WHERE file_path=? AND id<>?",
                            rusqlite::params![&mapped, item_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .with_context(|| format!("probe item path conflict for {mapped}"))?;
                    if let Some(keep_id) = conflict {
                        merge_item_into(&tx, item_id, keep_id, Some(target_id))?;
                        tx.execute("DELETE FROM items WHERE id=?", rusqlite::params![item_id])
                            .with_context(|| format!("drop merged item {item_id}"))?;
                        merged_items += 1;
                    } else {
                        tx.execute(
                            "UPDATE items SET artist_id=?, file_path=? WHERE id=?",
                            rusqlite::params![target_id, mapped, item_id],
                        )
                        .with_context(|| {
                            format!("reassign item {item_id} to artist {target_id}")
                        })?;
                        updated += 1;
                    }
                }
                for table in [
                    "scan_seen",
                    "scan_candidates",
                    "move_candidates",
                    "move_history",
                ] {
                    if !table_exists(&tx, table)? {
                        continue;
                    }
                    tx.execute(
                        &format!("UPDATE {table} SET artist_id=? WHERE artist_id=?"),
                        rusqlite::params![target_id, artist_id],
                    )
                    .with_context(|| format!("reassign {table} rows to artist {target_id}"))?;
                }
                merge_artist_folder_plans(&tx, artist_id, target_id)?;
                merge_artist_relationships(&tx, artist_id, target_id)?;
                if column_exists(&tx, "artists", "missing")? {
                    let (source_missing, target_missing): (i64, i64) = tx
                        .query_row(
                            "SELECT
                               (SELECT missing FROM artists WHERE id=?),
                               (SELECT missing FROM artists WHERE id=?)",
                            rusqlite::params![artist_id, target_id],
                            |r| Ok((r.get(0)?, r.get(1)?)),
                        )
                        .with_context(|| {
                            format!("probe missing state of artists {artist_id}/{target_id}")
                        })?;
                    if source_missing == 0 || target_missing == 0 {
                        let sql = if column_exists(&tx, "artists", "missing_at")? {
                            "UPDATE artists SET missing=0, missing_at=NULL WHERE id=?"
                        } else {
                            "UPDATE artists SET missing=0 WHERE id=?"
                        };
                        tx.execute(sql, rusqlite::params![target_id])
                            .with_context(|| format!("keep merged artist {target_id} active"))?;
                    }
                }
                tx.execute(
                    "DELETE FROM artists WHERE id=?",
                    rusqlite::params![artist_id],
                )
                .with_context(|| format!("drop merged alias artist {artist_id}"))?;
                merged_artists += 1;
            } else {
                tx.execute(
                    "UPDATE artists SET path=? WHERE id=?",
                    rusqlite::params![&new_path, artist_id],
                )?;
                updated += 1;
            }
        }
    }

    // Bulk-rewrite remaining path columns (non-conflicting rows first for UNIQUE columns).
    let path_columns = [
        ("items", "file_path", true),
        ("artist_link_documents", "file_path", true),
        ("scan_seen", "file_path", false),
        ("scan_candidates", "file_path", false),
        ("move_candidates", "old_path", false),
        ("move_candidates", "new_path", false),
        ("move_history", "old_path", false),
        ("move_history", "new_path", false),
    ];
    for (table, column, unique) in path_columns {
        let exists: i64 = tx
            .query_row(
                "SELECT COUNT(1) FROM sqlite_master WHERE type='table' AND name=?",
                rusqlite::params![table],
                |r| r.get(0),
            )
            .with_context(|| format!("probe presence of table {table}"))?;
        if exists == 0 {
            continue;
        }
        for (root_n, real_n) in &pairs {
            let sql = if unique {
                format!(
                    "UPDATE {table}
                     SET {column} = ? || substr({column}, length(?) + 1)
                     WHERE ({column}=? OR {column} LIKE ?)
                       AND {column} NOT LIKE '%/../%'
                       AND {column} NOT LIKE '%/..'
                       AND NOT EXISTS (
                         SELECT 1 FROM {table} AS existing
                         WHERE existing.{column} = ? || substr({table}.{column}, length(?) + 1)
                           AND existing.rowid <> {table}.rowid
                       )"
                )
            } else {
                format!(
                    "UPDATE {table}
                     SET {column} = ? || substr({column}, length(?) + 1)
                     WHERE ({column}=? OR {column} LIKE ?)
                       AND {column} NOT LIKE '%/../%'
                       AND {column} NOT LIKE '%/..'"
                )
            };
            let n = if unique {
                tx.execute(
                    &sql,
                    rusqlite::params![
                        real_n,
                        root_n,
                        root_n,
                        format!("{root_n}/%"),
                        real_n,
                        root_n
                    ],
                )?
            } else {
                tx.execute(
                    &sql,
                    rusqlite::params![real_n, root_n, root_n, format!("{root_n}/%")],
                )?
            };
            updated += n as i64;
        }
        // Remaining unique conflicts on items: merge into the real path row.
        if unique && table == "items" {
            for (root_n, real_n) in &pairs {
                let rows = tx
                    .prepare(&format!(
                        "SELECT id, file_path FROM {table} WHERE {column}=? OR {column} LIKE ?"
                    ))?
                    .query_map(rusqlite::params![root_n, format!("{root_n}/%")], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (item_id, old_path) in rows {
                    let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
                    let conflict: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM items WHERE file_path=? AND id<>?",
                            rusqlite::params![&new_path, item_id],
                            |r| r.get(0),
                        )
                        .optional()
                        .with_context(|| format!("probe item path conflict for {new_path}"))?;
                    if let Some(keep_id) = conflict {
                        merge_item_into(&tx, item_id, keep_id, None)?;
                        tx.execute("DELETE FROM items WHERE id=?", rusqlite::params![item_id])
                            .with_context(|| format!("drop merged item {item_id}"))?;
                        merged_items += 1;
                    } else {
                        tx.execute(
                            "UPDATE items SET file_path=? WHERE id=?",
                            rusqlite::params![&new_path, item_id],
                        )?;
                        updated += 1;
                    }
                }
            }
        }
    }

    set_migration_signature(&tx, &signature)?;
    tx.commit()?;
    Ok(json!({
        "updated": updated,
        "merged_artists": merged_artists,
        "merged_items": merged_items,
    }))
}

/// Mirror of `app/database.py:_configure_connection` so the Rust side behaves
/// identically to the Python side (WAL, busy_timeout, foreign_keys, NATURAL_NOCASE).
fn configure_connection(conn: &Connection, read_only: bool) -> Result<()> {
    let busy_timeout = env::var("SQLITE_BUSY_TIMEOUT_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(30_000);
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
