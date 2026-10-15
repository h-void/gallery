//! Read-only source-link index for artist folders.
//!
//! TXT, HTML, and HTM files stay in their authorized media folders. This module
//! only records links and their source locations in SQLite; it never writes or
//! moves a source document.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;
use std::sync::OnceLock;

use anyhow::{anyhow, Context, Result};
use encoding_rs::{Encoding, GB18030, UTF_16BE, UTF_16LE, UTF_8};
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

use crate::media_roots::{path_under_authorized_roots, MediaRoots};

// ponytail: bound local reads; raise this only if real source lists exceed 32 MiB.
const MAX_SOURCE_BYTES: u64 = 32 * 1024 * 1024;
const MAX_CONTEXT_CHARS: usize = 360;
const MAX_LABEL_CHARS: usize = 180;

#[derive(Clone, Debug)]
struct SourceItem {
    id: i64,
    file_path: String,
    file_name: String,
    file_size: i64,
    file_mtime: f64,
    folder_name: String,
}

#[derive(Clone, Debug)]
struct ExistingDocument {
    file_path: String,
    file_name: String,
    folder_name: String,
    file_size: i64,
    file_mtime: f64,
    parse_status: String,
}

#[derive(Clone, Debug)]
struct ExtractedLink {
    normalized_url: String,
    raw_url: String,
    host: String,
    label: String,
    passcode: String,
    line_number: i64,
    context: String,
}

#[derive(Clone, Debug)]
struct LinkClassification {
    category: String,
    provider_name: String,
}

#[derive(Clone, Debug, Serialize)]
struct LinkSourceResponse {
    item_id: i64,
    file_path: String,
    file_name: String,
    folder_name: String,
    file_kind: String,
    line_number: i64,
    label: String,
    passcode: String,
    context: String,
}

#[derive(Serialize)]
struct ArtistLinkResponse {
    url: String,
    display_url: String,
    host: String,
    category: String,
    provider_name: String,
    availability: &'static str,
    passcodes: Vec<String>,
    source_count: usize,
    sources: Vec<LinkSourceResponse>,
}

#[derive(Serialize)]
struct LinkDocumentResponse {
    item_id: i64,
    file_path: String,
    file_name: String,
    folder_name: String,
    file_kind: String,
    file_size: i64,
    file_mtime: f64,
    parse_status: String,
    parse_error: String,
    link_count: i64,
    indexed_at: Option<f64>,
}

#[derive(Clone, Debug)]
struct LinkAccumulator {
    display_url: String,
    host: String,
    passcodes: BTreeSet<String>,
    sources: Vec<LinkSourceResponse>,
}

pub(crate) fn ensure_link_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS artist_link_documents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            artist_id INTEGER NOT NULL REFERENCES artists(id) ON DELETE CASCADE,
            item_id INTEGER NOT NULL UNIQUE REFERENCES items(id) ON DELETE CASCADE,
            file_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            folder_name TEXT NOT NULL DEFAULT '',
            file_kind TEXT NOT NULL,
            file_size INTEGER NOT NULL DEFAULT 0,
            file_mtime REAL NOT NULL DEFAULT 0,
            content_hash TEXT NOT NULL DEFAULT '',
            parse_status TEXT NOT NULL DEFAULT 'pending',
            parse_error TEXT NOT NULL DEFAULT '',
            link_count INTEGER NOT NULL DEFAULT 0,
            indexed_at REAL
        );
        CREATE UNIQUE INDEX IF NOT EXISTS idx_artist_link_documents_path
            ON artist_link_documents(artist_id, file_path);
        CREATE INDEX IF NOT EXISTS idx_artist_link_documents_artist
            ON artist_link_documents(artist_id, parse_status);

        CREATE TABLE IF NOT EXISTS artist_link_occurrences (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            document_id INTEGER NOT NULL REFERENCES artist_link_documents(id) ON DELETE CASCADE,
            normalized_url TEXT NOT NULL,
            raw_url TEXT NOT NULL,
            host TEXT NOT NULL,
            label TEXT NOT NULL DEFAULT '',
            passcode TEXT NOT NULL DEFAULT '',
            line_number INTEGER NOT NULL DEFAULT 0,
            context TEXT NOT NULL DEFAULT '',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        CREATE INDEX IF NOT EXISTS idx_artist_link_occurrences_document
            ON artist_link_occurrences(document_id);
        CREATE INDEX IF NOT EXISTS idx_artist_link_occurrences_url
            ON artist_link_occurrences(normalized_url);

        CREATE TABLE IF NOT EXISTS artist_link_domain_rules (
            domain TEXT PRIMARY KEY,
            category TEXT NOT NULL DEFAULT 'cloud_drive',
            provider_name TEXT NOT NULL DEFAULT '',
            created_at REAL NOT NULL DEFAULT (strftime('%s','now')),
            updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
        );
        "#,
    )
    .context("initialize artist link index schema")?;
    Ok(())
}

pub fn reindex_scanned_artist_links(
    conn: &Connection,
    roots: &MediaRoots,
    artist_ids: &[i64],
) -> Result<Value> {
    ensure_link_schema(conn)?;
    let ids: BTreeSet<i64> = artist_ids.iter().copied().filter(|id| *id > 0).collect();
    let mut indexed_documents = 0i64;
    let mut skipped_documents = 0i64;
    let mut links = 0i64;
    let mut errors = 0i64;
    let mut artist_errors = Vec::new();

    for artist_id in ids {
        match reindex_artist_links_inner(conn, roots, artist_id, None) {
            Ok(result) => {
                indexed_documents += result["indexed_documents"].as_i64().unwrap_or(0);
                skipped_documents += result["skipped_documents"].as_i64().unwrap_or(0);
                links += result["links"].as_i64().unwrap_or(0);
                errors += result["errors"].as_i64().unwrap_or(0);
            }
            Err(error) => {
                errors += 1;
                artist_errors.push(json!({"artist_id": artist_id, "error": error.to_string()}));
            }
        }
    }

    Ok(json!({
        "ok": artist_errors.is_empty(),
        "artists": artist_ids.len(),
        "indexed_documents": indexed_documents,
        "skipped_documents": skipped_documents,
        "links": links,
        "errors": errors,
        "artist_errors": artist_errors,
    }))
}

pub fn reindex_artist_links(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
) -> Result<Value> {
    ensure_link_schema(conn)?;
    reindex_artist_links_inner(conn, roots, artist_id, None)
}

/// Reindex only the given items, grouped by artist.
///
/// This is the incremental path used when a hash batch resolves text items: the
/// caller knows exactly which items changed, so there is no reason to walk every
/// text the artist owns. See the comment in `reindex_artist_links_inner` for the
/// measurement and for why the full reindex after a complete scan still covers
/// the rest.
pub fn reindex_scanned_items_links(
    conn: &Connection,
    roots: &MediaRoots,
    items_by_artist: &BTreeMap<i64, BTreeSet<i64>>,
) -> Result<Value> {
    ensure_link_schema(conn)?;
    let mut indexed_documents = 0i64;
    let mut skipped_documents = 0i64;
    let mut links = 0i64;
    let mut errors = 0i64;
    let mut artist_errors = Vec::new();

    for (artist_id, item_ids) in items_by_artist {
        if *artist_id <= 0 || item_ids.is_empty() {
            continue;
        }
        match reindex_artist_links_inner(conn, roots, *artist_id, Some(item_ids)) {
            Ok(result) => {
                indexed_documents += result["indexed_documents"].as_i64().unwrap_or(0);
                skipped_documents += result["skipped_documents"].as_i64().unwrap_or(0);
                links += result["links"].as_i64().unwrap_or(0);
                errors += result["errors"].as_i64().unwrap_or(0);
            }
            Err(error) => {
                errors += 1;
                artist_errors.push(json!({"artist_id": *artist_id, "error": error.to_string()}));
            }
        }
    }

    Ok(json!({
        "ok": artist_errors.is_empty(),
        "artists": items_by_artist.len(),
        "indexed_documents": indexed_documents,
        "skipped_documents": skipped_documents,
        "links": links,
        "errors": errors,
        "artist_errors": artist_errors,
    }))
}

/// Refresh `file_path` / `file_name` / `folder_name` for documents whose source
/// row moved, without stat'ing anything.
///
/// Deliberately narrow: it never touches `file_size` or `file_mtime`, because
/// those are the values a full reindex compares against the filesystem, and
/// writing them from `items` (which is only as fresh as the last scan) would
/// hide a real content change.
fn sync_renamed_documents(conn: &Connection, artist_id: i64) -> Result<i64> {
    // `idx_artist_link_documents_path` is UNIQUE on (artist_id, file_path), so a
    // rename that would collide with another document's path is left alone
    // rather than failing the whole update.
    let mut stmt = conn.prepare(
        "SELECT d.item_id, i.file_path, i.file_name, COALESCE(i.folder_name, '')
         FROM artist_link_documents d
         JOIN items i ON i.id = d.item_id
         WHERE d.artist_id = ?1
           AND (
               d.file_path <> i.file_path
               OR d.file_name <> i.file_name
               OR d.folder_name <> COALESCE(i.folder_name, '')
           )
           AND NOT EXISTS (
               SELECT 1 FROM artist_link_documents other
               WHERE other.artist_id = d.artist_id
                 AND other.file_path = i.file_path
                 AND other.item_id <> d.item_id
           )",
    )?;
    let drifted: Vec<(i64, String, String, String)> = stmt
        .query_map(params![artist_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let count = drifted.len() as i64;
    for (item_id, file_path, file_name, folder_name) in drifted {
        conn.execute(
            "UPDATE artist_link_documents
             SET file_path=?, file_name=?, folder_name=?
             WHERE item_id=?",
            params![file_path, file_name, folder_name, item_id],
        )?;
    }
    Ok(count)
}

fn reindex_artist_links_inner(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    scope: Option<&BTreeSet<i64>>,
) -> Result<Value> {
    let items = list_source_items(conn, artist_id)?;
    let mut indexed_documents = 0i64;
    let mut skipped_documents = 0i64;
    let mut links = 0i64;
    let mut errors = 0i64;
    let mut failures = Vec::new();

    for mut item in items {
        // §9.2: a scoped run only touches the items the caller just changed.
        // Everything else for this artist was already indexed and has not been
        // modified since -- content changes and renames reach this code through
        // the full reindex that runs after a *complete* scan, which is how
        // changed files are discovered in the first place. Without the scope,
        // indexing one new text re-stat'ed and re-queried every text the artist
        // already had: measured linear in that count, 485ms of a 490ms call at
        // N=2000, and the useful part is one document.
        if let Some(scope) = scope {
            if !scope.contains(&item.id) {
                continue;
            }
        }
        let real_path = match roots.map_to_real(&item.file_path) {
            Ok(path) => path,
            Err(error) => {
                record_document_error(conn, artist_id, &item, &error.to_string())?;
                errors += 1;
                failures.push(json!({"item_id": item.id, "file_path": item.file_path, "error": error.to_string()}));
                continue;
            }
        };
        if !path_under_authorized_roots(&real_path, roots) {
            let error = "source document is outside authorized media roots";
            record_document_error(conn, artist_id, &item, error)?;
            errors += 1;
            failures.push(json!({"item_id": item.id, "file_path": item.file_path, "error": error}));
            continue;
        }
        let metadata = match source_file_metadata(&real_path) {
            Ok(metadata) => metadata,
            Err(error) => {
                record_document_error(conn, artist_id, &item, &error.to_string())?;
                errors += 1;
                failures.push(json!({"item_id": item.id, "file_path": item.file_path, "error": error.to_string()}));
                continue;
            }
        };
        item.file_size = metadata.len() as i64;
        item.file_mtime = metadata
            .modified()
            .ok()
            .and_then(|mtime| mtime.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|duration| duration.as_secs_f64())
            .unwrap_or(0.0);
        let existing = existing_document(conn, item.id)?;
        if let Some(ref document) = existing {
            let mtime_matches = (document.file_mtime - item.file_mtime).abs() <= 0.0001;
            if document.file_size == item.file_size
                && mtime_matches
                && document.parse_status == "ready"
            {
                let metadata_changed = document.file_path != item.file_path
                    || document.file_name != item.file_name
                    || document.folder_name != item.folder_name;
                if metadata_changed {
                    update_document_metadata(conn, &item)?;
                }
                skipped_documents += 1;
                continue;
            }
        }
        let bytes = match read_source_file(&real_path) {
            Ok(bytes) => bytes,
            Err(error) => {
                record_document_error(conn, artist_id, &item, &error.to_string())?;
                errors += 1;
                failures.push(json!({"item_id": item.id, "file_path": item.file_path, "error": error.to_string()}));
                continue;
            }
        };

        let kind = source_kind(&item.file_name).unwrap_or("txt");
        let text = decode_source_text(&bytes, kind == "html");
        let extracted = if kind == "html" {
            extract_html_links(&text)
        } else {
            extract_text_links(&text)
        };
        let content_hash = blake3::hash(&bytes).to_hex().to_string();
        replace_document_links(conn, artist_id, &item, kind, &content_hash, &extracted)?;
        indexed_documents += 1;
        links += extracted.len() as i64;
    }

    if scope.is_some() {
        // A scoped run still has to keep display metadata honest for the rows it
        // skipped, otherwise renaming an already-indexed text would leave the
        // stored path stale until the next full reindex. This is one query per
        // artist, not one per skipped document.
        sync_renamed_documents(conn, artist_id)?;
    }
    prune_stale_documents(conn, artist_id)?;
    Ok(json!({
        "ok": true,
        "artist_id": artist_id,
        "indexed_documents": indexed_documents,
        "skipped_documents": skipped_documents,
        "links": links,
        "errors": errors,
        "failures": failures,
    }))
}

pub fn artist_links_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    if !link_schema_exists(conn)? {
        return Ok(empty_artist_links_response(artist_id));
    }
    let documents = list_link_documents(conn, artist_id)?;
    let mut groups: BTreeMap<String, LinkAccumulator> = BTreeMap::new();
    let mut stmt = conn.prepare(
        "SELECT o.normalized_url, o.raw_url, o.host, o.label, o.passcode, o.line_number, o.context,
                d.item_id, d.file_path, d.file_name, d.folder_name, d.file_kind
         FROM artist_link_occurrences o
         JOIN artist_link_documents d ON d.id=o.document_id
         WHERE d.artist_id=? AND d.parse_status='ready'
         ORDER BY o.normalized_url, d.file_name, o.line_number",
    )?;
    let rows = stmt.query_map(params![artist_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, i64>(5)?,
            row.get::<_, String>(6)?,
            row.get::<_, i64>(7)?,
            row.get::<_, String>(8)?,
            row.get::<_, String>(9)?,
            row.get::<_, String>(10)?,
            row.get::<_, String>(11)?,
        ))
    })?;

    for row in rows {
        let (
            normalized_url,
            raw_url,
            host,
            label,
            passcode,
            line_number,
            context,
            item_id,
            file_path,
            file_name,
            folder_name,
            file_kind,
        ) = row?;
        let entry = groups
            .entry(normalized_url)
            .or_insert_with(|| LinkAccumulator {
                display_url: raw_url,
                host,
                passcodes: BTreeSet::new(),
                sources: Vec::new(),
            });
        if !passcode.is_empty() {
            entry.passcodes.insert(passcode.clone());
        }
        entry.sources.push(LinkSourceResponse {
            item_id,
            file_path,
            file_name,
            folder_name,
            file_kind,
            line_number,
            label,
            passcode,
            context,
        });
    }

    let mut cloud_drives = 0usize;
    let mut links = Vec::with_capacity(groups.len());
    let domain_rules = load_domain_rules(conn)?;
    for (url, group) in groups {
        let classification = classify_host_with_rules(&group.host, &domain_rules);
        if classification.category == "cloud_drive" {
            cloud_drives += 1;
        }
        // Passcodes are only meaningful for cloud drives. Plain links can
        // otherwise absorb a nearby "提取码/密码/passcode" token in their
        // extracted context (for example kemono.su post pages), so expose
        // neither for non-cloud links. Custom artist_link_domain_rules still
        // apply and can upgrade a domain to cloud_drive.
        let expose_passcodes = classification.category == "cloud_drive";
        let passcodes = if expose_passcodes {
            group.passcodes.into_iter().collect()
        } else {
            Vec::new()
        };
        let sources = if expose_passcodes {
            group.sources
        } else {
            group
                .sources
                .into_iter()
                .map(|mut source| {
                    source.passcode.clear();
                    source.context.clear();
                    source
                })
                .collect()
        };
        links.push(ArtistLinkResponse {
            url,
            display_url: group.display_url,
            host: group.host,
            category: classification.category,
            provider_name: classification.provider_name,
            availability: "unverified",
            passcodes,
            source_count: sources.len(),
            sources,
        });
    }

    let parse_errors = documents
        .iter()
        .filter(|document| document.parse_status == "error")
        .count();
    Ok(json!({
        "artist_id": artist_id,
        "summary": {
            "links": links.len(),
            "cloud_drives": cloud_drives,
            "documents": documents.len(),
            "errors": parse_errors,
        },
        "links": links,
        "documents": documents,
    }))
}

fn link_schema_exists(conn: &Connection) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='artist_link_documents')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn empty_artist_links_response(artist_id: i64) -> Value {
    json!({
        "artist_id": artist_id,
        "summary": {"links": 0, "cloud_drives": 0, "documents": 0, "errors": 0},
        "links": [],
        "documents": [],
    })
}

fn list_source_items(conn: &Connection, artist_id: i64) -> Result<Vec<SourceItem>> {
    let mut stmt = conn.prepare(
        "SELECT id, file_path, file_name, file_size, file_mtime, folder_name
         FROM items
         WHERE artist_id=? AND missing=0 AND media_type='text'
         ORDER BY file_path",
    )?;
    let rows = stmt
        .query_map(params![artist_id], |row| {
            Ok(SourceItem {
                id: row.get(0)?,
                file_path: row.get(1)?,
                file_name: row.get(2)?,
                file_size: row.get(3)?,
                file_mtime: row.get(4)?,
                folder_name: row.get(5)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows
        .into_iter()
        .filter(|item| source_kind(&item.file_name).is_some())
        .collect())
}

fn existing_document(conn: &Connection, item_id: i64) -> Result<Option<ExistingDocument>> {
    conn.query_row(
        "SELECT file_path, file_name, folder_name, file_size, file_mtime, parse_status FROM artist_link_documents WHERE item_id=?",
        params![item_id],
        |row| {
            Ok(ExistingDocument {
                file_path: row.get(0)?,
                file_name: row.get(1)?,
                folder_name: row.get(2)?,
                file_size: row.get(3)?,
                file_mtime: row.get(4)?,
                parse_status: row.get(5)?,
            })
        },
    )
    .optional()
    .map_err(Into::into)
}

fn update_document_metadata(conn: &Connection, item: &SourceItem) -> Result<()> {
    conn.execute(
        "UPDATE artist_link_documents
         SET file_path=?, file_name=?, folder_name=?, file_size=?, file_mtime=?
         WHERE item_id=?",
        params![
            item.file_path,
            item.file_name,
            item.folder_name,
            item.file_size,
            item.file_mtime,
            item.id,
        ],
    )?;
    Ok(())
}

fn record_document_error(
    conn: &Connection,
    artist_id: i64,
    item: &SourceItem,
    error: &str,
) -> Result<()> {
    let kind = source_kind(&item.file_name).unwrap_or("txt");
    conn.execute(
        "INSERT INTO artist_link_documents
         (artist_id, item_id, file_path, file_name, folder_name, file_kind, file_size, file_mtime,
          parse_status, parse_error, indexed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, 'error', ?, strftime('%s','now'))
         ON CONFLICT(item_id) DO UPDATE SET
          artist_id=excluded.artist_id, file_path=excluded.file_path, file_name=excluded.file_name,
          folder_name=excluded.folder_name, file_kind=excluded.file_kind, file_size=excluded.file_size,
          file_mtime=excluded.file_mtime, parse_status='error', parse_error=excluded.parse_error,
          indexed_at=excluded.indexed_at",
        params![
            artist_id,
            item.id,
            item.file_path,
            item.file_name,
            item.folder_name,
            kind,
            item.file_size,
            item.file_mtime,
            truncate_text(error, MAX_CONTEXT_CHARS),
        ],
    )?;
    Ok(())
}

fn replace_document_links(
    conn: &Connection,
    artist_id: i64,
    item: &SourceItem,
    kind: &str,
    content_hash: &str,
    links: &[ExtractedLink],
) -> Result<()> {
    let tx = conn.unchecked_transaction()?;
    tx.execute(
        "INSERT INTO artist_link_documents
         (artist_id, item_id, file_path, file_name, folder_name, file_kind, file_size, file_mtime,
          content_hash, parse_status, parse_error, link_count, indexed_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'ready', '', ?, strftime('%s','now'))
         ON CONFLICT(item_id) DO UPDATE SET
          artist_id=excluded.artist_id, file_path=excluded.file_path, file_name=excluded.file_name,
          folder_name=excluded.folder_name, file_kind=excluded.file_kind, file_size=excluded.file_size,
          file_mtime=excluded.file_mtime, content_hash=excluded.content_hash, parse_status='ready',
          parse_error='', link_count=excluded.link_count, indexed_at=excluded.indexed_at",
        params![
            artist_id,
            item.id,
            item.file_path,
            item.file_name,
            item.folder_name,
            kind,
            item.file_size,
            item.file_mtime,
            content_hash,
            links.len() as i64,
        ],
    )?;
    let document_id: i64 = tx.query_row(
        "SELECT id FROM artist_link_documents WHERE item_id=?",
        params![item.id],
        |row| row.get(0),
    )?;
    tx.execute(
        "DELETE FROM artist_link_occurrences WHERE document_id=?",
        params![document_id],
    )?;
    let mut insert = tx.prepare(
        "INSERT INTO artist_link_occurrences
         (document_id, normalized_url, raw_url, host, label, passcode, line_number, context)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
    )?;
    for link in links {
        insert.execute(params![
            document_id,
            link.normalized_url,
            link.raw_url,
            link.host,
            link.label,
            link.passcode,
            link.line_number,
            link.context,
        ])?;
    }
    drop(insert);
    tx.commit()?;
    Ok(())
}

/// Drop link documents whose item row is permanently gone (deleted outright,
/// or replaced by a different item row). Documents survive a temporary
/// `missing=1` mark so a mount blip cannot CASCADE-clear the artist's whole
/// link index; the file is re-parsed only when its metadata changed after it
/// returns.
fn prune_stale_documents(conn: &Connection, artist_id: i64) -> Result<()> {
    conn.execute(
        "DELETE FROM artist_link_documents
         WHERE artist_id=?
           AND NOT EXISTS (SELECT 1 FROM items WHERE items.id=artist_link_documents.item_id)",
        params![artist_id],
    )?;
    Ok(())
}

fn list_link_documents(conn: &Connection, artist_id: i64) -> Result<Vec<LinkDocumentResponse>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, file_path, file_name, folder_name, file_kind, file_size, file_mtime,
                parse_status, parse_error, link_count, indexed_at
         FROM artist_link_documents
         WHERE artist_id=?
         ORDER BY file_path",
    )?;
    let rows = stmt
        .query_map(params![artist_id], |row| {
            Ok(LinkDocumentResponse {
                item_id: row.get(0)?,
                file_path: row.get(1)?,
                file_name: row.get(2)?,
                folder_name: row.get(3)?,
                file_kind: row.get(4)?,
                file_size: row.get(5)?,
                file_mtime: row.get(6)?,
                parse_status: row.get(7)?,
                parse_error: row.get(8)?,
                link_count: row.get(9)?,
                indexed_at: row.get(10)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn read_source_file(path: &Path) -> Result<Vec<u8>> {
    let metadata = source_file_metadata(path)?;
    if metadata.len() > MAX_SOURCE_BYTES {
        return Err(anyhow!(
            "source document exceeds {} MiB index limit",
            MAX_SOURCE_BYTES / (1024 * 1024)
        ));
    }
    fs::read(path).with_context(|| format!("read source document {}", path.display()))
}

fn source_file_metadata(path: &Path) -> Result<fs::Metadata> {
    let metadata =
        fs::metadata(path).with_context(|| format!("stat source document {}", path.display()))?;
    if !metadata.is_file() {
        return Err(anyhow!("source document is not a regular file"));
    }
    Ok(metadata)
}

fn source_kind(file_name: &str) -> Option<&'static str> {
    let extension = file_name.rsplit('.').next()?.to_ascii_lowercase();
    match extension.as_str() {
        "txt" => Some("txt"),
        "html" | "htm" => Some("html"),
        _ => None,
    }
}

pub(crate) fn is_link_source_file(file_name: &str) -> bool {
    source_kind(file_name).is_some()
}

fn decode_source_text(bytes: &[u8], is_html: bool) -> String {
    if bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return UTF_8
            .decode_without_bom_handling(&bytes[3..])
            .0
            .into_owned();
    }
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return UTF_16LE
            .decode_without_bom_handling(&bytes[2..])
            .0
            .into_owned();
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return UTF_16BE
            .decode_without_bom_handling(&bytes[2..])
            .0
            .into_owned();
    }
    if is_html {
        if let Some(encoding) = html_declared_encoding(bytes) {
            return encoding.decode(bytes).0.into_owned();
        }
    }
    if let Ok(text) = std::str::from_utf8(bytes) {
        return text.to_string();
    }
    GB18030.decode(bytes).0.into_owned()
}

fn html_declared_encoding(bytes: &[u8]) -> Option<&'static Encoding> {
    static CHARSET: OnceLock<Regex> = OnceLock::new();
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(8192)]);
    let pattern = CHARSET
        .get_or_init(|| Regex::new(r#"(?is)charset\s*=\s*[\"']?\s*([A-Za-z0-9._-]+)"#).unwrap());
    pattern
        .captures(&head)
        .and_then(|captures| captures.get(1))
        .and_then(|capture| Encoding::for_label(capture.as_str().as_bytes()))
}

/// Line numbers for one document text, computed in a single pass. Per-link
/// rescans from byte 0 are quadratic on large source lists.
struct LineIndex {
    newlines: Vec<usize>,
}

impl LineIndex {
    fn new(text: &str) -> Self {
        Self {
            newlines: text
                .bytes()
                .enumerate()
                .filter(|(_, byte)| *byte == b'\n')
                .map(|(index, _)| index)
                .collect(),
        }
    }

    fn line_at(&self, byte_index: usize) -> i64 {
        self.newlines
            .partition_point(|&position| position < byte_index) as i64
            + 1
    }
}

fn extract_text_links(text: &str) -> Vec<ExtractedLink> {
    static URL: OnceLock<Regex> = OnceLock::new();
    let pattern = URL.get_or_init(|| Regex::new(r#"(?i)\bhttps?://[^\s<>\"'`]+"#).unwrap());
    let lines = LineIndex::new(text);
    let mut links = Vec::new();
    for matched in pattern.find_iter(text) {
        let raw = trim_url_punctuation(matched.as_str());
        let context = compact_text(
            &context_at(text, matched.start(), matched.end()),
            MAX_CONTEXT_CHARS,
        );
        if let Some(link) =
            normalize_link(raw, String::new(), lines.line_at(matched.start()), context)
        {
            links.push(link);
        }
    }
    dedupe_links(links)
}

fn extract_html_links(text: &str) -> Vec<ExtractedLink> {
    let lines = LineIndex::new(text);
    let mut links = Vec::new();
    let mut cursor = 0usize;
    while cursor < text.len() {
        let Some(relative) = find_ascii_case_insensitive(&text[cursor..], "<a") else {
            break;
        };
        let start = cursor + relative;
        let name_end = start + 2;
        if !is_tag_name_boundary(text, name_end) {
            cursor = name_end;
            continue;
        }
        let Some(tag_end) = find_tag_end(text, name_end) else {
            break;
        };
        let attributes = &text[name_end..tag_end];
        let self_closing = attributes.trim_end().ends_with('/');
        let close_start = if self_closing {
            tag_end + 1
        } else {
            find_ascii_case_insensitive(&text[tag_end + 1..], "</a")
                .map(|offset| tag_end + 1 + offset)
                .unwrap_or(tag_end + 1)
        };
        let label = if close_start > tag_end + 1 {
            compact_text(
                &strip_html_markup(&text[tag_end + 1..close_start]),
                MAX_LABEL_CHARS,
            )
        } else {
            String::new()
        };
        if let Some(href) = html_attribute(attributes, "href") {
            let context_end = if close_start > tag_end + 1 {
                close_start
            } else {
                tag_end + 1
            };
            let context = compact_text(&context_at(text, start, context_end), MAX_CONTEXT_CHARS);
            if let Some(link) = normalize_link(&href, label, lines.line_at(start), context) {
                links.push(link);
            }
        }
        cursor = if close_start > tag_end + 1 {
            find_tag_end(text, close_start + 3)
                .map(|end| end + 1)
                .unwrap_or(close_start + 3)
        } else {
            tag_end + 1
        };
    }

    links.extend(extract_text_links(&strip_html_markup(text)));
    dedupe_links(links)
}

fn normalize_link(
    raw: &str,
    label: String,
    line_number: i64,
    context: String,
) -> Option<ExtractedLink> {
    let raw_url = decode_html_entities(trim_url_punctuation(raw).trim());
    let parsed = Url::parse(&raw_url).ok()?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return None;
    }
    let host = parsed.host_str()?.to_ascii_lowercase();
    Some(ExtractedLink {
        normalized_url: parsed.as_str().to_string(),
        raw_url,
        host,
        label: compact_text(&label, MAX_LABEL_CHARS),
        passcode: extract_passcode(&parsed, &context),
        line_number,
        context,
    })
}

fn extract_passcode(url: &Url, context: &str) -> String {
    for (key, value) in url.query_pairs() {
        let key = key.to_ascii_lowercase();
        if matches!(
            key.as_str(),
            "pwd" | "password" | "passcode" | "code" | "key" | "access_code"
        ) && is_passcode(&value)
        {
            return value.into_owned();
        }
    }

    static PASSCODE: OnceLock<Regex> = OnceLock::new();
    let pattern = PASSCODE.get_or_init(|| {
        Regex::new(
            r#"(?i)(?:提取码|访问码|密码|解压密码|passcode|password|access[ _-]?code|code|pwd|key)\s*[:：=]?\s*([A-Za-z0-9_-]{3,64})"#,
        )
        .unwrap()
    });
    pattern
        .captures(context)
        .and_then(|captures| captures.get(1))
        .map(|capture| capture.as_str().to_string())
        .filter(|value| is_passcode(value))
        .unwrap_or_default()
}

fn is_passcode(value: &str) -> bool {
    let length = value.chars().count();
    (3..=64).contains(&length)
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || ch == '_' || ch == '-')
}

fn dedupe_links(links: Vec<ExtractedLink>) -> Vec<ExtractedLink> {
    let mut by_location: BTreeMap<(String, i64), ExtractedLink> = BTreeMap::new();
    for link in links {
        let key = (link.normalized_url.clone(), link.line_number);
        match by_location.get_mut(&key) {
            Some(existing) => {
                if existing.label.is_empty() && !link.label.is_empty() {
                    existing.label = link.label;
                }
                if existing.passcode.is_empty() && !link.passcode.is_empty() {
                    existing.passcode = link.passcode;
                }
            }
            None => {
                by_location.insert(key, link);
            }
        }
    }
    by_location.into_values().collect()
}

fn trim_url_punctuation(value: &str) -> &str {
    let mut trimmed = value.trim();
    while let Some(last) = trimmed.chars().last() {
        let remove = match last {
            '.' | ',' | ';' | ':' | '!' | '?' | '，' | '。' | '；' | '：' | '！' | '？' | '、' => {
                true
            }
            ')' => trimmed.matches(')').count() > trimmed.matches('(').count(),
            ']' => trimmed.matches(']').count() > trimmed.matches('[').count(),
            '}' => trimmed.matches('}').count() > trimmed.matches('{').count(),
            _ => false,
        };
        if !remove {
            break;
        }
        trimmed = &trimmed[..trimmed.len() - last.len_utf8()];
    }
    trimmed
}

fn context_at(text: &str, start: usize, end: usize) -> String {
    if text.is_empty() {
        return String::new();
    }
    let start = start.min(text.len());
    let end = end.min(text.len()).max(start);

    let safe_start = (0..=start)
        .rev()
        .find(|&idx| text.is_char_boundary(idx))
        .unwrap_or(0);
    let safe_end = (end..=text.len())
        .find(|&idx| text.is_char_boundary(idx))
        .unwrap_or(text.len());

    let prefix_start = text[..safe_start]
        .char_indices()
        .rev()
        .nth(120)
        .map(|(index, _)| index)
        .unwrap_or(0);
    let suffix_end = text[safe_end..]
        .char_indices()
        .nth(240)
        .map(|(index, _)| safe_end + index)
        .unwrap_or(text.len());
    let safe_prefix = prefix_start.min(suffix_end);
    let safe_suffix = suffix_end.max(safe_prefix);
    text.get(safe_prefix..safe_suffix).unwrap_or("").to_string()
}

fn compact_text(value: &str, max_chars: usize) -> String {
    let compact = value.split_whitespace().collect::<Vec<_>>().join(" ");
    truncate_text(&compact, max_chars)
}

fn truncate_text(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let mut output = String::new();
    for _ in 0..max_chars {
        let Some(ch) = chars.next() else {
            return output;
        };
        output.push(ch);
    }
    if chars.next().is_some() {
        output.push_str("...");
    }
    output
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    let needle = needle.as_bytes();
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| {
            window
                .iter()
                .zip(needle)
                .all(|(left, right)| left.eq_ignore_ascii_case(right))
        })
}

fn is_tag_name_boundary(text: &str, index: usize) -> bool {
    text.as_bytes()
        .get(index)
        .map(|byte| byte.is_ascii_whitespace() || matches!(*byte, b'/' | b'>'))
        .unwrap_or(true)
}

fn find_tag_end(text: &str, start: usize) -> Option<usize> {
    let bytes = text.as_bytes();
    let slice = bytes.get(start..)?;
    let mut quote = None;
    for (offset, byte) in slice.iter().enumerate() {
        match quote {
            Some(active) if *byte == active => quote = None,
            Some(_) => {}
            None if matches!(*byte, b'\'' | b'\"') => quote = Some(*byte),
            None if *byte == b'>' => return Some(start + offset),
            None => {}
        }
    }
    None
}

fn html_attribute(attributes: &str, wanted: &str) -> Option<String> {
    let bytes = attributes.as_bytes();
    let mut index = 0usize;
    while index < bytes.len() {
        while index < bytes.len() && (bytes[index].is_ascii_whitespace() || bytes[index] == b'/') {
            index += 1;
        }
        let name_start = index;
        while index < bytes.len()
            && (bytes[index].is_ascii_alphanumeric() || matches!(bytes[index], b':' | b'_' | b'-'))
        {
            index += 1;
        }
        if name_start == index {
            index += 1;
            continue;
        }
        let name = &attributes[name_start..index];
        while index < bytes.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
        let mut value = None;
        if index < bytes.len() && bytes[index] == b'=' {
            index += 1;
            while index < bytes.len() && bytes[index].is_ascii_whitespace() {
                index += 1;
            }
            let value_start = index;
            if index < bytes.len() && matches!(bytes[index], b'\'' | b'\"') {
                let quote = bytes[index];
                index += 1;
                let quoted_start = index;
                while index < bytes.len() && bytes[index] != quote {
                    index += 1;
                }
                value = Some(&attributes[quoted_start..index]);
                if index < bytes.len() {
                    index += 1;
                }
            } else {
                // Unquoted attribute values may contain `/` per the HTML spec
                // (e.g. href=https://x.com/a); only whitespace terminates.
                while index < bytes.len() && !bytes[index].is_ascii_whitespace() {
                    index += 1;
                }
                value = Some(&attributes[value_start..index]);
            }
        }
        if name.eq_ignore_ascii_case(wanted) {
            return value.map(decode_html_entities);
        }
    }
    None
}

fn strip_html_markup(value: &str) -> String {
    let without_scripts = remove_html_block(value, "script");
    let without_styles = remove_html_block(&without_scripts, "style");
    let mut output = String::with_capacity(without_styles.len());
    let mut inside_tag = false;
    let mut quote = None;
    for ch in without_styles.chars() {
        if inside_tag {
            match quote {
                Some(active) if ch == active => quote = None,
                Some(_) => {}
                None if matches!(ch, '\'' | '\"') => quote = Some(ch),
                None if ch == '>' => {
                    inside_tag = false;
                    output.push(' ');
                }
                None => {}
            }
        } else if ch == '<' {
            inside_tag = true;
            output.push(' ');
        } else {
            output.push(ch);
        }
    }
    decode_html_entities(&output)
}

fn remove_html_block(value: &str, tag: &str) -> String {
    let lower = value.to_ascii_lowercase();
    let open = format!("<{tag}");
    let close = format!("</{tag}");
    let mut output = String::with_capacity(value.len());
    let mut cursor = 0usize;
    while let Some(relative) = lower[cursor..].find(&open) {
        let start = cursor + relative;
        output.push_str(&value[cursor..start]);
        let Some(close_relative) = lower[start..].find(&close) else {
            return output;
        };
        let close_start = start + close_relative;
        let Some(close_end_relative) = lower[close_start..].find('>') else {
            return output;
        };
        cursor = close_start + close_end_relative + 1;
    }
    output.push_str(&value[cursor..]);
    output
}

fn decode_html_entities(value: &str) -> String {
    let mut output = String::with_capacity(value.len());
    let mut rest = value;
    while let Some(start) = rest.find('&') {
        output.push_str(&rest[..start]);
        let entity_start = start + 1;
        let slice_to_search = &rest[entity_start..];
        let mut max_scan = slice_to_search.len().min(12);
        while !slice_to_search.is_char_boundary(max_scan) {
            max_scan -= 1;
        }
        let candidate_slice = &slice_to_search[..max_scan];
        if let Some(semi_pos) = candidate_slice.find(';') {
            let entity = &candidate_slice[..semi_pos];
            if !entity.is_empty()
                && !entity.contains(|c: char| c.is_whitespace() || c == '&' || c == '<')
            {
                let decoded = match entity {
                    "amp" => Some('&'),
                    "quot" => Some('\"'),
                    "apos" | "#39" => Some('\''),
                    "lt" => Some('<'),
                    "gt" => Some('>'),
                    "nbsp" => Some(' '),
                    _ => entity
                        .strip_prefix("#x")
                        .or_else(|| entity.strip_prefix("#X"))
                        .and_then(|number| u32::from_str_radix(number, 16).ok())
                        .or_else(|| {
                            entity
                                .strip_prefix('#')
                                .and_then(|number| number.parse::<u32>().ok())
                        })
                        .and_then(char::from_u32),
                };
                if let Some(ch) = decoded {
                    output.push(ch);
                    rest = &slice_to_search[semi_pos + 1..];
                    continue;
                }
            }
        }
        output.push('&');
        rest = slice_to_search;
    }
    output.push_str(rest);
    output
}

/// Load custom domain rules once per response instead of one query per URL.
fn load_domain_rules(conn: &Connection) -> Result<Vec<(String, String, String)>> {
    let mut stmt =
        conn.prepare("SELECT domain, category, provider_name FROM artist_link_domain_rules")?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn classify_host_with_rules(host: &str, rules: &[(String, String, String)]) -> LinkClassification {
    // Longest matching domain wins, mirroring the previous
    // `ORDER BY length(domain) DESC LIMIT 1` SQL.
    let matched = rules
        .iter()
        .filter(|(domain, _, _)| host == domain || host.ends_with(&format!(".{domain}")))
        .max_by_key(|(domain, _, _)| domain.len());
    if let Some((_, category, provider_name)) = matched {
        return LinkClassification {
            category: category.clone(),
            provider_name: if provider_name.is_empty() {
                host.to_string()
            } else {
                provider_name.clone()
            },
        };
    }

    const CLOUD_DRIVES: &[(&str, &str)] = &[
        ("pan.baidu.com", "百度网盘"),
        ("yun.baidu.com", "百度网盘"),
        ("alipan.com", "阿里云盘"),
        ("aliyundrive.com", "阿里云盘"),
        ("pan.quark.cn", "夸克网盘"),
        ("123pan.com", "123 云盘"),
        ("lanzoue.com", "蓝奏云"),
        ("lanzou.com", "蓝奏云"),
        ("weiyun.com", "微云"),
        ("cloud.189.cn", "天翼云盘"),
        ("drive.uc.cn", "UC 网盘"),
        ("drive.google.com", "Google Drive"),
        ("docs.google.com", "Google Drive"),
        ("dropbox.com", "Dropbox"),
        ("db.tt", "Dropbox"),
        ("onedrive.live.com", "OneDrive"),
        ("1drv.ms", "OneDrive"),
        ("sharepoint.com", "OneDrive"),
        ("mega.nz", "MEGA"),
        ("mega.io", "MEGA"),
        ("box.com", "Box"),
        ("pcloud.com", "pCloud"),
        ("mediafire.com", "MediaFire"),
        ("drive.proton.me", "Proton Drive"),
        ("icloud.com", "iCloud Drive"),
        ("yadi.sk", "Yandex Disk"),
        ("disk.yandex.com", "Yandex Disk"),
        ("terabox.com", "TeraBox"),
        ("gofile.io", "GoFile"),
        ("wetransfer.com", "WeTransfer"),
    ];
    for (domain, provider_name) in CLOUD_DRIVES {
        if host == *domain || host.ends_with(&format!(".{domain}")) {
            return LinkClassification {
                category: "cloud_drive".to_string(),
                provider_name: (*provider_name).to_string(),
            };
        }
    }
    LinkClassification {
        category: "other".to_string(),
        provider_name: host.to_string(),
    }
}
