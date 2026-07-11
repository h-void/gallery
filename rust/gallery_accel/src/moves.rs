use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};

use crate::media_roots::MediaRoots;
use crate::move_filters::move_candidate_where;
use crate::move_rows::{query_move_rows, MoveRow};
use crate::normalize_pagination;

pub fn move_candidates_response(
    conn: &Connection,
    roots: &MediaRoots,
    status: &str,
    hide_grouped: bool,
    limit: Option<i64>,
    offset: Option<i64>,
) -> Result<Value> {
    let (limit, offset) = normalize_pagination(limit, offset);
    let total = count_move_candidates(conn, status, hide_grouped)?;
    let candidates = list_move_candidates(conn, roots, status, hide_grouped, limit, offset)?;
    let waiting_hash_count = if status == "pending" {
        count_waiting_hash_candidates(conn)?
    } else {
        0
    };
    Ok(json!({
        "candidates": candidates,
        "total": total,
        "limit": limit,
        "offset": offset,
        "has_more": offset + limit < total,
        "waiting_hash_count": waiting_hash_count,
    }))
}

fn count_move_candidates(conn: &Connection, status: &str, hide_grouped: bool) -> Result<i64> {
    let (where_sql, params) = move_candidate_where(status, hide_grouped);
    conn.query_row(
        &format!(
            "
            SELECT COUNT(*)
            FROM move_candidates mc
            LEFT JOIN items i ON i.id = mc.item_id
            WHERE {where_sql}
            "
        ),
        rusqlite::params_from_iter(params.iter()),
        |row| row.get(0),
    )
    .context("count move candidates")
}

fn list_move_candidates(
    conn: &Connection,
    roots: &MediaRoots,
    status: &str,
    hide_grouped: bool,
    limit: i64,
    offset: i64,
) -> Result<Vec<MoveRow>> {
    let (where_sql, params) = move_candidate_where(status, hide_grouped);
    let mut query_params = params;
    query_params.push(limit.to_string());
    query_params.push(offset.to_string());
    let rows = query_move_rows(
        conn,
        roots,
        &format!(
            "
            SELECT mc.*
            FROM move_candidates mc
            LEFT JOIN items i ON i.id = mc.item_id
            WHERE {where_sql}
            ORDER BY mc.created_at, mc.id
            LIMIT ? OFFSET ?
            "
        ),
        rusqlite::params_from_iter(query_params.iter()),
    )?;
    Ok(rows)
}

fn count_waiting_hash_candidates(conn: &Connection) -> Result<i64> {
    conn.query_row(
        "
        SELECT COUNT(*)
        FROM (
            SELECT 's' || id AS key
            FROM scan_candidates
            WHERE status IN ('pending', 'candidate')
              AND hash_status != 'done'
            UNION
            SELECT
                CASE
                    WHEN scan_candidate_id IS NOT NULL THEN 's' || scan_candidate_id
                    ELSE 'm' || id
                END AS key
            FROM move_candidates
            WHERE status='pending'
              AND reason='missing_hash_not_ready'
        )
        ",
        [],
        |row| row.get(0),
    )
    .context("count waiting hash candidates")
}
