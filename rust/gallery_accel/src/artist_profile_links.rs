//! Manually maintained social and subscription links for an artist.

use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::Serialize;
use serde_json::{json, Value};
use url::Url;

const MAX_PLATFORM_CHARS: usize = 120;
const MAX_URL_CHARS: usize = 4096;

#[derive(Serialize)]
struct ArtistProfileLink {
    id: i64,
    artist_id: i64,
    kind: String,
    platform: String,
    url: String,
    host: String,
    created_at: f64,
    updated_at: f64,
}

pub(crate) fn ensure_artist_profile_links_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS artist_profile_links (
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
        CREATE INDEX IF NOT EXISTS idx_artist_profile_links_artist_kind
            ON artist_profile_links(artist_id, kind, platform);
        "#,
    )?;
    Ok(())
}

pub fn artist_profile_links_response(conn: &Connection, artist_id: i64) -> Result<Value> {
    if !profile_links_schema_exists(conn)? {
        return Ok(empty_response(artist_id));
    }
    let links = list_artist_profile_links(conn, artist_id)?;
    let social = links.iter().filter(|link| link.kind == "social").count();
    let subscription = links
        .iter()
        .filter(|link| link.kind == "subscription")
        .count();
    Ok(json!({
        "artist_id": artist_id,
        "summary": {"social": social, "subscription": subscription},
        "links": links,
    }))
}

pub fn create_artist_profile_link(
    conn: &Connection,
    artist_id: i64,
    kind: &str,
    platform: &str,
    url: &str,
) -> Result<Value> {
    ensure_artist_profile_links_schema(conn)?;
    if !artist_exists(conn, artist_id)? {
        return Err(anyhow!("artist not found"));
    }
    let kind = normalize_kind(kind)?;
    let (url, host) = normalize_url(url)?;
    let platform = normalize_platform(platform, &host)?;
    conn.execute(
        "INSERT INTO artist_profile_links (artist_id, kind, platform, url, host, created_at, updated_at)
         VALUES (?, ?, ?, ?, ?, strftime('%s','now'), strftime('%s','now'))
         ON CONFLICT(artist_id, kind, url) DO UPDATE SET
           platform=excluded.platform, host=excluded.host, updated_at=excluded.updated_at",
        params![artist_id, kind, platform, url, host],
    )?;
    let link = get_artist_profile_link(conn, artist_id, kind, &url)?
        .ok_or_else(|| anyhow!("profile link was not saved"))?;
    Ok(json!({"link": link}))
}

pub fn delete_artist_profile_link(
    conn: &Connection,
    artist_id: i64,
    link_id: i64,
) -> Result<Value> {
    let deleted = conn.execute(
        "DELETE FROM artist_profile_links WHERE id=? AND artist_id=?",
        params![link_id, artist_id],
    )?;
    Ok(json!({"deleted": deleted > 0, "id": link_id}))
}

fn profile_links_schema_exists(conn: &Connection) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='artist_profile_links')",
        [],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn empty_response(artist_id: i64) -> Value {
    json!({
        "artist_id": artist_id,
        "summary": {"social": 0, "subscription": 0},
        "links": [],
    })
}

fn artist_exists(conn: &Connection, artist_id: i64) -> Result<bool> {
    let exists: i64 = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM artists WHERE id=?)",
        params![artist_id],
        |row| row.get(0),
    )?;
    Ok(exists != 0)
}

fn normalize_kind(kind: &str) -> Result<&'static str> {
    match kind.trim() {
        "social" => Ok("social"),
        "subscription" => Ok("subscription"),
        _ => Err(anyhow!("link kind must be social or subscription")),
    }
}

fn normalize_url(value: &str) -> Result<(String, String)> {
    let value = value.trim();
    if value.is_empty() || value.chars().count() > MAX_URL_CHARS {
        return Err(anyhow!(
            "URL must contain at most {MAX_URL_CHARS} characters"
        ));
    }
    let parsed = Url::parse(value).map_err(|_| anyhow!("URL is not valid"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(anyhow!("URL must use http or https"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(anyhow!("URL must not include credentials"));
    }
    let host = parsed
        .host_str()
        .ok_or_else(|| anyhow!("URL must include a host"))?
        .to_ascii_lowercase();
    Ok((parsed.as_str().to_string(), host))
}

fn normalize_platform(value: &str, host: &str) -> Result<String> {
    let platform = value.trim();
    if platform.chars().count() > MAX_PLATFORM_CHARS {
        return Err(anyhow!(
            "platform must contain at most {MAX_PLATFORM_CHARS} characters"
        ));
    }
    Ok(if platform.is_empty() {
        host.to_string()
    } else {
        platform.to_string()
    })
}

fn list_artist_profile_links(conn: &Connection, artist_id: i64) -> Result<Vec<ArtistProfileLink>> {
    let mut stmt = conn.prepare(
        "SELECT id, artist_id, kind, platform, url, host, created_at, updated_at
         FROM artist_profile_links WHERE artist_id=?
         ORDER BY CASE kind WHEN 'social' THEN 0 ELSE 1 END, platform COLLATE NOCASE, id",
    )?;
    let links = stmt
        .query_map(params![artist_id], row_to_artist_profile_link)?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(links)
}

fn get_artist_profile_link(
    conn: &Connection,
    artist_id: i64,
    kind: &str,
    url: &str,
) -> Result<Option<ArtistProfileLink>> {
    conn.query_row(
        "SELECT id, artist_id, kind, platform, url, host, created_at, updated_at
         FROM artist_profile_links WHERE artist_id=? AND kind=? AND url=?",
        params![artist_id, kind, url],
        row_to_artist_profile_link,
    )
    .optional()
    .map_err(Into::into)
}

fn row_to_artist_profile_link(row: &rusqlite::Row<'_>) -> rusqlite::Result<ArtistProfileLink> {
    Ok(ArtistProfileLink {
        id: row.get(0)?,
        artist_id: row.get(1)?,
        kind: row.get(2)?,
        platform: row.get(3)?,
        url: row.get(4)?,
        host: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture_conn() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/library/Artist');",
        )
        .unwrap();
        ensure_artist_profile_links_schema(&conn).unwrap();
        conn
    }

    #[test]
    fn creates_updates_lists_and_deletes_profile_links() {
        let conn = fixture_conn();
        let social =
            create_artist_profile_link(&conn, 1, "social", "X", "https://x.com/example").unwrap();
        let social_id = social["link"]["id"].as_i64().unwrap();
        create_artist_profile_link(&conn, 1, "social", "Twitter", "https://x.com/example").unwrap();
        create_artist_profile_link(&conn, 1, "subscription", "", "https://patreon.com/example")
            .unwrap();

        let response = artist_profile_links_response(&conn, 1).unwrap();
        assert_eq!(response["summary"]["social"], 1);
        assert_eq!(response["summary"]["subscription"], 1);
        assert_eq!(response["links"][0]["platform"], "Twitter");
        assert_eq!(response["links"][1]["platform"], "patreon.com");

        assert_eq!(
            delete_artist_profile_link(&conn, 1, social_id).unwrap()["deleted"].as_bool(),
            Some(true)
        );
        assert_eq!(
            artist_profile_links_response(&conn, 1).unwrap()["summary"]["social"],
            0
        );
    }

    #[test]
    fn rejects_invalid_links_without_a_network_request() {
        let conn = fixture_conn();
        assert!(
            create_artist_profile_link(&conn, 1, "social", "X", "mailto:test@example.com").is_err()
        );
        assert!(
            create_artist_profile_link(&conn, 1, "unknown", "X", "https://x.com/example").is_err()
        );
    }

    #[test]
    fn returns_empty_data_when_an_old_read_only_database_has_no_table() {
        let conn = Connection::open_in_memory().unwrap();
        let response = artist_profile_links_response(&conn, 1).unwrap();
        assert!(response["links"].as_array().unwrap().is_empty());
    }
}
