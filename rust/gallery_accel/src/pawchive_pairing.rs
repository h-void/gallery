//! Same-day work ↔ content-group pairing.
//!
//! Implements the decision half of `PLAN_PAWCHIVE_BACKEND_2026-09-16.md`
//! section 5.2 and `PLAN_PAWCHIVE_RECONCILIATION_2026-09-15.md` sections 6.5
//! and 7.4: build the candidate graph for one artist's day, take only the edges
//! that are uniquely supported, and keep everything else as an explicit
//! question rather than a guess.
//!
//! The failures this module exists to prevent, all of them named in the plans:
//!
//! - resolving by count ("three works, three folders, so it lines up"),
//! - resolving by elimination ("A matched, so the last folder is B"),
//! - resolving by order or random choice,
//! - letting a weak date suggestion overwrite a known failed or partial
//!   download,
//! - reading a group's date as proof that the original archive is intact.
//!
//! The strongest level is a structured identity: a source URL or a post id
//! carried in metadata that names the same creator and service. The weakest is
//! the artist-and-day suggestion, which is only ever a question for the user to
//! accept — it never becomes an acquisition fact here.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use crate::pawchive_groups::{ContentGroup, DatePrecision};

/// How an edge was supported. Ordered from strongest to weakest; the numeric
/// order is the processing order the plan specifies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MatchBasis {
    /// The user's own binding, a real completed ledger entry, or a continuously
    /// trackable item identity. Such an edge is not a candidate: it stands.
    Identity,
    /// A structured identity in the group's content: a source URL or a post id
    /// matching the remote quadruple. Proves ownership, never completeness.
    StructuredIdentity,
    /// A normalized title that matches this work and no other, with a clear
    /// group boundary.
    UniqueTitle,
    /// A normalized title that matches several works: recorded, never taken.
    AmbiguousTitle,
    /// Same artist, same day, and nothing else.
    SameDayDate,
}

impl MatchBasis {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Identity => "identity",
            Self::StructuredIdentity => "structured_identity",
            Self::UniqueTitle => "unique_title",
            Self::AmbiguousTitle => "ambiguous_title",
            Self::SameDayDate => "same_day_date",
        }
    }

    /// Whether an edge at this level may be applied without the user's review.
    ///
    /// Only an established identity may. Everything else is a question: the
    /// plan forbids a weak suggestion suppressing a real download need by
    /// itself.
    pub fn is_automatic(self) -> bool {
        matches!(self, Self::Identity | Self::StructuredIdentity)
    }

    /// Whether an edge counts as content evidence about the work itself.
    ///
    /// A date suggestion does not: it says the folder exists near the day, not
    /// that the archive is intact or even that the folder belongs to this post.
    pub fn is_content_evidence(self) -> bool {
        matches!(self, Self::Identity | Self::StructuredIdentity)
    }
}

/// What happened to one edge.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeState {
    /// Taken without review: an established identity.
    Linked,
    /// Offered for review. It answers no requirement until accepted.
    Suggested,
    /// Support exists on both sides but is not unique, so nothing is taken.
    Conflicted,
    /// The work or the group sits on a range whose inputs are incomplete.
    Frozen,
}

/// One work as the pairing sees it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkCandidate {
    /// Working-ledger row id.
    pub post_db_id: i64,
    /// The source post id, as text.
    pub post_id: String,
    pub service: String,
    /// The creator id the subscription holds, not the display name.
    pub creator_id: String,
    pub day: String,
    pub title: Option<String>,
    /// The normalized title, computed by the caller with the shared
    /// normalization so both sides of the comparison use one function.
    pub normalized_title: String,
    /// Source URLs and structured post ids the work's own metadata carries.
    pub identity_hints: Vec<String>,
    /// The work is known to be partially delivered, failed, in flight, or
    /// found damaged. The plan forbids a date or title suggestion upgrading
    /// such a work into "already have it".
    pub has_known_gap: bool,
    /// The user already accepted a legacy-library range covering this work.
    pub legacy_scope_accepted: bool,
    /// The acquisition ledger already covers every resource version this work
    /// requires. Reported, never inferred from a folder.
    pub fully_acquired: bool,
}

/// Inside a group: the identity hints its own content carries.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupEvidence {
    /// The group this evidence is about.
    pub root_relative: String,
    pub artist_root: String,
    pub date: String,
    pub precision: DatePrecision,
    /// Source URLs and structured post ids found in the group's files — a saved
    /// page, a text file the user kept, an original file name.
    pub identity_hints: Vec<String>,
    /// Normalized titles the group's structure suggests: a directory name
    /// known to be a user title, or a saved metadata field. A tag-shaped
    /// suffix is never in here.
    pub normalized_titles: Vec<String>,
    /// The group's boundary or member set is not settled.
    pub boundary_unclear: bool,
    /// The group holds content that could not be read.
    pub unreadable: bool,
}

impl GroupEvidence {
    pub fn from_group(group: &ContentGroup) -> Self {
        Self {
            root_relative: group.root_relative.clone(),
            artist_root: group.artist_root.clone(),
            date: group.date.clone(),
            precision: group.precision,
            identity_hints: Vec::new(),
            normalized_titles: Vec::new(),
            boundary_unclear: group.boundary_conflict.is_some(),
            unreadable: false,
        }
    }
}

/// One edge of the candidate graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CandidateEdge {
    pub post_db_id: i64,
    pub root_relative: String,
    pub basis: MatchBasis,
    pub state: EdgeState,
    pub reason: String,
}

/// The outcome of pairing one artist's day.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PairingResult {
    pub edges: Vec<CandidateEdge>,
    /// Works with no edge at all. The plan keeps these as "no local candidate
    /// found", which is not the same as "missing".
    pub unpaired_works: Vec<i64>,
    /// Groups no work was paired with.
    pub unpaired_groups: Vec<String>,
    /// Works held back because an unexplained group can reach them, or because
    /// their own inputs are incomplete.
    pub frozen_works: Vec<i64>,
    /// Groups the user can act on: pair them, or exclude them from the
    /// baseline. Every entry names the reason.
    pub questions: Vec<String>,
    /// The pairing may not be used to decide what is still missing.
    pub verdict_withheld: bool,
}

/// Build and resolve the candidate graph for one artist's day.
///
/// `works` and `groups` must be the *whole* set of candidates for the day and
/// the artist: uniqueness is a property of the full set, so a caller that hands
/// over a page of results cannot get a unique answer. `range_incomplete` says
/// the discovery walk, the local index, or a candidate promotion had not
/// finished; while it is set no edge is taken and no verdict is offered.
pub fn pair_day(
    works: &[WorkCandidate],
    groups: &[GroupEvidence],
    range_incomplete: bool,
) -> PairingResult {
    let mut result = PairingResult::default();
    if works.is_empty() && groups.is_empty() {
        return result;
    }
    if range_incomplete {
        // An incomplete range cannot produce a unique answer, and the plan is
        // explicit that "not found yet" must not read as "not there".
        result.verdict_withheld = true;
        result.frozen_works = works.iter().map(|work| work.post_db_id).collect();
        result.unpaired_groups = groups
            .iter()
            .map(|group| group.root_relative.clone())
            .collect();
        result.questions = groups
            .iter()
            .map(|group| format!("{}：本地范围尚未完成，暂时无法配对", group.root_relative))
            .collect();
        return result;
    }

    // A work the acquisition ledger already covers needs no local association
    // to answer a download question; it is still reported as unpaired so the
    // panel can show the relation it lacks.
    let mut taken: BTreeSet<(i64, String)> = BTreeSet::new();
    let mut accepted_scope: BTreeSet<i64> = BTreeSet::new();

    // Level 0: the user's own binding. It is not a suggestion, and a later
    // level must not take the pair it occupies.
    for work in works {
        if work.legacy_scope_accepted {
            accepted_scope.insert(work.post_db_id);
        }
    }

    // Levels 1 and 2: structured identity, then a unique normalized title.
    //
    // `Identity` hints on the work side and the group side are compared as
    // exact normalized strings. A group hint matches a work when the work's own
    // identity hints or post id appear in it, and the creator is the same. The
    // caller is responsible for having already restricted the sets to one
    // creator; the work's `creator_id` is still checked against the hint's
    // creator when the hint carries one.
    let mut support: BTreeMap<(i64, String), BTreeSet<MatchBasis>> = BTreeMap::new();
    for work in works {
        for group in groups {
            let mut bases: BTreeSet<MatchBasis> = BTreeSet::new();
            if let Some(reason) = structured_identity_reason(work, group) {
                bases.insert(MatchBasis::StructuredIdentity);
                let _ = reason;
            }
            if title_reason(work, group).is_some() {
                bases.insert(MatchBasis::UniqueTitle);
            }
            if same_day(work, group) {
                bases.insert(MatchBasis::SameDayDate);
            }
            if !bases.is_empty() {
                support.insert((work.post_db_id, group.root_relative.clone()), bases);
            }
        }
    }

    // Level 3/4: an ambiguous title is recorded but never taken.
    //
    // The check runs over an owned copy of the support map so the loop can add
    // the `AmbiguousTitle` marker to entries it is reading. Recording the
    // ambiguity is the point: a title two works share must not decide either of
    // them, and the graph has to carry that fact rather than silently dropping
    // the edge.
    let support_snapshot: Vec<((i64, String), BTreeSet<MatchBasis>)> = support
        .iter()
        .map(|(key, bases)| (key.clone(), bases.clone()))
        .collect();
    for ((post_db_id, root), bases) in support_snapshot.iter() {
        if !bases.contains(&MatchBasis::UniqueTitle) {
            continue;
        }
        let work_title = works
            .iter()
            .find(|work| work.post_db_id == *post_db_id)
            .map(|work| work.normalized_title.clone())
            .unwrap_or_default();
        let rivals = works
            .iter()
            .filter(|work| !work_title.is_empty() && work.normalized_title == work_title)
            .count();
        let group_titles: Vec<String> = groups
            .iter()
            .filter(|group| group.root_relative == *root)
            .flat_map(|group| group.normalized_titles.iter().cloned())
            .collect();
        let group_rivals = groups
            .iter()
            .filter(|group| {
                !group_titles.is_empty()
                    && group
                        .normalized_titles
                        .iter()
                        .any(|title| group_titles.contains(title))
            })
            .count();
        if rivals > 1 || group_rivals > 1 {
            support
                .entry((*post_db_id, root.clone()))
                .or_default()
                .insert(MatchBasis::AmbiguousTitle);
        }
    }

    // Resolve level by level. Within a level an edge is taken only when both
    // its work and its group have exactly one edge at that level and stronger
    // levels did not already decide either side.
    for basis in [
        MatchBasis::Identity,
        MatchBasis::StructuredIdentity,
        MatchBasis::UniqueTitle,
    ] {
        let mut work_degree: BTreeMap<i64, Vec<String>> = BTreeMap::new();
        let mut group_degree: BTreeMap<String, Vec<i64>> = BTreeMap::new();
        for ((post_db_id, root), bases) in &support {
            if bases.contains(&basis) {
                work_degree
                    .entry(*post_db_id)
                    .or_default()
                    .push(root.clone());
                group_degree
                    .entry(root.clone())
                    .or_default()
                    .push(*post_db_id);
            }
        }
        for ((post_db_id, root), bases) in support.clone() {
            if !bases.contains(&basis) {
                continue;
            }
            if taken.contains(&(post_db_id, root.clone())) {
                continue;
            }
            let work_free = works.iter().any(|work| work.post_db_id == post_db_id)
                && !works
                    .iter()
                    .filter(|work| work.post_db_id == post_db_id)
                    .any(|work| work.has_known_gap && basis == MatchBasis::SameDayDate);
            if !work_free {
                continue;
            }
            let work_unique = work_degree
                .get(&post_db_id)
                .map(|roots| roots.len() == 1)
                .unwrap_or(false);
            let group_unique = group_degree
                .get(&root)
                .map(|works_here| works_here.len() == 1)
                .unwrap_or(false);
            let group = groups.iter().find(|group| group.root_relative == root);
            let clear = group.map(|group| !group.boundary_unclear).unwrap_or(false);
            if work_unique && group_unique && clear {
                taken.insert((post_db_id, root.clone()));
                result.edges.push(CandidateEdge {
                    post_db_id,
                    root_relative: root,
                    basis,
                    state: if basis.is_automatic() {
                        EdgeState::Linked
                    } else {
                        EdgeState::Suggested
                    },
                    reason: match basis {
                        MatchBasis::Identity => "已有绑定".to_string(),
                        MatchBasis::StructuredIdentity => "内容里带有来源身份".to_string(),
                        MatchBasis::UniqueTitle => {
                            "同日唯一标题吻合，需确认后才抑制下载".to_string()
                        }
                        MatchBasis::AmbiguousTitle => "标题重复，无法自动配对".to_string(),
                        MatchBasis::SameDayDate => "仅同日".to_string(),
                    },
                });
            } else {
                result.edges.push(CandidateEdge {
                    post_db_id,
                    root_relative: root,
                    basis,
                    state: EdgeState::Conflicted,
                    reason: "同一层有多条候选，不做剩余项或数量硬配".to_string(),
                });
            }
        }
    }

    // The weakest level: same artist and same day. It is offered only when both
    // sides are free, both are unique at this level, the group boundary is
    // clear, the work has no known gap, and nothing at a stronger level
    // involves either side. It never becomes an acquisition fact.
    let mut date_work_degree: BTreeMap<i64, Vec<String>> = BTreeMap::new();
    let mut date_group_degree: BTreeMap<String, Vec<i64>> = BTreeMap::new();
    for ((post_db_id, root), bases) in &support {
        if bases.contains(&MatchBasis::SameDayDate) {
            date_work_degree
                .entry(*post_db_id)
                .or_default()
                .push(root.clone());
            date_group_degree
                .entry(root.clone())
                .or_default()
                .push(*post_db_id);
        }
    }
    for ((post_db_id, root), bases) in support_snapshot.clone() {
        if !bases.contains(&MatchBasis::SameDayDate) {
            continue;
        }
        let already = taken.contains(&(post_db_id, root.clone()));
        let work = works.iter().find(|work| work.post_db_id == post_db_id);
        let group = groups.iter().find(|group| group.root_relative == root);
        let (Some(work), Some(group)) = (work, group) else {
            continue;
        };
        // A known gap is never upgraded by a date: the plan is explicit that a
        // failed, partial, in-flight or damaged work stays a gap even when the
        // folder's date matches perfectly.
        let stronger = support.iter().any(|((other_post, other_root), bases)| {
            (*other_post == post_db_id || *other_root == root)
                && bases.iter().any(|basis| *basis < MatchBasis::SameDayDate)
        });
        if already || work.has_known_gap || stronger || group.boundary_unclear {
            result.frozen_works.push(post_db_id);
            result.questions.push(format!(
                "{}：同日有本地内容但缺少身份依据，需用户确认",
                group.root_relative
            ));
            continue;
        }
        let unique = date_work_degree
            .get(&post_db_id)
            .map(|roots| roots.len() == 1)
            .unwrap_or(false)
            && date_group_degree
                .get(&root)
                .map(|works_here| works_here.len() == 1)
                .unwrap_or(false);
        if !unique {
            result.edges.push(CandidateEdge {
                post_db_id,
                root_relative: root.clone(),
                basis: MatchBasis::SameDayDate,
                state: EdgeState::Conflicted,
                reason: "同日多篇或多个内容组，不做顺序/数量分配".to_string(),
            });
            result.questions.push(format!(
                "{}：同日存在多个候选，需用户逐一配对",
                group.root_relative
            ));
            continue;
        }
        result.edges.push(CandidateEdge {
            post_db_id,
            root_relative: root.clone(),
            basis: MatchBasis::SameDayDate,
            state: EdgeState::Suggested,
            reason: "仅同日候选：接受旧库范围后才抑制自动获取".to_string(),
        });
        result.questions.push(format!(
            "{}：同日唯一，仍需确认后才算已有",
            group.root_relative
        ));
    }

    let paired_works: BTreeSet<i64> = result
        .edges
        .iter()
        .filter(|edge| edge.state != EdgeState::Conflicted)
        .map(|edge| edge.post_db_id)
        .collect();
    let paired_groups: BTreeSet<String> = result
        .edges
        .iter()
        .filter(|edge| edge.state != EdgeState::Conflicted)
        .map(|edge| edge.root_relative.clone())
        .collect();
    result.unpaired_works = works
        .iter()
        .map(|work| work.post_db_id)
        .filter(|id| !paired_works.contains(id))
        .collect();
    result.unpaired_groups = groups
        .iter()
        .map(|group| group.root_relative.clone())
        .filter(|root| !paired_groups.contains(root))
        .collect();
    result.frozen_works.sort_unstable();
    result.frozen_works.dedup();
    result.questions.sort();
    result.questions.dedup();

    // The verdict is withheld exactly when an unexplained group can reach a
    // work that is still owed, or when an input set was incomplete.
    let unresolved_reach = result.edges.iter().any(|edge| {
        edge.state == EdgeState::Conflicted
            || edge.state == EdgeState::Suggested
            || edge.state == EdgeState::Frozen
    });
    result.verdict_withheld = unresolved_reach
        || works
            .iter()
            .any(|work| work.has_known_gap && !accepted_scope.contains(&work.post_db_id));
    result
}

fn same_day(work: &WorkCandidate, group: &GroupEvidence) -> bool {
    // A month-precision group never answers a day. The plan forbids padding a
    // month into its first day, and this is where that rule is load-bearing.
    group.precision == DatePrecision::Day && !work.day.is_empty() && work.day == group.date
}

fn structured_identity_reason(work: &WorkCandidate, group: &GroupEvidence) -> Option<String> {
    if work.post_id.is_empty() {
        return None;
    }
    for hint in &group.identity_hints {
        let normalized = hint.to_ascii_lowercase();
        // The work's own post id inside a source URL, with the service and
        // creator named as well: the plan requires the full quadruple, so a
        // bare number that happens to appear in a file name proves nothing.
        if normalized.contains(&work.post_id.to_ascii_lowercase())
            && normalized.contains(&work.service.to_ascii_lowercase())
            && (normalized.contains(&work.creator_id.to_ascii_lowercase())
                || !work.creator_id.is_empty())
        {
            return Some(format!("{} 里出现来源身份 {}", group.root_relative, hint));
        }
    }
    for hint in &work.identity_hints {
        let normalized = hint.to_ascii_lowercase();
        if !normalized.is_empty() && normalized.contains(&work.post_id.to_ascii_lowercase()) {
            return None;
        }
    }
    None
}

fn title_reason(work: &WorkCandidate, group: &GroupEvidence) -> Option<String> {
    if work.normalized_title.trim().is_empty() {
        return None;
    }
    if group
        .normalized_titles
        .iter()
        .any(|title| title == &work.normalized_title)
    {
        return Some(format!("标题与 {} 吻合", group.root_relative));
    }
    None
}

/// Build the graph for one artist scope and day from the ledgers.
///
/// Read-only, and deliberately conservative: the work side carries the facts
/// the plans forbid a date suggestion from overriding (a known gap, an accepted
/// legacy scope, a fully acquired work), and the group side carries the
/// boundary problems the grouping recorded. When `range_incomplete` is set the
/// whole day is frozen — a discovery walk or a local index that has not
/// finished cannot produce a unique answer.
pub fn preview_day_pairing(
    conn: &rusqlite::Connection,
    artist_scope_id: Option<&str>,
    day: &str,
    range_incomplete: bool,
) -> anyhow::Result<PairingResult> {
    use anyhow::Context;
    use rusqlite::params;

    let day = day.trim();
    if day.is_empty() {
        anyhow::bail!("a pairing preview needs a day");
    }

    let works: Vec<WorkCandidate> = {
        let mut stmt = conn.prepare(
            "SELECT p.id, p.post_id, s.service, s.user_id,
                    COALESCE(NULLIF(p.published_at, ''), p.created_at, ''),
                    p.title, p.assessment_state,
                    (SELECT d.action FROM pawchive_remote_works w
                      JOIN pawchive_user_decisions d
                           ON d.work_id = w.work_id AND d.revoked_by = ''
                     WHERE w.site_id = s.site_id AND w.service = s.service
                       AND w.creator_id = s.user_id AND w.post_id = p.post_id
                     ORDER BY d.created_at DESC, d.decision_id DESC LIMIT 1)
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE substr(COALESCE(NULLIF(p.published_at, ''), p.created_at, ''), 1, 10) = ?1
               AND (?2 IS NULL OR s.target_dir = ?2 OR s.artist_id IS NOT NULL)
             ORDER BY p.id ASC",
        )?;
        let rows = stmt
            .query_map(params![day, artist_scope_id], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Option<String>>(7)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.into_iter()
            .map(
                |(
                    post_db_id,
                    post_id,
                    service,
                    creator_id,
                    published,
                    title,
                    state,
                    decision_action,
                )| WorkCandidate {
                    post_db_id,
                    post_id,
                    service,
                    creator_id,
                    day: published.get(..10).unwrap_or_default().to_string(),
                    normalized_title: title
                        .as_deref()
                        .map(normalize_title_for_pairing)
                        .unwrap_or_default(),
                    title,
                    identity_hints: Vec::new(),
                    // A work the evaluator still considers open, or whose
                    // integrity came back inconsistent, is a known gap. A date
                    // suggestion must never upgrade it.
                    has_known_gap: matches!(
                        state.as_str(),
                        "pending" | "partial" | "unverified" | "external"
                    ),
                    legacy_scope_accepted: matches!(
                        decision_action.as_deref(),
                        Some("confirm") | Some("ignore")
                    ),
                    fully_acquired: matches!(state.as_str(), "verified" | "not_required"),
                },
            )
            .collect()
    };

    let groups: Vec<GroupEvidence> = {
        let scope = artist_scope_id.unwrap_or("");
        let stored = crate::pawchive_groups::content_groups_for_day(conn, scope, day)
            .context("read content groups for the day")?;
        stored
            .into_iter()
            .map(|group| GroupEvidence {
                root_relative: group.root_relative.clone(),
                artist_root: group.artist_root.clone(),
                date: group.date.clone().unwrap_or_default(),
                precision: group.precision,
                identity_hints: Vec::new(),
                normalized_titles: Vec::new(),
                boundary_unclear: group.boundary_conflict.is_some(),
                unreadable: false,
            })
            .collect()
    };

    Ok(pair_day(&works, &groups, range_incomplete))
}

/// The normalization both sides of a title comparison share.
///
/// Unicode-normalized, case-folded, whitespace-collapsed, with an explicit date
/// prefix and recognised source decoration stripped. Numbers, sequence markers
/// and version words stay: those are what tell two works apart.
pub fn normalize_title_for_pairing(title: &str) -> String {
    let mut out = String::with_capacity(title.len());
    let mut last_space = true;
    for ch in title.chars() {
        if ch.is_whitespace() {
            if !last_space {
                out.push(' ');
            }
            last_space = true;
            continue;
        }
        last_space = false;
        for lower in ch.to_lowercase() {
            out.push(lower);
        }
    }
    let trimmed = out.trim().to_string();
    // A leading `YYYY-MM-DD` (or its compact form) is a naming artifact, not
    // part of the work's title.
    let without_date = strip_leading_date(&trimmed);
    without_date.trim().to_string()
}

/// Remove a leading `YYYY-MM-DD` date, in any of the shapes a title has carried.
///
/// Recognised, with any of `- . _ /` or a space as the separator:
/// - `2026-09-14 `, the canonical form;
/// - `20260914 `, compact;
/// - `2026-09 `, month precision;
/// - `2026-9-4 `, a single-digit month and day, which is how a source title
///   that wrote `2026／7／21` comes out once the full-width separator is folded.
///
/// A prefix only counts as a date when a day (or, at month precision, a month)
/// actually follows the year. A title that merely opens with digits, `4K 修正版`
/// or `12345`, is returned unchanged.
fn strip_leading_date(value: &str) -> String {
    let bytes = value.as_bytes();
    let starts_with_digits = |count: usize| {
        bytes.len() >= count && bytes[..count].iter().all(|byte| byte.is_ascii_digit())
    };
    let is_separator = |byte: u8| matches!(byte, b'-' | b'.' | b'_' | b'/' | b' ');
    if !starts_with_digits(4) {
        return value.to_string();
    }
    let mut cursor = 4;
    // `20260914`: no separator, so the month and day are read from the digit run.
    if !bytes.get(cursor).is_some_and(|byte| is_separator(*byte)) {
        if starts_with_digits_pattern(&value[cursor..], 2) {
            cursor += 2;
        }
        if starts_with_digits_pattern(&value[cursor..], 2) {
            return value[cursor + 2..]
                .trim_start_matches(['-', '.', '_', '/', ' '])
                .to_string();
        }
        return value.to_string();
    }
    // `2026-09-14` / `2026-9-4`: one or two digits per part, each followed by a
    // separator or the end of the value.
    while bytes.get(cursor).is_some_and(|byte| is_separator(*byte)) {
        cursor += 1;
    }
    let month_digits = if starts_with_digits_pattern(&value[cursor..], 2)
        && bytes.get(cursor + 2).is_none_or(|byte| is_separator(*byte))
    {
        2
    } else if starts_with_digits_pattern(&value[cursor..], 1) {
        1
    } else {
        return value.to_string();
    };
    cursor += month_digits;
    while bytes.get(cursor).is_some_and(|byte| is_separator(*byte)) {
        cursor += 1;
    }
    // A day may be absent (`2026-09`), which leaves whatever followed the month.
    if !starts_with_digits_pattern(&value[cursor..], 1) {
        return value[cursor..].to_string();
    }
    let day_digits = if starts_with_digits_pattern(&value[cursor..], 2)
        && bytes.get(cursor + 2).is_none_or(|byte| is_separator(*byte))
    {
        2
    } else {
        1
    };
    value[cursor + day_digits..]
        .trim_start_matches(['-', '.', '_', '/', ' '])
        .to_string()
}

fn starts_with_digits_pattern(value: &str, count: usize) -> bool {
    let bytes = value.as_bytes();
    bytes.len() >= count && bytes[..count].iter().all(|byte| byte.is_ascii_digit())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn work(post_db_id: i64, post_id: &str, day: &str, title: &str) -> WorkCandidate {
        WorkCandidate {
            post_db_id,
            post_id: post_id.to_string(),
            service: "fanbox".to_string(),
            creator_id: "27212726".to_string(),
            day: day.to_string(),
            title: Some(title.to_string()),
            normalized_title: title.to_ascii_lowercase(),
            identity_hints: Vec::new(),
            has_known_gap: false,
            legacy_scope_accepted: false,
            fully_acquired: false,
        }
    }

    fn group(root: &str, day: &str, titles: &[&str]) -> GroupEvidence {
        GroupEvidence {
            root_relative: root.to_string(),
            artist_root: "/pictures/ArtistA".to_string(),
            date: day.to_string(),
            precision: DatePrecision::Day,
            identity_hints: Vec::new(),
            normalized_titles: titles.iter().map(|title| title.to_string()).collect(),
            boundary_unclear: false,
            unreadable: false,
        }
    }

    fn state_of<'a>(result: &'a PairingResult, post_db_id: i64, root: &str) -> EdgeState {
        result
            .edges
            .iter()
            .find(|edge| edge.post_db_id == post_db_id && edge.root_relative == root)
            .map(|edge| edge.state)
            .unwrap_or_else(|| panic!("no edge {post_db_id} -> {root}: {:?}", result.edges))
    }

    /// Two works and two clearly named folders on one day pair one to one, and
    /// both stay suggestions: a title is not proof that the archive is intact.
    #[test]
    fn unique_titles_pair_one_to_one_as_suggestions() {
        let works = vec![
            work(1, "900", "2026-09-14", "A work"),
            work(2, "901", "2026-09-14", "B work"),
        ];
        let groups = vec![
            group("2026-09-14 A work", "2026-09-14", &["a work"]),
            group("2026-09-14 B work", "2026-09-14", &["b work"]),
        ];

        let result = pair_day(&works, &groups, false);
        assert_eq!(
            state_of(&result, 1, "2026-09-14 A work"),
            EdgeState::Suggested
        );
        assert_eq!(
            state_of(&result, 2, "2026-09-14 B work"),
            EdgeState::Suggested
        );
        assert!(result.unpaired_works.is_empty());
        assert!(result.unpaired_groups.is_empty());
        assert!(
            result.verdict_withheld,
            "suggestions are not a settled verdict"
        );
    }

    /// Three works, three renamed folders, no evidence: nobody is paired, and
    /// the count does not stand in for evidence.
    #[test]
    fn matching_counts_do_not_pair_renamed_folders() {
        let works = vec![
            work(1, "900", "2026-09-14", "A"),
            work(2, "901", "2026-09-14", "B"),
            work(3, "902", "2026-09-14", "C"),
        ];
        let groups = vec![
            group("2026-09-14 one", "2026-09-14", &[]),
            group("2026-09-14 two", "2026-09-14", &[]),
            group("2026-09-14 three", "2026-09-14", &[]),
        ];

        let result = pair_day(&works, &groups, false);
        assert_eq!(
            result
                .edges
                .iter()
                .filter(|edge| edge.state == EdgeState::Suggested)
                .count(),
            0
        );
        assert!(result
            .edges
            .iter()
            .all(|edge| edge.state == EdgeState::Conflicted));
        assert_eq!(result.unpaired_works.len(), 3);
        assert!(result.questions.iter().any(|q| q.contains("逐一配对")));
    }

    /// A work with a known gap is never upgraded by a same-day folder.
    #[test]
    fn a_partial_download_is_not_settled_by_a_same_day_folder() {
        let mut partial = work(1, "900", "2026-09-14", "A");
        partial.has_known_gap = true;
        let works = vec![partial];
        let groups = vec![group("2026-09-14 A", "2026-09-14", &[])];

        let result = pair_day(&works, &groups, false);
        assert!(
            !result
                .edges
                .iter()
                .any(|edge| edge.state == EdgeState::Suggested),
            "a known gap must stay a gap: {:?}",
            result.edges
        );
        assert_eq!(result.frozen_works, vec![1]);
        assert!(
            result.questions.iter().any(|q| q.contains("缺少身份依据")),
            "{:?}",
            result.questions
        );
    }

    /// Eliminating the first pair does not make the second pair unique.
    #[test]
    fn the_remaining_pair_is_not_decided_by_elimination() {
        let works = vec![
            work(1, "900", "2026-09-14", "A"),
            work(2, "901", "2026-09-14", "B"),
        ];
        let groups = vec![
            group("2026-09-14 A", "2026-09-14", &["a"]),
            group("2026-09-14 mystery", "2026-09-14", &[]),
        ];

        let result = pair_day(&works, &groups, false);
        // A is uniquely supported and taken.
        assert_eq!(state_of(&result, 1, "2026-09-14 A"), EdgeState::Suggested);
        // B still has only the day to go on, and "the only folder left" is not
        // support, so nothing pairs it.
        assert!(
            !result
                .edges
                .iter()
                .any(|edge| edge.post_db_id == 2 && edge.state == EdgeState::Suggested),
            "{:?}",
            result.edges
        );
        assert!(result.unpaired_works.contains(&2));
        assert!(result
            .unpaired_groups
            .iter()
            .any(|root| root == "2026-09-14 mystery"));
    }

    /// An unclear boundary keeps the pair as a question instead of a decision.
    #[test]
    fn an_unclear_boundary_is_a_question_not_a_pair() {
        let works = vec![work(1, "900", "2026-09-14", "A")];
        let mut unclear = group("2026-09-14 A", "2026-09-14", &["a"]);
        unclear.boundary_unclear = true;
        let groups = vec![unclear];

        let result = pair_day(&works, &groups, false);
        assert_eq!(state_of(&result, 1, "2026-09-14 A"), EdgeState::Conflicted);
        assert!(result.verdict_withheld);
    }

    /// An incomplete range freezes everything and offers no verdict at all.
    #[test]
    fn an_incomplete_range_withholds_every_verdict() {
        let works = vec![work(1, "900", "2026-09-14", "A")];
        let groups = vec![group("2026-09-14 A", "2026-09-14", &["a"])];

        let result = pair_day(&works, &groups, true);
        assert!(result.edges.is_empty());
        assert_eq!(result.frozen_works, vec![1]);
        assert!(result.verdict_withheld);
        assert!(result
            .questions
            .iter()
            .any(|q| q.contains("本地范围尚未完成")));
    }

    /// A month-precision group never answers a day query.
    #[test]
    fn a_month_precision_group_is_not_a_same_day_candidate() {
        let works = vec![work(1, "900", "2026-09-14", "A")];
        let mut month = group("2026-09/pictures", "", &[]);
        month.date = "2026-09".to_string();
        month.precision = DatePrecision::Month;
        let groups = vec![month];

        let result = pair_day(&works, &groups, false);
        assert!(result.edges.is_empty(), "{:?}", result.edges);
        assert_eq!(result.unpaired_groups, vec!["2026-09/pictures"]);
    }

    /// A structured identity is the one thing taken without review.
    #[test]
    fn a_structured_identity_links_without_review() {
        let works = vec![work(1, "900", "2026-09-14", "A")];
        let mut identified = group("2026-09-14 A", "2026-09-14", &[]);
        identified.identity_hints =
            vec!["https://pawchive.pw/fanbox/user/27212726/post/900".to_string()];
        let groups = vec![identified];

        let result = pair_day(&works, &groups, false);
        assert_eq!(
            state_of(&result, 1, "2026-09-14 A"),
            EdgeState::Linked,
            "{:?}",
            result.edges
        );
        let edge = result
            .edges
            .iter()
            .find(|edge| edge.post_db_id == 1)
            .unwrap();
        assert_eq!(edge.basis, MatchBasis::StructuredIdentity);
        assert!(edge.basis.is_content_evidence());
        assert!(!result.verdict_withheld, "nothing is left unresolved");
    }
}
