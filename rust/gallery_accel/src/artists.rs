use anyhow::Result;
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::natural_sort::natural_compare;
use crate::pinyin_search::search_text_for_values;

#[derive(Clone, Serialize, Debug)]
struct ArtistRow {
    id: i64,
    name: String,
    path: String,
    missing: i64,
    missing_at: Option<f64>,
    created_at: i64,
    item_count: i64,
    /// Pinyin / search haystack (Python `list_artists` adds this for UI search).
    search_text: String,
}

#[derive(Clone, Serialize, Debug)]
struct ArtistDetailRow {
    id: i64,
    name: String,
    path: String,
    missing: i64,
    missing_at: Option<f64>,
    created_at: i64,
}

/// Historical FastAPI contract: bare JSON **array** of artists (not `{artists:[]}`).
/// Static UI does `state.artists = asArray(await API.get('/api/artists'))`.
struct CachedArtists {
    response: Value,
    cached_at: Instant,
}

static ARTISTS_CACHE: OnceLock<Mutex<Option<CachedArtists>>> = OnceLock::new();
const ARTISTS_CACHE_TTL: Duration = Duration::from_secs(10);

pub fn invalidate_artists_cache() {
    if let Some(mutex) = ARTISTS_CACHE.get() {
        if let Ok(mut guard) = mutex.lock() {
            *guard = None;
        }
    }
}

pub fn artists_response_cached(conn: &Connection) -> Result<Value> {
    let mutex = ARTISTS_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = mutex.lock() {
        if let Some(cached) = &*guard {
            if cached.cached_at.elapsed() < ARTISTS_CACHE_TTL {
                return Ok(cached.response.clone());
            }
        }
    }
    let res = json!(list_artists(conn)?);
    if let Ok(mut guard) = mutex.lock() {
        *guard = Some(CachedArtists {
            response: res.clone(),
            cached_at: Instant::now(),
        });
    }
    Ok(res)
}

pub fn artists_response(conn: &Connection) -> Result<Value> {

    #[cfg(not(test))]
    {
        artists_response_cached(conn)
    }
}

pub fn artist_detail_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    Ok(json!({ "artist": get_artist_detail(conn, artist_id)? }))
}

fn list_artists(conn: &Connection) -> Result<Vec<ArtistRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT a.id, a.name, a.path, a.missing, a.missing_at, a.created_at,
               COUNT(i.id) AS item_count
        FROM artists a
        LEFT JOIN items i ON i.artist_id = a.id
            AND i.missing = 0
            AND (i.media_type IN ('image', 'video', 'source', 'archive', 'text') OR i.is_archive = 1)
        WHERE a.missing = 0
        GROUP BY a.id
        ",
    )?;
    let mut artists = stmt
        .query_map([], |row| {
            let name: String = row.get("name")?;
            let search_text = search_text_for_values(&[&name]);
            Ok(ArtistRow {
                id: row.get("id")?,
                name,
                path: row.get("path")?,
                missing: row.get("missing")?,
                missing_at: row.get("missing_at")?,
                created_at: row.get("created_at")?,
                item_count: row.get("item_count")?,
                search_text,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    artists.sort_by(|left, right| natural_compare(&left.name, &right.name));
    Ok(artists)
}

fn get_artist_detail(conn: &Connection, artist_id: i64) -> Result<Option<ArtistDetailRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT id, name, path, missing, missing_at, created_at
        FROM artists
        WHERE id=?
        ",
    )?;
    let mut rows = stmt.query([artist_id])?;
    let Some(row) = rows.next()? else {
        return Ok(None);
    };
    Ok(Some(ArtistDetailRow {
        id: row.get("id")?,
        name: row.get("name")?,
        path: row.get("path")?,
        missing: row.get("missing")?,
        missing_at: row.get("missing_at")?,
        created_at: row.get("created_at")?,
    }))
}
