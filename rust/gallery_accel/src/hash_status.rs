use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn hash_status_response(conn: &Connection) -> Result<Value> {
    // Product UI expects blake3_available / workers-style fields; native path
    // always has blake3 (crate dependency). Worker knobs come from env for parity.
    // These mirror what `workers::spawn_hash_loop` actually runs: the batch size
    // defaults to `MAX_PAGINATION_LIMIT` and is clamped the same way, the
    // interval is whole seconds, and an unset `HASH_INTERVAL` means the loop is
    // disabled. The old values (batch 32, interval 1.0, "worker not running")
    // described a configuration nothing in the process used.
    let workers = std::env::var("HASH_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(1);
    let interval_secs = std::env::var("HASH_INTERVAL")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|seconds| *seconds > 0);
    let interval = interval_secs.map(|seconds| seconds as f64).unwrap_or(0.0);
    let batch_size = std::env::var("HASH_BATCH_SIZE")
        .ok()
        .and_then(|value| value.trim().parse::<i64>().ok())
        .unwrap_or(crate::MAX_PAGINATION_LIMIT)
        .clamp(1, crate::MAX_PAGINATION_LIMIT);
    Ok(json!({
        "blake3_available": true,
        "workers": workers,
        "blake3_threads": workers,
        "interval": interval,
        "batch_size": batch_size,
        "resolve_batch_size": batch_size,
        "worker": {
            // `thread_alive` is what the maintenance UI reads: the status copy
            // (正在检查 vs 等待检查), the 待判断 panel's short tick, and the
            // duplicates auto-refresh all key off it, so a missing field kept
            // every one of them on the idle path while hashing was under way.
            // `running` stays for callers that already read it.
            "running": interval_secs.is_some(),
            "thread_alive": interval_secs.is_some(),
            "note": "in-process hash batches via POST /api/hash/run",
        },
        "items": hash_status_counts(conn, "items", "missing=0", false)?,
        "scan_candidates": hash_status_counts(
            conn,
            "scan_candidates",
            "
            status IN ('pending', 'candidate')
            AND NOT EXISTS (
                SELECT 1
                FROM move_candidates mc
                WHERE mc.scan_candidate_id = scan_candidates.id
                  AND mc.status = 'pending'
            )
            ",
            true,
        )?,
    }))
}

fn hash_status_counts(
    conn: &Connection,
    table: &str,
    where_sql: &str,
    include_done_unresolved: bool,
) -> Result<HashMap<String, i64>> {
    let query = format!(
        "SELECT hash_status, COUNT(*) AS count FROM {table} WHERE {where_sql} GROUP BY hash_status"
    );
    let mut counts = HashMap::from([
        ("pending".to_string(), 0),
        ("processing".to_string(), 0),
        ("done".to_string(), 0),
        ("error".to_string(), 0),
    ]);
    let mut stmt = conn.prepare(&query)?;
    for row in stmt.query_map([], |row| {
        Ok((
            row.get::<_, Option<String>>("hash_status")?
                .unwrap_or_else(|| "pending".to_string()),
            row.get::<_, i64>("count")?,
        ))
    })? {
        let (status, count) = row?;
        counts.insert(status, count);
    }
    let total = counts.values().sum();
    // The `done` count for this same predicate is already in `counts`: the
    // GROUP BY above saw exactly the rows the old second `COUNT(*) ... AND
    // hash_status='done'` asked for. Re-querying it doubled the work and opened
    // a window where a concurrent write could make the two statements disagree.
    let done_unresolved = if include_done_unresolved {
        *counts.get("done").unwrap_or(&0)
    } else {
        0
    };
    let remaining = counts.get("pending").unwrap_or(&0)
        + counts.get("processing").unwrap_or(&0)
        + counts.get("error").unwrap_or(&0)
        + done_unresolved;
    counts.insert("total".to_string(), total);
    counts.insert("done_unresolved".to_string(), done_unresolved);
    counts.insert("remaining".to_string(), remaining);
    Ok(counts)
}
