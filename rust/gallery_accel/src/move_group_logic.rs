use std::cmp::Ordering;
use std::collections::{HashMap, HashSet};

use rusqlite::Row;
use serde::Serialize;

use crate::media_roots::MediaRoots;
use crate::move_rows::MoveRow;
use crate::path_display::display_path;

#[derive(Clone, Debug)]
pub(crate) struct GroupSourceRow {
    pub(crate) id: i64,
    pub(crate) item_id: Option<i64>,
    pub(crate) artist_id: i64,
    pub(crate) reason: String,
    pub(crate) scan_candidate_id: Option<i64>,
    pub(crate) new_path: String,
    pub(crate) source_item_exists: Option<i64>,
    pub(crate) source_item_missing: Option<i64>,
    pub(crate) joined_scan_candidate_id: Option<i64>,
    pub(crate) scan_candidate_status: Option<String>,
    pub(crate) scan_candidate_path: Option<String>,
    pub(crate) item_artist_id: Option<i64>,
    pub(crate) item_artist_name: String,
    pub(crate) item_artist_path: String,
    pub(crate) candidate_artist_name: String,
    pub(crate) candidate_artist_path: String,
    pub(crate) display_item_artist_path: String,
    pub(crate) display_candidate_artist_path: String,
    pub(crate) is_cross_artist: bool,
    pub(crate) same_artist_name: bool,
}

#[derive(Serialize, Debug)]
pub(crate) struct GroupRow {
    pub(crate) item_artist_id: Option<i64>,
    pub(crate) candidate_artist_id: Option<i64>,
    pub(crate) reason: String,
    pub(crate) candidate_count: i64,
    pub(crate) item_artist_name: String,
    pub(crate) candidate_artist_name: String,
    pub(crate) item_artist_path: String,
    pub(crate) candidate_artist_path: String,
    pub(crate) display_item_artist_path: String,
    pub(crate) display_candidate_artist_path: String,
    pub(crate) is_cross_artist: bool,
    pub(crate) same_artist_name: bool,
    pub(crate) can_apply: bool,
    pub(crate) blocked_reason: String,
    pub(crate) blocked_candidate_count: i64,
    pub(crate) applicable_candidate_count: i64,
    pub(crate) sample_candidates: Vec<MoveRow>,
    #[serde(skip_serializing)]
    pub(crate) sample_ids: Vec<i64>,
    #[serde(skip_serializing)]
    pub(crate) move_ids: Vec<i64>,
}

pub(crate) fn group_source_from_row(
    row: &Row<'_>,
    roots: &MediaRoots,
) -> rusqlite::Result<GroupSourceRow> {
    let item_artist_id: Option<i64> = row.get("item_artist_id")?;
    let candidate_artist_id: i64 = row.get("candidate_artist_id")?;
    let item_artist_name: Option<String> = row.get("item_artist_name")?;
    let candidate_artist_name: Option<String> = row.get("candidate_artist_name")?;
    let item_artist_path: Option<String> = row.get("item_artist_path")?;
    let candidate_artist_path: Option<String> = row.get("candidate_artist_path")?;
    let item_artist_name = item_artist_name.unwrap_or_default();
    let candidate_artist_name = candidate_artist_name.unwrap_or_default();
    let item_artist_path = item_artist_path.unwrap_or_default();
    let candidate_artist_path = candidate_artist_path.unwrap_or_default();
    let is_cross_artist = item_artist_id
        .map(|id| id != candidate_artist_id)
        .unwrap_or(false);
    let same_artist_name = !item_artist_name.is_empty()
        && !candidate_artist_name.is_empty()
        && item_artist_name.to_lowercase() == candidate_artist_name.to_lowercase();
    Ok(GroupSourceRow {
        id: row.get("id")?,
        item_id: row.get("item_id")?,
        artist_id: candidate_artist_id,
        reason: row.get("reason")?,
        scan_candidate_id: row.get("scan_candidate_id")?,
        new_path: row.get("new_path")?,
        source_item_exists: row.get("source_item_exists")?,
        source_item_missing: row.get("source_item_missing")?,
        joined_scan_candidate_id: row.get("joined_scan_candidate_id")?,
        scan_candidate_status: row.get("scan_candidate_status")?,
        scan_candidate_path: row.get("scan_candidate_path")?,
        item_artist_id,
        item_artist_name,
        item_artist_path: item_artist_path.clone(),
        candidate_artist_name,
        candidate_artist_path: candidate_artist_path.clone(),
        display_item_artist_path: if item_artist_path.is_empty() {
            String::new()
        } else {
            display_path(&item_artist_path, roots)
        },
        display_candidate_artist_path: if candidate_artist_path.is_empty() {
            String::new()
        } else {
            display_path(&candidate_artist_path, roots)
        },
        is_cross_artist,
        same_artist_name,
    })
}

pub(crate) fn is_stale_group_row(row: &GroupSourceRow) -> bool {
    if row.item_id.is_some() && row.source_item_exists.is_none() {
        return true;
    }
    if row.scan_candidate_id.is_some() && row.joined_scan_candidate_id.is_none() {
        return true;
    }
    if row.joined_scan_candidate_id.is_some() {
        let status = row.scan_candidate_status.as_deref();
        if !matches!(status, Some("pending") | Some("candidate")) {
            return true;
        }
        if row.scan_candidate_path.as_deref().unwrap_or("") != row.new_path {
            return true;
        }
    }
    if row.source_item_exists.is_some() && row.source_item_missing.unwrap_or(0) == 0 {
        return true;
    }
    false
}

pub(crate) fn group_key(row: &GroupSourceRow) -> (Option<i64>, Option<i64>, String) {
    (row.item_artist_id, Some(row.artist_id), row.reason.clone())
}

pub(crate) fn can_apply_group(
    item_artist_id: Option<i64>,
    candidate_artist_id: Option<i64>,
    reason: &str,
) -> bool {
    matches!((item_artist_id, candidate_artist_id), (Some(item), Some(candidate)) if item != candidate)
        && reason == "manual_needed"
}

pub(crate) fn duplicate_target_move_ids(rows: &[GroupSourceRow]) -> HashSet<i64> {
    let mut ids_by_target: HashMap<(String, String), Vec<i64>> = HashMap::new();
    for row in rows {
        if let Some(scan_candidate_id) = row.scan_candidate_id {
            ids_by_target
                .entry(("scan_candidate".to_string(), scan_candidate_id.to_string()))
                .or_default()
                .push(row.id);
        }
        if !row.new_path.is_empty() {
            ids_by_target
                .entry(("path".to_string(), row.new_path.clone()))
                .or_default()
                .push(row.id);
        }
    }
    let mut duplicate_ids = HashSet::new();
    for ids in ids_by_target.values() {
        if ids.len() > 1 {
            duplicate_ids.extend(ids.iter().copied());
        }
    }
    duplicate_ids
}

pub(crate) fn compare_groups(left: &GroupRow, right: &GroupRow) -> Ordering {
    let left_apply = if left.can_apply { 0 } else { 1 };
    let right_apply = if right.can_apply { 0 } else { 1 };
    left_apply
        .cmp(&right_apply)
        .then_with(|| right.candidate_count.cmp(&left.candidate_count))
        .then_with(|| {
            left.item_artist_name
                .to_lowercase()
                .cmp(&right.item_artist_name.to_lowercase())
        })
        .then_with(|| {
            left.candidate_artist_name
                .to_lowercase()
                .cmp(&right.candidate_artist_name.to_lowercase())
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duplicate_target_ids_include_scan_candidate_and_new_path_matches() {
        let rows = vec![
            GroupSourceRow {
                id: 1,
                item_id: Some(1),
                artist_id: 2,
                reason: "manual_needed".to_string(),
                scan_candidate_id: Some(7),
                new_path: "/new/a.jpg".to_string(),
                source_item_exists: Some(1),
                source_item_missing: Some(1),
                joined_scan_candidate_id: Some(7),
                scan_candidate_status: Some("pending".to_string()),
                scan_candidate_path: Some("/new/a.jpg".to_string()),
                item_artist_id: Some(1),
                item_artist_name: "A".to_string(),
                item_artist_path: "/old/A".to_string(),
                candidate_artist_name: "A".to_string(),
                candidate_artist_path: "/new/A".to_string(),
                display_item_artist_path: "/old/A".to_string(),
                display_candidate_artist_path: "/new/A".to_string(),
                is_cross_artist: true,
                same_artist_name: true,
            },
            GroupSourceRow {
                id: 2,
                item_id: Some(2),
                artist_id: 2,
                reason: "manual_needed".to_string(),
                scan_candidate_id: Some(7),
                new_path: "/new/a.jpg".to_string(),
                source_item_exists: Some(2),
                source_item_missing: Some(1),
                joined_scan_candidate_id: Some(7),
                scan_candidate_status: Some("pending".to_string()),
                scan_candidate_path: Some("/new/a.jpg".to_string()),
                item_artist_id: Some(1),
                item_artist_name: "A".to_string(),
                item_artist_path: "/old/A".to_string(),
                candidate_artist_name: "A".to_string(),
                candidate_artist_path: "/new/A".to_string(),
                display_item_artist_path: "/old/A".to_string(),
                display_candidate_artist_path: "/new/A".to_string(),
                is_cross_artist: true,
                same_artist_name: true,
            },
        ];
        assert_eq!(duplicate_target_move_ids(&rows), HashSet::from([1, 2]));
    }
}
