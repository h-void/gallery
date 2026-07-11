use std::collections::HashMap;

use anyhow::Result;
use rusqlite::Connection;
use serde_json::{json, Value};

pub fn hash_status_response(conn: &Connection) -> Result<Value> {
    // Product UI expects blake3_available / workers-style fields; native path
    // always has blake3 (crate dependency). Worker knobs come from env for parity.
    let workers = std::env::var("HASH_WORKERS")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(1);
    let interval = std::env::var("HASH_INTERVAL")
        .ok()
        .and_then(|v| v.trim().parse::<f64>().ok())
        .unwrap_or(1.0);
    let batch_size = std::env::var("HASH_BATCH_SIZE")
        .ok()
        .and_then(|v| v.trim().parse::<i64>().ok())
        .unwrap_or(32);
    Ok(json!({
        "blake3_available": true,
        "workers": workers,
        "blake3_threads": workers,
        "interval": interval,
        "batch_size": batch_size,
        "resolve_batch_size": batch_size,
        "worker": {"running": false, "note": "in-process hash batches via POST /api/hash/run"},
        "items": hash_status_counts(conn, "items", "missing=0")?,
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
        )?,
    }))
}

fn hash_status_counts(
    conn: &Connection,
    table: &str,
    where_sql: &str,
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
    let remaining = counts.get("pending").unwrap_or(&0)
        + counts.get("processing").unwrap_or(&0)
        + counts.get("error").unwrap_or(&0);
    counts.insert("total".to_string(), total);
    counts.insert("remaining".to_string(), remaining);
    Ok(counts)
}
