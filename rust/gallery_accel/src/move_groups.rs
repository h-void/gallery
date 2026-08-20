use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::move_filters::move_candidate_where;
use crate::move_group_logic::{
    can_apply_group, compare_groups, duplicate_target_move_ids, group_key, group_source_from_row,
    is_stale_group_row, GroupRow, GroupSourceRow,
};
use crate::move_rows::{query_move_rows, MoveRow};

pub fn move_candidate_groups_response(
    conn: &Connection,
    roots: &MediaRoots,
    status: &str,
    sample_limit: Option<i64>,
) -> Result<Value> {
    let sample_limit = sample_limit.unwrap_or(5);
    let groups = list_move_candidate_groups(conn, roots, status, sample_limit)?;
    Ok(json!({
        "count": groups.len() as i64,
        "groups": groups,
    }))
}

fn list_move_candidate_groups(
    conn: &Connection,
    roots: &MediaRoots,
    status: &str,
    sample_limit: i64,
) -> Result<Vec<GroupRow>> {
    let (where_sql, params) = move_candidate_where(status, false);
    let mut stmt = conn.prepare(&format!(
        "
        SELECT
            mc.id,
            mc.item_id,
            mc.artist_id AS candidate_artist_id,
            mc.reason,
            mc.scan_candidate_id,
            mc.new_path,
            mc.created_at,
            i.id AS source_item_exists,
            i.missing AS source_item_missing,
            sc.id AS joined_scan_candidate_id,
            sc.status AS scan_candidate_status,
            sc.file_path AS scan_candidate_path,
            target.id AS target_item_id,
            i.artist_id AS item_artist_id,
            item_artist.name AS item_artist_name,
            item_artist.path AS item_artist_path,
            candidate_artist.name AS candidate_artist_name,
            candidate_artist.path AS candidate_artist_path
        FROM move_candidates mc
        LEFT JOIN items i ON i.id = mc.item_id
        LEFT JOIN scan_candidates sc ON sc.id = mc.scan_candidate_id
        LEFT JOIN items target
          ON target.file_path = mc.new_path
         AND target.missing = 0
        LEFT JOIN artists item_artist ON item_artist.id = i.artist_id
        LEFT JOIN artists candidate_artist ON candidate_artist.id = mc.artist_id
        WHERE {where_sql}
        ORDER BY mc.created_at, mc.id
        "
    ))?;
    let rows: Vec<GroupSourceRow> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            group_source_from_row(row, roots)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let moves: Vec<GroupSourceRow> = rows
        .into_iter()
        .filter(|row| !is_stale_group_row(row))
        .collect();
    let duplicate_ids = if status == "pending" {
        duplicate_target_move_ids(&moves)
    } else {
        HashSet::new()
    };
    let mut groups_by_key: HashMap<(Option<i64>, Option<i64>, String), GroupRow> = HashMap::new();
    for row in moves {
        let key = group_key(&row);
        let group = groups_by_key
            .entry(key.clone())
            .or_insert_with(|| GroupRow {
                item_artist_id: key.0,
                candidate_artist_id: key.1,
                reason: key.2,
                candidate_count: 0,
                item_artist_name: row.item_artist_name.clone(),
                candidate_artist_name: row.candidate_artist_name.clone(),
                item_artist_path: row.item_artist_path.clone(),
                candidate_artist_path: row.candidate_artist_path.clone(),
                display_item_artist_path: row.display_item_artist_path.clone(),
                display_candidate_artist_path: row.display_candidate_artist_path.clone(),
                is_cross_artist: row.is_cross_artist,
                same_artist_name: row.same_artist_name,
                can_apply: can_apply_group(key.0, key.1, &row.reason),
                blocked_reason: String::new(),
                blocked_candidate_count: 0,
                applicable_candidate_count: 0,
                sample_candidates: Vec::new(),
                sample_ids: Vec::new(),
                move_ids: Vec::new(),
            });
        group.candidate_count += 1;
        group.move_ids.push(row.id);
        if (group.sample_ids.len() as i64) < sample_limit {
            group.sample_ids.push(row.id);
        }
    }

    let mut groups: Vec<GroupRow> = groups_by_key.into_values().collect();
    let sample_ids: Vec<i64> = groups
        .iter()
        .flat_map(|group| group.sample_ids.iter().copied())
        .collect();
    let sample_by_id = sample_moves_by_id(conn, roots, &sample_ids)?;
    for group in &mut groups {
        group.sample_candidates = group
            .sample_ids
            .iter()
            .filter_map(|id| sample_by_id.get(id).cloned())
            .collect();
        let group_duplicate_count = group
            .move_ids
            .iter()
            .filter(|id| duplicate_ids.contains(id))
            .count() as i64;
        if group_duplicate_count > 0 {
            group.blocked_reason = "duplicate_target_candidates".to_string();
            group.blocked_candidate_count = group_duplicate_count;
            group.applicable_candidate_count =
                (group.candidate_count - group_duplicate_count).max(0);
            group.can_apply = group.can_apply && group.applicable_candidate_count > 0;
        } else if group.can_apply {
            group.applicable_candidate_count = group.candidate_count;
        }
    }
    groups.sort_by(compare_groups);
    Ok(groups)
}

fn sample_moves_by_id(
    conn: &Connection,
    roots: &MediaRoots,
    ids: &[i64],
) -> Result<HashMap<i64, MoveRow>> {
    if ids.is_empty() {
        return Ok(HashMap::new());
    }
    let placeholders = ids.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let rows = query_move_rows(
        conn,
        roots,
        &format!(
            "
            SELECT *
            FROM move_candidates
            WHERE id IN ({placeholders})
            ORDER BY created_at, id
            "
        ),
        rusqlite::params_from_iter(ids.iter()),
    )?;
    Ok(rows.into_iter().map(|row| (row.id, row)).collect())
}
