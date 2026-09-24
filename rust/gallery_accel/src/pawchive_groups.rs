//! Logical content groups for local reconciliation.
//!
//! Implements the grouping half of `PLAN_PAWCHIVE_BACKEND_2026-09-16.md`
//! section 5.1 and `PLAN_PAWCHIVE_RECONCILIATION_2026-09-15.md` sections
//! 6.3/7.4: turn the indexed paths under one authorized artist root into the
//! groups a work can be associated with, and record *why* each boundary was
//! drawn.
//!
//! The rules the code below exists to hold:
//!
//! - a date in a path is a boundary only at day precision. A year or a month is
//!   a collection layer that narrows the context; a pure date container is one
//!   boundary whether it holds directories or a zip and its extracted tree.
//! - an unreadable or ambiguous structure is kept as ambiguity, never resolved
//!   by counting directories or by "the remaining post must be this group".
//! - a group needs real content. An empty directory, a downloaded `.part`, and a
//!   directory holding only this application's own metadata text are not
//!   content, while the user's own non-empty `.txt` or `.psd` is.
//! - the group's date keeps how it was found: which path component, at what
//!   precision, and by which parser version. The existing `detected_date`
//!   column cannot reconstruct that, which is why the reason travels with it.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use crate::media_roots::MediaRoots;
use crate::media_type::media_type_for_file;
use anyhow::{Context, Result};
use chrono::Utc;
use rusqlite::{params, Connection, OptionalExtension, Transaction, TransactionBehavior};
use serde::{Deserialize, Serialize};

/// Version of the path parser below. Stored with every group so a later parser
/// can tell which readings it may reinterpret and which are historical fact.
pub const GROUP_PARSER_VERSION: &str = "1";

/// Precision of a date found in a path.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DatePrecision {
    /// No date was recognized anywhere on the path.
    Unknown,
    /// A year only (`2026`).
    Year,
    /// A year and month (`2026-09`, `202609`, `2026.09`).
    Month,
    /// A full calendar day.
    Day,
}

impl DatePrecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Unknown => "unknown",
            Self::Year => "year",
            Self::Month => "month",
            Self::Day => "day",
        }
    }

    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "year" => Self::Year,
            "month" => Self::Month,
            "day" => Self::Day,
            _ => Self::Unknown,
        }
    }

    /// Whether a reading at this precision may serve as a group boundary.
    ///
    /// Only a day does. A month is not expanded to its first day — the plan is
    /// explicit that month-only evidence must not be padded into a day — so a
    /// month names a context, not a work root.
    pub fn is_boundary(self) -> bool {
        self == Self::Day
    }
}

/// One path component read as a date.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DateComponent {
    /// Which component of the relative path carries the date, zero-based.
    pub component_index: usize,
    /// The component's text, verbatim.
    pub component: String,
    /// Canonical `YYYY-MM-DD` for a day, `YYYY-MM` for a month, `YYYY` for a
    /// year. Empty when nothing was recognized.
    pub canonical: String,
    pub precision: DatePrecision,
    /// The attribute on `media_type::detect_date_value_from_folder`'s result
    /// this reading came from, when the shared date parser recognized it.
    pub basis: String,
}

/// Why a component was skipped rather than treated as a work root.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CollectionLayer {
    pub component_index: usize,
    pub component: String,
    pub reason: String,
}

/// One file inside a group.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupMember {
    /// Path relative to the artist root, with `/` separators.
    pub relative_path: String,
    pub file_name: String,
    pub file_size: u64,
    /// `image` / `video` / `archive` / `other`, as the library classifies it.
    pub media_type: String,
    /// Whether this application generated the file as bookkeeping text.
    pub derived: bool,
}

/// A logical content group: one work root and everything under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentGroup {
    /// Path relative to the artist root, always a directory (or the artist root
    /// itself, as the empty string).
    pub root_relative: String,
    /// The artist root this group was found under, as indexed.
    pub artist_root: String,
    pub date: String,
    pub precision: DatePrecision,
    /// Where the date came from, when one was found.
    pub date_component: Option<DateComponent>,
    /// Precision-only readings above the boundary (a year, a month) that gave
    /// context without drawing the boundary themselves.
    pub context: Vec<DateComponent>,
    /// Components skipped as collection layers, with the rule that skipped them.
    pub collection_layers: Vec<CollectionLayer>,
    /// A structural problem worth a human look: a nested full date, a date
    /// container whose children disagree, or a boundary that cannot be dated.
    pub boundary_conflict: Option<String>,
    pub members: Vec<GroupMember>,
    pub member_count: usize,
    pub total_bytes: u64,
    pub parser_version: String,
}

impl ContentGroup {
    /// Whether this group holds content a work could be associated with.
    ///
    /// The definition the plan gives: private, non-zero, not only this
    /// application's own bookkeeping text.
    pub fn has_content(&self) -> bool {
        self.members
            .iter()
            .any(|member| !member.derived && member.file_size > 0)
    }
}

/// Build the index entries the grouping reads, for one artist.
///
/// The media index is the entry point the plan names: one query per artist
/// rather than a walk of the artist's directory, so a scope of a few thousand
/// files costs a read instead of a traversal. Three things are deliberately
/// *not* turned into entries:
///
/// - a row the library has marked missing: its content is not there, and
///   counting it would read as a group that exists;
/// - this application's own bookkeeping text (`content N.txt` / `links N.txt`),
///   which is a naming fact, not the user's content;
/// - staging and temporary artifacts (`.gallery_pawchive_*`, `*.part`,
///   `*.crawljob`, `Thumbs.db`).
///
/// Directory entries are derived from the file paths themselves, so a
/// directory that holds files is present exactly once and an empty directory is
/// simply absent — the caller that needs "the folder exists but is empty"
/// records it as an explicit `IndexEntry::Directory`.
pub fn group_index_entries(
    conn: &rusqlite::Connection,
    artist_id: i64,
) -> anyhow::Result<GroupingResult> {
    use anyhow::Context;
    use rusqlite::params;

    let artist_root: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id = ?1",
            params![artist_id],
            |row| row.get(0),
        )
        .optional()
        .context("read the artist root")?;
    let Some(artist_root) = artist_root else {
        anyhow::bail!("unknown artist: {artist_id}");
    };

    let files: Vec<(String, String, i64, String)> = {
        let mut stmt = conn.prepare(
            "SELECT i.file_path, i.file_name, i.file_size, COALESCE(i.media_type, '')
             FROM items i
             WHERE i.artist_id = ?1 AND i.missing = 0
             ORDER BY i.file_path ASC",
        )?;
        let rows = stmt
            .query_map(params![artist_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })?
            .collect::<rusqlite::Result<Vec<(String, String, i64, String)>>>()?;
        rows
    };

    let mut entries: Vec<IndexEntry> = Vec::new();
    let mut seen_directories: BTreeSet<String> = BTreeSet::new();
    for (file_path, file_name, file_size, _media_type) in files {
        // The relative path under the artist root is what the grouping reads.
        let relative = relative_under_root(&artist_root, &file_path);
        let Some(relative) = relative else {
            continue;
        };
        if relative.is_empty() {
            continue;
        }
        let transient = {
            let lower = file_name.to_ascii_lowercase();
            lower.starts_with(".gallery_pawchive_")
                || lower.ends_with(".part")
                || lower.ends_with(".crawljob")
                || lower == ".ds_store"
                || lower.starts_with("thumbs.db")
        };
        if transient {
            continue;
        }
        // The directories on the way to this file, so the grouping sees the
        // structure even when a level holds only subdirectories.
        let parts: Vec<&str> = relative.split('/').collect();
        for depth in 1..parts.len() {
            let directory = parts[..depth].join("/");
            if seen_directories.insert(directory.clone()) {
                entries.push(IndexEntry::Directory {
                    relative_path: directory,
                    artist_root: artist_root.clone(),
                });
            }
        }
        entries.push(IndexEntry::File {
            relative_path: relative,
            artist_root: artist_root.clone(),
            file_size: file_size.max(0) as u64,
            content_hash: String::new(),
        });
    }

    // The entries are grouped, and the result is returned without writing
    // anything: persisting it is `apply_grouping`'s job, and a reader that only
    // wants to look must not create a ledger row.
    Ok(group_index(&entries))
}

/// The path of `file_path` relative to `artist_root`, with `/` separators, or
/// `None` when the file is not under that root.
///
/// The comparison is component-wise and case-insensitive on Windows-style
/// separators, but it never reports a relative path for a file that merely
/// shares a prefix with the root: `/pictures/ArtistA-old` is not inside
/// `/pictures/ArtistA`, and the plan's whole point is that a boundary has to
/// be a path boundary.
pub fn relative_under_root(artist_root: &str, file_path: &str) -> Option<String> {
    let normalize = |value: &str| value.replace('\\', "/").trim_end_matches('/').to_string();
    let root = normalize(artist_root);
    let file = normalize(file_path);
    if root.is_empty() {
        return None;
    }
    if file == root {
        return Some(String::new());
    }
    if let Some(rest) = file.strip_prefix(&format!("{root}/")) {
        return Some(rest.to_string());
    }
    // A Windows path compared against a root that differs only in case, or a
    // root that itself carries a trailing separator, still has to resolve.
    if cfg!(windows) {
        let root_lower = root.to_ascii_lowercase();
        let file_lower = file.to_ascii_lowercase();
        if let Some(rest) = file_lower.strip_prefix(&format!("{root_lower}/")) {
            let offset = rest.len();
            return Some(file[file.len() - offset..].to_string());
        }
    }
    None
}

/// One entry of the index: a file, a directory, or a note that something could
/// not be read. Directories are carried explicitly so an empty directory can be
/// seen as empty rather than as absent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IndexEntry {
    File {
        relative_path: String,
        /// Absolute or display path, passed through to the member record.
        artist_root: String,
        file_size: u64,
        /// Content hash when the index already has one. Two files with the same
        /// hash are the same bytes; the plan allows that as a hint, never as
        /// proof that two works are one.
        content_hash: String,
    },
    Directory {
        relative_path: String,
        artist_root: String,
    },
    /// A path the index saw but could not read.
    Unreadable {
        relative_path: String,
        artist_root: String,
        reason: String,
    },
}

impl IndexEntry {
    fn relative_path(&self) -> &str {
        match self {
            Self::File { relative_path, .. }
            | Self::Directory { relative_path, .. }
            | Self::Unreadable { relative_path, .. } => relative_path,
        }
    }

    fn artist_root(&self) -> &str {
        match self {
            Self::File { artist_root, .. }
            | Self::Directory { artist_root, .. }
            | Self::Unreadable { artist_root, .. } => artist_root,
        }
    }

    pub fn as_file(
        relative_path: impl Into<String>,
        artist_root: impl Into<String>,
        file_size: u64,
    ) -> Self {
        Self::File {
            relative_path: relative_path.into(),
            artist_root: artist_root.into(),
            file_size,
            content_hash: String::new(),
        }
    }
}

/// The result of grouping one artist root's index.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct GroupingResult {
    /// Groups with real content, in path order.
    pub groups: Vec<ContentGroup>,
    /// Groups that describe a structure the plan keeps as ambiguity.
    pub conflicts: Vec<BoundaryConflict>,
    /// Directories that held no content at all. Reported, not silently dropped:
    /// "I have the folder" and "the folder is empty" are different answers.
    pub empty_roots: Vec<String>,
    /// Paths the index could not read. A range containing one of these is not
    /// complete, and the caller must say so rather than report "nothing found".
    pub unreadable: Vec<String>,
}

/// A structure that could not be resolved into one group per work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoundaryConflict {
    /// The path the conflict is about.
    pub relative_path: String,
    pub kind: String,
    pub detail: String,
}

/// Split a relative path into non-empty components.
fn components(relative: &str) -> Vec<String> {
    relative
        .split(['/', '\\'])
        .map(str::trim)
        .filter(|part| !part.is_empty() && *part != ".")
        .map(|part| part.to_string())
        .collect()
}

/// Read one path component as a date, reusing the shared parser the scanner and
/// the media index already use.
///
/// `media_type::extract_date_value_from_folder` recognizes full dates
/// (`YYYY-MM-DD`, `YYYYMMDD`), the parent/child `YYYYMM/DD_...` shape, and month
/// forms (`YYYYMM`, `YYYY-MM`, `YYYY/MM`, `YYYY.MM`). It reports the raw,
/// precision-preserving value, so the precision here is read off that raw form
/// rather than guessed from the canonical one — the canonical value pads a
/// month to its first day, and treating that as a day would invent a date the
/// path never stated.
///
/// A bare year is recognized here only, because the shared parser does not
/// classify it: the plan's year collection layer (`2026/`) has to be
/// distinguishable from a work root.
fn read_date_component(index: usize, component: &str) -> Option<DateComponent> {
    let trimmed = component.trim();
    if trimmed.len() == 4 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Some(DateComponent {
            component_index: index,
            component: component.to_string(),
            canonical: trimmed.to_string(),
            precision: DatePrecision::Year,
            basis: "bare_year".to_string(),
        });
    }
    let value = crate::media_type::extract_date_value_from_folder(trimmed)?;
    let precision = match value.raw.len() {
        10 => DatePrecision::Day,
        7 => DatePrecision::Month,
        _ => return None,
    };
    Some(DateComponent {
        component_index: index,
        component: component.to_string(),
        canonical: if precision == DatePrecision::Month {
            value.raw.clone()
        } else {
            value.canonical.clone()
        },
        precision,
        basis: format!("shared_parser:{}", value.raw),
    })
}

/// Whether a component is exactly a collection-layer shape.
///
/// Only the shapes the plan names are skipped without a date read: a bare year
/// (`2026`), a bare year-month in the three supported spellings, and a bare
/// day. Anything else — including a name that merely starts with a year — is
/// left to the date parser and the boundary rules below.
fn collection_layer_reason(component: &str) -> Option<&'static str> {
    let trimmed = component.trim();
    if trimmed.len() == 4 && trimmed.chars().all(|ch| ch.is_ascii_digit()) {
        return Some("年份集合层");
    }
    let digits: String = trimmed.chars().filter(|ch| ch.is_ascii_digit()).collect();
    let separators_ok = trimmed
        .chars()
        .all(|ch| ch.is_ascii_digit() || ch == '-' || ch == '.');
    if separators_ok && digits.len() == 6 {
        return Some("年月集合层");
    }
    if separators_ok && digits.len() == 8 {
        return Some("纯日期容器");
    }
    None
}

/// Whether a file name is this application's own bookkeeping text.
///
/// The shapes `publish_post_text_files` writes: `content {id}.txt` and
/// `links {title} {id}.txt`. A user's own text file keeps its own name and is
/// content; the plan is explicit that a `.txt` extension alone proves nothing.
fn is_derived_text(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    if !lower.ends_with(".txt") {
        return false;
    }
    lower.starts_with("content ") || lower.starts_with("links ")
}

/// Whether a file is a staging or temporary artifact rather than content.
fn is_transient(file_name: &str) -> bool {
    let lower = file_name.to_ascii_lowercase();
    lower.starts_with(".gallery_pawchive_")
        || lower.ends_with(".part")
        || lower.ends_with(".crawljob")
        || lower == ".ds_store"
        || lower.starts_with("thumbs.db")
}

/// Group one artist root's index into logical content groups.
pub fn group_index(entries: &[IndexEntry]) -> GroupingResult {
    let mut result = GroupingResult::default();
    // root -> the files under it, plus the structure notes seen on the way.
    struct Candidate {
        artist_root: String,
        root_relative: String,
        date_component: Option<DateComponent>,
        context: Vec<DateComponent>,
        collection_layers: Vec<CollectionLayer>,
        members: Vec<GroupMember>,
        boundary_conflict: Option<String>,
    }
    let mut candidates: BTreeMap<String, Candidate> = BTreeMap::new();
    let mut directories: BTreeMap<String, (String, bool)> = BTreeMap::new();

    for entry in entries {
        let artist_root = entry.artist_root().to_string();
        let parts = components(entry.relative_path());
        match entry {
            IndexEntry::Directory { .. } => {
                directories.insert(
                    entry.relative_path().to_string(),
                    (artist_root.clone(), false),
                );
                if parts.is_empty() {
                    candidates
                        .entry(String::new())
                        .or_insert_with(|| Candidate {
                            artist_root: artist_root.clone(),
                            root_relative: String::new(),
                            date_component: None,
                            context: Vec::new(),
                            collection_layers: Vec::new(),
                            members: Vec::new(),
                            boundary_conflict: None,
                        });
                }
                continue;
            }
            IndexEntry::Unreadable { reason, .. } => {
                result
                    .unreadable
                    .push(format!("{}（{reason}）", entry.relative_path()));
                continue;
            }
            IndexEntry::File { file_size, .. } => {
                let file_name = parts.last().cloned().unwrap_or_default();
                if is_transient(&file_name) {
                    continue;
                }
                // Walk the directory components and decide where this file's
                // group starts.
                let mut boundary: Option<usize> = None;
                let mut context: Vec<DateComponent> = Vec::new();
                let mut skipped: Vec<CollectionLayer> = Vec::new();
                let mut nested_day: Option<(usize, DateComponent)> = None;
                for (index, component) in parts.iter().enumerate() {
                    if index + 1 == parts.len() {
                        break; // the file name is not a directory component
                    }
                    let read = read_date_component(index, component);
                    if let Some(read) = read {
                        if read.precision.is_boundary() {
                            if boundary.is_none() {
                                boundary = Some(index);
                            } else {
                                // A second full date below the first: the plan
                                // keeps this as a conflict rather than merging
                                // the two into one work.
                                nested_day = Some((index, read));
                            }
                            continue;
                        }
                        // A component whose whole text is a collection-layer
                        // shape is recorded as a layer. The reading is kept in
                        // the context as well: the precision a group can claim
                        // comes from the most precise context it saw, and a
                        // month bucket still says "this is a month, never a
                        // day" — which is exactly the answer the plan demands
                        // for a month-only library.
                        if let Some(reason) = collection_layer_reason(component) {
                            skipped.push(CollectionLayer {
                                component_index: index,
                                component: component.clone(),
                                reason: reason.to_string(),
                            });
                        }
                        context.push(read);
                        continue;
                    }
                    if let Some(reason) = collection_layer_reason(component) {
                        skipped.push(CollectionLayer {
                            component_index: index,
                            component: component.clone(),
                            reason: reason.to_string(),
                        });
                    }
                }
                let root_relative = match boundary {
                    Some(index) => parts[..=index].join("/"),
                    None => String::new(),
                };
                let date_component =
                    boundary.and_then(|index| read_date_component(index, &parts[index]));
                // The boundary component is recorded as a skipped layer as well
                // when its own shape is a container: the plan calls the pure
                // date container a layer to skip *and* the work root, and the
                // record has to say both or a later reader will think the root
                // was chosen by a date-prefix rule.
                if let Some(index) = boundary {
                    if let Some(reason) = collection_layer_reason(&parts[index]) {
                        if !skipped.iter().any(|layer| layer.component_index == index) {
                            skipped.push(CollectionLayer {
                                component_index: index,
                                component: parts[index].clone(),
                                reason: reason.to_string(),
                            });
                        }
                    }
                }
                skipped.sort_by_key(|layer| layer.component_index);
                let entry_candidate =
                    candidates
                        .entry(root_relative.clone())
                        .or_insert_with(|| Candidate {
                            artist_root: artist_root.clone(),
                            root_relative: root_relative.clone(),
                            date_component: date_component.clone(),
                            context: context.clone(),
                            collection_layers: skipped.clone(),
                            members: Vec::new(),
                            boundary_conflict: None,
                        });
                // A second file in the same group may have been read through a
                // different set of components (a sibling under the same root
                // reached by a different depth). The union is what the group
                // saw, so a reading one file saw is not lost because another
                // file reached the root directly.
                for reading in &context {
                    if !entry_candidate.context.iter().any(|known| known == reading) {
                        entry_candidate.context.push(reading.clone());
                    }
                }
                for layer in &skipped {
                    if !entry_candidate
                        .collection_layers
                        .iter()
                        .any(|known| known == layer)
                    {
                        entry_candidate.collection_layers.push(layer.clone());
                    }
                }
                entry_candidate
                    .collection_layers
                    .sort_by_key(|layer| layer.component_index);
                if let Some((index, nested)) = nested_day {
                    entry_candidate.boundary_conflict = Some(format!(
                        "嵌套日期：{} 之后又出现 {}（第 {} 段）",
                        date_component
                            .as_ref()
                            .map(|date| date.component.as_str())
                            .unwrap_or("作品根"),
                        nested.component,
                        index + 1
                    ));
                }
                let media_type = media_type_for_file(&file_name).unwrap_or("other");
                entry_candidate.members.push(GroupMember {
                    relative_path: entry.relative_path().to_string(),
                    file_name: file_name.clone(),
                    file_size: *file_size,
                    media_type: media_type.to_string(),
                    derived: is_derived_text(&file_name),
                });
            }
        }
    }

    // A directory that held files is not empty, and a directory holding only
    // this application's text is not content either.
    let mut roots_with_content: std::collections::BTreeSet<String> =
        std::collections::BTreeSet::new();
    for (root_relative, candidate) in &candidates {
        if candidate.members.iter().any(|member| {
            !member.derived
                && member.file_size > 0
                && member.relative_path.starts_with(root_relative.as_str())
        }) {
            roots_with_content.insert(root_relative.clone());
        }
    }

    let mut ordered: Vec<(&String, &Candidate)> = candidates.iter().collect();
    ordered.sort_by(|left, right| left.0.cmp(right.0));
    for (root_relative, candidate) in ordered {
        let mut members = candidate.members.clone();
        members.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let total_bytes: u64 = members.iter().map(|member| member.file_size).sum();
        let group = ContentGroup {
            root_relative: candidate.root_relative.clone(),
            artist_root: candidate.artist_root.clone(),
            date: candidate
                .date_component
                .as_ref()
                .map(|date| date.canonical.clone())
                .unwrap_or_default(),
            precision: candidate
                .date_component
                .as_ref()
                .map(|date| date.precision)
                .unwrap_or_else(|| {
                    candidate
                        .context
                        .iter()
                        .map(|date| date.precision)
                        .max()
                        .unwrap_or(DatePrecision::Unknown)
                }),
            date_component: candidate.date_component.clone(),
            context: candidate.context.clone(),
            collection_layers: candidate.collection_layers.clone(),
            boundary_conflict: candidate.boundary_conflict.clone(),
            member_count: members.len(),
            total_bytes,
            members,
            parser_version: GROUP_PARSER_VERSION.to_string(),
        };
        if let Some(conflict) = &candidate.boundary_conflict {
            result.conflicts.push(BoundaryConflict {
                relative_path: root_relative.clone(),
                kind: "nested_date".to_string(),
                detail: conflict.clone(),
            });
        }
        if !roots_with_content.contains(root_relative) {
            // Recorded as an empty root rather than as a group: the plan's
            // "empty directory, only part files, or only our own text" rule.
            result.empty_roots.push(root_relative.clone());
            continue;
        }
        result.groups.push(group);
    }

    // Directories the index listed that ended up in no non-empty group.
    let mut listed: Vec<String> = directories
        .keys()
        .filter(|path| !roots_with_content.contains(path.as_str()))
        .cloned()
        .collect();
    listed.sort();
    for path in listed {
        if !result.empty_roots.contains(&path) {
            result.empty_roots.push(path);
        }
    }
    result
}

// ---------------------------------------------------------------------------
// The reconciliation ledger
//
// `group_index` answers "which groups does this index describe". Persisting that
// answer is a separate job, and the plan keeps the two apart on purpose: the
// algorithm is pure, the ledger is written once per observation and read many
// times by the same-day candidate reader (reconciliation §6.5) and the calendar
// (§6.3). Three rules drive the code below.
//
// - A group is identified by its root path, so re-observing a root lands on the
//   same `group_id` instead of growing a new row every scan.
// - `generation` counts real changes of the observation. A group whose members,
//   date and precision did not move keeps its generation, so a consumer can use
//   it as a change marker without diffing every member itself.
// - Nothing is deleted. A root that no longer appears is tombstoned (`gone`),
//   and a member that a later observation no longer lists keeps its row, because
//   member sets, locations and work links elsewhere still reference it.
// ---------------------------------------------------------------------------

/// Status of a group whose root the latest observation still describes.
pub const GROUP_STATE_ACTIVE: &str = "active";
/// Status of a group whose root no longer appears, or of a member a later
/// observation no longer lists. The row stays as history either way.
pub const GROUP_STATE_GONE: &str = "gone";

/// What one `apply_grouping` call did to the ledger.
///
/// The counters answer the plan's "the range is incomplete, say so" rule: a
/// caller that saw `unreadable > 0` may not report the range as complete, and
/// `empty_roots` is where "the folder exists and is empty" is recorded rather
/// than silently dropped.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct GroupingApplyReport {
    /// Roots observed for the first time.
    pub created: usize,
    /// Rows whose observation really changed, including one coming back from
    /// `gone`. Their `generation` was bumped.
    pub updated: usize,
    /// Rows whose observation is byte-for-byte the stored one. Nothing was
    /// written for these, not even `updated_at`.
    pub unchanged: usize,
    /// Rows tombstoned by this apply because the root no longer appears.
    pub gone: usize,
    /// Boundary conflicts the grouping reported; kept for the caller.
    pub conflicts: usize,
    /// Roots that held no content.
    pub empty_roots: usize,
    /// Paths the index could not read.
    pub unreadable: usize,
}

/// A persisted group, as read back by the list and same-day readers.
///
/// `state`/`tombstoned_at` carry the tombstone instead of a delete, and
/// `date_component_*` / `context_json` / `collection_layers_json` carry the
/// boundary evidence, so a reader can tell *why* the root was drawn without
/// re-running the parser (reconciliation §6.3).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredContentGroup {
    /// Surrogate row id, used for the stable listing order and cursors.
    pub id: i64,
    pub group_id: String,
    pub artist_scope_id: String,
    pub root_relative: String,
    pub artist_root: String,
    /// Canonical `YYYY-MM-DD` when the root was dated at day precision, `NULL`
    /// otherwise. A month reading is never widened into a day.
    pub date: Option<String>,
    pub precision: DatePrecision,
    /// Which parser form produced the date, e.g. `shared_parser:2026-09-14`.
    pub date_basis: String,
    /// Zero-based index of the path component the date was read from.
    pub date_component_index: Option<i64>,
    /// That component's text, verbatim.
    pub date_component: String,
    /// JSON array of the precision-only readings above the boundary.
    pub context_json: String,
    /// JSON array of the collection layers that were skipped.
    pub collection_layers_json: String,
    pub parser_version: String,
    /// The structural problem that kept this root ambiguous, if any.
    pub boundary_conflict: Option<String>,
    /// Bumped only when the observation really changed.
    pub generation: i64,
    /// `active` or `gone`.
    pub state: String,
    /// When the root disappeared, if it did.
    pub tombstoned_at: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Members not tombstoned.
    pub member_count: i64,
    /// Sum of the active members' sizes.
    pub total_bytes: i64,
}

impl StoredContentGroup {
    /// Whether the latest observation still describes this root.
    pub fn is_active(&self) -> bool {
        self.state == GROUP_STATE_ACTIVE
    }
}

/// A member row, including tombstoned ones. History stays readable: a caller
/// that wants only live members filters on `state`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGroupMember {
    pub group_id: String,
    pub relative_path: String,
    pub file_name: String,
    pub file_size: i64,
    pub media_type: String,
    /// This application's own bookkeeping text, not user content.
    pub derived: bool,
    pub content_hash: String,
    /// `active` or `gone`.
    pub state: String,
    pub tombstoned_at: Option<String>,
}

impl StoredGroupMember {
    pub fn is_active(&self) -> bool {
        self.state == GROUP_STATE_ACTIVE
    }
}

/// One persistent location of a group.
///
/// Locations are deliberately independent of `folder_rename_plans`: the plan
/// calls the rename plan a task row that is cleaned up when it succeeds, while a
/// location is where the group *is*. A group may have several, and a
/// user-chosen one is an override the automatic apply must not re-stamp.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredGroupLocation {
    pub group_id: String,
    pub relative_path: String,
    /// The group generation this location was last confirmed against by the
    /// automatic grouping.
    pub generation: i64,
    pub manual_override: bool,
    pub source_operation: String,
    pub created_at: String,
    pub updated_at: String,
}

/// Create the reconciliation ledger if it is not there yet.
///
/// Idempotent: every statement is `IF NOT EXISTS`, and the column pass covers a
/// table that some earlier build created without the columns the reads need.
pub fn ensure_content_group_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS content_groups (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL UNIQUE,
            artist_scope_id TEXT NOT NULL,
            root_relative TEXT NOT NULL,
            artist_root TEXT NOT NULL,
            date TEXT,
            precision TEXT NOT NULL DEFAULT 'unknown',
            date_basis TEXT NOT NULL DEFAULT '',
            date_component_index INTEGER,
            date_component TEXT NOT NULL DEFAULT '',
            context_json TEXT NOT NULL DEFAULT '[]',
            collection_layers_json TEXT NOT NULL DEFAULT '[]',
            parser_version TEXT NOT NULL DEFAULT '',
            boundary_conflict TEXT,
            generation INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'active',
            -- The user's exclusion from the reconciliation baseline: a recorded
            -- act with its reason and the generation it was made against, never
            -- a deletion of the group or its history.
            baseline_state TEXT NOT NULL DEFAULT 'active',
            baseline_reason TEXT NOT NULL DEFAULT '',
            baseline_at TEXT,
            baseline_generation INTEGER NOT NULL DEFAULT 0,
            tombstoned_at TEXT,
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT '',
            UNIQUE(artist_root, root_relative)
        );

        CREATE TABLE IF NOT EXISTS content_group_members (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL,
            relative_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_size INTEGER NOT NULL DEFAULT 0,
            media_type TEXT NOT NULL DEFAULT 'other',
            derived INTEGER NOT NULL DEFAULT 0,
            content_hash TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'active',
            tombstoned_at TEXT,
            UNIQUE(group_id, relative_path)
        );

        CREATE TABLE IF NOT EXISTS content_group_locations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL,
            relative_path TEXT NOT NULL,
            generation INTEGER NOT NULL DEFAULT 0,
            manual_override INTEGER NOT NULL DEFAULT 0,
            source_operation TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT '',
            UNIQUE(group_id, relative_path)
        );

        CREATE INDEX IF NOT EXISTS idx_content_groups_scope_id
            ON content_groups(artist_scope_id, id);
        CREATE INDEX IF NOT EXISTS idx_content_groups_scope_day
            ON content_groups(artist_scope_id, precision, date, state);
        CREATE INDEX IF NOT EXISTS idx_content_group_members_group_id
            ON content_group_members(group_id, id);
        CREATE INDEX IF NOT EXISTS idx_content_group_locations_group_id
            ON content_group_locations(group_id, id);

        -- A relation the user recorded between a work and the group its content
        -- was found in. It is a user fact, so it is revocable, it names the
        -- revisions it was decided against, and `shared` is the explicit
        -- statement that a group holds content of more than one work. Without
        -- that statement two works pointing at one directory would be a claim
        -- about content nobody made.
        CREATE TABLE IF NOT EXISTS work_group_links (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            work_id TEXT NOT NULL,
            group_id TEXT NOT NULL,
            basis TEXT NOT NULL DEFAULT '',
            confidence REAL NOT NULL DEFAULT 0,
            shared INTEGER NOT NULL DEFAULT 0,
            manifest_version INTEGER NOT NULL DEFAULT 0,
            group_generation INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL DEFAULT '',
            revoked_at TEXT NOT NULL DEFAULT '',
            UNIQUE(work_id, group_id)
        );

        CREATE INDEX IF NOT EXISTS idx_work_group_links_work
            ON work_group_links(work_id, revoked_at);
        CREATE INDEX IF NOT EXISTS idx_work_group_links_group
            ON work_group_links(group_id, revoked_at);",
    )?;
    for (table, columns) in [
        (
            "content_groups",
            [
                ("artist_scope_id", "TEXT NOT NULL DEFAULT ''"),
                ("root_relative", "TEXT NOT NULL DEFAULT ''"),
                ("artist_root", "TEXT NOT NULL DEFAULT ''"),
                ("date", "TEXT"),
                ("precision", "TEXT NOT NULL DEFAULT 'unknown'"),
                ("date_basis", "TEXT NOT NULL DEFAULT ''"),
                ("date_component_index", "INTEGER"),
                ("date_component", "TEXT NOT NULL DEFAULT ''"),
                ("context_json", "TEXT NOT NULL DEFAULT '[]'"),
                ("collection_layers_json", "TEXT NOT NULL DEFAULT '[]'"),
                ("parser_version", "TEXT NOT NULL DEFAULT ''"),
                ("boundary_conflict", "TEXT"),
                ("generation", "INTEGER NOT NULL DEFAULT 0"),
                ("state", "TEXT NOT NULL DEFAULT 'active'"),
                ("tombstoned_at", "TEXT"),
                ("created_at", "TEXT NOT NULL DEFAULT ''"),
                ("updated_at", "TEXT NOT NULL DEFAULT ''"),
                // The user's exclusion from the reconciliation baseline, with
                // the reason and when it was made. A recorded act, not a delete:
                // the group and its history stay readable.
                ("baseline_state", "TEXT NOT NULL DEFAULT 'active'"),
                ("baseline_reason", "TEXT NOT NULL DEFAULT ''"),
                ("baseline_at", "TEXT"),
                // The group generation the exclusion was made against. An
                // exclusion that predates a change does not cover the content
                // that is there now.
                ("baseline_generation", "INTEGER NOT NULL DEFAULT 0"),
            ]
            .as_slice(),
        ),
        (
            "content_group_members",
            [
                ("file_name", "TEXT NOT NULL DEFAULT ''"),
                ("file_size", "INTEGER NOT NULL DEFAULT 0"),
                ("media_type", "TEXT NOT NULL DEFAULT 'other'"),
                ("derived", "INTEGER NOT NULL DEFAULT 0"),
                ("content_hash", "TEXT NOT NULL DEFAULT ''"),
                ("state", "TEXT NOT NULL DEFAULT 'active'"),
                ("tombstoned_at", "TEXT"),
            ]
            .as_slice(),
        ),
        (
            "content_group_locations",
            [
                ("generation", "INTEGER NOT NULL DEFAULT 0"),
                ("manual_override", "INTEGER NOT NULL DEFAULT 0"),
                ("source_operation", "TEXT NOT NULL DEFAULT ''"),
                ("created_at", "TEXT NOT NULL DEFAULT ''"),
                ("updated_at", "TEXT NOT NULL DEFAULT ''"),
            ]
            .as_slice(),
        ),
    ] {
        let existing = conn
            .prepare(&format!("PRAGMA table_info({table})"))?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (name, definition) in columns {
            if !existing.iter().any(|column| column == name) {
                conn.execute(
                    &format!("ALTER TABLE {table} ADD COLUMN {name} {definition}"),
                    [],
                )?;
            }
        }
    }
    Ok(())
}

/// The `group_id` of the group at one root.
///
/// Derived from `(artist_root, root_relative)` — the pair the plan's unique key
/// already treats as one group — so a re-observation addresses the same row and
/// the apply stays idempotent without carrying ids in the pure algorithm.
fn group_id_for(artist_root: &str, root_relative: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"gallery_accel/content_group/v1\0");
    hasher.update(artist_root.as_bytes());
    hasher.update(&[0u8]);
    hasher.update(root_relative.as_bytes());
    let hex = hasher.finalize().to_hex();
    format!("cg_{}", &hex[..32])
}

/// One member in the canonical form the observation fingerprint hashes.
type MemberObservation = (String, String, i64, String, bool);

/// The collection layers of a group, canonicalised: the same layers in the same
/// order produce the same JSON, so a re-apply compares equal.
fn collection_layers_json(group: &ContentGroup) -> String {
    let mut layers = group.collection_layers.clone();
    layers.sort_by(|left, right| {
        left.component_index
            .cmp(&right.component_index)
            .then_with(|| left.component.cmp(&right.component))
            .then_with(|| left.reason.cmp(&right.reason))
    });
    serde_json::to_string(&layers).unwrap_or_else(|_| "[]".to_string())
}

/// The precision-only readings above the boundary, canonicalised the same way.
fn context_json(group: &ContentGroup) -> String {
    let mut context = group.context.clone();
    context.sort_by(|left, right| {
        left.component_index
            .cmp(&right.component_index)
            .then_with(|| left.canonical.cmp(&right.canonical))
            .then_with(|| left.basis.cmp(&right.basis))
    });
    serde_json::to_string(&context).unwrap_or_else(|_| "[]".to_string())
}

/// Hash of one observation of a group.
///
/// Everything the group row and its member rows are allowed to claim about the
/// observation goes in: the date, its precision and basis, the boundary
/// evidence, the parser version, and the member set sorted by path. Two applies
/// of the same `GroupingResult` hash equal, which is what keeps the generation
/// still.
#[allow(clippy::too_many_arguments)]
fn observation_fingerprint(
    date: Option<&str>,
    precision: &str,
    date_basis: &str,
    date_component_index: Option<i64>,
    date_component: &str,
    context_json: &str,
    collection_layers_json: &str,
    parser_version: &str,
    boundary_conflict: Option<&str>,
    members: &[MemberObservation],
) -> String {
    let mut payload = String::new();
    payload.push_str("content-group-observation/v1\n");
    for field in [
        date.unwrap_or(""),
        precision,
        date_basis,
        date_component,
        context_json,
        collection_layers_json,
        parser_version,
        boundary_conflict.unwrap_or(""),
    ] {
        payload.push_str(field);
        payload.push('\u{1f}');
    }
    payload.push_str(
        &date_component_index
            .map(|value| value.to_string())
            .unwrap_or_default(),
    );
    payload.push('\n');
    for (relative_path, file_name, file_size, media_type, derived) in members {
        payload.push_str(relative_path);
        payload.push('\u{1f}');
        payload.push_str(file_name);
        payload.push('\u{1f}');
        payload.push_str(&file_size.to_string());
        payload.push('\u{1f}');
        payload.push_str(media_type);
        payload.push('\u{1f}');
        payload.push_str(if *derived { "1" } else { "0" });
        payload.push('\n');
    }
    blake3::hash(payload.as_bytes()).to_hex().to_string()
}

/// One member as stored, used to decide whether a write is needed.
struct StoredMemberRow {
    file_name: String,
    file_size: i64,
    media_type: String,
    derived: bool,
    state: String,
}

/// One group row as stored, used to decide whether the observation moved.
struct StoredGroupRow {
    group_id: String,
    artist_scope_id: String,
    date: Option<String>,
    precision: String,
    date_basis: String,
    date_component_index: Option<i64>,
    date_component: String,
    context_json: String,
    collection_layers_json: String,
    parser_version: String,
    boundary_conflict: Option<String>,
    generation: i64,
    state: String,
}

impl StoredGroupRow {
    /// The fingerprint of what is stored right now, including only the members
    /// the latest observation still lists.
    fn fingerprint(&self, members: &[(String, StoredMemberRow)]) -> String {
        let active: Vec<MemberObservation> = members
            .iter()
            .filter(|(_, member)| member.state == GROUP_STATE_ACTIVE)
            .map(|(relative_path, member)| {
                (
                    relative_path.clone(),
                    member.file_name.clone(),
                    member.file_size,
                    member.media_type.clone(),
                    member.derived,
                )
            })
            .collect();
        observation_fingerprint(
            self.date.as_deref(),
            &self.precision,
            &self.date_basis,
            self.date_component_index,
            &self.date_component,
            &self.context_json,
            &self.collection_layers_json,
            &self.parser_version,
            self.boundary_conflict.as_deref(),
            &active,
        )
    }
}

fn load_stored_groups(
    conn: &Connection,
    artist_root: &str,
) -> Result<BTreeMap<String, StoredGroupRow>> {
    let mut statement = conn.prepare(
        "SELECT group_id, artist_scope_id, date, precision, date_basis, date_component_index,
                date_component, context_json, collection_layers_json, parser_version,
                boundary_conflict, generation, state
           FROM content_groups WHERE artist_root = ?1",
    )?;
    let rows = statement.query_map(params![artist_root], |row| {
        Ok(StoredGroupRow {
            group_id: row.get(0)?,
            artist_scope_id: row.get(1)?,
            date: row.get(2)?,
            precision: row.get(3)?,
            date_basis: row.get(4)?,
            date_component_index: row.get(5)?,
            date_component: row.get(6)?,
            context_json: row.get(7)?,
            collection_layers_json: row.get(8)?,
            parser_version: row.get(9)?,
            boundary_conflict: row.get(10)?,
            generation: row.get(11)?,
            state: row.get(12)?,
        })
    })?;
    let mut groups = BTreeMap::new();
    for row in rows {
        let row = row?;
        groups.insert(row.group_id.clone(), row);
    }
    Ok(groups)
}

fn load_stored_members(
    conn: &Connection,
    group_id: &str,
) -> Result<Vec<(String, StoredMemberRow)>> {
    let mut statement = conn.prepare(
        "SELECT relative_path, file_name, file_size, media_type, derived, state
           FROM content_group_members WHERE group_id = ?1 ORDER BY relative_path",
    )?;
    let rows = statement.query_map(params![group_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            StoredMemberRow {
                file_name: row.get(1)?,
                file_size: row.get(2)?,
                media_type: row.get(3)?,
                derived: row.get::<_, i64>(4)? != 0,
                state: row.get(5)?,
            },
        ))
    })?;
    let mut members = Vec::new();
    for row in rows {
        members.push(row?);
    }
    Ok(members)
}

/// Persist one artist root's grouping.
///
/// One transaction. Applying the same `GroupingResult` twice writes nothing and
/// bumps no generation: the observation is compared through
/// [`observation_fingerprint`] against the stored rows before anything is
/// written. A root that no longer appears is tombstoned together with its
/// members, never deleted, and a member the new observation no longer lists
/// keeps its row as a tombstone.
pub fn apply_grouping(
    conn: &Connection,
    artist_scope_id: &str,
    artist_root: &str,
    result: &GroupingResult,
) -> Result<GroupingApplyReport> {
    ensure_content_group_schema(conn)?;
    let now = Utc::now().to_rfc3339();
    let mut report = GroupingApplyReport {
        conflicts: result.conflicts.len(),
        empty_roots: result.empty_roots.len(),
        unreadable: result.unreadable.len(),
        ..Default::default()
    };
    let tx = conn
        .unchecked_transaction()
        .context("begin content group apply")?;
    let existing = load_stored_groups(&tx, artist_root)?;
    let mut observed: BTreeSet<String> = BTreeSet::new();

    for group in &result.groups {
        let group_id = group_id_for(artist_root, &group.root_relative);
        observed.insert(group_id.clone());

        // The observation, in the canonical form both sides of the comparison
        // share.
        let date = if group.date.trim().is_empty() {
            None
        } else {
            Some(group.date.clone())
        };
        let date_basis = group
            .date_component
            .as_ref()
            .map(|component| component.basis.clone())
            .unwrap_or_default();
        let date_component_index = group
            .date_component
            .as_ref()
            .map(|component| component.component_index as i64);
        let date_component = group
            .date_component
            .as_ref()
            .map(|component| component.component.clone())
            .unwrap_or_default();
        let context = context_json(group);
        let layers = collection_layers_json(group);
        let conflict = group.boundary_conflict.clone();
        let mut observed_members: Vec<&GroupMember> = group.members.iter().collect();
        observed_members.sort_by(|left, right| left.relative_path.cmp(&right.relative_path));
        let members: Vec<MemberObservation> = observed_members
            .iter()
            .map(|member| {
                (
                    member.relative_path.clone(),
                    member.file_name.clone(),
                    member.file_size as i64,
                    member.media_type.clone(),
                    member.derived,
                )
            })
            .collect();
        let incoming = observation_fingerprint(
            date.as_deref(),
            group.precision.as_str(),
            &date_basis,
            date_component_index,
            &date_component,
            &context,
            &layers,
            &group.parser_version,
            conflict.as_deref(),
            &members,
        );

        let generation = match existing.get(&group_id) {
            None => {
                report.created += 1;
                tx.execute(
                    "INSERT INTO content_groups (
                         group_id, artist_scope_id, root_relative, artist_root, date, precision,
                         date_basis, date_component_index, date_component, context_json,
                         collection_layers_json, parser_version, boundary_conflict, generation,
                         state, tombstoned_at, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, 1,
                             'active', NULL, ?14, ?14)",
                    params![
                        group_id,
                        artist_scope_id,
                        group.root_relative,
                        artist_root,
                        date,
                        group.precision.as_str(),
                        date_basis,
                        date_component_index,
                        date_component,
                        context,
                        layers,
                        group.parser_version,
                        conflict,
                        now,
                    ],
                )?;
                1
            }
            Some(current) => {
                let stored_members = load_stored_members(&tx, &group_id)?;
                let observation_moved = current.fingerprint(&stored_members) != incoming;
                // A root that comes back is a new observation even when its
                // members look the same: the group left and returned.
                let resurrected = current.state != GROUP_STATE_ACTIVE;
                let scope_moved = current.artist_scope_id != artist_scope_id;
                let generation = if observation_moved || resurrected {
                    current.generation + 1
                } else {
                    current.generation
                };
                if observation_moved || resurrected || scope_moved {
                    tx.execute(
                        "UPDATE content_groups SET
                             artist_scope_id = ?2, date = ?3, precision = ?4, date_basis = ?5,
                             date_component_index = ?6, date_component = ?7, context_json = ?8,
                             collection_layers_json = ?9, parser_version = ?10,
                             boundary_conflict = ?11, generation = ?12, state = 'active',
                             tombstoned_at = NULL, updated_at = ?13
                         WHERE group_id = ?1",
                        params![
                            group_id,
                            artist_scope_id,
                            date,
                            group.precision.as_str(),
                            date_basis,
                            date_component_index,
                            date_component,
                            context,
                            layers,
                            group.parser_version,
                            conflict,
                            generation,
                            now,
                        ],
                    )?;
                    report.updated += 1;
                } else {
                    report.unchanged += 1;
                }
                apply_members(&tx, &group_id, &members, &stored_members, &now)?;
                register_location(&tx, &group_id, &group.root_relative, generation, &now)?;
                continue;
            }
        };

        // A new group: its members and its own observed root.
        apply_members(&tx, &group_id, &members, &[], &now)?;
        register_location(&tx, &group_id, &group.root_relative, generation, &now)?;
    }

    // Roots the latest observation of this scope no longer describes. They keep
    // their row, their members and their generation history.
    let disappeared: Vec<String> = existing
        .values()
        .filter(|group| {
            !observed.contains(&group.group_id)
                && group.state == GROUP_STATE_ACTIVE
                && group.artist_scope_id == artist_scope_id
        })
        .map(|group| group.group_id.clone())
        .collect();
    for group_id in disappeared {
        let stored_members = load_stored_members(&tx, &group_id)?;
        let active_members: Vec<&String> = stored_members
            .iter()
            .filter(|(_, member)| member.state == GROUP_STATE_ACTIVE)
            .map(|(relative_path, _)| relative_path)
            .collect();
        for relative_path in active_members {
            tx.execute(
                "UPDATE content_group_members SET state = 'gone', tombstoned_at = ?3
                   WHERE group_id = ?1 AND relative_path = ?2",
                params![group_id, relative_path, now],
            )?;
        }
        let generation: i64 = tx.query_row(
            "SELECT generation FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )?;
        // The member set really changed, so the generation moves with it.
        tx.execute(
            "UPDATE content_groups SET state = 'gone', tombstoned_at = ?2, generation = ?3,
                    updated_at = ?2
               WHERE group_id = ?1",
            params![group_id, now, generation + 1],
        )?;
        report.gone += 1;
    }

    tx.commit().context("commit content group apply")?;
    Ok(report)
}

/// Write the observed members, tombstoning the ones that disappeared.
///
/// A row that already matches is left alone on purpose: the second apply of the
/// same observation must not touch the table at all.
fn apply_members(
    conn: &Connection,
    group_id: &str,
    members: &[MemberObservation],
    stored: &[(String, StoredMemberRow)],
    now: &str,
) -> Result<()> {
    let stored_by_path: BTreeMap<&str, &StoredMemberRow> = stored
        .iter()
        .map(|(path, member)| (path.as_str(), member))
        .collect();
    for (relative_path, file_name, file_size, media_type, derived) in members {
        match stored_by_path.get(relative_path.as_str()) {
            Some(member)
                if member.state == GROUP_STATE_ACTIVE
                    && member.file_name == *file_name
                    && member.file_size == *file_size
                    && member.media_type == *media_type
                    && member.derived == *derived => {}
            Some(_) => {
                conn.execute(
                    "UPDATE content_group_members SET
                         file_name = ?3, file_size = ?4, media_type = ?5, derived = ?6,
                         state = 'active', tombstoned_at = NULL
                     WHERE group_id = ?1 AND relative_path = ?2",
                    params![
                        group_id,
                        relative_path,
                        file_name,
                        file_size,
                        media_type,
                        if *derived { 1i64 } else { 0i64 },
                    ],
                )?;
            }
            None => {
                // `content_hash` stays empty: the grouping result does not carry
                // the index's hash yet, and an invented value would be worse
                // than a missing one.
                conn.execute(
                    "INSERT INTO content_group_members (
                         group_id, relative_path, file_name, file_size, media_type, derived,
                         content_hash, state, tombstoned_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, '', 'active', NULL)",
                    params![
                        group_id,
                        relative_path,
                        file_name,
                        file_size,
                        media_type,
                        if *derived { 1i64 } else { 0i64 },
                    ],
                )?;
            }
        }
    }
    for (relative_path, member) in stored {
        if member.state == GROUP_STATE_ACTIVE
            && !members.iter().any(|(path, ..)| path == relative_path)
        {
            conn.execute(
                "UPDATE content_group_members SET state = 'gone', tombstoned_at = ?3
                   WHERE group_id = ?1 AND relative_path = ?2",
                params![group_id, relative_path, now],
            )?;
        }
    }
    Ok(())
}

/// Record the observed root as a location of the group.
///
/// A group can hold several locations; this one only maintains the location the
/// observation itself proves. A manual override is left untouched, generation
/// included — the plan makes the user's chosen location the explicit position
/// that a later automatic target must not silently move.
fn register_location(
    conn: &Connection,
    group_id: &str,
    relative_path: &str,
    generation: i64,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO content_group_locations (
             group_id, relative_path, generation, manual_override, source_operation,
             created_at, updated_at)
         VALUES (?1, ?2, ?3, 0, 'grouping', ?4, ?4)
         ON CONFLICT(group_id, relative_path) DO UPDATE SET
             generation = excluded.generation,
             updated_at = excluded.updated_at
         WHERE content_group_locations.manual_override = 0
           AND content_group_locations.generation <> excluded.generation",
        params![group_id, relative_path, generation, now],
    )?;
    Ok(())
}

/// Every group the ledger knows for one scope, oldest date first.
///
/// Order is `date ASC NULLS LAST`, then `root_relative`, then `group_id`: the
/// undated groups come last because "no date" is not "the oldest date".
pub fn list_content_groups(
    conn: &Connection,
    artist_scope_id: Option<&str>,
    include_gone: bool,
) -> Result<Vec<StoredContentGroup>> {
    let mut sql = String::from(GROUP_SELECT);
    let mut bindings: Vec<rusqlite::types::Value> = Vec::new();
    sql.push_str(" WHERE 1 = 1");
    if let Some(scope) = artist_scope_id {
        sql.push_str(" AND g.artist_scope_id = ?");
        bindings.push(rusqlite::types::Value::Text(scope.to_string()));
    }
    if !include_gone {
        sql.push_str(" AND g.state = 'active'");
    }
    sql.push_str(GROUP_GROUP_BY);
    sql.push_str(GROUP_ORDER_BY);
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(rusqlite::params_from_iter(bindings.iter()), map_group_row)?;
    let mut groups = Vec::new();
    for row in rows {
        groups.push(row?);
    }
    Ok(groups)
}

/// The groups of one scope that can answer a specific day.
///
/// The day reader the same-day pairing needs. It requires `precision = 'day'`,
/// so a month-precision group never answers a day query: the plan forbids
/// widening a month into its first day, and a group whose date came from a month
/// reading must stay a candidate for the whole month instead.
pub fn content_groups_for_day(
    conn: &Connection,
    artist_scope_id: &str,
    day: &str,
) -> Result<Vec<StoredContentGroup>> {
    let sql = format!(
        "{GROUP_SELECT} WHERE g.artist_scope_id = ?1 AND g.state = 'active' \
         AND g.precision = 'day' AND g.date = ?2{GROUP_GROUP_BY}{GROUP_ORDER_BY}"
    );
    let mut statement = conn.prepare(&sql)?;
    let rows = statement.query_map(params![artist_scope_id, day], map_group_row)?;
    let mut groups = Vec::new();
    for row in rows {
        groups.push(row?);
    }
    Ok(groups)
}

/// The members of a group, tombstones included.
///
/// History is readable on purpose: a member that a later observation no longer
/// lists keeps its row, and the caller filters on `state` when it wants the
/// live set.
pub fn content_group_members(conn: &Connection, group_id: &str) -> Result<Vec<StoredGroupMember>> {
    let mut statement = conn.prepare(
        "SELECT group_id, relative_path, file_name, file_size, media_type, derived,
                content_hash, state, tombstoned_at
           FROM content_group_members WHERE group_id = ?1 ORDER BY relative_path",
    )?;
    let rows = statement.query_map(params![group_id], |row| {
        Ok(StoredGroupMember {
            group_id: row.get(0)?,
            relative_path: row.get(1)?,
            file_name: row.get(2)?,
            file_size: row.get(3)?,
            media_type: row.get(4)?,
            derived: row.get::<_, i64>(5)? != 0,
            content_hash: row.get(6)?,
            state: row.get(7)?,
            tombstoned_at: row.get(8)?,
        })
    })?;
    let mut members = Vec::new();
    for row in rows {
        members.push(row?);
    }
    Ok(members)
}

/// Every persistent location of one group, manual overrides first.
pub fn content_group_locations(
    conn: &Connection,
    group_id: &str,
) -> Result<Vec<StoredGroupLocation>> {
    let mut statement = conn.prepare(
        "SELECT group_id, relative_path, generation, manual_override, source_operation,
                created_at, updated_at
           FROM content_group_locations WHERE group_id = ?1
          ORDER BY manual_override DESC, relative_path",
    )?;
    let rows = statement.query_map(params![group_id], |row| {
        Ok(StoredGroupLocation {
            group_id: row.get(0)?,
            relative_path: row.get(1)?,
            generation: row.get(2)?,
            manual_override: row.get::<_, i64>(3)? != 0,
            source_operation: row.get(4)?,
            created_at: row.get(5)?,
            updated_at: row.get(6)?,
        })
    })?;
    let mut locations = Vec::new();
    for row in rows {
        locations.push(row?);
    }
    Ok(locations)
}

/// The directory the ledger currently stands behind for this group.
///
/// Resolution follows `PLAN_PAWCHIVE_RECONCILIATION_2026-09-15.md` §7.5: the
/// user's explicit position first, then the location the current observation
/// proves. A location recorded under an older generation describes where the
/// content *used to* be, so reusing it would publish into a directory the group
/// has left — and a location the ledger cannot point at on disk is not
/// "verified", it is a memory.
///
/// `None` therefore means "the ledger has nothing it can stand behind": an
/// unknown group, an empty root, a location the observation has moved past, or a
/// path that is no longer a directory under an authorized media root.
pub fn verified_group_location(
    conn: &Connection,
    group_id: &str,
    roots: &MediaRoots,
) -> Result<Option<PathBuf>> {
    ensure_content_group_schema(conn)?;
    let Some((artist_root, generation)) = conn
        .query_row(
            "SELECT artist_root, generation FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?
    else {
        return Ok(None);
    };
    if artist_root.trim().is_empty() {
        return Ok(None);
    }
    // The user's own decision outranks the observation. It is recorded without a
    // generation on purpose, so a later grouping pass cannot revoke the position
    // by moving the group on.
    //
    // The candidates are walked in that order and the first *verifiable* one
    // wins. An override whose directory is gone is not an answer, but the
    // observation-proven location still is — and using it is what keeps a new
    // attachment out of a freshly rendered directory. Refusing every location
    // the moment one of them fails would fall back to the template and split the
    // work, which is the defect this resolution exists to prevent.
    let candidates: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT relative_path FROM content_group_locations
              WHERE group_id = ?1 AND (manual_override = 1 OR generation = ?2)
              ORDER BY manual_override DESC, updated_at DESC, relative_path",
        )?;
        let rows =
            statement.query_map(params![group_id, generation], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    for relative in candidates {
        if relative.trim().is_empty() {
            continue;
        }
        let path = Path::new(&artist_root).join(&relative);
        if !path.is_dir() {
            continue;
        }
        if !crate::media_roots::path_under_authorized_roots(&path, roots) {
            continue;
        }
        return Ok(Some(path));
    }
    Ok(None)
}

/// The ledger's answer to "where does a new resource of this work belong?".
///
/// Three outcomes, and the difference is load-bearing. A work with no group is
/// *new*: the shared render rules name its directory, and that is correct. A
/// work split across several groups has no single destination at all. Collapsing
/// both into `None` is what let a scattered work be filed into a freshly
/// invented template directory; the plan asks for `location_ambiguous` instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedLocation {
    /// Exactly one group, with a location that exists and is authorized.
    Known(PathBuf),
    /// No active group link: render the new work with the shared rules.
    NoGroup,
    /// Several groups are live. Nobody has said which one the new member
    /// belongs to, so this refuses rather than picks.
    Ambiguous { groups: usize },
}

impl VerifiedLocation {
    /// The destination, when the ledger can name exactly one.
    pub fn path(self) -> Option<PathBuf> {
        match self {
            Self::Known(path) => Some(path),
            Self::NoGroup | Self::Ambiguous { .. } => None,
        }
    }

    /// The refusal to publish, when the ledger cannot name one and the caller is
    /// not allowed to invent it either.
    ///
    /// `None` for `NoGroup`, because rendering a new work is not a refusal.
    pub fn ambiguous_reason(&self) -> Option<String> {
        match self {
            Self::Ambiguous { groups } => Some(format!(
                "location_ambiguous：该作品关联 {groups} 个内容组，没有唯一目的地，\
                 已停止发布。请先在复查中确认成员归属"
            )),
            Self::Known(_) | Self::NoGroup => None,
        }
    }
}

/// Where a new resource for this post belongs, when the ledger already knows
/// where the work lives.
///
/// A work linked to exactly one group inherits that group's verified location.
/// A work linked to several groups has no single answer, and this deliberately
/// does not pick one: the plan holds such a resource for confirmation rather
/// than inventing a directory for it.
pub fn verified_location_for_post(
    conn: &Connection,
    post_db_id: i64,
    roots: &MediaRoots,
) -> Result<VerifiedLocation> {
    ensure_content_group_schema(conn)?;
    let Some((work_id, _)) = crate::pawchive_pairing_write::work_of_post(conn, post_db_id)? else {
        return Ok(VerifiedLocation::NoGroup);
    };
    let groups: Vec<String> = {
        let mut statement = conn.prepare(
            "SELECT group_id FROM work_group_links
              WHERE work_id = ?1 AND revoked_at = '' ORDER BY id",
        )?;
        let rows = statement.query_map(params![work_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    match groups.len() {
        0 => Ok(VerifiedLocation::NoGroup),
        1 => Ok(match verified_group_location(conn, &groups[0], roots)? {
            Some(path) => VerifiedLocation::Known(path),
            // One group, but its recorded location no longer exists or sits
            // outside the authorized roots. That is a stale ledger row, not a
            // scattered work: there is still exactly one claimed destination,
            // so this keeps the previous fallback rather than inventing a new
            // refusal for a case the plan does not name.
            None => VerifiedLocation::NoGroup,
        }),
        groups => Ok(VerifiedLocation::Ambiguous { groups }),
    }
}

/// Whether the group ledger exists in this database at all.
///
/// The tidy/archive paths run on installs that have never grouped anything, and
/// creating the schema from inside a move transaction would be a side effect
/// nobody asked for. The caller gets a no-op instead.
fn group_ledger_present(conn: &Connection) -> Result<bool> {
    Ok(conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_groups' LIMIT 1",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false))
}

// ---------------------------------------------------------------------------
// The 整理 / 发布 fence (backend plan §6.3).
//
// A move and a publish must not complete across different generations of the
// same group: the publish would land a file in the directory the move has
// already renamed away from, and the ledger would then name a location nothing
// occupies. Neither side can see the other's in-memory state, so the fence is
// two rows in the database:
//
// - 整理 registers a move intent in a pre-check, committed *before* it touches
//   the filesystem. It refuses to start while a publish holds a reservation.
// - 发布 takes a short-lived reservation before it renames anything into the
//   group's directory, and refuses while a move intent is pending.
//
// The directory handle the publish already binds is the other half of the
// guarantee: even a fence that was checked a moment ago cannot save a publish
// whose parent directory has since been swapped out from under it.
// ---------------------------------------------------------------------------

/// A move intent that has been registered but not finished.
pub const GROUP_MOVE_INTENT_PENDING: &str = "pending";
/// A move that finished. The row is removed; the location it produced is what
/// remains, so clearing the intent cannot lose a mapping.
pub const GROUP_MOVE_INTENT_APPLIED: &str = "applied";
/// A move that failed. The row is kept so the next attempt can see that this
/// group owes a tidy, rather than silently forgetting it.
pub const GROUP_MOVE_INTENT_FAILED: &str = "failed";

/// How long a publish reservation counts as active.
///
/// A publish that died without releasing must not block tidying forever, so the
/// reservation expires on its own. It is deliberately much longer than one
/// publish takes and much shorter than a tidy round waits.
pub const PUBLISH_RESERVATION_TTL_SECS: i64 = 900;

/// How long a registered 整理 intent stays held without being finished.
///
/// The counterpart of the reservation TTL above, for the same reason in the
/// other direction: `MoveIntentGuard` finishes an intent on drop, and drop does
/// not run when the process is killed or the power goes. Without a lease such a
/// row stays `pending` forever and refuses every publish and every later tidy
/// into that group until somebody edits the database.
///
/// Deliberately much longer than a group move takes — the tidy registers the
/// intent, renames, rewrites the ledger and finishes — and much shorter than an
/// operator would tolerate being stuck.
pub const MOVE_INTENT_LEASE_SECS: i64 = 900;

/// Intents this process registered and has not finished.
///
/// Reclaiming an intent left behind by a dead worker needs a liveness proof,
/// and the filesystem lock supplies it across processes — the OS drops it when
/// its owner dies. `flock` is per open file description, though: two threads of
/// this process each open their own handle and both take the exclusive lock
/// without seeing one another. This set is the missing half. An intent named
/// here belongs to a live thread of this process, so no other thread reclaims
/// it while that thread is between the rename and its commit.
static LIVE_INTENTS: Mutex<Option<HashSet<(String, String)>>> = Mutex::new(None);

fn live_intents() -> std::sync::MutexGuard<'static, Option<HashSet<(String, String)>>> {
    LIVE_INTENTS
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The key an intent is owned under: its database file plus its group.
///
/// Group ids already embed the artist root, but two isolated libraries can be
/// built around the same synthetic root, so the database path is part of it.
fn intent_owner_key(conn: &Connection, group_id: &str) -> (String, String) {
    (
        conn.path().unwrap_or_default().to_string(),
        group_id.to_string(),
    )
}

fn remember_intent(conn: &Connection, group_id: &str) {
    live_intents()
        .get_or_insert_with(HashSet::new)
        .insert(intent_owner_key(conn, group_id));
}

pub(crate) fn forget_intent(conn: &Connection, group_id: &str) {
    if let Some(live) = live_intents().as_mut() {
        live.remove(&intent_owner_key(conn, group_id));
    }
}

/// Release a pending move intent whose owner is gone.
///
/// Call only while holding the exclusive group-operation lock —
/// `lock_group_operations(conn, true)` — because that lock is what turns "gone"
/// from a guess into a fact: a tidy takes it before registering an intent and
/// holds it through the filesystem move and the ledger commit, so a pending row
/// that nobody is holding the lock for cannot belong to a worker that is still
/// moving anything.
///
/// The lease remains the backstop for what this cannot see (a database opened
/// from another machine, a lock the platform refuses). What this adds is that
/// the wait is no longer the whole lease: a tidy killed between the rename and
/// the commit leaves a fresh intent behind, and without this the recovery entry
/// the plan names in V1 is refused for the next 900 seconds, so the directory
/// keeps its new name while every row still names the old one.
///
/// Returns whether it released one.
pub fn reclaim_orphaned_move_intent(conn: &Connection, group_id: &str) -> Result<bool> {
    if !move_fence_present(conn) {
        return Ok(false);
    }
    let row: Option<(String, i64)> = conn
        .query_row(
            "SELECT state, lease_expires_at FROM group_move_intents WHERE group_id = ?1",
            params![group_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((state, _lease_expires_at)) = row else {
        return Ok(false);
    };
    if state != GROUP_MOVE_INTENT_PENDING {
        return Ok(false);
    }
    if live_intents()
        .as_ref()
        .is_some_and(|live| live.contains(&intent_owner_key(conn, group_id)))
    {
        return Ok(false);
    }
    conn.execute(
        "UPDATE group_move_intents
            SET state = ?1, reason = ?2, updated_at = ?3
          WHERE group_id = ?4 AND state = ?5",
        params![
            GROUP_MOVE_INTENT_FAILED,
            "整理进程在移动结束前中断（无人持有操作锁），意向已失效",
            stamp(),
            group_id,
            GROUP_MOVE_INTENT_PENDING,
        ],
    )?;
    forget_intent(conn, group_id);
    Ok(true)
}

/// Hold the filesystem phase independently of the ledger's recovery timeout.
/// A lease expiring does not prove that its worker stopped. Publishers share
/// this database-scoped lock; a tidy takes it exclusively before reading group
/// ownership and keeps it through the filesystem move and ledger commit.
/// The OS releases it even after a killed process. Never unlink the sidecar:
/// replacing its inode would let two workers lock different files.
/// Private in-memory databases have no cross-process filesystem phase.
pub fn lock_group_operations(conn: &Connection, moving: bool) -> Result<Option<std::fs::File>> {
    let Some(path) = conn.path().filter(|path| !path.is_empty()) else {
        return Ok(None);
    };
    let mut lock_path = crate::fs_util::safe_canonicalize(path)
        .context("cannot resolve group-operation database")?
        .into_os_string();
    lock_path.push(".group-operations.lock");
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(PathBuf::from(lock_path))
        .context("cannot open group-operation lock")?;
    let result = if moving {
        file.try_lock()
    } else {
        file.try_lock_shared()
    };
    result.map_err(|error| anyhow::anyhow!("整理或发布仍在进行，请稍后重试：{error}"))?;
    Ok(Some(file))
}

/// The group whose directory contains `path`, plus the generation it is at.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OwnedGroup {
    pub group_id: String,
    pub generation: i64,
    pub artist_root: String,
    pub root_relative: String,
}

fn ensure_move_fence_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS group_move_intents (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL UNIQUE,
            baseline_generation INTEGER NOT NULL DEFAULT 0,
            state TEXT NOT NULL DEFAULT 'pending',
            source_operation TEXT NOT NULL DEFAULT '',
            reason TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL DEFAULT ''
        );
        CREATE TABLE IF NOT EXISTS group_publish_reservations (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            group_id TEXT NOT NULL,
            owner TEXT NOT NULL,
            expires_at INTEGER NOT NULL,
            created_at TEXT NOT NULL DEFAULT '',
            UNIQUE(group_id, owner)
        );
        CREATE INDEX IF NOT EXISTS idx_group_publish_reservations_group
            ON group_publish_reservations(group_id);",
    )?;
    ensure_move_intent_lease_column(conn)?;
    Ok(())
}

/// Add `lease_expires_at` to `group_move_intents` if it is not there yet.
///
/// Installs predating the lease have the table without the column, and the
/// table being present is exactly why `CREATE TABLE IF NOT EXISTS` will not add
/// it — so this has to run on the already-exists path too, not only when the
/// schema is being created.
///
/// The default of 0 is deliberate: an intent left pending by a crash before
/// this upgrade has no lease to honour, so it is reclaimable at once instead of
/// wedging its group until somebody edited the database.
fn ensure_move_intent_lease_column(conn: &Connection) -> Result<()> {
    if move_intent_has_lease_column(conn)? {
        return Ok(());
    }
    conn.execute(
        "ALTER TABLE group_move_intents
         ADD COLUMN lease_expires_at INTEGER NOT NULL DEFAULT 0",
        [],
    )?;
    Ok(())
}

/// Whether `group_move_intents` already carries the lease column.
fn move_intent_has_lease_column(conn: &Connection) -> Result<bool> {
    let mut stmt = conn.prepare("PRAGMA table_info(group_move_intents)")?;
    let names: Vec<String> = stmt
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(names.iter().any(|name| name == "lease_expires_at"))
}

/// Whether the fence tables exist.
///
/// An install that never grouped anything has no groups to fence, and creating
/// tables inside somebody else's transaction is not something a read may do.
fn move_fence_present(conn: &Connection) -> bool {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name='group_move_intents' LIMIT 1",
        [],
        |_| Ok(true),
    )
    .optional()
    .unwrap_or(None)
    .unwrap_or(false)
}

fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs() as i64)
        .unwrap_or(0)
}

fn stamp() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// The group that owns `path`: the deepest active group root that contains it.
///
/// Resolved by walking the path's own ancestors against the indexed
/// `(artist_root, root_relative)` pair, so it is a handful of lookups rather
/// than a scan of every group.
pub fn owning_group_for_path(conn: &Connection, path: &str) -> Result<Option<OwnedGroup>> {
    if !group_ledger_present(conn)? {
        return Ok(None);
    }
    let roots: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT DISTINCT artist_root FROM content_groups WHERE state='active'")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let normalized = path.replace('\\', "/");
    let mut best: Option<OwnedGroup> = None;
    for root in roots {
        let Some(relative) = relative_under_root(&root, &normalized) else {
            continue;
        };
        // Every ancestor of the path, deepest first, plus the path itself.
        let mut candidates: Vec<String> = Vec::new();
        let mut current = relative.clone();
        loop {
            candidates.push(current.clone());
            match current.rfind('/') {
                Some(index) => current = current[..index].to_string(),
                None => break,
            }
        }
        candidates.push(String::new());
        for candidate in candidates {
            let found: Option<(String, i64)> = conn
                .query_row(
                    "SELECT group_id, generation FROM content_groups
                     WHERE artist_root = ?1 AND root_relative = ?2 AND state = 'active'",
                    params![root, candidate],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            if let Some((group_id, generation)) = found {
                let deeper = best
                    .as_ref()
                    .map(|held| candidate.len() > held.root_relative.len())
                    .unwrap_or(true);
                if deeper {
                    best = Some(OwnedGroup {
                        group_id,
                        generation,
                        artist_root: root.clone(),
                        root_relative: candidate,
                    });
                }
                break;
            }
        }
    }
    Ok(best)
}

/// Every active group whose root is `path` or sits under it.
///
/// Used by the tidy side: renaming a directory moves every group inside it, not
/// just the one that happens to be named by the plan.
pub fn groups_under_path(conn: &Connection, path: &str) -> Result<Vec<OwnedGroup>> {
    if !group_ledger_present(conn)? {
        return Ok(Vec::new());
    }
    let roots: Vec<String> = {
        let mut stmt =
            conn.prepare("SELECT DISTINCT artist_root FROM content_groups WHERE state='active'")?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };
    let prefix = format!("{}/", path.replace('\\', "/").trim_end_matches('/'));
    let mut out = Vec::new();
    for root in roots {
        let mut stmt = conn.prepare(
            "SELECT group_id, generation, root_relative FROM content_groups
             WHERE artist_root = ?1 AND state = 'active'",
        )?;
        let rows = stmt.query_map(params![root], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        for row in rows {
            let (group_id, generation, root_relative) = row?;
            let absolute = if root_relative.is_empty() {
                root.clone()
            } else {
                format!("{}/{}", root.trim_end_matches('/'), root_relative)
            };
            let normalized = absolute.replace('\\', "/");
            if normalized == prefix.trim_end_matches('/') || normalized.starts_with(&prefix) {
                out.push(OwnedGroup {
                    group_id,
                    generation,
                    artist_root: root.clone(),
                    root_relative,
                });
            }
        }
    }
    Ok(out)
}

/// Register the tidy side's intent to move `group`'s directory.
///
/// Refuses while a publish holds a live reservation for the group: the plan
/// says 整理 must wait or explicitly block an active publish, and blocking with
/// a reason is the only one of those two this call can honestly do — it has no
/// way to wait for another process.
pub fn begin_group_move_intent(
    conn: &Connection,
    group_id: &str,
    source_operation: &str,
) -> Result<()> {
    begin_group_move_intent_at(conn, group_id, source_operation, now_secs())
}

/// `begin_group_move_intent` with the clock handed in, so a test can ask what
/// happens once a lease has lapsed without waiting out the lease.
fn begin_group_move_intent_at(
    conn: &Connection,
    group_id: &str,
    source_operation: &str,
    now: i64,
) -> Result<()> {
    ensure_move_fence_schema(conn)?;
    // The read and the write have to be one serializable step. As separate
    // autocommit statements a concurrent publish reads the same empty fence and
    // each side concludes the other is idle, which is the race this fence
    // exists to prevent — see
    // `a_tidy_intent_racing_a_publish_reservation_cannot_both_proceed`.
    // IMMEDIATE rather than DEFERRED, and that is not bikeshedding: this
    // sequence happens to begin with a write today, so a DEFERRED transaction
    // would take its write lock on the first statement and look correct — until
    // someone reorders these statements and the fence silently stops holding.
    // IMMEDIATE takes the lock at BEGIN, so the loser's read cannot precede the
    // winner's commit no matter what order the statements end up in.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM group_publish_reservations WHERE expires_at <= ?1",
        params![now],
    )?;
    let publishing: Option<String> = tx
        .query_row(
            "SELECT owner FROM group_publish_reservations WHERE group_id = ?1 LIMIT 1",
            params![group_id],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(owner) = publishing {
        anyhow::bail!("该作品组正在发布文件（{owner}），整理已阻断；请等发布结束后重试");
    }
    // Only a lease that is still running blocks. `held_move_intent` releases a
    // lapsed one, which is the whole difference between a killed tidy costing
    // one lease and costing the group.
    if let Some((state, _generation)) = held_move_intent(&tx, group_id, now)? {
        if state == GROUP_MOVE_INTENT_PENDING {
            anyhow::bail!("该作品组已有一个进行中的整理移动，不能重复登记");
        }
    }
    let generation: i64 = tx
        .query_row(
            "SELECT generation FROM content_groups WHERE group_id = ?1",
            params![group_id],
            |row| row.get(0),
        )
        .optional()?
        .unwrap_or(0);
    tx.execute(
        "DELETE FROM group_move_intents WHERE group_id = ?1",
        params![group_id],
    )?;
    tx.execute(
        "INSERT INTO group_move_intents
             (group_id, baseline_generation, state, source_operation, reason,
              created_at, updated_at, lease_expires_at)
         VALUES (?1, ?2, ?3, ?4, '', ?5, ?5, ?6)",
        params![
            group_id,
            generation,
            GROUP_MOVE_INTENT_PENDING,
            source_operation,
            stamp(),
            now + MOVE_INTENT_LEASE_SECS
        ],
    )?;
    tx.commit()?;
    // Claimed in the process's own name only once the row exists: another
    // thread may reclaim it before then, never after.
    remember_intent(conn, group_id);
    Ok(())
}

/// The move intent recorded for `group`, if one is still held.
///
/// A pending intent whose lease has lapsed is released here rather than
/// honoured. `MoveIntentGuard` finishes intents on drop, and drop does not run
/// when a process is killed or the power goes, so a lapsed lease means the
/// owner is not coming back. Honouring such a row would refuse every tidy and
/// every publish into this group until somebody edited the database by hand —
/// see `an_intent_left_by_a_killed_process_does_not_wedge_the_group_forever`.
///
/// The row is marked failed, not deleted: the plan asks for a recoverable
/// intent, and an interrupted tidy is exactly the case a later reconciliation
/// may still have to roll forward. Deleting it would lose the only trace.
fn held_move_intent(
    tx: &Transaction<'_>,
    group_id: &str,
    now: i64,
) -> Result<Option<(String, i64)>> {
    let row: Option<(String, i64, i64)> = tx
        .query_row(
            "SELECT state, baseline_generation, lease_expires_at
               FROM group_move_intents WHERE group_id = ?1",
            params![group_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let Some((state, generation, lease_expires_at)) = row else {
        return Ok(None);
    };
    if state == GROUP_MOVE_INTENT_PENDING && lease_expires_at <= now {
        tx.execute(
            "UPDATE group_move_intents
                SET state = ?1, reason = ?2, updated_at = ?3
              WHERE group_id = ?4",
            params![
                GROUP_MOVE_INTENT_FAILED,
                "整理进程在移动结束前中断（租约到期），意向已失效",
                stamp(),
                group_id
            ],
        )?;
        forget_intent(&tx, group_id);
        return Ok(None);
    }
    Ok(Some((state, generation)))
}

/// Finish a move intent. A successful one is removed; a failed one stays as a
/// recoverable reminder, which is what the plan asks for.
pub fn finish_group_move_intent(
    conn: &Connection,
    group_id: &str,
    state: &str,
    reason: &str,
) -> Result<()> {
    if !move_fence_present(conn) {
        return Ok(());
    }
    if state == GROUP_MOVE_INTENT_APPLIED {
        conn.execute(
            "DELETE FROM group_move_intents WHERE group_id = ?1",
            params![group_id],
        )?;
        forget_intent(conn, group_id);
        return Ok(());
    }
    conn.execute(
        "UPDATE group_move_intents
            SET state = ?1, reason = ?2, updated_at = ?3
          WHERE group_id = ?4",
        params![state, reason, stamp(), group_id],
    )?;
    forget_intent(conn, group_id);
    Ok(())
}

/// Take a short-lived reservation for publishing into `group`'s directory.
///
/// Refuses while a move intent is pending: the directory the publish resolved
/// is about to be renamed, and writing into it would leave the ledger naming a
/// location the file is not in.
pub fn claim_publish_reservation(conn: &Connection, group_id: &str, owner: &str) -> Result<()> {
    claim_publish_reservation_at(conn, group_id, owner, now_secs())
}

/// `claim_publish_reservation` with the clock handed in — same reason as
/// `begin_group_move_intent_at`: a lease lapse is a thing a test must be able
/// to reach without waiting for it.
fn claim_publish_reservation_at(
    conn: &Connection,
    group_id: &str,
    owner: &str,
    now: i64,
) -> Result<()> {
    if !move_fence_present(conn) {
        // An install that never grouped anything has nothing to fence, and a
        // read may not create tables inside somebody else's transaction. A
        // group that *does* exist must be fenced even on the first publish,
        // though: skipping the reservation because the table is not there yet
        // would leave the very first publish invisible to 整理.
        if !group_ledger_present(conn)? {
            return Ok(());
        }
        ensure_move_fence_schema(conn)?;
    } else {
        // The table is there, which is exactly the case `CREATE TABLE IF NOT
        // EXISTS` will not fix: an install predating the lease keeps the
        // column-less shape until this runs for it.
        ensure_move_intent_lease_column(conn)?;
    }
    // Same reasoning as `begin_group_move_intent`: check and claim have to be
    // one serializable step, or a tidy that starts after this read simply
    // registers its intent and both sides proceed.
    let tx = Transaction::new_unchecked(conn, TransactionBehavior::Immediate)?;
    tx.execute(
        "DELETE FROM group_publish_reservations WHERE expires_at <= ?1",
        params![now],
    )?;
    // Only a live lease blocks. A lapsed one belongs to a tidy that was killed,
    // and refusing on its behalf would wedge this group permanently — see
    // `held_move_intent`.
    if let Some((state, generation)) = held_move_intent(&tx, group_id, now)? {
        if state == GROUP_MOVE_INTENT_PENDING {
            anyhow::bail!(
                "该作品组正在整理移动中（generation {generation}），暂停发布；请等整理结束后重试"
            );
        }
    }
    tx.execute(
        "INSERT INTO group_publish_reservations (group_id, owner, expires_at, created_at)
         VALUES (?1, ?2, ?3, ?4)
         ON CONFLICT(group_id, owner) DO UPDATE SET expires_at = excluded.expires_at",
        params![group_id, owner, now + PUBLISH_RESERVATION_TTL_SECS, stamp()],
    )?;
    tx.commit()?;
    Ok(())
}

/// Drop a publish reservation. Safe to call when none was taken.
pub fn release_publish_reservation(conn: &Connection, group_id: &str, owner: &str) {
    if !move_fence_present(conn) {
        return;
    }
    let _ = conn.execute(
        "DELETE FROM group_publish_reservations WHERE group_id = ?1 AND owner = ?2",
        params![group_id, owner],
    );
}

/// Rewrite the group ledger after a directory or file moved, **inside the
/// caller's transaction**.
///
/// The tidy execution already rewrites `items.file_path` and deletes the plan
/// row in one transaction. Without this the group ledger keeps the old
/// relative path, so the work's verified location disappears the first time a
/// user renames a folder — and the next attachment is rendered into the
/// pre-rename directory. That is the exact outcome the subscription plan
/// forbids ("已有作品补新附件…不能重新创建用户改名前的目录"), and it is why
/// this has to share the move's transaction rather than run afterwards: a
/// crash between the two would leave the rename committed and the mapping
/// stale, which is the case the backend plan calls out by name.
///
/// `moves` are `(old_path, new_path)` pairs as the database stores them; each
/// is resolved against `artist_root` and then applied both as an exact match
/// and as a directory prefix. A pair that does not resolve under the root is
/// skipped rather than guessed at. Returns the number of groups whose recorded
/// location changed.
pub fn relocate_groups_in_tx(
    tx: &rusqlite::Transaction<'_>,
    artist_root: &str,
    moves: &[(String, String)],
    source_operation: &str,
) -> Result<usize> {
    if moves.is_empty() || !group_ledger_present(tx)? {
        return Ok(0);
    }
    let root = artist_root.to_string();
    let now = Utc::now().to_rfc3339();
    let mut touched: Vec<(String, String)> = Vec::new();

    // Members and the group's own root are rewritten together: a group whose
    // directory was renamed has to stay findable under the new name, and its
    // members have to follow it or the member set stops describing reality.
    for (old_raw, new_raw) in moves {
        let (Some(old), Some(new)) = (
            relative_under_root(&root, old_raw),
            relative_under_root(&root, new_raw),
        ) else {
            continue;
        };
        if old.trim().is_empty() || new.trim().is_empty() || old == new {
            continue;
        }
        let old_prefix = format!("{}/", old.trim_end_matches('/'));
        // A directory rewrite keeps every file name; only an exact match means
        // the file itself moved, and only then can its name have changed.
        let new_file_name = new.rsplit('/').next().unwrap_or("").to_string();
        tx.execute(
            "UPDATE content_group_members
                SET relative_path = CASE
                      WHEN relative_path = ?1 THEN ?2
                      ELSE ?2 || substr(relative_path, length(?1) + 1)
                    END,
                    file_name = CASE WHEN relative_path = ?1 THEN ?4 ELSE file_name END
              WHERE relative_path = ?1 OR instr(relative_path, ?3) = 1",
            params![old, new, old_prefix, new_file_name],
        )?;

        let mut stmt = tx.prepare(
            "SELECT group_id, root_relative FROM content_groups
              WHERE root_relative = ?1 OR instr(root_relative, ?2) = 1",
        )?;
        let groups = stmt
            .query_map(params![old, old_prefix], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        for (group_id, root_relative) in groups {
            let next = if root_relative == *old {
                new.clone()
            } else {
                format!(
                    "{}{}",
                    new.trim_end_matches('/'),
                    &root_relative[old.len()..]
                )
            };
            tx.execute(
                "UPDATE content_groups SET root_relative = ?2, updated_at = ?3
                  WHERE group_id = ?1",
                params![group_id, next, now],
            )?;
            touched.push((group_id, next));
        }
    }

    if touched.is_empty() {
        return Ok(0);
    }

    // One location row per group, carrying the group's current generation so
    // `verified_group_location` can see it. The old row is rewritten rather
    // than left behind: it names the same directory the group now lives at.
    let mut seen = std::collections::HashSet::new();
    let mut groups_touched = 0;
    for (group_id, relative_path) in touched.iter().rev() {
        if !seen.insert(group_id.as_str()) {
            continue;
        }
        let generation: i64 = tx
            .query_row(
                "SELECT generation FROM content_groups WHERE group_id = ?1",
                params![group_id],
                |row| row.get(0),
            )
            .unwrap_or(0);
        let changed = tx.execute(
            "UPDATE content_group_locations
                SET relative_path = ?2, generation = ?3, updated_at = ?4
              WHERE group_id = ?1
                AND manual_override = 0",
            params![group_id, relative_path, generation, now],
        )?;
        if changed == 0 {
            tx.execute(
                "INSERT INTO content_group_locations
                     (group_id, relative_path, generation, manual_override, source_operation,
                      created_at, updated_at)
                 VALUES (?1, ?2, ?3, 0, ?4, ?5, ?5)
                 ON CONFLICT(group_id, relative_path) DO UPDATE SET
                     generation = excluded.generation,
                     source_operation = excluded.source_operation,
                     updated_at = excluded.updated_at",
                params![group_id, relative_path, generation, source_operation, now],
            )?;
        }
        groups_touched += 1;
    }
    Ok(groups_touched)
}

/// Mark one location as (or stop treating it as) the user's explicit position.
///
/// Returns whether anything changed, so a caller can tell a real override from a
/// repeat of the state it already had. A location that is not in the ledger yet
/// is created when the call carries something to record: a manual decision, or a
/// source operation. An empty `source_operation` keeps whatever source the row
/// already had.
pub fn mark_group_location_manual(
    conn: &Connection,
    group_id: &str,
    relative_path: &str,
    manual: bool,
    source_operation: &str,
) -> Result<bool> {
    ensure_content_group_schema(conn)?;
    let now = Utc::now().to_rfc3339();
    let flag = if manual { 1i64 } else { 0i64 };
    let tx = conn
        .unchecked_transaction()
        .context("begin group location override")?;
    let existing: Option<(i64, String)> = tx
        .query_row(
            "SELECT manual_override, source_operation FROM content_group_locations
              WHERE group_id = ?1 AND relative_path = ?2",
            params![group_id, relative_path],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let changed = match existing {
        Some((current_manual, current_source)) => {
            if current_manual == flag
                && (source_operation.is_empty() || current_source == source_operation)
            {
                false
            } else {
                let next_source = if source_operation.is_empty() {
                    current_source
                } else {
                    source_operation.to_string()
                };
                tx.execute(
                    "UPDATE content_group_locations SET
                         manual_override = ?3, source_operation = ?4, updated_at = ?5
                     WHERE group_id = ?1 AND relative_path = ?2",
                    params![group_id, relative_path, flag, next_source, now],
                )?;
                true
            }
        }
        None if !manual && source_operation.is_empty() => false,
        None => {
            tx.execute(
                "INSERT INTO content_group_locations (
                     group_id, relative_path, generation, manual_override, source_operation,
                     created_at, updated_at)
                 VALUES (?1, ?2, 0, ?3, ?4, ?5, ?5)",
                params![group_id, relative_path, flag, source_operation, now],
            )?;
            true
        }
    };
    tx.commit().context("commit group location override")?;
    Ok(changed)
}

const GROUP_SELECT: &str = "SELECT g.id, g.group_id, g.artist_scope_id, g.root_relative, \
     g.artist_root, g.date, g.precision, g.date_basis, g.date_component_index, g.date_component, \
     g.context_json, g.collection_layers_json, g.parser_version, g.boundary_conflict, \
     g.generation, g.state, g.tombstoned_at, g.created_at, g.updated_at, \
     COALESCE(SUM(CASE WHEN m.state = 'active' THEN 1 ELSE 0 END), 0) AS member_count, \
     COALESCE(SUM(CASE WHEN m.state = 'active' THEN m.file_size ELSE 0 END), 0) AS total_bytes \
     FROM content_groups g LEFT JOIN content_group_members m ON m.group_id = g.group_id";

const GROUP_GROUP_BY: &str = " GROUP BY g.id";

const GROUP_ORDER_BY: &str = " ORDER BY (g.date IS NULL OR g.date = '') ASC, g.date ASC, \
     g.root_relative ASC, g.group_id ASC";

fn map_group_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<StoredContentGroup> {
    let precision: String = row.get(6)?;
    let date: Option<String> = row.get(5)?;
    let conflict: Option<String> = row.get(13)?;
    Ok(StoredContentGroup {
        id: row.get(0)?,
        group_id: row.get(1)?,
        artist_scope_id: row.get(2)?,
        root_relative: row.get(3)?,
        artist_root: row.get(4)?,
        date: date.filter(|value| !value.is_empty()),
        precision: DatePrecision::parse(&precision),
        date_basis: row.get(7)?,
        date_component_index: row.get(8)?,
        date_component: row.get(9)?,
        context_json: row.get(10)?,
        collection_layers_json: row.get(11)?,
        parser_version: row.get(12)?,
        boundary_conflict: conflict.filter(|value| !value.is_empty()),
        generation: row.get(14)?,
        state: row.get(15)?,
        tombstoned_at: row.get(16)?,
        created_at: row.get(17)?,
        updated_at: row.get(18)?,
        member_count: row.get(19)?,
        total_bytes: row.get(20)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(path: &str, size: u64) -> IndexEntry {
        IndexEntry::as_file(path, "/pictures/ArtistA", size)
    }

    fn dir(path: &str) -> IndexEntry {
        IndexEntry::Directory {
            relative_path: path.to_string(),
            artist_root: "/pictures/ArtistA".to_string(),
        }
    }

    fn group_for<'a>(result: &'a GroupingResult, root: &str) -> &'a ContentGroup {
        result
            .groups
            .iter()
            .find(|group| group.root_relative == root)
            .unwrap_or_else(|| panic!("no group at {root}: {:?}", result.groups))
    }

    /// A year above the work is a collection layer, and the work root is the
    /// dated directory under it — including the `{year}/{month}/date title`
    /// shape the plan lists.
    #[test]
    fn a_year_or_month_layer_narrows_the_context_instead_of_drawing_the_boundary() {
        let result = group_index(&[
            file("2026/2026-09-14 A/images/1.png", 10),
            file("2026/2026-09-14 A/2.png", 11),
            file("2026/202609/2026-09-15 B/1.png", 12),
        ]);

        let a = group_for(&result, "2026/2026-09-14 A");
        assert_eq!(a.date, "2026-09-14");
        assert_eq!(a.precision, DatePrecision::Day);
        assert_eq!(a.member_count, 2, "both files under the work root");
        assert_eq!(a.date_component.as_ref().unwrap().component_index, 1);
        assert_eq!(
            a.date_component.as_ref().unwrap().basis,
            "shared_parser:2026-09-14",
            "the reading records which parser form produced it"
        );
        assert!(
            a.collection_layers
                .iter()
                .any(|layer| layer.component == "2026" && layer.reason == "年份集合层"),
            "{:?}",
            a.collection_layers
        );

        let b = group_for(&result, "2026/202609/2026-09-15 B");
        assert_eq!(b.date, "2026-09-15");
        assert!(b
            .collection_layers
            .iter()
            .any(|layer| layer.component == "202609" && layer.reason == "年月集合层"));
        assert!(
            b.context
                .iter()
                .any(|date| date.precision == DatePrecision::Month),
            "the month layer is still recorded as context"
        );
    }

    /// Two works on the same day are two groups, and a zip with the tree it
    /// extracted into is one.
    #[test]
    fn same_day_directories_are_separate_groups_and_a_zip_shares_its_tree() {
        let result = group_index(&[
            file("2026-09-14 A/1.png", 10),
            file("2026-09-14 A/a.zip", 20),
            file("2026-09-14 A/a/1.png", 11),
            file("2026-09-14 A/a/2.png", 12),
            file("2026-09-14 B/1.png", 13),
        ]);

        let a = group_for(&result, "2026-09-14 A");
        let b = group_for(&result, "2026-09-14 B");
        assert_eq!(a.member_count, 4);
        assert_eq!(b.member_count, 1);
        assert_eq!(
            result
                .groups
                .iter()
                .filter(|group| group.date == "2026-09-14")
                .count(),
            2
        );
    }

    /// A full date under a full date is a conflict, not one merged work.
    #[test]
    fn a_nested_full_date_is_kept_as_a_conflict() {
        let result = group_index(&[file("2026-09-14 A/2026-09-14 B/1.png", 10)]);

        let outer = group_for(&result, "2026-09-14 A");
        assert!(
            outer.boundary_conflict.is_some(),
            "the outer boundary records the inner date"
        );
        assert!(
            result
                .conflicts
                .iter()
                .any(|conflict| conflict.relative_path == "2026-09-14 A"),
            "{:?}",
            result.conflicts
        );
    }

    /// A pure date container may be one work or several; either way the group
    /// is the container, and its children are not counted as extra works.
    #[test]
    fn a_pure_date_container_stays_one_boundary() {
        let result = group_index(&[
            file("2026-09-14/A/1.png", 10),
            file("2026-09-14/B/2.png", 11),
        ]);

        assert_eq!(result.groups.len(), 1);
        let group = group_for(&result, "2026-09-14");
        assert_eq!(group.member_count, 2);
        assert_eq!(group.date, "2026-09-14");
        assert!(
            group
                .collection_layers
                .iter()
                .any(|layer| layer.component == "2026-09-14"),
            "the container is a skipped collection layer and the boundary"
        );
    }

    /// The month-only case must not be padded into a day.
    #[test]
    fn a_month_only_path_stays_month_precision() {
        let result = group_index(&[file("2026-06/pictures/1.png", 10)]);

        // No day boundary exists, so the group is the artist root, dated only
        // to the month.
        let group = group_for(&result, "");
        assert_eq!(group.precision, DatePrecision::Month);
        assert_eq!(group.date, "");
        assert_eq!(
            group
                .context
                .iter()
                .map(|date| date.canonical.as_str())
                .collect::<Vec<_>>(),
            vec!["2026-06"]
        );
    }

    /// A directory with nothing but our own text, nothing but a part file, or
    /// nothing at all is not a group.
    #[test]
    fn generated_text_transient_files_and_empty_directories_are_not_content() {
        let result = group_index(&[
            file("2026-09-14 empty/content 900.txt", 120),
            file("2026-09-14 empty/links title 900.txt", 90),
            file("2026-09-14 partial/.gallery_pawchive_a.part", 4096),
            dir("2026-09-14 truly-empty"),
        ]);

        assert!(result.groups.is_empty(), "{:?}", result.groups);
        assert!(
            result
                .empty_roots
                .iter()
                .any(|root| root == "2026-09-14 empty"),
            "the text-only folder is reported as empty: {:?}",
            result.empty_roots
        );
        assert!(result
            .empty_roots
            .iter()
            .any(|root| root == "2026-09-14 truly-empty"));

        // The user's own text file is content.
        let own = group_index(&[file("2026-09-14 note/我的说明.txt", 30)]);
        assert_eq!(own.groups.len(), 1);
        assert!(own.groups[0].has_content());
    }

    /// A path that could not be read is reported, because a range containing
    /// one is not complete — "not found" and "not readable" differ.
    #[test]
    fn an_unreadable_path_is_reported_rather_than_dropped() {
        let result = group_index(&[
            file("2026-09-14 A/1.png", 10),
            IndexEntry::Unreadable {
                relative_path: "2026-09-14 B".to_string(),
                artist_root: "/pictures/ArtistA".to_string(),
                reason: "permission denied".to_string(),
            },
        ]);

        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.unreadable, vec!["2026-09-14 B（permission denied）"]);
    }

    /// A path with no date at all is undated, and the group root still says
    /// which component the parser looked at.
    #[test]
    fn an_undated_path_is_unknown_precision_not_a_guessed_day() {
        let result = group_index(&[file("renamed folder/1.png", 10)]);

        let group = group_for(&result, "");
        assert_eq!(group.precision, DatePrecision::Unknown);
        assert_eq!(group.date, "");
        assert!(group.context.is_empty());
        assert!(group.date_component.is_none());
        assert_eq!(group.members.len(), 1);
    }

    // -----------------------------------------------------------------------
    // Ledger tests. The fixture is the shared schema plus the ledger's own
    // tables, so the reads run against the same connection shape the product
    // process uses.
    // -----------------------------------------------------------------------

    const SCOPE: &str = "artist-scope:A";
    const ROOT: &str = "/pictures/ArtistA";

    fn fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        // The same stubs the pawchive module's own tests create: its schema
        // references these two tables and its migrations read them.
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY, name TEXT);
             CREATE TABLE IF NOT EXISTS download_publish_jobs (id INTEGER PRIMARY KEY);",
        )
        .unwrap();
        crate::pawchive::ensure_pawchive_schema(&conn).unwrap();
        ensure_content_group_schema(&conn).unwrap();
        conn
    }

    fn apply(conn: &Connection, result: &GroupingResult) -> GroupingApplyReport {
        apply_grouping(conn, SCOPE, ROOT, result).unwrap()
    }

    fn stored<'a>(groups: &'a [StoredContentGroup], root: &str) -> &'a StoredContentGroup {
        groups
            .iter()
            .find(|group| group.root_relative == root)
            .unwrap_or_else(|| panic!("no stored group at {root:?}: {groups:?}"))
    }

    #[test]
    fn applying_the_same_grouping_twice_changes_nothing() {
        let conn = fixture();
        let result = group_index(&[
            file("2026-09-14 A/1.png", 10),
            file("2026-09-14 B/1.png", 11),
        ]);

        let first = apply(&conn, &result);
        assert_eq!(
            (first.created, first.updated, first.unchanged, first.gone),
            (2, 0, 0, 0)
        );
        let before = list_content_groups(&conn, Some(SCOPE), true).unwrap();
        assert_eq!(before.len(), 2);
        assert!(before.iter().all(|group| group.generation == 1));

        let second = apply(&conn, &result);
        assert_eq!(
            (
                second.created,
                second.updated,
                second.unchanged,
                second.gone
            ),
            (0, 0, 2, 0)
        );
        let after = list_content_groups(&conn, Some(SCOPE), true).unwrap();
        assert_eq!(before, after, "a repeated apply wrote nothing at all");
    }

    #[test]
    fn one_more_file_bumps_only_that_groups_generation() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 B/1.png", 11),
            ]),
        );

        let report = apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 A/2.png", 12),
                file("2026-09-14 B/1.png", 11),
            ]),
        );
        assert_eq!(
            (report.created, report.updated, report.unchanged),
            (0, 1, 1)
        );

        let groups = list_content_groups(&conn, None, false).unwrap();
        assert_eq!(stored(&groups, "2026-09-14 A").generation, 2);
        assert_eq!(stored(&groups, "2026-09-14 A").member_count, 2);
        assert_eq!(stored(&groups, "2026-09-14 B").generation, 1);
    }

    #[test]
    fn a_group_that_disappears_is_gone_with_tombstoned_members_and_readable_history() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 B/1.png", 11),
            ]),
        );

        let report = apply(&conn, &group_index(&[file("2026-09-14 B/1.png", 11)]));
        assert_eq!(
            (
                report.created,
                report.updated,
                report.unchanged,
                report.gone
            ),
            (0, 0, 1, 1)
        );

        let all = list_content_groups(&conn, Some(SCOPE), true).unwrap();
        let gone = stored(&all, "2026-09-14 A");
        assert_eq!(gone.state, GROUP_STATE_GONE);
        assert!(!gone.is_active());
        assert!(gone.tombstoned_at.is_some(), "the root keeps a tombstone");
        assert_eq!(gone.generation, 2, "the member set really changed");
        assert!(
            list_content_groups(&conn, Some(SCOPE), false)
                .unwrap()
                .iter()
                .all(|group| group.root_relative != "2026-09-14 A"),
            "a gone group is not a candidate read"
        );

        let members = content_group_members(&conn, &gone.group_id).unwrap();
        assert_eq!(members.len(), 1, "the member row is kept, not deleted");
        assert_eq!(members[0].relative_path, "2026-09-14 A/1.png");
        assert_eq!(members[0].state, GROUP_STATE_GONE);
        assert!(members[0].tombstoned_at.is_some());

        // History stays readable, and shrinking the same observation again is a
        // no-op rather than another generation.
        let again = apply(&conn, &group_index(&[file("2026-09-14 B/1.png", 11)]));
        assert_eq!((again.created, again.updated, again.gone), (0, 0, 0));
        assert_eq!(list_content_groups(&conn, Some(SCOPE), true).unwrap(), all);
    }

    #[test]
    fn a_member_that_disappears_keeps_a_tombstone_row() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 A/2.png", 11),
            ]),
        );
        let groups = list_content_groups(&conn, None, false).unwrap();
        let group_id = stored(&groups, "2026-09-14 A").group_id.clone();

        let report = apply(&conn, &group_index(&[file("2026-09-14 A/1.png", 10)]));
        assert_eq!(report.updated, 1);

        let members = content_group_members(&conn, &group_id).unwrap();
        assert_eq!(
            members.len(),
            2,
            "the removed member is tombstoned, not gone"
        );
        let removed = members
            .iter()
            .find(|member| member.relative_path == "2026-09-14 A/2.png")
            .unwrap();
        assert_eq!(removed.state, GROUP_STATE_GONE);
        assert!(removed.tombstoned_at.is_some());
        assert!(members
            .iter()
            .find(|member| member.relative_path == "2026-09-14 A/1.png")
            .unwrap()
            .is_active());

        let groups = list_content_groups(&conn, None, false).unwrap();
        let group = stored(&groups, "2026-09-14 A");
        assert_eq!(group.member_count, 1, "only the live member is counted");
        assert!(group.is_active(), "the group itself survives");
    }

    #[test]
    fn a_month_precision_group_never_answers_a_day_query() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-06/pictures/1.png", 10),
                file("2026-06-15 Work/1.png", 11),
            ]),
        );

        let all = list_content_groups(&conn, Some(SCOPE), false).unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(stored(&all, "").precision, DatePrecision::Month);
        assert_eq!(stored(&all, "").date, None, "a month is not a day");

        let day = content_groups_for_day(&conn, SCOPE, "2026-06-15").unwrap();
        assert_eq!(day.len(), 1);
        assert_eq!(day[0].root_relative, "2026-06-15 Work");
        assert!(
            content_groups_for_day(&conn, SCOPE, "2026-06-01")
                .unwrap()
                .is_empty(),
            "the month group is not expanded into the first of the month"
        );
        assert!(content_groups_for_day(&conn, SCOPE, "2026-06")
            .unwrap()
            .is_empty());
        assert!(
            content_groups_for_day(&conn, "artist-scope:B", "2026-06-15")
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn two_groups_under_one_artist_root_keep_distinct_ids() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 B/2.png", 11),
            ]),
        );

        let groups = list_content_groups(&conn, None, false).unwrap();
        assert_eq!(groups.len(), 2);
        let a = stored(&groups, "2026-09-14 A");
        let b = stored(&groups, "2026-09-14 B");
        assert_ne!(a.group_id, b.group_id);
        assert_eq!(a.artist_root, ROOT);
        assert_eq!(b.artist_scope_id, SCOPE);
        assert_eq!(a.date.as_deref(), Some("2026-09-14"));
        assert_eq!(a.date_basis, "shared_parser:2026-09-14");

        // The id is derived from the root, so it survives another observation.
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 B/2.png", 11),
            ]),
        );
        assert_eq!(list_content_groups(&conn, None, false).unwrap(), groups);
    }

    #[test]
    fn listing_order_is_date_first_with_unknown_dates_last() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("renamed folder/1.png", 10),
                file("2026-09-14 Late/1.png", 11),
                file("2026-06-15 Early/1.png", 12),
            ]),
        );

        let roots: Vec<String> = list_content_groups(&conn, None, false)
            .unwrap()
            .into_iter()
            .map(|group| group.root_relative)
            .collect();
        assert_eq!(
            roots,
            vec![
                "2026-06-15 Early".to_string(),
                "2026-09-14 Late".to_string(),
                String::new(),
            ]
        );
    }

    #[test]
    fn the_report_counts_what_the_grouping_reported() {
        let conn = fixture();
        let result = group_index(&[
            file("2026-09-14 A/2026-09-14 B/1.png", 10),
            file("2026-09-14 empty/content 900.txt", 120),
            IndexEntry::Unreadable {
                relative_path: "2026-09-14 C".to_string(),
                artist_root: ROOT.to_string(),
                reason: "permission denied".to_string(),
            },
        ]);
        assert_eq!(result.conflicts.len(), 1);
        assert_eq!(result.empty_roots, vec!["2026-09-14 empty".to_string()]);
        assert_eq!(result.unreadable.len(), 1);

        let report = apply(&conn, &result);
        assert_eq!(report.conflicts, result.conflicts.len());
        assert_eq!(report.empty_roots, result.empty_roots.len());
        assert_eq!(report.unreadable, result.unreadable.len());
        assert_eq!(report.created, result.groups.len());

        // A root that only holds our own text never becomes a group.
        let groups = list_content_groups(&conn, None, true).unwrap();
        assert_eq!(groups.len(), 1);
        assert!(stored(&groups, "2026-09-14 A").boundary_conflict.is_some());
    }

    #[test]
    fn a_manual_location_override_survives_regrouping_and_reports_changes() {
        let conn = fixture();
        apply(&conn, &group_index(&[file("2026-09-14 A/1.png", 10)]));
        let groups = list_content_groups(&conn, None, false).unwrap();
        let group_id = stored(&groups, "2026-09-14 A").group_id.clone();

        let initial = content_group_locations(&conn, &group_id).unwrap();
        assert_eq!(initial.len(), 1);
        assert_eq!(initial[0].relative_path, "2026-09-14 A");
        assert!(!initial[0].manual_override);
        assert_eq!(initial[0].source_operation, "grouping");
        assert_eq!(initial[0].generation, 1);

        assert!(
            mark_group_location_manual(&conn, &group_id, "2026-09-14 A", true, "user_rename")
                .unwrap()
        );
        assert!(
            !mark_group_location_manual(&conn, &group_id, "2026-09-14 A", true, "user_rename")
                .unwrap(),
            "repeating the same override changes nothing"
        );

        // A later observation bumps the group, but not the user's location.
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 A/2.png", 12),
            ]),
        );
        let groups = list_content_groups(&conn, None, false).unwrap();
        assert_eq!(stored(&groups, "2026-09-14 A").generation, 2);
        let after = content_group_locations(&conn, &group_id).unwrap();
        assert_eq!(after.len(), 1);
        assert!(after[0].manual_override);
        assert_eq!(after[0].source_operation, "user_rename");
        assert_eq!(
            after[0].generation, 1,
            "the manual location is not re-stamped"
        );

        // A group can hold several locations, and one the ledger does not know
        // yet is created when the call carries a real operation.
        assert!(mark_group_location_manual(
            &conn,
            &group_id,
            "moved/2026-09-14 A",
            false,
            "rename"
        )
        .unwrap());
        assert!(
            !mark_group_location_manual(&conn, &group_id, "unknown/place", false, "").unwrap(),
            "nothing to record, so no row is invented"
        );
        assert_eq!(content_group_locations(&conn, &group_id).unwrap().len(), 2);
    }

    /// The download target follows the ledger, not a guess from the disk. A
    /// group's verified location is the user's position when the disk still
    /// holds it, otherwise the location the current observation proves; a
    /// location recorded under an older generation is where the content used to
    /// be and is never reused.
    #[test]
    fn a_verified_location_comes_from_the_ledger_and_a_stale_one_is_refused() {
        use crate::media_roots::MediaRoots;

        let dir = tempfile::tempdir().unwrap();
        let artist_root = dir.path().join("ArtistA");
        std::fs::create_dir_all(artist_root.join("2026-09-14 renamed")).unwrap();
        std::fs::create_dir_all(artist_root.join("2026-09-14 old")).unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let conn = fixture();
        conn.execute(
            "INSERT INTO content_groups
                 (group_id, artist_scope_id, root_relative, artist_root, date, precision,
                  generation, state, created_at, updated_at)
             VALUES ('grp-a', ?1, '2026-09-14 renamed', ?2, '2026-09-14', 'day', 7,
                     'active', '', '')",
            params![SCOPE, artist_root.to_string_lossy()],
        )
        .unwrap();
        let put_location = |relative: &str, generation: i64, manual: bool| {
            conn.execute(
                "INSERT INTO content_group_locations
                     (group_id, relative_path, generation, manual_override, source_operation,
                      created_at, updated_at)
                 VALUES ('grp-a', ?1, ?2, ?3, 'test', '', '')",
                params![relative, generation, if manual { 1i64 } else { 0i64 }],
            )
            .unwrap();
        };

        // No location row at all: the ledger has nothing to stand behind.
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            None
        );

        // The location the current observation proves is used.
        put_location("2026-09-14 renamed", 7, false);
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(artist_root.join("2026-09-14 renamed"))
        );

        // A location from an older generation is history, not an answer.
        put_location("2026-09-14 old", 3, false);
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(artist_root.join("2026-09-14 renamed"))
        );

        // The user's own position outranks the observation, and its generation
        // is deliberately not what decides.
        conn.execute(
            "UPDATE content_group_locations SET manual_override = 1
              WHERE group_id = 'grp-a' AND relative_path = '2026-09-14 old'",
            [],
        )
        .unwrap();
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(artist_root.join("2026-09-14 old"))
        );

        // An override the disk no longer holds is not verified, and the
        // observation-proven location takes over rather than letting the caller
        // render a brand-new directory.
        std::fs::remove_dir_all(artist_root.join("2026-09-14 old")).unwrap();
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(artist_root.join("2026-09-14 renamed"))
        );

        // A location outside the authorized roots is refused even when it
        // exists: publishing happens inside the media roots or not at all.
        let outside = tempfile::tempdir().unwrap();
        put_location(&outside.path().to_string_lossy(), 7, true);
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(artist_root.join("2026-09-14 renamed"))
        );

        // An unknown group has no location, and a group with no artist root is
        // not something to join a path onto.
        assert_eq!(
            verified_group_location(&conn, "grp-missing", &roots).unwrap(),
            None
        );
    }

    /// A shared file-backed database with the group ledger and both fence
    /// tables, for the two-connection races below.
    fn fenced_db(tag: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join(format!("fence-{tag}.db"));
        {
            let seed = Connection::open(&path).unwrap();
            crate::ingest_publish::ensure_ingest_publish_schema(&seed).unwrap();
            crate::pawchive::ensure_pawchive_schema(&seed).unwrap();
            ensure_content_group_schema(&seed).unwrap();
            ensure_move_fence_schema(&seed).unwrap();
            seed.execute(
                "INSERT INTO content_groups
                     (group_id, artist_scope_id, root_relative, artist_root, generation)
                 VALUES ('grp-a', 'scope-a', 'A/2026/01', '/media', 3)",
                [],
            )
            .unwrap();
        }
        (dir, path)
    }

    /// An authorizer that pauses, once, when `table` is about to be inserted
    /// into — after the caller has read, before it has written. The channel
    /// tells the other side the window is open; the sleep keeps it open.
    fn pause_at_insert(
        table: String,
        entered: std::sync::mpsc::Sender<()>,
    ) -> impl FnMut(rusqlite::hooks::AuthContext<'_>) -> rusqlite::hooks::Authorization + Send + 'static
    {
        use rusqlite::hooks::{AuthAction, Authorization};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::time::Duration;

        let fired = std::sync::Arc::new(AtomicBool::new(false));
        move |ctx| {
            if let AuthAction::Insert { table_name } = ctx.action {
                if table_name == table.as_str() && !fired.swap(true, Ordering::SeqCst) {
                    let _ = entered.send(());
                    std::thread::sleep(Duration::from_millis(400));
                }
            }
            Authorization::Allow
        }
    }

    /// Run 整理 and 发布 against two connections, holding the side named in
    /// `pause` between its read and its write. That is the only window a fence
    /// can be wrong about, and a channel rather than a barrier is what makes it
    /// deterministic: the other side starts only once the paused side is known
    /// to be inside the window, so the outcome does not depend on which thread
    /// the scheduler happens to like.
    fn race_the_fence(
        tag: &str,
        pause: &str,
    ) -> (
        tempfile::TempDir,
        Result<(), anyhow::Error>,
        Result<(), anyhow::Error>,
    ) {
        use std::sync::mpsc;
        use std::time::Duration;

        let (dir, path) = fenced_db(tag);
        let conn_a = Connection::open(&path).unwrap();
        conn_a.busy_timeout(Duration::from_secs(5)).unwrap();
        let conn_b = Connection::open(&path).unwrap();
        conn_b.busy_timeout(Duration::from_secs(5)).unwrap();

        let (tx, rx) = mpsc::channel::<()>();
        let paused_is_tidy = pause == "tidy";
        if paused_is_tidy {
            conn_a.authorizer(Some(pause_at_insert("group_move_intents".to_string(), tx)));
        } else {
            conn_b.authorizer(Some(pause_at_insert(
                "group_publish_reservations".to_string(),
                tx,
            )));
        }

        // Only one side waits, so the receiver goes to that one — it is not
        // Clone, and an Option in each closure would still move it twice.
        let (tidy, publish) = if paused_is_tidy {
            let tidy = std::thread::spawn(move || {
                begin_group_move_intent(&conn_a, "grp-a", "folder_rename")
            });
            let publish = std::thread::spawn(move || {
                let _ = rx.recv();
                claim_publish_reservation(&conn_b, "grp-a", "publish:job-1")
            });
            (tidy, publish)
        } else {
            let tidy = std::thread::spawn(move || {
                let _ = rx.recv();
                begin_group_move_intent(&conn_a, "grp-a", "folder_rename")
            });
            let publish = std::thread::spawn(move || {
                claim_publish_reservation(&conn_b, "grp-a", "publish:job-1")
            });
            (tidy, publish)
        };
        let tidy = tidy.join().unwrap();
        let publish = publish.join().unwrap();
        (dir, tidy, publish)
    }

    /// Assert the outcome every fence race has to produce: exactly one side
    /// proceeds, and the other is refused *by the fence* — "database is locked"
    /// would mean the fence was never reached.
    fn assert_exactly_one_proceeds(
        tidy: &Result<(), anyhow::Error>,
        publish: &Result<(), anyhow::Error>,
    ) {
        let agreed = usize::from(tidy.is_ok()) + usize::from(publish.is_ok());
        assert_eq!(
            agreed, 1,
            "the fence agreed to both sides: tidy={tidy:?} publish={publish:?}"
        );
        let refused = match (tidy, publish) {
            (Err(error), _) => error.to_string(),
            (_, Err(error)) => error.to_string(),
            _ => unreachable!(),
        };
        assert!(
            refused.contains("整理") || refused.contains("发布"),
            "the loser should be refused by the fence, not by SQLite: {refused}"
        );
    }

    /// Env var that turns this test binary into the victim: instead of running
    /// the assertions it registers a move intent and then waits to be killed.
    const VICTIM_DB_ENV: &str = "GALLERY_FENCE_VICTIM_DB";

    /// The victim half: register an intent, announce it, then hang until the
    /// parent kills this process. Nothing here ever returns, so the process
    /// dies with the intent still `pending` — which is precisely the state a
    /// power loss or an OOM kill leaves behind.
    fn hold_an_intent_until_killed(db_path: &str) {
        let conn = Connection::open(db_path).unwrap();
        let _operation_lock = lock_group_operations(&conn, true).unwrap();
        begin_group_move_intent(&conn, "grp-a", "folder_rename").unwrap();
        std::fs::write(
            std::path::Path::new(db_path).with_extension("ready"),
            "ready",
        )
        .unwrap();
        loop {
            std::thread::sleep(std::time::Duration::from_secs(3600));
        }
    }

    /// A tidy that is killed mid-move leaves its intent `pending`, because
    /// `MoveIntentGuard` releases it on drop and drop does not run when a
    /// process is killed. Before the lease, that one row refused every publish
    /// and every later tidy into that group for good — the fence would outlive
    /// the thing it was fencing by an unbounded margin, and the only way out was
    /// editing the database.
    ///
    /// This is two real processes: the child is this same test binary, run again
    /// with an env var that sends it into `hold_an_intent_until_killed`. That
    /// also closes the gap the concurrency batch left open, where both sides of
    /// the fence were separate connections inside one process.
    #[test]
    fn an_intent_left_by_a_killed_process_does_not_wedge_the_group_forever() {
        if let Ok(db_path) = std::env::var(VICTIM_DB_ENV) {
            hold_an_intent_until_killed(&db_path);
            return;
        }

        let (_dir, path) = fenced_db("killed-tidy");
        let ready = path.with_extension("ready");
        let mut child = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                // The full path, not the bare name: with `--exact` the harness
                // matches the whole thing.
                "pawchive_groups::tests::an_intent_left_by_a_killed_process_does_not_wedge_the_group_forever",
                "--exact",
                "--nocapture",
            ])
            .env(VICTIM_DB_ENV, &path)
            .spawn()
            .expect("spawn the victim process");

        let mut registered = ready.exists();
        for _ in 0..400 {
            if registered {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
            registered = ready.exists();
        }
        // Inspect while the child is alive, but always reap it before asserting.
        let blocked_while_alive = if registered {
            let conn = Connection::open(&path).unwrap();
            lock_group_operations(&conn, false).is_err()
        } else {
            false
        };
        // Kill unconditionally: a victim left running would hang the suite.
        let _ = child.kill();
        let _ = child.wait();
        assert!(registered, "the victim never registered its intent");
        assert!(blocked_while_alive, "a live mover must retain the OS lock");

        let conn = Connection::open(&path).unwrap();
        drop(lock_group_operations(&conn, false).unwrap());
        conn.busy_timeout(std::time::Duration::from_secs(5))
            .unwrap();
        let (state, lease): (String, i64) = conn
            .query_row(
                "SELECT state, lease_expires_at FROM group_move_intents WHERE group_id='grp-a'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            state, GROUP_MOVE_INTENT_PENDING,
            "a killed tidy must leave the intent pending — that is what Drop cannot clean up"
        );

        // Immediately after the kill the lease is still running, so the fence
        // still holds. That is the honest cost of not being able to observe
        // another process: bounded by the lease, not unbounded as before.
        let refused = claim_publish_reservation(&conn, "grp-a", "publish:job-1").unwrap_err();
        assert!(
            refused.to_string().contains("整理"),
            "a live lease must still refuse a publish: {refused}"
        );

        // Once the lease lapses the intent is released and the group is usable
        // again in both directions, which is the whole point of having one.
        claim_publish_reservation_at(&conn, "grp-a", "publish:job-1", lease + 1).unwrap();
        release_publish_reservation(&conn, "grp-a", "publish:job-1");
        begin_group_move_intent_at(&conn, "grp-a", "folder_rename", lease + 1).unwrap();
    }

    /// An install predating the lease holds the table without the column. A
    /// crash that happened *before* the fix existed left exactly such a row
    /// pending, and it must not wedge its group forever on account of being
    /// older than the code that knows how to release it.
    #[test]
    fn a_move_intent_from_before_the_lease_existed_is_reclaimable() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("legacy-fence.db");
        let conn = Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE group_move_intents (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                group_id TEXT NOT NULL UNIQUE,
                baseline_generation INTEGER NOT NULL DEFAULT 0,
                state TEXT NOT NULL DEFAULT 'pending',
                source_operation TEXT NOT NULL DEFAULT '',
                reason TEXT NOT NULL DEFAULT '',
                created_at TEXT NOT NULL DEFAULT '',
                updated_at TEXT NOT NULL DEFAULT ''
            );
            CREATE TABLE group_publish_reservations (
                id INTEGER PRIMARY KEY AUTOINCREMENT,
                group_id TEXT NOT NULL,
                owner TEXT NOT NULL,
                expires_at INTEGER NOT NULL,
                created_at TEXT NOT NULL DEFAULT '',
                UNIQUE(group_id, owner)
            );
            INSERT INTO group_move_intents (group_id, state) VALUES ('grp-a', 'pending');
            CREATE TABLE content_groups (group_id TEXT NOT NULL, generation INTEGER NOT NULL DEFAULT 0);
            INSERT INTO content_groups (group_id, generation) VALUES ('grp-a', 3);",
        )
        .unwrap();

        // The migration adds the column with a default of 0, which is the same
        // as a lease that lapsed long ago: nothing is holding this group.
        claim_publish_reservation(&conn, "grp-a", "publish:legacy").unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM group_move_intents WHERE group_id='grp-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            state, GROUP_MOVE_INTENT_FAILED,
            "the row is kept, marked failed — recoverable, not silently dropped"
        );

        // And the group is usable again in the other direction too.
        release_publish_reservation(&conn, "grp-a", "publish:legacy");
        begin_group_move_intent(&conn, "grp-a", "folder_rename").unwrap();
    }

    #[test]
    fn group_operation_lock_outlives_both_ledger_leases() {
        let (_dir, path) = fenced_db("long-operation");
        let first = Connection::open(&path).unwrap();
        let second = Connection::open(&path).unwrap();
        let moving = lock_group_operations(&first, true).unwrap();
        begin_group_move_intent_at(&first, "grp-a", "folder_rename", 0).unwrap();
        assert!(lock_group_operations(&second, false).is_err());
        assert!(lock_group_operations(&second, true).is_err());
        drop(moving);

        let publish = lock_group_operations(&first, false).unwrap();
        claim_publish_reservation_at(&first, "grp-a", "job", MOVE_INTENT_LEASE_SECS + 1).unwrap();
        first
            .execute("UPDATE group_publish_reservations SET expires_at = 0", [])
            .unwrap();
        let another_publish = lock_group_operations(&second, false).unwrap();
        assert!(lock_group_operations(&second, true).is_err());
        drop(publish);
        assert!(lock_group_operations(&first, true).is_err());
        drop(another_publish);
        drop(lock_group_operations(&second, true).unwrap());
    }

    /// Two connections, two threads. The fence has to hold under real
    /// interleaving, not only under an order a single-threaded test chooses.
    ///
    /// Both directions read "is the other side busy" and then write their own
    /// row. Unless those statements are one serializable transaction, both
    /// reads can land before both writes: each side sees an empty fence and
    /// both proceed, which is the race the fence exists for.
    #[test]
    fn a_tidy_intent_racing_a_publish_reservation_cannot_both_proceed() {
        let (_dir, tidy, publish) = race_the_fence("tidy-paused", "tidy");
        assert_exactly_one_proceeds(&tidy, &publish);
    }

    /// The mirror of the race above: now 发布 is the side held between its read
    /// and its write, and 整理 is the one that has to be refused. Only fencing
    /// one direction leaves this one open.
    #[test]
    fn a_publish_reservation_racing_a_tidy_intent_cannot_both_proceed() {
        let (_dir, tidy, publish) = race_the_fence("publish-paused", "publish");
        assert_exactly_one_proceeds(&tidy, &publish);
    }

    /// §6.3: the fence between 整理 and 发布. Both directions are asserted
    /// because implementing only one is the easy mistake — the two sides live in
    /// different files and neither can see the other's state.
    #[test]
    fn a_move_and_a_publish_cannot_hold_the_same_group_at_once() {
        let conn = fixture();
        apply(
            &conn,
            &group_index(&[
                file("2026-09-14 A/1.png", 10),
                file("2026-09-14 A/nested/2.png", 12),
                file("2026-09-15 B/1.png", 11),
            ]),
        );
        let groups = list_content_groups(&conn, None, false).unwrap();
        let group_id = groups
            .iter()
            .find(|group| group.root_relative == "2026-09-14 A")
            .unwrap()
            .group_id
            .clone();

        // The owner lookup resolves the deepest group that contains the path,
        // so a publish into a subdirectory is fenced by the group above it.
        let owned = owning_group_for_path(&conn, "/pictures/ArtistA/2026-09-14 A/nested")
            .unwrap()
            .unwrap();
        assert_eq!(owned.group_id, group_id);
        assert_eq!(owned.root_relative, "2026-09-14 A");
        assert_eq!(
            owning_group_for_path(&conn, "/pictures/ArtistA/nowhere").unwrap(),
            None
        );

        // 1. A pending move intent refuses the publish.
        begin_group_move_intent(&conn, &group_id, "folder_rename").unwrap();
        let refused = claim_publish_reservation(&conn, &group_id, "publish:1");
        assert!(refused.is_err(), "a pending move must refuse the publish");
        assert!(refused.unwrap_err().to_string().contains("正在整理移动中"));

        // 2. Once the move finishes, the publish may take its reservation.
        finish_group_move_intent(&conn, &group_id, GROUP_MOVE_INTENT_APPLIED, "").unwrap();
        claim_publish_reservation(&conn, &group_id, "publish:1").unwrap();

        // 3. And while that reservation lives, 整理 is blocked rather than
        // silently waiting for something it cannot wait for.
        let blocked = begin_group_move_intent(&conn, &group_id, "folder_rename");
        assert!(blocked.is_err(), "a live publish must block the tidy move");
        assert!(blocked.unwrap_err().to_string().contains("整理已阻断"));

        // 4. Releasing the reservation frees the group again. A publish that
        // died without releasing must not do the opposite, so the reservation
        // also expires on its own.
        release_publish_reservation(&conn, &group_id, "publish:1");
        begin_group_move_intent(&conn, &group_id, "folder_rename").unwrap();

        // 5. A second tidy on the same group is refused: two moves fencing one
        // group would each think it owns the directory.
        assert!(begin_group_move_intent(&conn, &group_id, "folder_rename").is_err());

        // 6. A failed move leaves a recoverable intent, and the next tidy can
        // re-register instead of being stuck behind it forever.
        finish_group_move_intent(
            &conn,
            &group_id,
            GROUP_MOVE_INTENT_FAILED,
            "permission_denied",
        )
        .unwrap();
        let state: String = conn
            .query_row(
                "SELECT state FROM group_move_intents WHERE group_id = ?1",
                params![group_id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, GROUP_MOVE_INTENT_FAILED);
        begin_group_move_intent(&conn, &group_id, "folder_rename").unwrap();
    }

    /// A work linked to one group inherits that group's location; a work linked
    /// to several has no single answer and must not have one picked for it.
    #[test]
    fn a_work_inherits_a_location_only_when_it_has_one_group() {
        use crate::media_roots::MediaRoots;

        let dir = tempfile::tempdir().unwrap();
        let artist_root = dir.path().join("ArtistA");
        std::fs::create_dir_all(artist_root.join("2026-09-14 A")).unwrap();
        std::fs::create_dir_all(artist_root.join("2026-09-14 B")).unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );
        let conn = fixture();
        crate::pawchive::ensure_pawchive_schema(&conn).unwrap();
        conn.execute_batch(
            "INSERT INTO kemono_subscriptions
                 (id, service, user_id, target_dir, enabled, created_at, updated_at)
             VALUES (1, 'fanbox', '27212726', '/pictures/ArtistA', 1, 't', 't');
             INSERT INTO kemono_posts (id, subscription_id, post_id, status, created_at, updated_at)
             VALUES (1, 1, '900', 'pending', 't', 't');",
        )
        .unwrap();
        for (group_id, relative) in [("grp-a", "2026-09-14 A"), ("grp-b", "2026-09-14 B")] {
            conn.execute(
                "INSERT INTO content_groups
                     (group_id, artist_scope_id, root_relative, artist_root, date, precision,
                      generation, state, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, '2026-09-14', 'day', 1, 'active', '', '')",
                params![group_id, SCOPE, relative, artist_root.to_string_lossy()],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO content_group_locations
                     (group_id, relative_path, generation, manual_override, source_operation,
                      created_at, updated_at)
                 VALUES (?1, ?2, 1, 0, 'grouping', '', '')",
                params![group_id, relative],
            )
            .unwrap();
        }
        let work_id = crate::pawchive_pairing_write::work_of_post(&conn, 1)
            .unwrap()
            .unwrap()
            .0;

        // No pairing yet: the post has no location to inherit, and a work with
        // no group is a *new* work, so the shared render rules still apply.
        assert_eq!(
            verified_location_for_post(&conn, 1, &roots).unwrap(),
            VerifiedLocation::NoGroup
        );

        conn.execute(
            "INSERT INTO work_group_links (work_id, group_id, basis, created_at, revoked_at)
             VALUES (?1, 'grp-a', 'user', '', '')",
            params![work_id],
        )
        .unwrap();
        assert_eq!(
            verified_location_for_post(&conn, 1, &roots).unwrap(),
            VerifiedLocation::Known(artist_root.join("2026-09-14 A"))
        );

        // A revoked pairing is not a pairing.
        conn.execute(
            "UPDATE work_group_links SET revoked_at = 't' WHERE work_id = ?1",
            params![work_id],
        )
        .unwrap();
        assert_eq!(
            verified_location_for_post(&conn, 1, &roots).unwrap(),
            VerifiedLocation::NoGroup
        );

        // Two live groups is exactly the case the plan holds for confirmation:
        // picking either one would be a claim about content nobody made. The
        // outcome has to be distinguishable from "new work", because only this
        // one refuses to publish.
        conn.execute(
            "UPDATE work_group_links SET revoked_at = '' WHERE work_id = ?1",
            params![work_id],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO work_group_links (work_id, group_id, basis, shared, created_at, revoked_at)
             VALUES (?1, 'grp-b', 'user', 1, '', '')",
            params![work_id],
        )
        .unwrap();
        let scattered = verified_location_for_post(&conn, 1, &roots).unwrap();
        assert_eq!(scattered, VerifiedLocation::Ambiguous { groups: 2 });
        assert_eq!(scattered.clone().path(), None);
        assert!(
            scattered
                .ambiguous_reason()
                .is_some_and(|reason| reason.contains("location_ambiguous")),
            "a scattered work must name the refusal, not just return nothing"
        );
    }

    /// A tidy rename has to move the work's location with it, in the same
    /// transaction as the item paths.
    ///
    /// The counterexample: a user renames `2026-09-14 old` to
    /// `2026-09-14 renamed` through the UI. The move updates `items` and
    /// deletes the plan row, but if the group ledger keeps the old relative
    /// path then `verified_group_location` no longer names anywhere that
    /// exists — and the next attachment gets rendered into the pre-rename
    /// directory, which is what the plan forbids. Recording it afterwards is
    /// not equivalent: a crash in between commits the rename and leaves the
    /// mapping stale.
    #[test]
    fn a_tidy_rename_moves_the_group_location_in_the_same_transaction() {
        use crate::media_roots::MediaRoots;

        let dir = tempfile::tempdir().unwrap();
        let artist_root = dir.path().join("ArtistA");
        let old_dir = artist_root.join("2026-09-14 old");
        let new_dir = artist_root.join("2026-09-14 renamed");
        std::fs::create_dir_all(&old_dir).unwrap();
        std::fs::create_dir_all(&new_dir).unwrap();
        let roots = MediaRoots::identical(
            vec![dir.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        let conn = fixture();
        ensure_content_group_schema(&conn).unwrap();
        let root_s = artist_root.to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO content_groups
                 (group_id, artist_scope_id, root_relative, artist_root, date, precision,
                  generation, state, created_at, updated_at)
             VALUES ('grp-a', 'artist:1', '2026-09-14 old', ?1, '2026-09-14', 'day',
                     3, 'active', '', '')",
            params![root_s],
        )
        .unwrap();
        for member in ["2026-09-14 old/a.png", "2026-09-14 old/sub/b.png"] {
            conn.execute(
                "INSERT INTO content_group_members
                     (group_id, relative_path, file_name, file_size, media_type, derived,
                      content_hash, state)
                 VALUES ('grp-a', ?1, 'x', 1, 'image', 0, '', 'active')",
                params![member],
            )
            .unwrap();
        }
        conn.execute(
            "INSERT INTO content_group_locations
                 (group_id, relative_path, generation, manual_override, source_operation,
                  created_at, updated_at)
             VALUES ('grp-a', '2026-09-14 old', 3, 0, 'grouping', '', '')",
            [],
        )
        .unwrap();

        let old_abs = old_dir.to_string_lossy().to_string();
        let new_abs = new_dir.to_string_lossy().to_string();

        {
            let tx = conn.unchecked_transaction().unwrap();
            let touched = relocate_groups_in_tx(
                &tx,
                &root_s,
                &[(old_abs.clone(), new_abs.clone())],
                "folder_rename",
            )
            .unwrap();
            assert_eq!(touched, 1, "the renamed group is the one that moved");
            tx.commit().unwrap();
        }

        let root_relative: String = conn
            .query_row(
                "SELECT root_relative FROM content_groups WHERE group_id = 'grp-a'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(root_relative, "2026-09-14 renamed");

        // Members follow the directory, including the ones in subdirectories.
        let members: Vec<String> = conn
            .prepare("SELECT relative_path FROM content_group_members WHERE group_id = 'grp-a' ORDER BY id")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<rusqlite::Result<Vec<_>>>()
            .unwrap();
        assert_eq!(
            members,
            vec![
                "2026-09-14 renamed/a.png".to_string(),
                "2026-09-14 renamed/sub/b.png".to_string()
            ]
        );

        // The whole point: the work is still findable at its new location.
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(new_dir.clone())
        );

        // Undoing the rename is a move too, and has to move it back.
        {
            let tx = conn.unchecked_transaction().unwrap();
            relocate_groups_in_tx(&tx, &root_s, &[(new_abs, old_abs)], "folder_rename_undo")
                .unwrap();
            tx.commit().unwrap();
        }
        assert_eq!(
            verified_group_location(&conn, "grp-a", &roots).unwrap(),
            Some(old_dir)
        );
    }

    /// A move on an install that never grouped anything is a no-op, not a
    /// reason to create the schema inside somebody else's transaction.
    #[test]
    fn a_tidy_rename_on_an_install_without_groups_changes_nothing() {
        let conn = Connection::open_in_memory().unwrap();
        let tx = conn.unchecked_transaction().unwrap();
        assert_eq!(
            relocate_groups_in_tx(
                &tx,
                "/artist",
                &[("/artist/a".to_string(), "/artist/b".to_string())],
                "folder_rename"
            )
            .unwrap(),
            0
        );
        tx.commit().unwrap();
    }

    // -----------------------------------------------------------------------
    // Index → groups. This is the reader the reconciliation has to be able to
    // call: the plan's entry point is the media index, not a directory walk.
    // -----------------------------------------------------------------------

    fn index_fixture() -> Connection {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
             CREATE TABLE items (
                 id INTEGER PRIMARY KEY AUTOINCREMENT,
                 artist_id INTEGER NOT NULL,
                 file_path TEXT UNIQUE NOT NULL,
                 file_name TEXT NOT NULL,
                 file_size INTEGER NOT NULL DEFAULT 0,
                 content_hash TEXT NOT NULL DEFAULT '',
                 media_type TEXT NOT NULL DEFAULT 'image',
                 missing INTEGER NOT NULL DEFAULT 0
             );",
        )
        .unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'ArtistA', '/pictures/ArtistA')",
            [],
        )
        .unwrap();
        conn
    }

    fn insert_item(conn: &Connection, path: &str, size: i64, missing: i64) {
        let name = path.rsplit('/').next().unwrap_or(path);
        conn.execute(
            "INSERT INTO items (artist_id, file_path, file_name, file_size, missing)
             VALUES (1, ?1, ?2, ?3, ?4)",
            rusqlite::params![path, name, size, missing],
        )
        .unwrap();
    }

    /// The builder reads one artist's index and produces the same grouping the
    /// pure algorithm does for the equivalent paths — including the two shapes
    /// that made it necessary: a work root under a year layer, and a zip beside
    /// the tree it extracted into.
    #[test]
    fn the_index_builder_groups_an_artists_files_the_way_the_plan_describes() {
        let conn = index_fixture();
        insert_item(&conn, "/pictures/ArtistA/2026/2026-09-14 A/1.png", 10, 0);
        insert_item(&conn, "/pictures/ArtistA/2026/2026-09-14 A/a.zip", 20, 0);
        insert_item(&conn, "/pictures/ArtistA/2026/2026-09-14 A/a/2.png", 11, 0);
        insert_item(&conn, "/pictures/ArtistA/2026-09-14 B/1.png", 12, 0);

        let result = group_index_entries(&conn, 1).unwrap();
        assert_eq!(result.groups.len(), 2, "{:?}", result.groups);
        let a = result
            .groups
            .iter()
            .find(|group| group.root_relative == "2026/2026-09-14 A")
            .expect("the work root is under the year layer");
        assert_eq!(a.date, "2026-09-14");
        assert_eq!(a.member_count, 3, "the zip and its tree are one group");
        let b = result
            .groups
            .iter()
            .find(|group| group.root_relative == "2026-09-14 B")
            .expect("the second work");
        assert_eq!(b.member_count, 1);
    }

    /// A path outside the artist root is not part of that artist's grouping,
    /// and a sibling whose name merely starts with the root's name is outside
    /// it too.
    #[test]
    fn files_outside_the_artist_root_are_not_grouped_into_it() {
        assert_eq!(
            relative_under_root("/pictures/ArtistA", "/pictures/ArtistA/2026-09-14 A/1.png")
                .as_deref(),
            Some("2026-09-14 A/1.png")
        );
        assert_eq!(
            relative_under_root("/pictures/ArtistA", "/pictures/ArtistA").as_deref(),
            Some("")
        );
        assert_eq!(
            relative_under_root("/pictures/ArtistA", "/pictures/ArtistA-old/1.png"),
            None
        );
        assert_eq!(
            relative_under_root("/pictures/ArtistA", "/pictures/ArtistB/1.png"),
            None
        );

        let conn = index_fixture();
        insert_item(&conn, "/pictures/ArtistA/2026-09-14 A/1.png", 10, 0);
        insert_item(&conn, "/pictures/ArtistA-old/2026-09-14 A/1.png", 10, 0);
        let result = group_index_entries(&conn, 1).unwrap();
        assert_eq!(result.groups.len(), 1);
        assert_eq!(result.groups[0].member_count, 1);
    }

    /// Missing rows, this application's own text, and staging artifacts never
    /// become groups.
    #[test]
    fn the_builder_skips_missing_rows_derived_text_and_transient_files() {
        let conn = index_fixture();
        insert_item(
            &conn,
            "/pictures/ArtistA/2026-09-14 A/content 900.txt",
            120,
            0,
        );
        insert_item(
            &conn,
            "/pictures/ArtistA/2026-09-14 A/links A 900.txt",
            90,
            0,
        );
        insert_item(
            &conn,
            "/pictures/ArtistA/2026-09-14 A/.gallery_pawchive_a.part",
            4096,
            0,
        );
        insert_item(&conn, "/pictures/ArtistA/2026-09-14 gone/1.png", 10, 1);

        let result = group_index_entries(&conn, 1).unwrap();
        assert!(result.groups.is_empty(), "{:?}", result.groups);
        assert!(
            result.empty_roots.iter().any(|root| root == "2026-09-14 A"),
            "the text-only root is reported as empty: {:?}",
            result.empty_roots
        );
        assert!(
            !result
                .empty_roots
                .iter()
                .any(|root| root == "2026-09-14 gone"),
            "a missing row is not a group at all, so there is nothing to report"
        );
    }

    /// The builder only reads: a preview must not create the ledger it is
    /// previewing.
    #[test]
    fn the_builder_writes_nothing_until_the_grouping_is_applied() {
        let conn = index_fixture();
        insert_item(&conn, "/pictures/ArtistA/2026-09-14 A/1.png", 10, 0);

        let result = group_index_entries(&conn, 1).unwrap();
        assert_eq!(result.groups.len(), 1);
        // The ledger tables do not exist on this database yet, which is the
        // strongest possible statement that the read wrote nothing.
        let exists: Option<String> = conn
            .query_row(
                "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'content_groups'",
                [],
                |row| row.get(0),
            )
            .optional()
            .unwrap();
        assert!(exists.is_none(), "the reader created a ledger table");

        // An unknown artist is an error, not an empty grouping.
        assert!(group_index_entries(&conn, 999).is_err());
    }
}
