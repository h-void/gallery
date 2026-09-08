use std::collections::{HashMap, HashSet};

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::move_filters::move_candidate_where;
use crate::path_display::display_path;
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
    let sample_limit = sample_limit.unwrap_or(5).clamp(0, 50);
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
        LIMIT 5000
        "
    ))?;
    let rows: Vec<GroupSourceRow> = stmt
        .query_map(rusqlite::params_from_iter(params.iter()), |row| {
            group_source_from_row(row, roots)
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    // The detail rows above come from a LIMIT window used ONLY for samples and
    // move-id lists. Group counts and `can_apply` are computed from an
    // unwindowed aggregate over the whole candidate population.
    let moves: Vec<GroupSourceRow> = rows
        .into_iter()
        .filter(|row| !is_stale_group_row(row))
        .collect();
    let duplicate_ids = if status == "pending" {
        duplicate_target_move_ids(&moves)
    } else {
        HashSet::new()
    };

    let mut total_stmt = conn.prepare(&format!(
        "
        WITH filtered AS (
            SELECT
                mc.id,
                mc.item_id,
                mc.scan_candidate_id,
                COALESCE(mc.new_path, '') AS new_path,
                mc.reason,
                mc.artist_id AS candidate_artist_id,
                i.artist_id AS item_artist_id,
                i.id AS source_item_exists,
                i.missing AS source_item_missing,
                sc.id AS sc_id,
                sc.status AS sc_status,
                sc.file_path AS sc_path
            FROM move_candidates mc
            LEFT JOIN items i ON i.id = mc.item_id
            LEFT JOIN scan_candidates sc ON sc.id = mc.scan_candidate_id
            WHERE {where_sql}
        ),
        live AS (
            SELECT * FROM filtered f
            WHERE (f.item_id IS NULL OR f.source_item_exists IS NOT NULL)
              AND (f.scan_candidate_id IS NULL OR f.sc_id IS NOT NULL)
              AND (f.sc_id IS NULL OR f.sc_status IN ('pending','candidate'))
              AND (f.sc_id IS NULL OR COALESCE(f.sc_path, '') = f.new_path)
              AND (f.source_item_exists IS NULL OR COALESCE(f.source_item_missing, 0) = 1)
        )
        SELECT
            live.item_artist_id,
            ia.name,
            ia.path,
            live.candidate_artist_id,
            ca.name,
            ca.path,
            live.reason,
            COUNT(*) AS candidate_count,
            SUM(CASE WHEN (
                    (live.scan_candidate_id IS NOT NULL AND
                     (SELECT COUNT(*) FROM live l2
                      WHERE l2.scan_candidate_id = live.scan_candidate_id) > 1)
                 OR (live.new_path <> '' AND
                     (SELECT COUNT(*) FROM live l3 WHERE l3.new_path = live.new_path) > 1)
                ) THEN 1 ELSE 0 END) AS blocked_count
        FROM live
        LEFT JOIN artists ia ON ia.id = live.item_artist_id
        LEFT JOIN artists ca ON ca.id = live.candidate_artist_id
        GROUP BY live.item_artist_id, ia.name, ia.path,
                 live.candidate_artist_id, ca.name, ca.path, live.reason
        ",
    ))?;
    let totals: GroupTotals =
        total_stmt
            .query_map(rusqlite::params_from_iter(params.iter()), |row| {
                Ok((
                    row.get::<_, Option<i64>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, i64>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?
            .into_iter()
            .map(
                |(item_artist_id, item_name, item_path, candidate_artist_id, cand_name, cand_path, reason, candidate_count, blocked_count)| {
                    let identity = GroupIdentity {
                        item_artist_name: item_name.unwrap_or_default(),
                        item_artist_path: item_path.unwrap_or_default(),
                        candidate_artist_name: cand_name.unwrap_or_default(),
                        candidate_artist_path: cand_path.unwrap_or_default(),
                    };
                    let key = (item_artist_id, Some(candidate_artist_id), reason);
                    (key, (candidate_count, blocked_count, identity))
                },
            )
            .collect();

    let mut groups_by_key: HashMap<(Option<i64>, Option<i64>, String), GroupRow> = HashMap::new();
    for (key, (candidate_count, blocked_count, identity)) in totals {
        // Mirror the historical row-accumulation semantics: a group that cannot
        // apply reports zero applicable candidates unless duplicates reduced it.
        let can_apply_base = can_apply_group(key.0, key.1, &key.2);
        let (can_apply, applicable_candidate_count) = if blocked_count > 0 {
            let applicable = (candidate_count - blocked_count).max(0);
            (can_apply_base && applicable > 0, applicable)
        } else if can_apply_base {
            (true, candidate_count)
        } else {
            (false, 0)
        };
        let item_artist_name = identity.item_artist_name.clone();
        let candidate_artist_name = identity.candidate_artist_name.clone();
        let same_artist_name = !item_artist_name.is_empty()
            && !candidate_artist_name.is_empty()
            && item_artist_name.to_lowercase() == candidate_artist_name.to_lowercase();
        let item_artist_path = identity.item_artist_path.clone();
        let candidate_artist_path = identity.candidate_artist_path.clone();
        groups_by_key.insert(
            key.clone(),
            GroupRow {
                item_artist_id: key.0,
                candidate_artist_id: key.1,
                reason: key.2,
                candidate_count,
                item_artist_name,
                candidate_artist_name,
                same_artist_name,
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
                item_artist_path,
                candidate_artist_path,
                is_cross_artist: matches!((key.0, key.1), (Some(item), Some(cand)) if item != cand),
                can_apply,
                blocked_reason: if blocked_count > 0 {
                    "duplicate_target_candidates".to_string()
                } else {
                    String::new()
                },
                blocked_candidate_count: blocked_count,
                applicable_candidate_count,
                sample_candidates: Vec::new(),
                sample_ids: Vec::new(),
                move_ids: Vec::new(),
                blocked_move_ids: Vec::new(),
            });
    }

    // Attach the windowed ids (samples / blocked id listing) to their groups.
    for row in moves {
        let key = group_key(&row);
        let Some(group) = groups_by_key.get_mut(&key) else {
            continue;
        };
        group.move_ids.push(row.id);
        if (group.sample_ids.len() as i64) < sample_limit {
            group.sample_ids.push(row.id);
        }
        if duplicate_ids.contains(&row.id) {
            group.blocked_move_ids.push(row.id);
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
    }
    groups.sort_by(compare_groups);
    Ok(groups)
}

/// Per-group aggregate row: (candidate_count, blocked_count, identity).
type GroupTotals = HashMap<(Option<i64>, Option<i64>, String), (i64, i64, GroupIdentity)>;

/// Artist names/paths attached to an aggregate group row.
struct GroupIdentity {
    item_artist_name: String,
    item_artist_path: String,
    candidate_artist_name: String,
    candidate_artist_path: String,
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
