//! The shared work-naming contract and the old-rule migration preview.
//!
//! Backend plan §6.2 / reconciliation plan §7.2–7.4 settle two facts that this
//! module exists to make structural rather than a matter of discipline:
//!
//! 1. **One input, several renderings.** `WorkNamingContext` is the single
//!    input download and organize both render from. A rendering may therefore
//!    differ only because the *semantics version* differs, never because the
//!    two sides read different facts about the same work.
//! 2. **Identity is not a path.** The three semantic versions keep the
//!    historical meanings apart. `SemanticVersion::LegacyLocal` is the local
//!    organize meaning — `{id}` was the plan/media id, `{userID}` the local
//!    artist id. `SemanticVersion::LegacyDownload` is the download meaning —
//!    `{id}` is the remote post id. `SemanticVersion::WorkV2` is the migrated
//!    meaning — remote tokens carry remote values only, and a remote value that
//!    does not exist is an empty value or a dropped token, **never** a local
//!    id standing in for one (`plan §6.2`: "普通本地作品不伪造远端 ID").
//!
//! The renderer is deliberately pure: no database, no filesystem, no clock.
//! `plan_naming_migration` is the only database reader here and it only reads.

use anyhow::{Context, Result};
use rusqlite::{Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::pawchive::{ensure_pawchive_schema, get_pawchive_settings, sanitize_path_component};
use crate::pawchive_groups::DatePrecision;

/// Default empty value when a caller does not configure one.
///
/// It matches `archive_format`'s profile default so a migration preview of the
/// organize rule shows the value the organize side would really render.
pub const DEFAULT_EMPTY_VALUE: &str = "untitled";

// ---------------------------------------------------------------------------
// The shared input
// ---------------------------------------------------------------------------

/// Every fact a naming rendering may consume, for one work (or one group).
///
/// Remote identity is four raw strings and stays text: the plan forbids
/// converting远端 IDs to integers, so no field here is numeric-typed.
///
/// `local_artist_id` / `local_plan_id` / `local_media_id` exist because the
/// organize side's `{id}` / `{userID}` historically meant those local rows, and
/// because the plan forbids inventing remote identity out of them. A caller
/// that has only local rows leaves the remote fields empty; the renderer then
/// answers with the configured empty value instead of a plausible-looking lie.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkNamingContext {
    // --- remote identity (raw strings) ---
    pub site: String,
    pub service: String,
    pub creator_id: String,
    pub post_id: String,

    // --- display facts ---
    /// Artist display name (what `{user}` / `{artist}` mean).
    pub artist: String,
    pub title: String,
    pub tags: Vec<String>,

    // --- dates ---
    /// Canonical `YYYY-MM-DD`, `YYYY-MM`, or `YYYY`; empty when unknown.
    pub date: String,
    pub date_precision: DatePrecision,
    /// Where the effective day came from (`published`, `group_override`, ...).
    pub date_basis: String,
    /// The raw published string, kept verbatim next to its normalization.
    pub raw_published: String,
    /// Timezone basis of `raw_published` (`+09:00`, `Z`, `unknown`, ...).
    pub raw_published_timezone_basis: String,
    /// A confirmed group-level manual date override. Wins over `date`, and is
    /// reported as its own basis so a preview cannot pass it off as published.
    pub manual_date_override: String,
    /// Real minute of day as `HH:MM`, only when the source carries one.
    pub time: Option<String>,
    /// Display format snapshot for this rendering (`iso`, `compact`,
    /// `year_month`, `iso_minute`).
    pub date_format: String,

    // --- original context snapshots ---
    /// The original folder value — never a previously rendered target. This is
    /// what `{folder}` renders, so `{folder}/{folder}` cannot stack prefixes.
    pub original_folder: String,
    /// The original name value for `{name}`.
    pub original_name: String,
    /// One-based position of this file inside the work; `None` when the work
    /// has no meaningful index.
    pub index: Option<usize>,
    /// File extension without the dot.
    pub ext: Option<String>,
    /// The task's own date, for `{task_date}`. Never the work's publication.
    pub task_date: Option<String>,

    // --- local ids the organize side uses ---
    pub local_artist_id: Option<i64>,
    pub local_plan_id: Option<i64>,
    pub local_media_id: Option<i64>,
}

impl Default for WorkNamingContext {
    /// An empty context. Its precision is `Unknown`, not `Day`: a context that
    /// carries no date must not claim day precision.
    fn default() -> Self {
        Self {
            site: String::new(),
            service: String::new(),
            creator_id: String::new(),
            post_id: String::new(),
            artist: String::new(),
            title: String::new(),
            tags: Vec::new(),
            date: String::new(),
            date_precision: DatePrecision::Unknown,
            date_basis: String::new(),
            raw_published: String::new(),
            raw_published_timezone_basis: String::new(),
            manual_date_override: String::new(),
            time: None,
            date_format: "iso".to_string(),
            original_folder: String::new(),
            original_name: String::new(),
            index: None,
            ext: None,
            task_date: None,
            local_artist_id: None,
            local_plan_id: None,
            local_media_id: None,
        }
    }
}

impl WorkNamingContext {
    /// The date a rendering should display.
    ///
    /// A confirmed group override wins; otherwise the normalized date. This is
    /// one resolution point so download and organize cannot disagree about
    /// which day a work is filed under.
    pub fn effective_date(&self) -> &str {
        let override_value = self.manual_date_override.trim();
        if override_value.is_empty() {
            self.date.trim()
        } else {
            override_value
        }
    }

    /// Which source the effective date came from.
    pub fn effective_date_basis(&self) -> &str {
        if self.manual_date_override.trim().is_empty() {
            let basis = self.date_basis.trim();
            if basis.is_empty() {
                "unknown"
            } else {
                basis
            }
        } else {
            "manual_override"
        }
    }

    /// The local value `{id}` / `{userID}` had under legacy local semantics.
    fn legacy_local_id(&self) -> Option<(&'static str, i64)> {
        self.local_plan_id
            .map(|id| ("local_plan_id", id))
            .or_else(|| self.local_media_id.map(|id| ("local_media_id", id)))
            .or_else(|| self.local_artist_id.map(|id| ("local_artist_id", id)))
    }
}

// ---------------------------------------------------------------------------
// Templates and semantic versions
// ---------------------------------------------------------------------------

/// The three templates one rule set renders with.
///
/// The organize side stores a single folder template plus profile parameters;
/// the download side stores folder/image/attachment templates. Both are read
/// into this shape so a preview compares like with like. `{}` on any field —
/// and `None` — means "no template configured for this purpose", which renders
/// empty rather than guessing the other side's text.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct NamingTemplateSet {
    pub folder: String,
    pub image: Option<String>,
    pub attachment: Option<String>,
    /// The current attachment's raw name, which is what `{name}` means inside
    /// the *file* templates. The folder template's `{name}` keeps its
    /// cross-side meaning: the `original_name` snapshot.
    pub attachment_name: Option<String>,
    /// The empty value a known token with nothing to say is replaced with.
    /// The organize side reads it from the profile (`empty_value`), so the
    /// preview must carry it or it renders a name the execution never produces.
    /// `None` keeps the documented default ([`DEFAULT_EMPTY_VALUE`]).
    pub empty_value: Option<String>,
}

impl NamingTemplateSet {
    pub fn folder_only(folder: impl Into<String>) -> Self {
        Self {
            folder: folder.into(),
            ..Self::default()
        }
    }
}

/// Which historical meaning a rendering is produced under.
///
/// The variants are ordered by how much they claim: `LegacyLocal` claims the
/// least (no remote identity at all), which is why an unknown stored value
/// parses to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SemanticVersion {
    /// Organize-side history: `{id}` = plan/media id, `{userID}` = local artist id.
    LegacyLocal,
    /// Download-side history: `{user}` = remote creator id, `{id}` = remote post id.
    LegacyDownload,
    /// Migrated shared rule: remote tokens only carry remote values.
    WorkV2,
}

impl SemanticVersion {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::LegacyLocal => "legacy_local",
            Self::LegacyDownload => "legacy_download",
            Self::WorkV2 => "work_v2",
        }
    }

    /// Unknown text parses to `LegacyLocal`: the answer that claims least.
    ///
    /// Guessing `WorkV2` from an unreadable value would put remote meaning on a
    /// template whose real meaning nobody recorded.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "legacy_local" => Self::LegacyLocal,
            "legacy_download" => Self::LegacyDownload,
            "work_v2" => Self::WorkV2,
            _ => Self::LegacyLocal,
        }
    }
}

impl std::fmt::Display for SemanticVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Render output
// ---------------------------------------------------------------------------

/// One substituted token, with where its value came from.
///
/// `value` is the value as it entered the path. When sanitization changed it,
/// `source` carries a `.sanitized` marker and the recorded value is the raw
/// one, so "what changed" is readable from the record rather than inferred.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedToken {
    pub token: String,
    pub value: String,
    pub source: String,
    /// Set when sanitization turned the raw value into the rendered one.
    #[serde(default)]
    pub value_sanitized: bool,
}

/// The result of one rendering.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RenderedNaming {
    pub folder_relative: String,
    pub image_filename: String,
    pub attachment_filename: String,
    /// Every token that reached a value, in template order.
    pub tokens: Vec<RenderedToken>,
    /// Everything the renderer had to decide or could not keep verbatim.
    pub warnings: Vec<String>,
    /// True when sanitization or empty-component dropping changed the output.
    pub lossy: bool,
}

/// Effective date split by the precision that is actually known.
struct DateParts {
    date: String,
    year: String,
    month: String,
    /// True only when a real minute is present; `{time}` hangs off this.
    has_minute: bool,
}

fn parse_compact_date(
    raw: &str,
    expected: DatePrecision,
) -> Option<(u32, Option<u32>, Option<u32>)> {
    let raw = raw.trim();
    if raw.is_empty() {
        return None;
    }
    match expected {
        DatePrecision::Day => {
            let bytes = raw.as_bytes();
            if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
                return None;
            }
            let year = raw.get(0..4)?.parse::<u32>().ok()?;
            let month = raw.get(5..7)?.parse::<u32>().ok()?;
            let day = raw.get(8..10)?.parse::<u32>().ok()?;
            // Calendar-valid, not merely in range: 1..=31 accepted `2026-02-31`
            // and rendered it into a folder name that the crate's chrono-based
            // parsers (archive_format::parse_date) reject for the same work.
            let _ = chrono::NaiveDate::from_ymd_opt(year as i32, month, day)?;
            Some((year, Some(month), Some(day)))
        }
        DatePrecision::Month => {
            let (year, month) = raw.split_once('-')?;
            if year.len() != 4 || month.len() != 2 {
                return None;
            }
            let year = year.parse::<u32>().ok()?;
            let month = month.parse::<u32>().ok()?;
            let _ = chrono::NaiveDate::from_ymd_opt(year as i32, month, 1)?;
            Some((year, Some(month), None))
        }
        DatePrecision::Year => {
            if raw.len() != 4 {
                return None;
            }
            Some((raw.parse::<u32>().ok()?, None, None))
        }
        // An unknown precision cannot be padded into a finer one, and it cannot
        // index `{year}`/`{month}` either: `2026` alone must not be read as a
        // year when nobody recorded that it is one.
        DatePrecision::Unknown => None,
    }
}

fn time_text(ctx: &WorkNamingContext, has_minute: bool) -> Option<String> {
    if !has_minute {
        return None;
    }
    let raw = ctx.time.as_deref()?.trim();
    let (hour, minute) = raw.split_once('-').or_else(|| raw.split_once(':'))?;
    if hour.len() != 2 || minute.len() < 2 {
        return None;
    }
    if !hour.chars().all(|ch| ch.is_ascii_digit()) || !minute.chars().all(|ch| ch.is_ascii_digit())
    {
        return None;
    }
    let minute_text = minute.get(..2)?;
    // A wall-clock time is only a time inside its range: `99:99` used to render
    // as `99-99` and land in a folder name.
    let hour_value: u32 = hour.parse().ok()?;
    let minute_value: u32 = minute_text.parse().ok()?;
    if hour_value > 23 || minute_value > 59 {
        return None;
    }
    Some(format!("{hour}-{minute_text}"))
}

fn date_parts(ctx: &WorkNamingContext) -> DateParts {
    let effective = ctx.effective_date();
    let parsed = parse_compact_date(effective, ctx.date_precision);
    let (year, month, day) = match parsed {
        Some((year, month, day)) => (year, month, day),
        None => {
            return DateParts {
                date: String::new(),
                year: String::new(),
                month: String::new(),
                has_minute: false,
            }
        }
    };
    // `{time}` and `iso_minute` need a real minute *and* a real day; a month
    // reading is never padded to `00-00` (reconciliation §7.3.4).
    let has_day = day.is_some();
    let time = if has_day {
        time_text(ctx, has_day)
    } else {
        None
    };
    let has_minute = time.is_some();
    let date = match (ctx.date_format.trim(), month, day) {
        ("compact", Some(month), Some(day)) => format!("{year}{month:02}{day:02}"),
        ("compact", Some(month), None) => format!("{year}{month:02}"),
        ("compact", None, _) => format!("{year}"),
        ("year_month", Some(month), _) => format!("{year}-{month:02}"),
        ("year_month", None, _) => format!("{year}"),
        // `iso` and `iso_minute` differ only by the minute suffix below.
        (_, Some(month), Some(day)) => format!("{year}-{month:02}-{day:02}"),
        (_, Some(month), None) => format!("{year}-{month:02}"),
        (_, None, _) => format!("{year}"),
    };
    let date = match (&time, ctx.date_format.trim()) {
        (Some(time), "iso_minute") => format!("{date} {time}"),
        _ => date,
    };
    DateParts {
        date,
        year: year.to_string(),
        month: month.map(|month| format!("{month:02}")).unwrap_or_default(),
        has_minute,
    }
}

/// Sanitize one substituted value, reporting whether anything changed.
fn sanitize_value(raw: &str) -> (String, bool) {
    let sanitized = sanitize_path_component(raw);
    let changed = sanitized != raw.trim();
    (sanitized, changed)
}

/// Final component rules that `sanitize_path_component` does not cover: an
/// empty component and trailing dots/spaces are not portable, so they are
/// repaired here and reported as lossy. Dots *inside* a name (`.zip`, `.tar.gz`)
/// are untouched — only a trailing run is a problem.
fn finalize_component(component: &str, warnings: &mut Vec<String>, lossy: &mut bool) -> String {
    let trimmed = sanitize_path_component(component);
    if trimmed.is_empty() {
        if !component.trim().is_empty() {
            *lossy = true;
            warnings.push(format!(
                "empty component after sanitization: {:?} was dropped",
                component
            ));
        }
        return String::new();
    }
    if trimmed != component {
        *lossy = true;
        warnings.push(format!(
            "component {component:?} needed trailing dot/space removal -> {trimmed:?}"
        ));
    }
    trimmed
}

struct Renderer<'a> {
    ctx: &'a WorkNamingContext,
    templates: &'a NamingTemplateSet,
    semantics: SemanticVersion,
    tokens: Vec<RenderedToken>,
    warnings: Vec<String>,
    lossy: bool,
}

impl<'a> Renderer<'a> {
    fn new(
        ctx: &'a WorkNamingContext,
        templates: &'a NamingTemplateSet,
        semantics: SemanticVersion,
    ) -> Self {
        Self {
            ctx,
            templates,
            semantics,
            tokens: Vec::new(),
            warnings: Vec::new(),
            lossy: false,
        }
    }

    fn empty_value(&self) -> &str {
        self.templates
            .empty_value
            .as_deref()
            .unwrap_or(DEFAULT_EMPTY_VALUE)
    }

    /// Record what a token was substituted with. `raw` is the value as the
    /// context stored it; `sanitized` says whether the value that reached the
    /// path is a repaired version of it, which is marked in `source` so a
    /// migration dialog can show the difference.
    fn record(&mut self, token: &str, raw: &str, sanitized: bool, source: &str) {
        if sanitized {
            self.lossy = true;
        }
        self.tokens.push(RenderedToken {
            token: format!("{{{token}}}"),
            value: raw.to_string(),
            source: if sanitized {
                format!("{source}.sanitized")
            } else {
                source.to_string()
            },
            value_sanitized: sanitized,
        });
    }

    /// Resolve one token name to its sanitized text, recording the raw value it
    /// came from. `None` means the token has no value at all (unknown name, or
    /// a known token with nothing to say) — an empty substitution is never
    /// recorded as if it had been filled.
    fn resolve(&mut self, token: &str, dates: &DateParts) -> Option<String> {
        // A remote value that does not exist is answered with the empty value,
        // never with a local id. `plan §6.2`.
        let remote = |value: &str| {
            let value = value.trim();
            if value.is_empty() {
                None
            } else {
                Some(value.to_string())
            }
        };
        let (raw, source) = match token {
            "user" | "artist" => (
                self.ctx.artist.clone(),
                if token == "artist" {
                    "context.artist"
                } else {
                    "context.artist(display)"
                },
            ),
            "title" => (self.ctx.title.clone(), "context.title"),
            "tags" => (self.ctx.tags.join("&"), "context.tags"),
            "site" => (self.ctx.site.clone(), "context.site"),
            "service" => (self.ctx.service.clone(), "context.service"),
            "creator" | "creator_id" => (self.ctx.creator_id.clone(), "context.creator_id"),
            "date" => (dates.date.clone(), "context.effective_date"),
            "year" => (dates.year.clone(), "context.effective_date.year"),
            "month" => (dates.month.clone(), "context.effective_date.month"),
            "folder" => (self.ctx.original_folder.clone(), "context.original_folder"),
            "index" => (
                self.ctx
                    .index
                    .map(|index| index.to_string())
                    .unwrap_or_default(),
                "context.index",
            ),
            "ext" => (
                self.ctx
                    .ext
                    .as_deref()
                    .map(|ext| ext.trim_start_matches('.').to_string())
                    .unwrap_or_default(),
                "context.ext",
            ),
            "task_date" => (
                self.ctx.task_date.clone().unwrap_or_default(),
                "context.task_date",
            ),
            "time" => (String::new(), ""),
            "id" => (String::new(), ""),
            "userID" | "user_id" => (String::new(), ""),
            "name" => (String::new(), ""),
            _ => return None,
        };
        // `{time}`, `{id}`/`{userID}`, and `{name}` need their own resolution
        // because their meaning depends on the semantics version or on the
        // file template being rendered.
        match token {
            "time" => {
                let time = time_text(self.ctx, dates.has_minute)?;
                self.record(token, &time, false, "context.time");
                return Some(time);
            }
            "id" | "userID" | "user_id" => {
                let (value, source) = match (self.semantics, token) {
                    (SemanticVersion::LegacyLocal, _) => match self.ctx.legacy_local_id() {
                        Some((local_source, id)) => (id.to_string(), local_source.to_string()),
                        None => return None,
                    },
                    (SemanticVersion::LegacyDownload | SemanticVersion::WorkV2, "id") => {
                        (remote(&self.ctx.post_id)?, "context.post_id".to_string())
                    }
                    (SemanticVersion::LegacyDownload | SemanticVersion::WorkV2, _) => (
                        remote(&self.ctx.creator_id)?,
                        "context.creator_id".to_string(),
                    ),
                };
                let (sanitized, changed) = sanitize_value(&value);
                if sanitized.is_empty() {
                    self.warnings.push(format!(
                        "{{{token}}} ({source}) sanitized to an empty value and was dropped"
                    ));
                    self.lossy = true;
                    return None;
                }
                self.record(token, &value, changed, &source);
                return Some(sanitized);
            }
            "name" => {
                let raw_name = self
                    .templates
                    .attachment_name
                    .as_deref()
                    .map(str::trim)
                    .filter(|name| !name.is_empty())
                    .or_else(|| {
                        let original = self.ctx.original_name.trim();
                        if original.is_empty() {
                            None
                        } else {
                            Some(original)
                        }
                    })?;
                let (sanitized, changed) = sanitize_value(raw_name);
                if sanitized.is_empty() {
                    self.warnings
                        .push("{name} sanitized to an empty value and was dropped".to_string());
                    self.lossy = true;
                    return None;
                }
                let source = if self.templates.attachment_name.is_some() {
                    "templates.attachment_name"
                } else {
                    "context.original_name"
                };
                self.record(token, raw_name, changed, source);
                return Some(sanitized);
            }
            _ => {}
        }

        let raw = raw.trim();
        // A value that is already empty never reached the path, so it is not
        // recorded as a substitution.
        if raw.is_empty() {
            return None;
        }
        // `sanitize_path_component` turns a traversal run into nothing, but the
        // check still happens before sanitization too, so a `{title}` of `..`
        // is reported as traversal rather than as "sanitized to empty".
        if raw == "." || raw == ".." {
            self.lossy = true;
            self.warnings.push(format!(
                "{{{token}}} ({source}) was a traversal component and was dropped"
            ));
            self.record(token, raw, false, source);
            return None;
        }
        let (sanitized, changed) = sanitize_value(raw);
        if sanitized.is_empty() {
            self.lossy = true;
            self.warnings.push(format!(
                "{{{token}}} ({source}) sanitized to an empty value and was dropped"
            ));
            self.record(token, raw, changed, source);
            return None;
        }
        self.record(token, raw, changed, source);
        Some(sanitized)
    }

    /// Parse and substitute one template. Returns the filled text (still
    /// unsplit, so path components can be repaired together).
    fn fill(&mut self, template: &str, dates: &DateParts) -> String {
        let mut out = String::with_capacity(template.len());
        let mut rest = template;
        while let Some(start) = rest.find('{') {
            out.push_str(&rest[..start]);
            let after = &rest[start + 1..];
            let Some(end) = after.find('}') else {
                // An unmatched `{` is not a token; keep the text and say so.
                self.warnings.push(format!(
                    "unmatched '{{' in template {template:?} was kept as literal text"
                ));
                out.push_str(&rest[start..]);
                rest = "";
                break;
            };
            let token = &after[..end];
            let close = start + 1 + end + 1;
            if token.is_empty() {
                self.warnings.push(format!(
                    "empty {{{}}} in template {template:?} was kept as literal text",
                    ""
                ));
                out.push_str(&rest[start..close]);
                rest = &rest[close..];
                continue;
            }
            match self.resolve(token, dates) {
                Some(text) => out.push_str(&text),
                None if is_known_token(token) => {
                    // A known token with no value: the configured empty value
                    // when there is one, otherwise the token disappears. Its
                    // separators are collapsed below.
                    out.push_str(self.empty_value());
                }
                None => {
                    // Unknown token names are reported, not silently deleted
                    // from the path (reconciliation §7.6). The braces come off
                    // and separators are sanitized so the output stays usable,
                    // but the name stays visible in the rendered path.
                    self.warnings.push(format!(
                        "unknown token {{{token}}} in template {template:?} was kept as literal text"
                    ));
                    out.push_str(&sanitize_path_component(token));
                    self.lossy = true;
                }
            }
            rest = &rest[close..];
        }
        out.push_str(rest);
        out
    }

    fn render_folder(&mut self, dates: &DateParts) -> String {
        let filled = self.fill(&self.templates.folder, dates);
        let mut components = Vec::new();
        // Both separators split a folder template: a literal `\\` in the
        // template is a path separator for the organize execution path
        // (archive_format::validate_rendered_target folds it to `/`), so the
        // preview must not flatten it into one component with `_`.
        for component in filled.split(['/', '\\']) {
            let component = component.split_whitespace().collect::<Vec<_>>().join(" ");
            if component.is_empty() {
                continue;
            }
            let component = finalize_component(&component, &mut self.warnings, &mut self.lossy);
            if component.is_empty() {
                continue;
            }
            if component == "." || component == ".." {
                self.lossy = true;
                self.warnings.push(format!(
                    "traversal component {component:?} was dropped from the folder path"
                ));
                continue;
            }
            components.push(component);
        }
        components.join("/")
    }

    fn render_file(
        &mut self,
        template: Option<&str>,
        dates: &DateParts,
        append_ext: bool,
    ) -> String {
        let Some(template) = template.map(str::trim).filter(|text| !text.is_empty()) else {
            return String::new();
        };
        let filled = self.fill(template, dates);
        let whitespace_collapsed = filled.split_whitespace().collect::<Vec<_>>().join(" ");
        if whitespace_collapsed != filled {
            self.lossy = true;
            self.warnings.push(format!(
                "filename whitespace was collapsed: {filled:?} -> {whitespace_collapsed:?}"
            ));
        }
        let cleaned = whitespace_collapsed.replace(['/', '\\'], "_");
        if cleaned != whitespace_collapsed {
            self.lossy = true;
            self.warnings.push(format!(
                "path separator removed from filename: {whitespace_collapsed:?} -> {cleaned:?}"
            ));
        }
        let mut out = finalize_component(&cleaned, &mut self.warnings, &mut self.lossy);
        if out.is_empty() {
            return out;
        }
        if append_ext {
            // The file's own extension is appended unless the rendered name
            // already ends with it — the same test the download execution path
            // uses (pawchive_url_naming). A dot in the middle of a name is not
            // an extension: `{title}` with title `Vol.2` used to render `Vol.2`
            // with no extension at all, while execution produced `Vol.2.jpg`.
            if let Some(ext) = self
                .ctx
                .ext
                .as_deref()
                .map(|ext| ext.trim_start_matches('.').trim())
                .filter(|ext| !ext.is_empty())
            {
                let already_named = out
                    .to_lowercase()
                    .ends_with(&format!(".{}", ext.to_lowercase()));
                if !already_named {
                    out.push_str(&format!(".{ext}"));
                }
            }
        }
        finalize_component(&out, &mut self.warnings, &mut self.lossy)
    }
}

/// Whether a token name is one this contract knows.
pub fn is_known_token(token: &str) -> bool {
    matches!(
        token,
        "user"
            | "artist"
            | "userID"
            | "user_id"
            | "id"
            | "title"
            | "tags"
            | "name"
            | "folder"
            | "index"
            | "ext"
            | "date"
            | "year"
            | "month"
            | "time"
            | "site"
            | "service"
            | "creator"
            | "creator_id"
            | "task_date"
    )
}

/// Render one context under one semantic version with one template set.
///
/// The contract, in the order the tests pin it:
///
/// - No token is mandatory. A template containing none of them renders fine.
/// - `{folder}` / `{name}` read the *original* snapshots, so `{folder}/{folder}`
///   cannot double a folder prefix.
/// - `{time}` and `iso_minute` output only with a real minute; day precision
///   falls back to the empty value rather than `00-00`.
/// - Unknown `{token}` names warn and stay visible in the path.
/// - Any sanitization that changes the output sets `lossy` and is recorded.
/// - `WorkV2` answers a missing remote value with the empty value; it never
///   substitutes a local id into a remote token.
/// - `LegacyLocal` resolves `{id}`/`{userID}` to the local plan/media/artist id
///   and says so in the token source; it never claims they are remote.
/// - `LegacyDownload` resolves `{user}` to the remote creator id and `{id}` to
///   the remote post id.
pub fn render(
    context: &WorkNamingContext,
    templates: &NamingTemplateSet,
    semantics: SemanticVersion,
) -> RenderedNaming {
    let dates = date_parts(context);
    let mut renderer = Renderer::new(context, templates, semantics);
    let folder_relative = renderer.render_folder(&dates);
    // Auto-extension belongs to images only: the image index names a file
    // without its extension, while an attachment template names an existing
    // file (`{name}` already ends in one), so appending would produce
    // `set-a.zip.jpg` for a file that is already named.
    let image_filename = renderer.render_file(templates.image.as_deref(), &dates, true);
    let attachment_filename = renderer.render_file(templates.attachment.as_deref(), &dates, false);
    if folder_relative.is_empty() {
        renderer
            .warnings
            .push("the folder rendering is empty".to_string());
    }
    RenderedNaming {
        folder_relative,
        image_filename,
        attachment_filename,
        tokens: renderer.tokens,
        warnings: renderer.warnings,
        lossy: renderer.lossy,
    }
}

// ---------------------------------------------------------------------------
// Migration preview
// ---------------------------------------------------------------------------

/// The four concrete renderings the plan requires for one work, plus the
/// conflicts between the rules they came from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamingMigrationPreview {
    pub work_key: i64,
    #[serde(default)]
    pub revision: i64,
    pub previews: Vec<NamingMigrationPreviewRow>,
    pub conflicts: Vec<NamingMigrationConflict>,
    /// Non-fatal notes about how the sources were read (an absent download
    /// template, an unreadable organize profile, ...).
    pub notes: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamingMigrationPreviewRow {
    pub label: String,
    pub templates: NamingTemplateSet,
    pub semantics: SemanticVersion,
    pub rendered: RenderedNaming,
}

/// Why two stored rule sources cannot be merged silently. The set is closed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum NamingMigrationConflictReason {
    /// The two templates are different text.
    TemplateTextDiffers,
    /// The same template text means two different things (reconciliation §7.2:
    /// a silent merge would change one side's output).
    SameTextDifferentSemantics,
    /// A version claim is missing or unreadable, so nothing can be decided.
    SemanticsUndecidable,
}

impl NamingMigrationConflictReason {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TemplateTextDiffers => "template_text_differs",
            Self::SameTextDifferentSemantics => "same_text_different_semantics",
            Self::SemanticsUndecidable => "semantics_undecidable",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamingMigrationConflict {
    pub reason: NamingMigrationConflictReason,
    pub left_label: String,
    pub right_label: String,
    /// What exactly differs, in words, for a migration dialog.
    pub detail: String,
}

impl NamingMigrationConflict {
    /// Stable machine-readable code, for API clients.
    pub fn reason_code(&self) -> &'static str {
        self.reason.as_str()
    }
}

/// The organize side's effective rule, as stored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OrganizeRuleSnapshot {
    pub folder_template: String,
    pub empty_value: String,
    pub date_format: String,
    pub tag_separator: String,
    pub profile_source: Option<String>,
}

/// Read the organize rule from the crate's `archive_profiles` reader.
///
/// `folder_rename_format_settings` is the only public reader for
/// `ARCHIVE_FORMAT_SETTINGS_KEY`, so the shared resolver consumes it instead of
/// re-reading `app_settings` with a second, drifting implementation.
pub fn read_organize_rule(
    conn: &Connection,
    artist_id: Option<i64>,
) -> Result<OrganizeRuleSnapshot> {
    let response = crate::folder_rename_format_settings(conn, artist_id)?;
    organize_rule_from_response(&response)
}

fn organize_rule_from_response(response: &Value) -> Result<OrganizeRuleSnapshot> {
    let profile = response
        .get("effective_profile")
        .ok_or_else(|| anyhow::anyhow!("archive format settings have no effective profile"))?;
    let folder_template = profile
        .get("template")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let empty_value = profile
        .get("empty_value")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_EMPTY_VALUE)
        .to_string();
    let date_format = profile
        .get("date_format")
        .and_then(Value::as_str)
        .unwrap_or("iso")
        .to_string();
    let tag_separator = profile
        .get("tag_separator")
        .and_then(Value::as_str)
        .unwrap_or("&")
        .to_string();
    let profile_source = response
        .get("profile_source")
        .and_then(Value::as_str)
        .map(str::to_string);
    Ok(OrganizeRuleSnapshot {
        folder_template,
        empty_value,
        date_format,
        tag_separator,
        profile_source,
    })
}

/// Describe one stored rule source and the semantic version its text means.
///
/// A rule whose stored text is empty cannot be worked out from text alone, so
/// the version claim is left empty and the caller reports
/// `SemanticsUndecidable` rather than assuming the new semantics.
fn row(
    label: &str,
    templates: NamingTemplateSet,
    semantics: Option<SemanticVersion>,
    semantics_text: &str,
    context: &WorkNamingContext,
    notes: &mut Vec<String>,
) -> NamingMigrationPreviewRow {
    let resolved = semantics.unwrap_or(SemanticVersion::LegacyLocal);
    if semantics.is_none() {
        notes.push(format!(
            "{label}: stored semantic version {semantics_text:?} is unreadable or absent; \
             read as legacy local so nothing is claimed"
        ));
    }
    let rendered = render(context, &templates, resolved);
    NamingMigrationPreviewRow {
        label: label.to_string(),
        templates,
        semantics: resolved,
        rendered,
    }
}

/// Build the four renderings and the conflicts between the stored rule sources
/// for one work. Read-only: it never writes settings, tasks, or media.
///
/// The four rows are, in order:
/// 1. legacy download template under legacy download semantics,
/// 2. the new shared template under `work_v2`,
/// 3. the legacy organize template under legacy local semantics,
/// 4. the new organize template under `work_v2`.
///
/// Rows 3 and 4 deliberately show the same organize text under two versions:
/// that is the migration question ("what does this rule become once the
/// meaning changes"), and it is what makes a same-text/different-semantics
/// conflict visible instead of silent.
pub fn plan_naming_migration(
    conn: &Connection,
    work_key: i64,
    context: &WorkNamingContext,
    artist_id: Option<i64>,
) -> Result<NamingMigrationPreview> {
    let mut notes = Vec::new();
    let organize = read_organize_rule(conn, artist_id)?;
    let download = get_pawchive_settings(conn)?;

    // The preview renders the empty value the organize profile is configured
    // with, so a row that shows `untitled` while execution writes `unnamed`
    // cannot happen.
    let organize_templates = NamingTemplateSet {
        folder: organize.folder_template.clone(),
        image: None,
        attachment: None,
        attachment_name: None,
        empty_value: Some(organize.empty_value.clone()),
    };

    // The legacy download template is the historical download-side folder text.
    // An absent one is *not* the organize text: substituting it silently would
    // invent a download template that never existed, so it is reported as
    // undecidable instead (see the conflict check below).
    let download_folder = download.folder_template.trim().to_string();
    let download_folder_absent = download_folder.is_empty();
    if download_folder_absent {
        notes.push(
            "no stored download folder template; legacy download semantics cannot be previewed"
                .to_string(),
        );
    }
    let legacy_download_templates = NamingTemplateSet {
        folder: download_folder.clone(),
        image: Some(download.image_template.clone()),
        attachment: Some(download.attachment_template.clone()),
        attachment_name: None,
        empty_value: None,
    };
    // The new shared rule is the migrated organize Default: one folder
    // authority for download and organize (`plan §6.2`).
    let shared_templates = organize_templates.clone();
    let new_organize_templates = organize_templates.clone();

    let download_semantics_text = "legacy_download";
    let organize_semantics_text = "legacy_local";

    let previews = vec![
        row(
            "legacy_download",
            legacy_download_templates,
            if download_folder_absent {
                None
            } else {
                Some(SemanticVersion::LegacyDownload)
            },
            download_semantics_text,
            context,
            &mut notes,
        ),
        row(
            "new_shared",
            shared_templates.clone(),
            Some(SemanticVersion::WorkV2),
            "work_v2",
            context,
            &mut notes,
        ),
        row(
            "legacy_organize",
            organize_templates,
            Some(SemanticVersion::LegacyLocal),
            organize_semantics_text,
            context,
            &mut notes,
        ),
        row(
            "new_organize",
            new_organize_templates,
            Some(SemanticVersion::WorkV2),
            "work_v2",
            context,
            &mut notes,
        ),
    ];

    let conflicts = detect_conflicts(&previews, download_folder_absent);
    let revision = naming_revision(conn)?;
    Ok(NamingMigrationPreview {
        work_key,
        revision,
        previews,
        conflicts,
        notes,
    })
}

fn detect_conflicts(
    previews: &[NamingMigrationPreviewRow],
    download_undecidable: bool,
) -> Vec<NamingMigrationConflict> {
    let by_label = |label: &str| previews.iter().find(|row| row.label == label);
    let mut conflicts = Vec::new();

    // 1. The download rule against the new shared rule. Both are folder
    //    authorities for the same work, so a difference is a real migration
    //    question. `legacy_download` and `legacy_organize` are deliberately not
    //    compared here: they are two *historical* rules that never had to
    //    agree, and reporting their text difference alone would bury the
    //    actionable conflict under noise the user cannot act on.
    compare_rule(
        by_label("legacy_download"),
        by_label("new_shared"),
        download_undecidable,
        &mut conflicts,
    );
    // 2. The organize rule's old meaning against its new meaning. Same template
    //    text with two meanings is exactly the silent-merge hazard
    //    (reconciliation §7.2), so this pair only ever yields a semantics
    //    conflict.
    compare_semantics_only(
        by_label("legacy_organize"),
        by_label("new_organize"),
        &mut conflicts,
    );
    conflicts
}

fn compare_rule(
    left: Option<&NamingMigrationPreviewRow>,
    right: Option<&NamingMigrationPreviewRow>,
    left_undecidable: bool,
    conflicts: &mut Vec<NamingMigrationConflict>,
) {
    let (Some(left), Some(right)) = (left, right) else {
        return;
    };
    if left_undecidable {
        conflicts.push(NamingMigrationConflict {
            reason: NamingMigrationConflictReason::SemanticsUndecidable,
            left_label: left.label.clone(),
            right_label: right.label.clone(),
            detail: format!(
                "{} has no stored template text, so its meaning cannot be decided; \
                 nothing is migrated from it",
                left.label
            ),
        });
        return;
    }
    if left.templates.folder.trim() == right.templates.folder.trim() {
        compare_semantics_only(Some(left), Some(right), conflicts);
        return;
    }
    conflicts.push(NamingMigrationConflict {
        reason: NamingMigrationConflictReason::TemplateTextDiffers,
        left_label: left.label.clone(),
        right_label: right.label.clone(),
        detail: format!(
            "{} is {:?} but {} is {:?}",
            left.label, left.templates.folder, right.label, right.templates.folder
        ),
    });
}

fn compare_semantics_only(
    left: Option<&NamingMigrationPreviewRow>,
    right: Option<&NamingMigrationPreviewRow>,
    conflicts: &mut Vec<NamingMigrationConflict>,
) {
    let (Some(left), Some(right)) = (left, right) else {
        return;
    };
    if left.templates.folder.trim() != right.templates.folder.trim()
        || left.semantics == right.semantics
    {
        return;
    }
    conflicts.push(NamingMigrationConflict {
        reason: NamingMigrationConflictReason::SameTextDifferentSemantics,
        left_label: left.label.clone(),
        right_label: right.label.clone(),
        detail: format!(
            "the same template {:?} means {} for {} but {} for {}; \
             the meaning cannot be merged silently",
            left.templates.folder,
            left.semantics.as_str(),
            left.label,
            right.semantics.as_str(),
            right.label
        ),
    });
}

/// Read-only preview for a work that has no naming context yet.
///
/// Returns the stored rule sources with an empty context so a caller can tell
/// "these are the rules", not "this is the target". Kept here so callers never
/// hand-roll a context just to ask what the rules are.
pub fn naming_rule_sources(
    conn: &Connection,
    artist_id: Option<i64>,
) -> Result<(OrganizeRuleSnapshot, crate::pawchive::PawchiveSettings)> {
    ensure_pawchive_schema(conn).context("pawchive schema")?;
    Ok((
        read_organize_rule(conn, artist_id)?,
        get_pawchive_settings(conn)?,
    ))
}

// ---------------------------------------------------------------------------
// Naming Revision & User Switch Application (B5 Gate)
// ---------------------------------------------------------------------------

/// Current naming rules revision from `app_settings`.
///
/// Starts at 1. Every explicit user switch (`apply_naming_migration` / `switch_naming`)
/// increments it. Previews carry this revision so `apply` can verify CAS consistency.
pub fn naming_revision(conn: &Connection) -> Result<i64> {
    ensure_pawchive_schema(conn).context("pawchive schema")?;
    let val: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = 'pawchive_naming_revision'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    match val {
        Some(s) => Ok(s.parse::<i64>().unwrap_or(1)),
        None => Ok(1),
    }
}

/// Check if any folder-archive split, merge, or rename plans are actively confirmed or executing.
///
/// Under B5, moving and publishing cannot race across generations. Naming changes
/// are blocked while archive split/merge operations are active to prevent path inconsistencies.
pub fn check_archive_coordination(conn: &Connection) -> Result<Option<String>> {
    let exists: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='folder_rename_plans'",
            [],
            |_| Ok(true),
        )
        .optional()?
        .unwrap_or(false);
    if !exists {
        return Ok(None);
    }
    let count: i64 = conn.query_row(
        "SELECT COUNT(*) FROM folder_rename_plans WHERE status IN ('confirmed', 'executing')",
        [],
        |row| row.get(0),
    )?;
    if count > 0 {
        return Ok(Some(format!(
            "{count} folder archive operation(s) are currently confirmed or executing"
        )));
    }
    Ok(None)
}

/// Request to switch or migrate naming templates.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamingApplyRequest {
    pub expected_revision: i64,
    #[serde(default)]
    pub semantic_version: Option<String>,
    #[serde(default)]
    pub folder_template: Option<String>,
    #[serde(default)]
    pub image_template: Option<String>,
    #[serde(default)]
    pub attachment_template: Option<String>,
    #[serde(default)]
    pub empty_value: Option<String>,
    #[serde(default)]
    pub date_format: Option<String>,
    #[serde(default)]
    pub tag_separator: Option<String>,
}

/// Outcome of an applied naming switch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NamingApplyOutcome {
    pub revision: i64,
    pub previous_revision: i64,
    pub semantic_version: String,
    pub folder_template: String,
    pub image_template: String,
    pub attachment_template: String,
}

/// Errors that can occur when applying a naming migration or switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NamingApplyError {
    RevisionConflict { expected: i64, current: i64 },
    ArchiveOperationInProgress(String),
    Database(String),
    Other(String),
}

impl std::fmt::Display for NamingApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RevisionConflict { expected, current } => {
                write!(
                    f,
                    "naming revision conflict: expected revision {expected} but current revision is {current}"
                )
            }
            Self::ArchiveOperationInProgress(detail) => {
                write!(f, "archive operation in progress: {detail}")
            }
            Self::Database(err) => write!(f, "database error: {err}"),
            Self::Other(msg) => write!(f, "{msg}"),
        }
    }
}

impl std::error::Error for NamingApplyError {}

/// Apply a reviewed naming migration or explicit rule switch.
///
/// Validates CAS revision against `expected_revision` (returns `RevisionConflict` on mismatch),
/// coordinates with folder-archive split/merge paths (returns `ArchiveOperationInProgress`),
/// updates download and organize Default templates to establish the single folder authority,
/// increments `pawchive_naming_revision`, and records an audit event in `pawchive_events`.
pub fn apply_naming_migration(
    conn: &Connection,
    req: &NamingApplyRequest,
) -> Result<NamingApplyOutcome, NamingApplyError> {
    ensure_pawchive_schema(conn).map_err(|e| NamingApplyError::Other(e.to_string()))?;
    let current_rev = naming_revision(conn).map_err(|e| NamingApplyError::Other(e.to_string()))?;
    if req.expected_revision != current_rev {
        return Err(NamingApplyError::RevisionConflict {
            expected: req.expected_revision,
            current: current_rev,
        });
    }

    if let Some(reason) =
        check_archive_coordination(conn).map_err(|e| NamingApplyError::Other(e.to_string()))?
    {
        return Err(NamingApplyError::ArchiveOperationInProgress(reason));
    }

    let mut settings =
        get_pawchive_settings(conn).map_err(|e| NamingApplyError::Other(e.to_string()))?;
    if let Some(folder) = &req.folder_template {
        settings.folder_template = folder.clone();
    }
    if let Some(img) = &req.image_template {
        settings.image_template = img.clone();
    }
    if let Some(att) = &req.attachment_template {
        settings.attachment_template = att.clone();
    }

    // Both sides of the switch, the revision and the audit event are one
    // decision: a failure anywhere must leave the previous rule fully in force,
    // never a download template the organize side never accepted. `load` already
    // answers the factory defaults when no row exists, so a read error is an
    // error here — falling back would silently replace every stored profile.
    let tx = rusqlite::Transaction::new_unchecked(conn, rusqlite::TransactionBehavior::Immediate)
        .map_err(|e| NamingApplyError::Database(e.to_string()))?;

    // Re-read under the write lock. The check above ran outside the
    // transaction, so two concurrent applies carrying the same
    // `expected_revision` both passed it and both committed the same next
    // revision; the later `save_pawchive_settings` then replaced the earlier
    // one's templates while both answered success, and one admin's naming
    // switch simply disappeared.
    let current_rev = naming_revision(&tx).map_err(|e| NamingApplyError::Other(e.to_string()))?;
    if req.expected_revision != current_rev {
        return Err(NamingApplyError::RevisionConflict {
            expected: req.expected_revision,
            current: current_rev,
        });
    }
    let prev_rev = current_rev;
    let next_rev = current_rev + 1;

    crate::pawchive::save_pawchive_settings(&tx, &settings)
        .map_err(|e| NamingApplyError::Other(e.to_string()))?;

    // Update Default organize profile to match folder authority
    let mut organize_settings =
        crate::archive_profiles::load(&tx).map_err(|e| NamingApplyError::Other(e.to_string()))?;

    if let Some(profiles) = organize_settings
        .get_mut("profiles")
        .and_then(Value::as_array_mut)
    {
        for p in profiles {
            if p.get("id").and_then(Value::as_str) == Some("default") {
                if let Some(folder) = &req.folder_template {
                    p["template"] = Value::String(folder.clone());
                }
                if let Some(empty) = &req.empty_value {
                    p["empty_value"] = Value::String(empty.clone());
                }
                if let Some(fmt) = &req.date_format {
                    p["date_format"] = Value::String(fmt.clone());
                }
                if let Some(sep) = &req.tag_separator {
                    p["tag_separator"] = Value::String(sep.clone());
                }
            }
        }
    }
    // Save-time validation lives in here (it renders every profile), so its
    // error is the answer that stops the switch: discarding it reported success
    // while the organize side kept the old rule and the revision was already
    // bumped.
    crate::archive_profiles::set_folder_rename_format_settings(&tx, &organize_settings, None)
        .map_err(|e| NamingApplyError::Other(e.to_string()))?;

    tx.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES ('pawchive_naming_revision', ?1, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at;",
        rusqlite::params![next_rev.to_string()],
    )
    .map_err(|e| NamingApplyError::Database(e.to_string()))?;

    let target_sem = req.semantic_version.as_deref().unwrap_or("work_v2");
    crate::pawchive::record_pawchive_event(
        &tx,
        "info",
        "naming_switch",
        None,
        "",
        &format!("naming rule switched to revision {next_rev}"),
        &format!(
            "semantic_version={target_sem}, folder_template={}",
            settings.folder_template
        ),
    );
    tx.commit()
        .map_err(|e| NamingApplyError::Database(e.to_string()))?;

    Ok(NamingApplyOutcome {
        revision: next_rev,
        previous_revision: prev_rev,
        semantic_version: target_sem.to_string(),
        folder_template: settings.folder_template,
        image_template: settings.image_template,
        attachment_template: settings.attachment_template,
    })
}

/// Explicit entry point for switching naming rules (alias to apply_naming_migration).
pub fn switch_naming(
    conn: &Connection,
    req: &NamingApplyRequest,
) -> Result<NamingApplyOutcome, NamingApplyError> {
    apply_naming_migration(conn, req)
}
