use std::env;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use anyhow::{anyhow, Context, Result};
use rusqlite::{Connection, OpenFlags};
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
        let mut conns = Vec::with_capacity(size);
        for _ in 0..size {
            conns.push(open_db(&db_path, config.read_only)?);
        }
        // Writable primary process must ensure schema exists (fail closed).
        if !config.read_only {
            ensure_product_schema(&conns[0])?;
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
fn ensure_product_schema(conn: &Connection) -> Result<()> {
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
        CREATE INDEX IF NOT EXISTS idx_items_artist ON items(artist_id);
        CREATE INDEX IF NOT EXISTS idx_items_path ON items(file_path);
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
    require_core_schema(conn)?;
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
            .unwrap_or(0);
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
            if roots.real_root_at(idx).map(|r| {
                r.replace('\\', "/").trim_end_matches('/') == root_n.as_str()
            }) == Some(true)
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
                .unwrap_or(0);
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

/// Rewrite legacy virtual media-root aliases in path columns to real authorized paths.
///
/// Signature-gated so the same root mapping runs only once. Conflicts reuse simple merge:
/// keep the target path row, reassign foreign keys from the source artist/item.
pub fn normalize_configured_media_paths(
    conn: &Connection,
    roots: &MediaRoots,
) -> Result<Value> {
    let signature = media_path_migration_signature(roots);
    let existing: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key='media_path_real_migration_signature'",
            [],
            |r| r.get(0),
        )
        .ok();
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
            .prepare(
                "SELECT id, path FROM artists WHERE path=? OR path LIKE ? ORDER BY id",
            )?
            .query_map(
                rusqlite::params![root_n, format!("{root_n}/%")],
                |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
            )?
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
                .ok();
            if let Some(target_id) = existing_id {
                // Merge tags/items onto the real-path artist; drop the virtual alias row.
                let _ = tx.execute(
                    "UPDATE tags SET artist_id=? WHERE artist_id=?",
                    rusqlite::params![target_id, artist_id],
                );
                // Items: reassign or merge on path conflict.
                let items = tx
                    .prepare("SELECT id, file_path FROM items WHERE artist_id=?")?
                    .query_map(rusqlite::params![artist_id], |r| {
                        Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?))
                    })?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (item_id, file_path) in items {
                    let mapped = if file_path == *root_n || file_path.starts_with(&format!("{root_n}/"))
                    {
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
                        .ok();
                    if let Some(keep_id) = conflict {
                        let _ = tx.execute(
                            "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
                             SELECT ?, tag_id FROM item_tags WHERE item_id=?",
                            rusqlite::params![keep_id, item_id],
                        );
                        let _ = tx.execute("DELETE FROM items WHERE id=?", rusqlite::params![item_id]);
                        merged_items += 1;
                    } else {
                        let _ = tx.execute(
                            "UPDATE items SET artist_id=?, file_path=? WHERE id=?",
                            rusqlite::params![target_id, mapped, item_id],
                        );
                        updated += 1;
                    }
                }
                for table in ["scan_seen", "scan_candidates", "move_candidates", "move_history"] {
                    let _ = tx.execute(
                        &format!("UPDATE {table} SET artist_id=? WHERE artist_id=?"),
                        rusqlite::params![target_id, artist_id],
                    );
                }
                let _ = tx.execute(
                    "UPDATE folder_rename_plans SET artist_id=? WHERE artist_id=?",
                    rusqlite::params![target_id, artist_id],
                );
                let _ = tx.execute("DELETE FROM artists WHERE id=?", rusqlite::params![artist_id]);
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
            .unwrap_or(0);
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
                    .query_map(
                        rusqlite::params![root_n, format!("{root_n}/%")],
                        |r| Ok((r.get::<_, i64>(0)?, r.get::<_, String>(1)?)),
                    )?
                    .collect::<rusqlite::Result<Vec<_>>>()?;
                for (item_id, old_path) in rows {
                    let new_path = format!("{real_n}{}", &old_path[root_n.len()..]);
                    let conflict: Option<i64> = tx
                        .query_row(
                            "SELECT id FROM items WHERE file_path=? AND id<>?",
                            rusqlite::params![&new_path, item_id],
                            |r| r.get(0),
                        )
                        .ok();
                    if let Some(keep_id) = conflict {
                        let _ = tx.execute(
                            "INSERT OR IGNORE INTO item_tags (item_id, tag_id)
                             SELECT ?, tag_id FROM item_tags WHERE item_id=?",
                            rusqlite::params![keep_id, item_id],
                        );
                        let _ = tx.execute("DELETE FROM items WHERE id=?", rusqlite::params![item_id]);
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
        ensure_product_schema(&conn).unwrap();

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
}
