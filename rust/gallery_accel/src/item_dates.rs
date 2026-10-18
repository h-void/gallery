//! Batch recognized-date override editing (`PUT /api/items/date`).
//!
//! One transaction applies a normalized `YYYY-MM` / `YYYY-MM-DD` manual date
//! to every selected item, or clears the override so each item falls back to
//! its most recently detected source date. Any invalid ID or date rejects the
//! whole request without partial writes.

use anyhow::{anyhow, Context, Result};
use rusqlite::{params, Connection, Transaction, TransactionBehavior};
use serde_json::{json, Value};

use crate::item_detail::effective_display_date;
use crate::media_type::parse_manual_date_input;

/// Canonical `YYYY-MM-DD` form of a raw precision-preserving date (day 01 for
/// month-only values), or empty when the raw value is not `YYYY-MM`/`YYYY-MM-DD`.
pub fn canonical_of_raw(raw: &str) -> String {
    let normalized = match parse_manual_date_input(raw) {
        Some(value) => value,
        None => return String::new(),
    };
    if normalized.len() == 7 {
        format!("{normalized}-01")
    } else {
        normalized
    }
}

/// The artist-relative parent folder path of an item (matching the
/// `folder_rename_plans.source_folder` key produced by `source_folder_for_item`),
/// or empty for items directly inside the artist root.
///
/// Hierarchy is decided by relative-path DEPTH, not by comparing directory
/// names: a subdirectory may legitimately share the artist's name, and name
/// equality would misread it as the artist root (hiding the folder from plan
/// invalidation and letting a stale target rename a real directory).
pub(crate) fn item_date_folder_name(artist_path: &str, file_path: &str) -> String {
    let artist_norm = artist_path.replace('\\', "/");
    let file_norm = file_path.replace('\\', "/");
    let artist = artist_norm.trim_end_matches('/');
    let full = file_norm.trim_end_matches('/');
    if artist.is_empty() || full.is_empty() || full == artist {
        return String::new();
    }
    let relative = match full.strip_prefix(artist) {
        // Only a '/' boundary counts; equal paths were handled above and an
        // empty remainder means the file sits directly in the artist root.
        Some(rest) if rest.starts_with('/') => rest.trim_start_matches('/'),
        // Not under the artist root: no plan can key on it.
        _ => return String::new(),
    };
    let segments: Vec<&str> = relative.split('/').filter(|s| !s.is_empty()).collect();
    if segments.len() < 2 {
        // Directly inside the artist root.
        return String::new();
    }
    segments[..segments.len() - 1].join("/")
}

/// Apply a manual recognized-date override to a batch of items belonging to
/// `artist_id`. `manual_date` is `None` for reset (clear the override and
/// restore the latest detected canonical date).
///
/// Validation rejects the whole request on any invalid input: empty or
/// duplicate item ids, non-positive ids, items that do not exist or belong to
/// another artist, or an unparseable date.
pub fn update_item_dates_response(
    conn: &Connection,
    artist_id: i64,
    item_ids: &[i64],
    manual_date: Option<&str>,
) -> Result<Value> {
    let mut requested = item_ids.to_vec();
    requested.sort_unstable();
    requested.dedup();
    if requested.is_empty() {
        return Err(anyhow!("item_ids must not be empty"));
    }
    if requested.iter().any(|id| *id <= 0) {
        return Err(anyhow!("item_ids must be positive"));
    }
    if requested.len() != item_ids.len() {
        return Err(anyhow!("item_ids must not contain duplicates"));
    }
    let parsed = match manual_date {
        Some(value) => {
            Some(parse_manual_date_input(value).ok_or_else(|| anyhow!("invalid date: {value}"))?)
        }
        None => None,
    };
    if artist_id <= 0 {
        return Err(anyhow!("artist_id must be positive"));
    }

    let placeholders = std::iter::repeat_n("?", requested.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut lookup_params: Vec<rusqlite::types::Value> = requested
        .iter()
        .map(|id| rusqlite::types::Value::Integer(*id))
        .collect();
    lookup_params.push(rusqlite::types::Value::Integer(artist_id));
    let artist_path: String = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            params![artist_id],
            |row| row.get(0),
        )
        .context("look up artist root")?;
    let rows = conn
        .prepare(&format!(
            "SELECT id, detected_date, date, file_path, folder_name, COALESCE(manual_date, '') FROM items WHERE id IN ({placeholders}) AND artist_id=?"
        ))?
        .query_map(rusqlite::params_from_iter(lookup_params.iter()), |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()
        .context("look up target items")?;
    if rows.len() != requested.len() {
        return Err(anyhow!(
            "{} item(s) do not exist or belong to this artist",
            requested.len() - rows.len()
        ));
    }

    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)
        .context("begin item date update")?;
    let mut updated: Vec<Value> = Vec::with_capacity(rows.len());
    let mut changed_folders: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (item_id, detected_date, legacy_date, file_path, folder_name, previous_manual) in rows {
        let (new_manual, new_date): (Option<String>, String) = match &parsed {
            Some(raw) => {
                let canonical = canonical_of_raw(raw);
                (Some(raw.clone()), canonical)
            }
            None => {
                let canonical = canonical_of_raw(&detected_date);
                let effective = if canonical.is_empty() {
                    legacy_date.clone()
                } else {
                    canonical
                };
                (None, effective)
            }
        };
        // Precision matters, not just the canonical value: a rewrite that keeps
        // `date` in place but changes `manual_date` precision (e.g. `2026-07`
        // -> `2026-07-01`) re-renders the plan target, so the plan key must still
        // be collected. Otherwise the row keeps a manually overridden date that
        // no confirmed plan was ever re-checked against.
        let date_changed = new_date != legacy_date;
        let precision_changed = new_manual.as_deref().unwrap_or_default() != previous_manual;
        if date_changed || precision_changed {
            // The plan key, not a hand-rolled folder path: `source_folder_for_item`
            // is what plan discovery stores in `folder_rename_plans.source_folder`,
            // including its legacy `folder_name` fallback, so invalidation finds
            // every plan that a stale target could execute.
            let folder = crate::folder_archive::source_folder_for_item(
                &artist_path,
                &file_path,
                &folder_name,
            )
            .unwrap_or_default();
            if !folder.is_empty() {
                changed_folders.insert(folder);
            }
        }
        let affected = tx
            .execute(
                "UPDATE items SET manual_date=?, date=? WHERE id=?",
                params![new_manual, new_date, item_id],
            )
            .context("update item date")?;
        if affected != 1 {
            return Err(anyhow!("item {item_id} changed during the batch"));
        }
        updated.push(json!({
            "item_id": item_id,
            "date": new_date,
            "detected_date": detected_date,
            "manual_date": new_manual,
            "display_date": effective_display_date(
                &detected_date,
                new_manual.as_deref(),
                &legacy_date,
            ),
        }));
    }
    // Invalidate stale confirmed plans inside the SAME transaction as the
    // date updates: committing first would leave a crash window where
    // confirmed plans keep outdated targets and later execution renames
    // folders destructively.
    let folders: Vec<String> = changed_folders.into_iter().collect();
    let refreshed =
        crate::folder_archive::invalidate_plans_after_item_date_change(&tx, artist_id, &folders)?;
    tx.commit().context("commit item date update")?;
    Ok(json!({
        "updated": updated.len(),
        "items": updated,
        "plans_invalidated": refreshed,
    }))
}
