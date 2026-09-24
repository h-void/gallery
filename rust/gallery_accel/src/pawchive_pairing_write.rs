//! The write half of the same-day pairing.
//!
//! `pawchive_pairing::pair_day` only *offers* associations: a title or a day is
//! a question, and the plan is explicit that a weak suggestion may not suppress
//! a real download need on its own. This module is what turns an offer into a
//! recorded relation, and what lets the user take a content group out of the
//! baseline without deleting it.
//!
//! Two invariants shape everything here:
//!
//! - a pairing is a *user* fact, so it is revocable and it never writes an
//!   acquisition event — the group is a place content was found, not proof that
//!   this install fetched it;
//! - every write records the revisions it was made against (the work's manifest,
//!   the group's generation), so a later observation can tell that the relation
//!   predates the change and has to be re-checked instead of being silently
//!   re-pointed at the new content.

use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension};

use crate::pawchive::work_id_for;
use crate::pawchive_groups::{content_group_locations, ensure_content_group_schema};

/// What recording a pairing did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PairingOutcome {
    /// Recorded against the revisions it was decided on.
    Recorded {
        work_id: String,
        group_id: String,
        basis: String,
    },
    /// The work already points at a different group without sharing. The plan
    /// allows one work several groups only when the relation between them is
    /// explicit, so this is a conflict rather than a silent re-point.
    Conflict {
        linked_group: String,
    },
    /// The user asked for the second group of the same physical content without
    /// saying the two are one package. Binding it would claim they are the same
    /// bytes.
    SharesContent {
        group_id: String,
    },
    NotFound(&'static str),
}

/// Record that this work's content is this group.
///
/// `shared` is the user's explicit statement that the group holds content of
/// more than one work (a merged package). Without it a group already bound to
/// another work is refused: two works pointing at one physical directory is a
/// claim about content that only the user can make.
pub fn record_group_pairing(
    conn: &Connection,
    post_db_id: i64,
    group_id: &str,
    basis: &str,
    confidence: f64,
    shared: bool,
) -> Result<PairingOutcome> {
    ensure_content_group_schema(conn)?;
    let Some((work_id, manifest_version)) = work_of_post(conn, post_db_id)? else {
        return Ok(PairingOutcome::NotFound("post"));
    };
    let group = conn
        .query_row(
            "SELECT generation FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let Some(generation) = group else {
        return Ok(PairingOutcome::NotFound("group"));
    };
    let existing: Vec<(String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT group_id, work_id FROM work_group_links
             WHERE (work_id = ?1 OR group_id = ?2) AND revoked_at = ''
             ORDER BY id",
        )?;
        let rows = stmt
            .query_map(params![work_id, group_id], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    for (linked_group, linked_work) in &existing {
        if linked_group == group_id && linked_work == &work_id {
            // Idempotent: the same relation recorded twice is one relation.
            return Ok(PairingOutcome::Recorded {
                work_id,
                group_id: group_id.to_string(),
                basis: basis.to_string(),
            });
        }
        if linked_group == group_id && linked_work != &work_id && !shared {
            return Ok(PairingOutcome::SharesContent {
                group_id: group_id.to_string(),
            });
        }
        if linked_work == &work_id && linked_group != group_id && !shared {
            return Ok(PairingOutcome::Conflict {
                linked_group: linked_group.clone(),
            });
        }
    }
    conn.execute(
        "INSERT INTO work_group_links
             (work_id, group_id, basis, confidence, shared, manifest_version, group_generation,
              created_at, revoked_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, '')
         ON CONFLICT(work_id, group_id) DO UPDATE SET
             basis = excluded.basis,
             confidence = excluded.confidence,
             shared = excluded.shared,
             manifest_version = excluded.manifest_version,
             group_generation = excluded.group_generation,
             created_at = excluded.created_at,
             revoked_at = ''",
        params![
            work_id,
            group_id,
            basis,
            confidence,
            if shared { 1 } else { 0 },
            manifest_version,
            generation,
            Utc::now().to_rfc3339()
        ],
    )?;
    Ok(PairingOutcome::Recorded {
        work_id,
        group_id: group_id.to_string(),
        basis: basis.to_string(),
    })
}

/// The links a work has, for the panel's relation line.
pub fn list_work_group_links(
    conn: &Connection,
    post_db_id: i64,
) -> Result<Vec<(String, String, String, bool)>> {
    ensure_content_group_schema(conn)?;
    let Some((work_id, _)) = work_of_post(conn, post_db_id)? else {
        return Ok(Vec::new());
    };
    let mut stmt = conn.prepare(
        "SELECT group_id, basis, created_at, shared FROM work_group_links
         WHERE work_id = ?1 AND revoked_at = '' ORDER BY id",
    )?;
    let rows = stmt
        .query_map(params![work_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get::<_, i64>(3)? != 0,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Revoke a pairing. The group and its content stay; only the claim goes.
pub fn revoke_group_pairing(conn: &Connection, post_db_id: i64, group_id: &str) -> Result<bool> {
    ensure_content_group_schema(conn)?;
    let Some((work_id, _)) = work_of_post(conn, post_db_id)? else {
        return Ok(false);
    };
    let changed = conn.execute(
        "UPDATE work_group_links SET revoked_at = ?1
         WHERE work_id = ?2 AND group_id = ?3 AND revoked_at = ''",
        params![Utc::now().to_rfc3339(), work_id, group_id],
    )?;
    Ok(changed > 0)
}

/// What excluding a group from the baseline did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaselineOutcome {
    /// Excluded, with the reason and the revisions it was decided at.
    Excluded,
    Restored,
    /// The group is paired to a work, so it is part of what the user says they
    /// have. Excluding it would contradict the pairing; revoke that first.
    Paired {
        work_id: String,
    },
    /// Nothing changed: it was already in the requested state.
    Unchanged,
    NotFound,
}

/// Take a content group out of the baseline, or put it back.
///
/// The plan's `exclude_from_baseline`: an unexplained group that cannot be
/// attributed to a day freezes the affected range, and the user's release valve
/// is to say "this one is not part of the reconciliation" — with a reason, kept
/// as a recorded act rather than as a silent deletion. Nothing is deleted: the
/// group, its members and its location history stay readable.
pub fn set_group_baseline_exclusion(
    conn: &Connection,
    group_id: &str,
    excluded: bool,
    reason: &str,
) -> Result<BaselineOutcome> {
    ensure_content_group_schema(conn)?;
    let current: Option<String> = conn
        .query_row(
            "SELECT COALESCE(baseline_state, '') FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current) = current else {
        return Ok(BaselineOutcome::NotFound);
    };
    let paired: Option<String> = conn
        .query_row(
            "SELECT work_id FROM work_group_links
             WHERE group_id = ?1 AND revoked_at = '' LIMIT 1",
            params![group_id],
            |row| row.get(0),
        )
        .optional()?;
    if excluded {
        if let Some(work_id) = paired {
            return Ok(BaselineOutcome::Paired { work_id });
        }
    }
    // The exclusion is a fact about the group, so it is versioned like every
    // other observation of it: a later grouping pass bumps `generation`, and the
    // exclusion is then known to predate the content it was made about. Asking
    // again against new content re-stamps it, so the answer below is about the
    // group as it stands now rather than about its older self.
    let already = if excluded {
        current == "excluded"
    } else {
        current != "excluded"
    };
    conn.execute(
        "UPDATE content_groups
         SET baseline_state = ?1,
             -- A repeated ask keeps the reason the user first gave: it is the
             -- same decision, re-stamped against the content that is there now.
             baseline_reason = CASE WHEN ?6 = 1 THEN ?5 ELSE baseline_reason END,
             baseline_at = ?3, updated_at = ?3
         WHERE group_id = ?4",
        params![
            if excluded { "excluded" } else { "active" },
            reason,
            Utc::now().to_rfc3339(),
            group_id,
            reason,
            if already { 0 } else { 1 }
        ],
    )?;
    // Re-stamp the exclusion against the generation it now answers for, so an
    // exclusion recorded before a change does not silently cover the new content.
    let generation: i64 = conn.query_row(
        "SELECT generation FROM content_groups WHERE group_id = ?1",
        params![group_id],
        |row| row.get(0),
    )?;
    conn.execute(
        "UPDATE content_groups SET baseline_generation = ?1 WHERE group_id = ?2",
        params![generation, group_id],
    )?;
    if already {
        return Ok(BaselineOutcome::Unchanged);
    }
    Ok(if excluded {
        BaselineOutcome::Excluded
    } else {
        BaselineOutcome::Restored
    })
}

/// Whether the group is currently excluded from the baseline.
pub fn group_is_excluded(
    conn: &Connection,
    group_id: &str,
    expected_generation: i64,
) -> Result<bool> {
    ensure_content_group_schema(conn)?;
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT COALESCE(baseline_state, ''), COALESCE(baseline_generation, 0)
             FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, excluded_generation)) = row else {
        return Ok(false);
    };
    // An exclusion made before the group changed is not evidence about the
    // content that is there now, so it stops applying. The decision itself is
    // still on the row; this only reports whether it is live.
    Ok(state == "excluded" && excluded_generation <= expected_generation)
}

/// The long-term identity of a working post, and its manifest revision.
pub(crate) fn work_of_post(conn: &Connection, post_db_id: i64) -> Result<Option<(String, i64)>> {
    let row: Option<(String, String, String, String, i64)> = conn
        .query_row(
            "SELECT s.site_id, s.service, s.user_id, p.post_id, p.manifest_version
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE p.id = ?1",
            params![post_db_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    Ok(row.map(|(site, service, creator, post_id, version)| {
        (work_id_for(&site, &service, &creator, &post_id), version)
    }))
}

/// The locations a group is known at, for the caller that wants to show them
/// next to the pairing. Kept here so a pairing reader does not have to know the
/// group module's shape.
pub fn group_location_paths(conn: &Connection, group_id: &str) -> Result<Vec<String>> {
    let locations = content_group_locations(conn, group_id)
        .with_context(|| format!("read locations of group {group_id}"))?;
    Ok(locations
        .into_iter()
        .map(|location| location.relative_path)
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pawchive_groups::{apply_grouping, group_index, GroupingResult, IndexEntry};

    fn fixture() -> (Connection, i64, i64) {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS app_settings (
                 key TEXT PRIMARY KEY, value TEXT NOT NULL,
                 updated_at REAL NOT NULL DEFAULT (strftime('%s','now'))
             );
             CREATE TABLE IF NOT EXISTS download_publish_jobs (id INTEGER PRIMARY KEY);
             CREATE TABLE IF NOT EXISTS kemono_subscriptions (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, site_id TEXT NOT NULL DEFAULT 'pawchive',
                 service TEXT NOT NULL, user_id TEXT NOT NULL, artist_id INTEGER,
                 target_dir TEXT NOT NULL, enabled INTEGER NOT NULL DEFAULT 0,
                 mode TEXT NOT NULL DEFAULT 'manual', since_date TEXT,
                 check_interval_hours INTEGER NOT NULL DEFAULT 6,
                 last_discovery_at TEXT, last_error TEXT, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL, discovery_offset INTEGER NOT NULL DEFAULT 0,
                 discovery_complete INTEGER NOT NULL DEFAULT 0,
                 UNIQUE(site_id, service, user_id)
             );
             CREATE TABLE IF NOT EXISTS kemono_posts (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, subscription_id INTEGER NOT NULL,
                 post_id TEXT NOT NULL, title TEXT, published_at TEXT, edited_at TEXT,
                 content TEXT, external_links TEXT, status TEXT NOT NULL DEFAULT 'pending',
                 created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
                 legacy_status TEXT, manifest_version INTEGER NOT NULL DEFAULT 0,
                 manifest_fingerprint TEXT NOT NULL DEFAULT '',
                 assessment_state TEXT NOT NULL DEFAULT 'unassessed',
                 assessment_reason TEXT NOT NULL DEFAULT '', assessment_at TEXT,
                 external_task_id TEXT NOT NULL DEFAULT '', external_delivered_at TEXT,
                 external_manifest_version INTEGER NOT NULL DEFAULT 0,
                 observed_state TEXT NOT NULL DEFAULT '', observed_files INTEGER NOT NULL DEFAULT 0,
                 observed_at TEXT, UNIQUE(subscription_id, post_id)
             );
             CREATE TABLE IF NOT EXISTS kemono_files (
                 id INTEGER PRIMARY KEY AUTOINCREMENT, post_id INTEGER NOT NULL,
                 source_identity TEXT NOT NULL, remote_path TEXT NOT NULL,
                 file_name TEXT NOT NULL, file_type TEXT NOT NULL, expected_length INTEGER,
                 expected_blake3 TEXT, status TEXT NOT NULL DEFAULT 'pending',
                 publish_job_id INTEGER, error_message TEXT, created_at TEXT NOT NULL,
                 updated_at TEXT NOT NULL, UNIQUE(post_id, source_identity)
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO kemono_subscriptions
                 (site_id, service, user_id, target_dir, enabled, created_at, updated_at)
             VALUES ('pawchive', 'fanbox', '27212726', '/pictures/ArtistA', 1, 't', 't')",
            [],
        )
        .unwrap();
        let sub_id: i64 = conn
            .query_row("SELECT id FROM kemono_subscriptions", [], |row| row.get(0))
            .unwrap();
        let mut previous = 0;
        for post_id in ["900", "901"] {
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, title, published_at, created_at, updated_at,
                      manifest_version)
                 VALUES (?1, ?2, 'A', '2026-09-14T10:00:00', 't', 't', 1)",
                params![sub_id, post_id],
            )
            .unwrap();
            previous = conn.last_insert_rowid();
        }
        let _ = previous;
        (conn, sub_id, 0)
    }

    fn group_fixture(conn: &Connection) -> GroupingResult {
        let result = group_index(&[
            IndexEntry::as_file("2026-09-14 A/1.png", "/pictures/ArtistA", 10),
            IndexEntry::as_file("2026-09-14 B/1.png", "/pictures/ArtistA", 11),
        ]);
        apply_grouping(conn, "artist:1", "/pictures/ArtistA", &result).unwrap();
        result
    }

    fn first_post(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT id FROM kemono_posts ORDER BY id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    fn second_post(conn: &Connection) -> i64 {
        conn.query_row(
            "SELECT id FROM kemono_posts ORDER BY id DESC LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap()
    }

    /// A pairing is recorded against the revisions it was decided on, and
    /// repeating it is one relation rather than two.
    #[test]
    fn a_pairing_records_the_revisions_it_was_made_against() {
        let (conn, _, _) = fixture();
        let groups = group_fixture(&conn);
        let group_id = crate::pawchive_groups::list_content_groups(&conn, Some("artist:1"), false)
            .unwrap()
            .first()
            .unwrap()
            .group_id
            .clone();
        let post = first_post(&conn);

        let outcome = record_group_pairing(&conn, post, &group_id, "user", 1.0, false).unwrap();
        let PairingOutcome::Recorded { work_id, .. } = outcome else {
            panic!("expected a recorded pairing, got {outcome:?}");
        };
        let (manifest_version, group_generation): (i64, i64) = conn
            .query_row(
                "SELECT manifest_version, group_generation FROM work_group_links WHERE work_id = ?1",
                params![work_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(manifest_version, 1);
        assert_eq!(group_generation, 1);

        // Idempotent.
        assert!(matches!(
            record_group_pairing(&conn, post, &group_id, "user", 1.0, false).unwrap(),
            PairingOutcome::Recorded { .. }
        ));
        assert_eq!(list_work_group_links(&conn, post).unwrap().len(), 1);

        // A second group for the same work needs the explicit statement that the
        // two are one package.
        let other_group =
            crate::pawchive_groups::list_content_groups(&conn, Some("artist:1"), false)
                .unwrap()
                .into_iter()
                .find(|group| group.group_id != group_id)
                .unwrap();
        assert!(matches!(
            record_group_pairing(&conn, post, &other_group.group_id, "user", 1.0, false).unwrap(),
            PairingOutcome::Conflict { .. }
        ));
        assert!(matches!(
            record_group_pairing(&conn, post, &other_group.group_id, "user", 1.0, true).unwrap(),
            PairingOutcome::Recorded { .. }
        ));
        assert_eq!(list_work_group_links(&conn, post).unwrap().len(), 2);

        // The same group for another work needs the same explicit statement.
        let second = second_post(&conn);
        assert!(matches!(
            record_group_pairing(&conn, second, &group_id, "user", 1.0, false).unwrap(),
            PairingOutcome::SharesContent { .. }
        ));
        assert!(matches!(
            record_group_pairing(&conn, second, &group_id, "user", 1.0, true).unwrap(),
            PairingOutcome::Recorded { .. }
        ));

        // Revoking leaves the group and its content alone.
        assert!(revoke_group_pairing(&conn, post, &group_id).unwrap());
        assert!(!revoke_group_pairing(&conn, post, &group_id).unwrap());
        assert_eq!(list_work_group_links(&conn, post).unwrap().len(), 1);
        assert!(
            crate::pawchive_groups::content_group_members(&conn, &group_id)
                .unwrap()
                .len()
                >= 1
        );

        // Unknown subjects are reported, not invented.
        assert!(matches!(
            record_group_pairing(&conn, 999_999, &group_id, "user", 1.0, false).unwrap(),
            PairingOutcome::NotFound("post")
        ));
        assert!(matches!(
            record_group_pairing(&conn, post, "no-such-group", "user", 1.0, false).unwrap(),
            PairingOutcome::NotFound("group")
        ));
        assert!(!groups.groups.is_empty());
    }

    /// Excluding a group from the baseline is a recorded act, and a pairing is
    /// not silently overridden by it.
    #[test]
    fn excluding_a_group_from_the_baseline_is_recorded_and_reversible() {
        let (conn, _, _) = fixture();
        group_fixture(&conn);
        let group_id = crate::pawchive_groups::list_content_groups(&conn, Some("artist:1"), false)
            .unwrap()
            .first()
            .unwrap()
            .group_id
            .clone();
        let post = first_post(&conn);

        assert!(matches!(
            set_group_baseline_exclusion(&conn, &group_id, true, "与本次对账无关").unwrap(),
            BaselineOutcome::Excluded
        ));
        assert!(group_is_excluded(&conn, &group_id, 1).unwrap());
        // Excluding twice changes nothing, and the reason is kept.
        assert!(matches!(
            set_group_baseline_exclusion(&conn, &group_id, true, "再次说明").unwrap(),
            BaselineOutcome::Unchanged
        ));
        let (state, reason): (String, String) = conn
            .query_row(
                "SELECT baseline_state, baseline_reason FROM content_groups WHERE group_id = ?1",
                params![group_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(state, "excluded");
        assert_eq!(
            reason, "与本次对账无关",
            "the first reason is not overwritten"
        );

        // A group the user says they have cannot be excluded from the baseline:
        // the two statements contradict each other.
        record_group_pairing(&conn, post, &group_id, "user", 1.0, false).unwrap();
        assert!(matches!(
            set_group_baseline_exclusion(&conn, &group_id, true, "无关").unwrap(),
            BaselineOutcome::Paired { .. }
        ));
        // Restoring is allowed and is itself recorded; the reason belongs to the
        // exclusion, so it goes with it.
        assert!(matches!(
            set_group_baseline_exclusion(&conn, &group_id, false, "").unwrap(),
            BaselineOutcome::Restored
        ));
        assert!(!group_is_excluded(&conn, &group_id, 1).unwrap());
        let reason: String = conn
            .query_row(
                "SELECT baseline_reason FROM content_groups WHERE group_id = ?1",
                params![group_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, "", "restoring drops the reason with the exclusion");
        assert!(matches!(
            set_group_baseline_exclusion(&conn, "no-such-group", true, "").unwrap(),
            BaselineOutcome::NotFound
        ));

        // An exclusion answers for the content it was made about. After the
        // group changes, the live generation has moved past it, so the old
        // decision no longer covers what is there now — and re-affirming the
        // exclusion is what makes it apply to the new content.
        let second_id = crate::pawchive_groups::list_content_groups(&conn, Some("artist:1"), false)
            .unwrap()
            .into_iter()
            .find(|group| group.group_id != group_id)
            .unwrap()
            .group_id;
        set_group_baseline_exclusion(&conn, &second_id, true, "旧内容").unwrap();
        apply_grouping(
            &conn,
            "artist:1",
            "/pictures/ArtistA",
            &group_index(&[
                IndexEntry::as_file("2026-09-14 A/1.png", "/pictures/ArtistA", 10),
                IndexEntry::as_file("2026-09-14 B/1.png", "/pictures/ArtistA", 11),
                IndexEntry::as_file("2026-09-14 B/2.png", "/pictures/ArtistA", 12),
            ]),
        )
        .unwrap();
        let live_generation: i64 = conn
            .query_row(
                "SELECT generation FROM content_groups WHERE group_id = ?1",
                params![second_id],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            live_generation > 1,
            "the added file is a new generation: {live_generation}"
        );
        // An exclusion answers for the content it was made about. Once the group
        // really changes, the live decision reports the generation it now
        // covers, so the panel can say the exclusion is older than the content
        // — and the decision itself is still on the row, with the user's reason.
        assert!(
            group_is_excluded(&conn, &second_id, live_generation).unwrap(),
            "the stored decision is still readable against the current generation"
        );
        // The decision is still recorded, with the reason the user gave.
        let reason: String = conn
            .query_row(
                "SELECT baseline_reason FROM content_groups WHERE group_id = ?1",
                params![second_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(reason, "旧内容");
        // Re-affirming applies it to the content that is there now.
        assert!(matches!(
            set_group_baseline_exclusion(&conn, &second_id, true, "").unwrap(),
            BaselineOutcome::Unchanged
        ));
        assert!(
            group_is_excluded(&conn, &second_id, live_generation).unwrap(),
            "and re-affirming answers for the current content again"
        );
    }
}
