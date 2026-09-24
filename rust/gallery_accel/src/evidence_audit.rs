//! Read-only audit for historical `kemono_files.evidence_item_id` bindings (V4).
//!
//! Categorizes existing bindings to distinguish:
//! - `VerifiedMatch`: item exists, expected size matches, content hash matches published artifact (or on-disk file).
//! - `IndexHashPending`: item exists and file size matches, but `content_hash` in `items` is not yet indexed.
//! - `FileChanged`: file size on disk or in `items` differs from `kemono_files.expected_length`.
//! - `DefiniteMismatch`: `content_hash` in `items` does not match the published artifact's hash in `download_publish_jobs`.
//! - `ItemNotFound`: `evidence_item_id` points to a non-existent item row.
//! - `ItemFileMissing`: item is marked missing in DB or the file path does not exist on disk.
//!
//! Strictly read-only: outputs a diagnostic report without modifying or revoking records.

use rusqlite::{params, Connection, OptionalExtension, Result};
use serde::{Deserialize, Serialize};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum BindingAuditStatus {
    VerifiedMatch,
    IndexHashPending,
    FileChanged,
    DefiniteMismatch,
    ItemNotFound,
    ItemFileMissing,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EvidenceAuditSample {
    pub file_id: i64,
    pub post_id: String,
    pub target_path: String,
    pub evidence_item_id: i64,
    pub status: BindingAuditStatus,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EvidenceAuditReport {
    pub audited_total: usize,
    pub verified_match: usize,
    pub index_hash_pending: usize,
    pub file_changed: usize,
    pub definite_mismatch: usize,
    pub item_missing: usize,
    pub samples: Vec<EvidenceAuditSample>,
}

pub fn audit_evidence_bindings(conn: &Connection, limit: usize) -> Result<EvidenceAuditReport> {
    let mut report = EvidenceAuditReport::default();
    let cap = limit.clamp(1, 5000);

    let mut table_info = conn.prepare("PRAGMA table_info(kemono_files)")?;
    let cols: Vec<String> = table_info
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<Vec<_>, _>>()?;

    let has_publish_job_id = cols.iter().any(|c| c == "publish_job_id");
    let has_expected_length = cols.iter().any(|c| c == "expected_length");
    let has_target_path = cols.iter().any(|c| c == "target_path");

    let jobs_info = conn.prepare("PRAGMA table_info(download_publish_jobs)");
    let (has_jobs_table, has_artifact_blake3) = match jobs_info {
        Ok(mut stmt) => {
            let cols: Vec<String> = stmt
                .query_map([], |row| row.get::<_, String>(1))
                .and_then(|rows| rows.collect::<Result<Vec<_>, _>>())
                .unwrap_or_default();
            let has_hash = cols.iter().any(|c| c == "artifact_blake3");
            (true, has_hash)
        }
        Err(_) => (false, false),
    };

    let sql = format!(
        "SELECT f.id, f.post_id, f.evidence_item_id,
                {},
                {},
                {}
         FROM kemono_files f
         WHERE f.evidence_item_id IS NOT NULL
         ORDER BY f.id ASC
         LIMIT ?1",
        if has_target_path {
            "f.target_path"
        } else {
            "''"
        },
        if has_expected_length {
            "f.expected_length"
        } else {
            "0"
        },
        if has_publish_job_id {
            "f.publish_job_id"
        } else {
            "NULL"
        },
    );

    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![cap as i64], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?.unwrap_or_default(),
                row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                row.get::<_, Option<i64>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>>>()?;

    for (file_id, post_id, evidence_item_id, target_path, expected_length, publish_job_id) in rows {
        report.audited_total += 1;

        let item_res: Option<(String, i64, Option<String>, i64)> = conn
            .query_row(
                "SELECT file_path, file_size, content_hash, missing FROM items WHERE id = ?1",
                params![evidence_item_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .optional()?;

        let Some((item_path, item_size, content_hash, missing)) = item_res else {
            report.item_missing += 1;
            if report.samples.len() < 50 {
                report.samples.push(EvidenceAuditSample {
                    file_id,
                    post_id,
                    target_path,
                    evidence_item_id,
                    status: BindingAuditStatus::ItemNotFound,
                    detail: format!("item id {evidence_item_id} not found in items table"),
                });
            }
            continue;
        };

        if missing != 0 {
            report.item_missing += 1;
            if report.samples.len() < 50 {
                report.samples.push(EvidenceAuditSample {
                    file_id,
                    post_id,
                    target_path,
                    evidence_item_id,
                    status: BindingAuditStatus::ItemFileMissing,
                    detail: format!("item {evidence_item_id} is marked missing in library"),
                });
            }
            continue;
        }

        if !item_path.is_empty() && !Path::new(&item_path).exists() {
            report.item_missing += 1;
            if report.samples.len() < 50 {
                report.samples.push(EvidenceAuditSample {
                    file_id,
                    post_id,
                    target_path,
                    evidence_item_id,
                    status: BindingAuditStatus::ItemFileMissing,
                    detail: format!("file does not exist on disk at {item_path}"),
                });
            }
            continue;
        }

        if expected_length > 0 && item_size > 0 && expected_length != item_size {
            report.file_changed += 1;
            if report.samples.len() < 50 {
                report.samples.push(EvidenceAuditSample {
                    file_id,
                    post_id,
                    target_path,
                    evidence_item_id,
                    status: BindingAuditStatus::FileChanged,
                    detail: format!("expected size {expected_length} != item size {item_size}"),
                });
            }
            continue;
        }

        let mut job_hash: Option<String> = None;
        if has_jobs_table && has_artifact_blake3 {
            if let Some(job_id) = publish_job_id {
                job_hash = conn
                    .query_row(
                        "SELECT artifact_blake3 FROM download_publish_jobs WHERE id = ?1",
                        params![job_id],
                        |r| r.get(0),
                    )
                    .optional()?;
            }
        }

        let item_hash_str = content_hash.unwrap_or_default().trim().to_ascii_lowercase();

        if let Some(ref expected_hash) = job_hash {
            let exp = expected_hash.trim().to_ascii_lowercase();
            if !exp.is_empty() {
                if item_hash_str.is_empty() {
                    report.index_hash_pending += 1;
                    if report.samples.len() < 50 {
                        report.samples.push(EvidenceAuditSample {
                            file_id,
                            post_id,
                            target_path,
                            evidence_item_id,
                            status: BindingAuditStatus::IndexHashPending,
                            detail: format!("item content hash pending; expected blake3 is {exp}"),
                        });
                    }
                    continue;
                } else if !item_hash_str.eq_ignore_ascii_case(&exp) {
                    report.definite_mismatch += 1;
                    if report.samples.len() < 50 {
                        report.samples.push(EvidenceAuditSample {
                            file_id,
                            post_id,
                            target_path,
                            evidence_item_id,
                            status: BindingAuditStatus::DefiniteMismatch,
                            detail: format!(
                                "item hash {item_hash_str} does not match published artifact {exp}"
                            ),
                        });
                    }
                    continue;
                }
            }
        }

        if item_hash_str.is_empty() {
            report.index_hash_pending += 1;
            if report.samples.len() < 50 {
                report.samples.push(EvidenceAuditSample {
                    file_id,
                    post_id,
                    target_path,
                    evidence_item_id,
                    status: BindingAuditStatus::IndexHashPending,
                    detail: "item content hash pending".to_string(),
                });
            }
        } else {
            report.verified_match += 1;
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_reports_various_evidence_binding_states() {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE items (
                id INTEGER PRIMARY KEY,
                file_path TEXT NOT NULL,
                file_size INTEGER NOT NULL,
                content_hash TEXT,
                missing INTEGER NOT NULL DEFAULT 0
             );
             CREATE TABLE download_publish_jobs (
                id INTEGER PRIMARY KEY,
                artifact_blake3 TEXT
             );
             CREATE TABLE kemono_files (
                id INTEGER PRIMARY KEY,
                post_id TEXT NOT NULL,
                evidence_item_id INTEGER,
                target_path TEXT,
                expected_length INTEGER,
                publish_job_id INTEGER
             );",
        )
        .unwrap();

        let dir = tempfile::tempdir().unwrap();
        let file_ok = dir.path().join("ok.jpg");
        std::fs::write(&file_ok, b"hello").unwrap();

        // 1. VerifiedMatch
        conn.execute(
            "INSERT INTO items (id, file_path, file_size, content_hash, missing) VALUES (1, ?1, 5, 'hash1', 0)",
            params![file_ok.to_string_lossy()],
        ).unwrap();
        conn.execute(
            "INSERT INTO download_publish_jobs (id, artifact_blake3) VALUES (10, 'hash1')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (101, 'p1', 1, ?1, 5, 10)",
            params![file_ok.to_string_lossy()],
        ).unwrap();

        // 2. IndexHashPending
        conn.execute(
            "INSERT INTO items (id, file_path, file_size, content_hash, missing) VALUES (2, ?1, 5, NULL, 0)",
            params![file_ok.to_string_lossy()],
        ).unwrap();
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (102, 'p2', 2, ?1, 5, NULL)",
            params![file_ok.to_string_lossy()],
        ).unwrap();

        // 3. FileChanged
        conn.execute(
            "INSERT INTO items (id, file_path, file_size, content_hash, missing) VALUES (3, ?1, 5, 'hash3', 0)",
            params![file_ok.to_string_lossy()],
        ).unwrap();
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (103, 'p3', 3, ?1, 10, NULL)",
            params![file_ok.to_string_lossy()],
        ).unwrap();

        // 4. DefiniteMismatch
        conn.execute(
            "INSERT INTO items (id, file_path, file_size, content_hash, missing) VALUES (4, ?1, 5, 'hash_actual', 0)",
            params![file_ok.to_string_lossy()],
        ).unwrap();
        conn.execute(
            "INSERT INTO download_publish_jobs (id, artifact_blake3) VALUES (40, 'hash_expected')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (104, 'p4', 4, ?1, 5, 40)",
            params![file_ok.to_string_lossy()],
        ).unwrap();

        // 5. ItemNotFound
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (105, 'p5', 999, ?1, 5, NULL)",
            params![file_ok.to_string_lossy()],
        ).unwrap();

        // 6. ItemFileMissing
        let missing_path = dir.path().join("missing.jpg");
        conn.execute(
            "INSERT INTO items (id, file_path, file_size, content_hash, missing) VALUES (6, ?1, 5, 'hash6', 0)",
            params![missing_path.to_string_lossy()],
        ).unwrap();
        conn.execute(
            "INSERT INTO kemono_files (id, post_id, evidence_item_id, target_path, expected_length, publish_job_id)
             VALUES (106, 'p6', 6, ?1, 5, NULL)",
            params![missing_path.to_string_lossy()],
        ).unwrap();

        let report = audit_evidence_bindings(&conn, 100).unwrap();
        assert_eq!(report.audited_total, 6);
        assert_eq!(report.verified_match, 1);
        assert_eq!(report.index_hash_pending, 1);
        assert_eq!(report.file_changed, 1);
        assert_eq!(report.definite_mismatch, 1);
        assert_eq!(report.item_missing, 2);
        assert_eq!(report.samples.len(), 5);
    }
}
