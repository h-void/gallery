use anyhow::Result;
use rusqlite::{Connection, Params};

#[derive(Clone, Debug)]
pub(crate) struct ArtistContext {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) path: String,
}

pub(crate) fn query_optional_i64<P: Params>(
    conn: &Connection,
    sql: &str,
    params: P,
) -> Result<Option<i64>> {
    let mut stmt = conn.prepare(sql)?;
    let mut rows = stmt.query(params)?;
    match rows.next()? {
        Some(row) => Ok(row.get(0)?),
        None => Ok(None),
    }
}

pub(crate) fn artist_context(
    conn: &Connection,
    artist_id: Option<i64>,
) -> Result<Option<ArtistContext>> {
    let Some(artist_id) = artist_id else {
        return Ok(None);
    };
    let mut stmt = conn.prepare("SELECT id, name, path FROM artists WHERE id=?")?;
    let mut rows = stmt.query([artist_id])?;
    if let Some(row) = rows.next()? {
        Ok(Some(ArtistContext {
            id: row.get(0)?,
            name: row.get(1)?,
            path: row.get(2)?,
        }))
    } else {
        Ok(None)
    }
}
