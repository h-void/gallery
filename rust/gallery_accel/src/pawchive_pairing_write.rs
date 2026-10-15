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
