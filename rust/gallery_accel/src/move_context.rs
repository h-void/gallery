use anyhow::Result;
use rusqlite::Connection;
use std::collections::{BTreeSet, HashMap};

#[derive(Clone, Debug)]
pub(crate) struct ArtistContext {
    pub(crate) id: i64,
    pub(crate) name: String,
    pub(crate) path: String,
}

/// Batch-loaded artist contexts for a page of rows: one `IN (...)` query
/// instead of two lookups per row.
#[derive(Default)]
pub(crate) struct ArtistContextStore {
    by_id: HashMap<i64, Option<ArtistContext>>,
}

impl ArtistContextStore {
    pub(crate) fn load(
        conn: &Connection,
        artist_ids: impl IntoIterator<Item = Option<i64>>,
    ) -> Result<Self> {
        let unique: BTreeSet<i64> = artist_ids.into_iter().flatten().collect();
        let mut by_id: HashMap<i64, Option<ArtistContext>> = HashMap::new();
        if unique.is_empty() {
            return Ok(Self { by_id });
        }
        let ids: Vec<i64> = unique.into_iter().collect();
        for chunk in ids.chunks(500) {
            let placeholders = std::iter::repeat_n("?", chunk.len())
                .collect::<Vec<_>>()
                .join(",");
            let mut stmt = conn.prepare(&format!(
                "SELECT id, name, path FROM artists WHERE id IN ({placeholders})"
            ))?;
            let mut rows = stmt.query(rusqlite::params_from_iter(chunk.iter()))?;
            while let Some(row) = rows.next()? {
                by_id.insert(
                    row.get::<_, i64>(0)?,
                    Some(ArtistContext {
                        id: row.get(0)?,
                        name: row.get(1)?,
                        path: row.get(2)?,
                    }),
                );
            }
        }
        for id in ids {
            by_id.entry(id).or_insert(None);
        }
        Ok(Self { by_id })
    }

    pub(crate) fn get(&self, artist_id: Option<i64>) -> Option<&ArtistContext> {
        let id = artist_id?;
        self.by_id.get(&id).and_then(|context| context.as_ref())
    }
}
