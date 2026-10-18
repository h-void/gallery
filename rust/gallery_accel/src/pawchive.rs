//! Pawchive subscription, post discovery, and download management.
//!
//! Implements Section 3, 4, 5, 6 of `PLAN_KEMONO_SUBSCRIPTION_2026-09-14.md`.
//! Clean-room Rust implementation tailored for Gallery's architecture:
//! - Direct communication via Pawchive OpenAPI v1 REST endpoints (no DOM scraping)
//! - SQLite durable ledger for subscriptions, posts, and file versions
//! - Detection and extraction of external netdisk links (Google Drive, Mega, Baidu, etc.)
//! - Integration with `ingest_publish` for safe atomic media publishing.

use anyhow::{anyhow, Context, Result};
use chrono::{DateTime, Duration, FixedOffset, NaiveDateTime, Utc};
use futures_util::StreamExt;
use regex::Regex;
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

use crate::db::DbPool;
use crate::media_roots::{path_under_authorized_roots, MediaRoots};

/// Fixed official hosts for Pawchive.
pub const PAWCHIVE_API_BASE: &str = "https://pawchive.pw/api/v1";
pub const PAWCHIVE_FILE_BASE: &str = "https://file.pawchive.pw/data";

/// The list endpoint paginates with `o` advancing by 50 (see plan section 2.2).
pub const PAWCHIVE_POSTS_PAGE_SIZE: u64 = 50;
/// Per-sync discovery page budget: 20 pages is the plan's work-batch bound and
/// caps one round at 1000 posts so a huge creator cannot stall the sync slot.
pub const PAWCHIVE_DISCOVERY_MAX_PAGES: usize = 20;
/// Per-sync detail budget: how many posts one round may ask the source about
/// individually. A summary page lists up to fifty posts and the fields it is
/// allowed to omit are exactly the ones a manifest needs, so the walk has to
/// follow up. The bound is a scheduling slice, not a completeness promise: an
/// unfinished backlog continues on the next round, and a post whose detail was
/// never read stays unproven instead of being read as an empty work.
pub const PAWCHIVE_DETAIL_MAX_POSTS: usize = 120;

/// Timeouts for every Pawchive HTTP client, mirroring the runtime download
/// client (`runtime_prepare::download_verified`).
///
/// The distinction matters for media, not for API calls: reqwest's `timeout`
/// is a deadline for the *whole* request, response body included, so a 30s
/// value does not mean "30s of silence is a failure" but "no resource larger
/// than 30s of transfer can ever be received". A resource that cannot finish
/// inside it restarts from zero on every round and never reaches the publish
/// step, which is a permanent failure the panel cannot explain. So: a short
/// connect budget, a stall guard that is generous enough for a slow NAS write
/// between chunks, and an upper bound that only exists to stop a hung response
/// from holding the sync slot forever.
///
/// They are one set of numbers because a manual task and an automatic round
/// fetch the same bytes from the same hosts. A second client carrying its own
/// deadline is how the automatic path once lost large resources while the
/// manual one — the user's own explicit request — lost them faster.
pub const PAWCHIVE_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);
pub const PAWCHIVE_STALL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);
pub const PAWCHIVE_MAX_REQUEST_DURATION: std::time::Duration =
    std::time::Duration::from_secs(24 * 60 * 60);

/// The client every Pawchive request goes through: the automatic round and the
/// manual task both, because both fetch the same bytes from the same hosts.
///
/// Building it in one place is the point. The manual attempt used to build its
/// own client with a bare 30s `timeout`, which is not "30s of silence is a
/// failure" but "no file larger than 30s of transfer can ever be received" — a
/// task the user asked for by hand could fail on a large attachment while the
/// same file downloaded fine from a scheduled round.
pub fn pawchive_http_client() -> Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .connect_timeout(PAWCHIVE_CONNECT_TIMEOUT)
        .read_timeout(PAWCHIVE_STALL_TIMEOUT)
        .timeout(PAWCHIVE_MAX_REQUEST_DURATION)
        .redirect(pawchive_redirect_policy())
        .build()?)
}

/// Pawchive creator profile from `/{service}/user/{id}/profile`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PawchiveProfile {
    pub id: String,
    pub name: Option<String>,
    pub service: String,
    pub indexed: Option<String>,
    pub updated: Option<String>,
}

/// Attachment or primary file reference in a post.
///
/// `id` is the source's own stable identifier for the attachment, when it
/// publishes one. The plan makes it the preferred resource identity: a path is
/// a location, and the same location can hold different bytes after a
/// replacement, while the attachment id names the resource itself. Older
/// responses do not carry it, which is why `source_identity` falls back to the
/// path and records which basis it used.
fn deserialize_optional_string_or_number<'de, D>(
    deserializer: D,
) -> std::result::Result<Option<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct OptStringOrNumberVisitor;
    impl<'de> serde::de::Visitor<'de> for OptStringOrNumberVisitor {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a string, integer, or null")
        }

        fn visit_str<E>(self, v: &str) -> std::result::Result<Self::Value, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }

        fn visit_string<E>(self, v: String) -> std::result::Result<Self::Value, E> {
            let trimmed = v.trim();
            if trimmed.is_empty() {
                Ok(None)
            } else {
                Ok(Some(trimmed.to_string()))
            }
        }

        fn visit_i64<E>(self, v: i64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_u64<E>(self, v: u64) -> std::result::Result<Self::Value, E> {
            Ok(Some(v.to_string()))
        }

        fn visit_none<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_some<D>(self, deserializer: D) -> std::result::Result<Self::Value, D::Error>
        where
            D: serde::Deserializer<'de>,
        {
            deserializer.deserialize_any(self)
        }

        fn visit_unit<E>(self) -> std::result::Result<Self::Value, E> {
            Ok(None)
        }
    }

    deserializer.deserialize_option(OptStringOrNumberVisitor)
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PawchiveFileRef {
    #[serde(
        default,
        deserialize_with = "deserialize_optional_string_or_number",
        alias = "attachment_id",
        alias = "file_id",
        alias = "attachmentId",
        alias = "fileId"
    )]
    pub id: Option<String>,
    pub name: Option<String>,
    pub path: Option<String>,
}

impl PawchiveFileRef {
    /// The resource identity to key this reference by, and what it rests on.
    ///
    /// An empty or whitespace-only id is not an identity: falling back to the
    /// path is the honest answer, and the basis returned here is what the
    /// ledger stores alongside the resource.
    pub fn resource_identity(&self) -> (String, &'static str) {
        if let Some(id) = self
            .id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return (id.to_string(), IDENTITY_BASIS_SOURCE_ID);
        }
        let path = self
            .path
            .as_deref()
            .map(normalize_remote_path)
            .unwrap_or_default();
        (path, IDENTITY_BASIS_PATH)
    }
}

/// Post summary returned by `/{service}/user/{id}?o={offset}`.
///
/// The resource fields are exactly the ones a summary is allowed to leave out.
/// `#[serde(default)]` alone would turn a missing `attachments` key into an
/// empty list, which reads the same as the source stating the post has none —
/// and treating those two as one answer retires every attachment the post was
/// known to have. The presence flags keep "the source said empty" apart from
/// "the source did not say".
#[derive(Debug, Clone, Serialize)]
pub struct PawchivePost {
    pub id: String,
    pub user: String,
    pub service: String,
    pub title: Option<String>,
    pub content: Option<String>,
    pub published: Option<String>,
    pub added: Option<String>,
    pub edited: Option<String>,
    pub file: Option<PawchiveFileRef>,
    pub attachments: Vec<PawchiveFileRef>,
    /// The response carried an `attachments` key.
    pub attachments_present: bool,
    /// The response carried a `file` key.
    pub file_present: bool,
    /// The response carried a `content` key.
    pub content_present: bool,
    /// The source's own completeness flag, when it publishes one.
    pub has_full: Option<bool>,
}

impl PawchivePost {
    /// What this observation actually saw of the work's resource set.
    ///
    /// A response that never mentions the resource fields has not said the post
    /// is empty; it has not answered the question. Only a response that carries
    /// them — including one that carries them as explicitly empty — can be read
    /// as the whole set.
    pub fn manifest_completeness(&self) -> ManifestCompleteness {
        if self.has_full == Some(false) {
            return ManifestCompleteness::Partial;
        }
        if self.attachments_present || self.file_present {
            ManifestCompleteness::Complete
        } else {
            ManifestCompleteness::Unknown
        }
    }

    /// The resource references this observation listed, primary file first.
    pub fn resource_refs(&self) -> Vec<&PawchiveFileRef> {
        let mut refs: Vec<&PawchiveFileRef> = Vec::new();
        if let Some(primary) = &self.file {
            refs.push(primary);
        }
        refs.extend(self.attachments.iter());
        refs
    }
}

/// Wire shape of a post.
///
/// Every resource key is an `Option` here so presence can be told from absence
/// before the typed post is built; the typed post is the only thing the rest of
/// the module sees.
#[derive(Deserialize)]
struct PawchivePostWire {
    id: String,
    #[serde(default)]
    user: String,
    #[serde(default)]
    service: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<String>,
    #[serde(default)]
    published: Option<String>,
    #[serde(default)]
    added: Option<String>,
    #[serde(default)]
    edited: Option<String>,
    #[serde(default)]
    file: Option<PawchiveFileRef>,
    #[serde(default)]
    attachments: Option<Vec<PawchiveFileRef>>,
    #[serde(default)]
    has_full: Option<bool>,
}

impl<'de> Deserialize<'de> for PawchivePost {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Value::deserialize(deserializer)?;
        let present = |key: &str| {
            value
                .as_object()
                .map(|object| object.contains_key(key))
                .unwrap_or(false)
        };
        let (attachments_present, file_present, content_present) =
            (present("attachments"), present("file"), present("content"));
        let wire: PawchivePostWire =
            serde_json::from_value(value).map_err(serde::de::Error::custom)?;
        Ok(PawchivePost {
            id: wire.id,
            user: wire.user,
            service: wire.service,
            title: wire.title,
            content: wire.content,
            published: wire.published,
            added: wire.added,
            edited: wire.edited,
            file: wire.file,
            attachments: wire.attachments.unwrap_or_default(),
            attachments_present,
            file_present,
            content_present,
            has_full: wire.has_full,
        })
    }
}

/// Media category for filtering and routing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FileCategory {
    Image,
    Video,
    Audio,
    Archive,
    Other,
}

impl FileCategory {
    pub fn from_filename(name: &str) -> Self {
        let lower = name.to_lowercase();
        let ext = lower.rsplit('.').next().unwrap_or("");
        match ext {
            "jpg" | "jpeg" | "png" | "gif" | "webp" | "bmp" | "avif" | "jxl" | "heic" | "heif"
            | "tif" | "tiff" => FileCategory::Image,
            "mp4" | "mkv" | "webm" | "avi" | "mov" | "wmv" | "flv" | "m4v" | "mpg" | "mpeg"
            | "ts" | "m2ts" => FileCategory::Video,
            "mp3" | "wav" | "flac" | "m4a" | "aac" | "ogg" | "opus" | "wma" | "aif" | "aiff" => {
                FileCategory::Audio
            }
            // Packaged media only. Source/resource formats (psd, clip, sai, ...)
            // are not archives and belong to `Other`.
            "zip" | "rar" | "7z" | "tar" | "gz" | "tgz" | "bz2" | "xz" | "lz4" | "zst" => {
                FileCategory::Archive
            }
            _ => FileCategory::Other,
        }
    }

    pub fn as_str(&self) -> &'static str {
        match self {
            FileCategory::Image => "image",
            FileCategory::Video => "video",
            FileCategory::Audio => "audio",
            FileCategory::Archive => "archive",
            FileCategory::Other => "other",
        }
    }
}

#[path = "pawchive_url_naming.rs"]
mod url_naming;
use url_naming::clamp_name_bytes;
pub use url_naming::{
    build_file_url, build_posts_url, build_profile_url, extract_archive_passwords,
    extract_external_links, format_post_datetime_display, normalize_post_date,
    normalize_post_datetime_format, normalize_remote_path, parse_creator_url,
    pawchive_redirect_policy, pawchive_redirect_policy_with_resolver, redirected_host_is_allowed,
    sanitize_path_component, validate_redirect_url, ArcDnsResolver, DnsResolverFn,
    PostNamingContext, MAX_RENDERED_NAME_BYTES,
};

#[path = "pawchive_manual.rs"]
mod manual;
pub use manual::{
    accept_legacy_scope, build_post_url, create_attempt, list_post_attempts, plan_manual_post,
    record_post_decision, record_selection, record_selection_for_filter, run_manual_attempt,
    AttemptError, AttemptOutcome, DecisionOutcome, FilterSelectionOutcome, LegacyScopeOutcome,
    ManualPostPlan, ManualResourcePlan, PostAttemptSummary, PostDecisionAction, SelectionError,
    SelectionOutcome, PAWCHIVE_FILTER_SELECTION_MAX_POSTS, PAWCHIVE_SELECTION_TTL_MINUTES,
};
pub(crate) use manual::{download_post_files_scoped, DownloadPostPolicy};

#[path = "pawchive_sync.rs"]
mod sync;
pub use sync::{
    fetch_creator_posts_api, fetch_creator_posts_paged, fetch_creator_posts_resumed,
    fetch_post_detail, finish_pawchive_sync, list_artist_posts_page, list_filtered_post_ids,
    list_subscription_posts, pawchive_round_due, pawchive_sync_status, run_pawchive_reconcile,
    run_pawchive_sync, subscription_summary, try_begin_pawchive_sync, DiscoveryOutcome,
    DiscoveryPage, PawchivePostPage, PawchivePostRow, PawchiveSyncStatus, PostAwaitingDetail,
    PostListFilter, StablePostPage, StablePostRow, SubscriptionSummary,
};

/// Ensure database schema for subscriptions and discovered files.
///
/// Migrates legacy databases rather than only creating fresh tables:
/// - drops the dead per-subscription `include_*` filter columns (their stale
///   defaults made 压缩包 a no-op; the global `PawchiveSettings` is the only
///   type filter now);
/// - adds the per-subscription download `coverage` ledger and the
///   `discovery_offset` resume cursor, so a resumed round skips the daily
///   pages it already finished instead of re-walking the whole creator list;
/// - adds the delivery-evidence columns that replace the old "the folder holds
///   a file, so the post is downloaded" rule (see `migrate_pawchive_evidence`).
pub fn ensure_pawchive_schema(conn: &Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kemono_subscriptions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            site_id TEXT NOT NULL DEFAULT 'pawchive',
            service TEXT NOT NULL,
            user_id TEXT NOT NULL,
            artist_id INTEGER REFERENCES artists(id),
            target_dir TEXT NOT NULL,
            enabled INTEGER NOT NULL DEFAULT 0,
            since_date TEXT,
            check_interval_hours INTEGER NOT NULL DEFAULT 6,
            last_discovery_at TEXT,
            last_error TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(site_id, service, user_id)
        );

        CREATE TABLE IF NOT EXISTS kemono_posts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            subscription_id INTEGER NOT NULL REFERENCES kemono_subscriptions(id) ON DELETE CASCADE,
            post_id TEXT NOT NULL,
            title TEXT,
            published_at TEXT,
            edited_at TEXT,
            content TEXT,
            external_links TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(subscription_id, post_id)
        );

        CREATE TABLE IF NOT EXISTS kemono_files (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            post_id INTEGER NOT NULL REFERENCES kemono_posts(id) ON DELETE CASCADE,
            source_identity TEXT NOT NULL,
            remote_path TEXT NOT NULL,
            file_name TEXT NOT NULL,
            file_type TEXT NOT NULL,
            expected_length INTEGER,
            expected_blake3 TEXT,
            status TEXT NOT NULL DEFAULT 'pending',
            publish_job_id INTEGER REFERENCES download_publish_jobs(id),
            error_message TEXT,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(post_id, source_identity)
        );

        CREATE INDEX IF NOT EXISTS idx_kemono_files_status ON kemono_files(status);
        CREATE INDEX IF NOT EXISTS idx_kemono_posts_sub ON kemono_posts(subscription_id);",
    )?;
    let columns = conn
        .prepare("PRAGMA table_info(kemono_subscriptions)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for legacy in ["include_images", "include_videos", "include_archives"] {
        if columns.iter().any(|column| column == legacy) {
            conn.execute(
                &format!("ALTER TABLE kemono_subscriptions DROP COLUMN {legacy}"),
                [],
            )?;
        }
    }
    ensure_columns(
        conn,
        "kemono_subscriptions",
        &[
            // Discovery resume cursor into the creator's offset-paged post list.
            // A resumed round skips the daily pages it already finished instead
            // of re-walking the whole creator history from offset 0.
            ("discovery_offset", "INTEGER NOT NULL DEFAULT 0"),
            // Whether the last walk actually reached the end of the creator's
            // list. Until it has, "no gap" only means "nothing found yet".
            ("discovery_complete", "INTEGER NOT NULL DEFAULT 0"),
            // Automatic or manual claiming. The default is manual, so a
            // subscription created before this column existed is migrated below
            // rather than reading as manual overnight.
            ("mode", "TEXT NOT NULL DEFAULT 'manual'"),
        ],
    )?;
    migrate_subscription_modes(conn)?;
    ensure_columns(
        conn,
        "kemono_posts",
        &[
            // The status this row carried before completion was derived from
            // evidence. Kept for audit: the old directory rule cannot be
            // reproduced, so it is the only record of how a row was settled.
            ("legacy_status", "TEXT"),
            // Bumped whenever the discovered resource set changes. A manual
            // confirmation is bound to the version it was made at, so new
            // attachments are re-assessed instead of staying confirmed.
            ("manifest_version", "INTEGER NOT NULL DEFAULT 0"),
            ("manifest_fingerprint", "TEXT NOT NULL DEFAULT ''"),
            // Derived, never authoritative: recomputed by the evaluator from
            // the manifest, the settings and the delivery evidence.
            ("assessment_state", "TEXT NOT NULL DEFAULT 'unassessed'"),
            ("assessment_reason", "TEXT NOT NULL DEFAULT ''"),
            ("assessment_at", "TEXT"),
            // The last external-downloader receipt that covered this post. Bound
            // to a manifest version like a user decision, so a later attachment
            // is not covered by a receipt that never saw it.
            ("external_task_id", "TEXT NOT NULL DEFAULT ''"),
            ("external_delivered_at", "TEXT"),
            ("external_manifest_version", "INTEGER NOT NULL DEFAULT 0"),
            // What the last read of this post's folder found. `missing`,
            // `empty` and `unreadable` are not the same finding: only the
            // first two mean the content could legitimately be absent.
            ("observed_state", "TEXT NOT NULL DEFAULT ''"),
            ("observed_files", "INTEGER NOT NULL DEFAULT 0"),
            ("observed_at", "TEXT"),
            // How the folder that was read was chosen. Not derivable from the
            // path once it is stored, and it is precisely the choice a later
            // rule change has to be able to revisit: a verdict that says "the
            // day's folder is present, so the post is had" rests on a folder
            // search, and a search rule that improves must be able to reach the
            // rows it already decided. `''` means the row was observed before
            // this column existed, so how its folder was found is not recorded.
            ("observed_source", "TEXT NOT NULL DEFAULT ''"),
            // The post's publication date in the local target timezone (UTC+8),
            // which is the date its folder is named after.
            //
            // `published_at` holds the source's own string: JST for
            // fanbox/fantia, UTC for patreon and friends, whatever offset the
            // service used otherwise. That is the right thing to keep — it is
            // the source's fact — but it is not the day this install files the
            // work under. Every day-scoped read (`substr(published_at, 1, 10)`)
            // used to compare the source's date against folders named with the
            // normalized one, so a post published at JST 00:37 landed in a
            // folder dated the previous day while the coverage calendar counted
            // it on the later one. Storing the derived date once, at write
            // time, lets every query group by the same day the folder uses.
            ("published_local_date", "TEXT NOT NULL DEFAULT ''"),
        ],
    )?;
    // Created after `assessment_state` exists — the index cannot be part of the
    // `CREATE TABLE` batch above, because on an upgraded database that column
    // arrives through `ensure_columns` and the index would fail first.
    //
    // The detail backlog and the assessment sweep both drive from the per-post
    // state, and neither the state nor `id` had an index: every round planned a
    // full scan of `kemono_posts` and then sorted it, for a queue that is
    // usually a small tail of the library.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_kemono_posts_state
             ON kemono_posts(assessment_state, id);",
    )?;
    ensure_columns(
        conn,
        "kemono_files",
        &[
            // What `source_identity` rests on: the source's own attachment id
            // (`source_id`) or the served path standing in for one (`path`).
            // Without this a later read cannot tell an identity from a location,
            // and the two answer the "was this replaced?" question differently.
            ("identity_basis", "TEXT NOT NULL DEFAULT 'path'"),
            // Where the delivered bytes came from. Empty means "nothing is
            // proven yet"; `download` is this install's own verified fetch,
            // `present` is content observed on disk without a receipt, and
            // `legacy` is inherited from the retired directory rule.
            ("delivery_source", "TEXT NOT NULL DEFAULT ''"),
            // Reserved once, when the task is created. A later retry must not
            // recompute it: the template and the attachment order can change,
            // and the published file stays where it was actually put.
            ("target_path", "TEXT NOT NULL DEFAULT ''"),
            ("attempt_count", "INTEGER NOT NULL DEFAULT 0"),
            ("error_class", "TEXT NOT NULL DEFAULT ''"),
            // Set only for a class the retry rules defer; a NULL means the
            // resource is claimable now, which is what the queue tests.
            ("next_attempt_at", "TEXT"),
            ("local_hash", "TEXT NOT NULL DEFAULT ''"),
            // Whether the bytes on disk are still the resource. Empty means
            // nobody has compared them; `mismatched`, `missing` and
            // `unreadable` all revoke the proof the delivery once carried,
            // without turning the resource back into a fresh download.
            ("integrity", "TEXT NOT NULL DEFAULT ''"),
            // Where `expected_blake3` came from: `source` when the site
            // published it, `local` when this install computed it from the
            // bytes it received. A locally computed digest proves the copy has
            // not changed since; it is not the source's own checksum.
            ("hash_source", "TEXT NOT NULL DEFAULT ''"),
            ("claimed_at", "TEXT"),
            // The worker that holds the claim, and the fencing token it was
            // granted. `reset_stale_download_claims` returns an expired row to
            // the queue without knowing whether the worker is still writing, so
            // the worker's authority has to be revocable by the database rather
            // than by agreement: a reclaim bumps the token, and every write the
            // worker makes is conditional on the pair it was granted. Without
            // this a receive that outlived its lease published over the row a
            // newer owner was already working on.
            ("claimed_by", "TEXT NOT NULL DEFAULT ''"),
            ("claim_token", "INTEGER NOT NULL DEFAULT 0"),
            ("verified_at", "TEXT"),
            // ETag/Last-Modified plus the declared length of the partial
            // receive, so a resume is only ever appended to its own version.
            ("resume_validator", "TEXT NOT NULL DEFAULT ''"),
            // Whether the source still lists this resource. A removed
            // attachment keeps its row — and whatever was downloaded from it —
            // but leaves the current requirement, so the work is not held open
            // by a file the source has withdrawn.
            ("present_in_manifest", "INTEGER NOT NULL DEFAULT 1"),
            ("retired_at", "TEXT"),
            // The media-library row this delivery became. Tags and everything
            // else bind to `items.id`, and a file the user later moves keeps
            // that id while its path changes, so both are stored.
            ("evidence_item_id", "INTEGER"),
        ],
    )?;
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS kemono_download_coverage (
            subscription_id INTEGER NOT NULL REFERENCES kemono_subscriptions(id) ON DELETE CASCADE,
            day TEXT NOT NULL,
            status TEXT NOT NULL DEFAULT 'pending',
            checked_at TEXT NOT NULL,
            PRIMARY KEY (subscription_id, day)
        );

        CREATE TABLE IF NOT EXISTS pawchive_events (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            created_at TEXT NOT NULL,
            level TEXT NOT NULL DEFAULT 'info',
            kind TEXT NOT NULL,
            subscription_id INTEGER,
            artist_name TEXT NOT NULL DEFAULT '',
            message TEXT NOT NULL,
            detail TEXT NOT NULL DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_events_id ON pawchive_events(id DESC);

        -- Reversible user decisions. A decision records what the user said
        -- about a manifest version; it never rewrites delivery evidence.
        CREATE TABLE IF NOT EXISTS pawchive_post_decisions (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            post_id INTEGER NOT NULL REFERENCES kemono_posts(id) ON DELETE CASCADE,
            manifest_version INTEGER NOT NULL,
            action TEXT NOT NULL,
            reason TEXT NOT NULL DEFAULT '',
            bound_path TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_post_decisions_post
            ON pawchive_post_decisions(post_id, id DESC);

        -- What the user selected, frozen. The selection is not a filter that is
        -- re-applied later: a manifest that changes between the review and the
        -- request must not silently widen or narrow what was asked for, and the
        -- panel has to be able to say what the task was created against.
        CREATE TABLE IF NOT EXISTS pawchive_selections (
            selection_id TEXT PRIMARY KEY,
            request_id TEXT NOT NULL UNIQUE,
            request_fingerprint TEXT NOT NULL,
            posts INTEGER NOT NULL DEFAULT 0,
            resources INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            expires_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS pawchive_selection_members (
            selection_id TEXT NOT NULL,
            post_id INTEGER NOT NULL,
            work_id TEXT NOT NULL DEFAULT '',
            manifest_version INTEGER NOT NULL DEFAULT 0,
            resource_key TEXT NOT NULL DEFAULT '',
            resource_version_id TEXT NOT NULL DEFAULT '',
            target_path TEXT NOT NULL DEFAULT '',
            -- `fetch` is a resource the selection still needs; `covered` is one
            -- the run decided already has evidence and therefore will not
            -- re-fetch unless the attempt asks for it explicitly.
            planned_state TEXT NOT NULL DEFAULT 'fetch',
            PRIMARY KEY (selection_id, post_id, resource_key)
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_selection_members_post
            ON pawchive_selection_members(post_id);

        -- One explicit request. An attempt is what the user asked for at one
        -- moment; the ledger of what was fetched stays separate, so a failed
        -- attempt cannot erase a success and a repeated request is one task.
        -- The request id is the idempotency key: the same request is the same
        -- task, and a later request is a new one even if it names the same work.
        CREATE TABLE IF NOT EXISTS pawchive_attempts (
            attempt_id TEXT PRIMARY KEY,
            selection_id TEXT NOT NULL,
            request_id TEXT NOT NULL UNIQUE,
            intent TEXT NOT NULL DEFAULT 'manual',
            state TEXT NOT NULL DEFAULT 'queued',
            posts INTEGER NOT NULL DEFAULT 0,
            resources INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_attempts_selection
            ON pawchive_attempts(selection_id);
        CREATE INDEX IF NOT EXISTS idx_pawchive_attempts_state
            ON pawchive_attempts(state, updated_at);

        -- One row per post of the attempt, so a partial failure is recorded
        -- against the post it belongs to and the panel can say which work is
        -- still outstanding.
        CREATE TABLE IF NOT EXISTS pawchive_attempt_posts (
            attempt_id TEXT NOT NULL,
            post_id INTEGER NOT NULL,
            work_id TEXT NOT NULL DEFAULT '',
            state TEXT NOT NULL DEFAULT 'queued',
            fetched INTEGER NOT NULL DEFAULT 0,
            error TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL,
            PRIMARY KEY (attempt_id, post_id)
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_attempt_posts_post
            ON pawchive_attempt_posts(post_id, attempt_id);

        -- One observation-only reconciliation pass. It reads the library and
        -- writes assessments; it never downloads and never claims a post.
        CREATE TABLE IF NOT EXISTS pawchive_reconcile_runs (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            trigger TEXT NOT NULL,
            started_at TEXT NOT NULL,
            finished_at TEXT,
            status TEXT NOT NULL DEFAULT 'running',
            summary TEXT NOT NULL DEFAULT ''
        );

        -- External-downloader receipts. The unique key is what makes a repeated
        -- delivery idempotent: the same task reporting the same version twice
        -- settles once. The bridge that would produce these is not verified
        -- against a real installation yet, so nothing writes here by itself.
        -- Deleting a subscription must not wipe historical external receipts,
        -- so post_id carries no foreign key cascade into the ephemeral post row.
        CREATE TABLE IF NOT EXISTS pawchive_external_receipts (
            id INTEGER PRIMARY KEY AUTOINCREMENT,
            work_id TEXT NOT NULL DEFAULT '',
            post_id INTEGER,
            manifest_version INTEGER NOT NULL,
            task_id TEXT NOT NULL,
            result TEXT NOT NULL,
            output_paths TEXT NOT NULL DEFAULT '',
            note TEXT NOT NULL DEFAULT '',
            recorded_at TEXT NOT NULL,
            UNIQUE(post_id, manifest_version, task_id)
        );",
    )?;
    ensure_columns(
        conn,
        "pawchive_external_receipts",
        &[
            // Long-term work identity so receipt history survives subscription deletion.
            ("work_id", "TEXT NOT NULL DEFAULT ''"),
            // Which bridge reported this. A receipt without a paired bridge is
            // history only: it can never settle the work it names.
            ("bridge_id", "TEXT NOT NULL DEFAULT ''"),
            // The digest of what this report said, so a repeated report is
            // recognised as the same fact rather than as a new delivery.
            ("content_fingerprint", "TEXT NOT NULL DEFAULT ''"),
            // Whether this row actually settled the post. A rejected receipt
            // stays in the table so the reason is auditable.
            ("settled", "INTEGER NOT NULL DEFAULT 0"),
            ("rejected_reason", "TEXT NOT NULL DEFAULT ''"),
        ],
    )?;
    let old_receipt_ddl: Option<String> = conn
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type = 'table' AND name = 'pawchive_external_receipts'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(sql) = old_receipt_ddl {
        if sql.contains("REFERENCES kemono_posts") && sql.contains("CASCADE") {
            conn.execute_batch(
                "CREATE TABLE pawchive_external_receipts_migrated (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    work_id TEXT NOT NULL DEFAULT '',
                    post_id INTEGER,
                    manifest_version INTEGER NOT NULL,
                    task_id TEXT NOT NULL,
                    result TEXT NOT NULL,
                    output_paths TEXT NOT NULL DEFAULT '',
                    note TEXT NOT NULL DEFAULT '',
                    recorded_at TEXT NOT NULL,
                    bridge_id TEXT NOT NULL DEFAULT '',
                    content_fingerprint TEXT NOT NULL DEFAULT '',
                    settled INTEGER NOT NULL DEFAULT 0,
                    rejected_reason TEXT NOT NULL DEFAULT '',
                    UNIQUE(post_id, manifest_version, task_id)
                );
                INSERT INTO pawchive_external_receipts_migrated
                    (id, work_id, post_id, manifest_version, task_id, result, output_paths, note,
                     recorded_at, bridge_id, content_fingerprint, settled, rejected_reason)
                SELECT id,
                       COALESCE(work_id, ''),
                       post_id, manifest_version, task_id, result, output_paths, note,
                       recorded_at, bridge_id, content_fingerprint, settled, rejected_reason
                FROM pawchive_external_receipts;
                DROP TABLE pawchive_external_receipts;
                ALTER TABLE pawchive_external_receipts_migrated RENAME TO pawchive_external_receipts;
                CREATE UNIQUE INDEX IF NOT EXISTS idx_pawchive_external_receipts_work
                    ON pawchive_external_receipts(work_id, manifest_version, task_id) WHERE work_id != '';
                CREATE INDEX IF NOT EXISTS idx_pawchive_external_receipts_post
                    ON pawchive_external_receipts(post_id);",
            )?;
        }
    }
    // ── Long-term acquisition ledger ───────────────────────────────────────
    //
    // Everything above is the working ledger: a post row belongs to a
    // subscription and dies with it, and a file row is re-derived from whatever
    // the source lists today. Neither can answer "was this resource ever
    // fetched", which is exactly what has to survive a re-discovery, a rename,
    // an extraction, or the user dropping and re-adding the subscription.
    //
    // These tables hold that answer. They carry no foreign key into the working
    // ledger on purpose: deleting a subscription unbinds scheduling, it does
    // not delete history, so a cascade must not be able to reach them.
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS pawchive_remote_works (
            work_id TEXT PRIMARY KEY,
            site_id TEXT NOT NULL,
            service TEXT NOT NULL,
            creator_id TEXT NOT NULL,
            post_id TEXT NOT NULL,
            raw_metadata TEXT NOT NULL DEFAULT '',
            metadata_revision INTEGER NOT NULL DEFAULT 0,
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            UNIQUE(site_id, service, creator_id, post_id)
        );

        -- Which subscriptions have discovered this work. A row here is a
        -- scheduling relation, not the work's existence: dropping it removes
        -- the binding and leaves the work itself in place.
        CREATE TABLE IF NOT EXISTS pawchive_subscription_works (
            subscription_id INTEGER NOT NULL,
            work_id TEXT NOT NULL,
            discovery_state TEXT NOT NULL DEFAULT 'discovered',
            created_at TEXT NOT NULL,
            updated_at TEXT NOT NULL,
            PRIMARY KEY (subscription_id, work_id)
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_subscription_works_work
            ON pawchive_subscription_works(work_id);

        -- A resource identity and a version are separate facts. The key is the
        -- source's own stable identity for the resource; the version says which
        -- bytes it held. `version_key = 'unknown'` is a real answer meaning the
        -- source offered no revision, strong digest or strong validator, and it
        -- must not be read as unchanged. An older version is never overwritten
        -- by a newer one: a replacement inserts a second row.
        CREATE TABLE IF NOT EXISTS pawchive_resource_versions (
            resource_version_id TEXT PRIMARY KEY,
            work_id TEXT NOT NULL,
            resource_key TEXT NOT NULL,
            version_key TEXT NOT NULL,
            identity_basis TEXT NOT NULL DEFAULT 'path',
            version_basis TEXT NOT NULL DEFAULT 'unknown',
            raw_ref TEXT NOT NULL DEFAULT '',
            file_type TEXT NOT NULL DEFAULT '',
            digest TEXT NOT NULL DEFAULT '',
            digest_source TEXT NOT NULL DEFAULT '',
            length INTEGER,
            validator TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL,
            UNIQUE(work_id, resource_key, version_key)
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_resource_versions_work
            ON pawchive_resource_versions(work_id);

        -- One observation of a work's resource set. An incomplete observation
        -- is stored beside the confirmed complete one instead of replacing it:
        -- a summary that omitted `attachments` is not evidence that the
        -- attachments were withdrawn.
        CREATE TABLE IF NOT EXISTS pawchive_work_manifests (
            manifest_id TEXT PRIMARY KEY,
            work_id TEXT NOT NULL,
            member_fingerprint TEXT NOT NULL,
            completeness TEXT NOT NULL DEFAULT 'unknown',
            source_revision TEXT NOT NULL DEFAULT '',
            observed_at TEXT NOT NULL,
            is_current INTEGER NOT NULL DEFAULT 0
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_work_manifests_work
            ON pawchive_work_manifests(work_id, is_current DESC);

        CREATE TABLE IF NOT EXISTS pawchive_manifest_members (
            manifest_id TEXT NOT NULL,
            resource_version_id TEXT NOT NULL,
            required_role TEXT NOT NULL DEFAULT 'direct',
            PRIMARY KEY (manifest_id, resource_version_id)
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_manifest_members_resource
            ON pawchive_manifest_members(resource_version_id);

        -- An append-only fact: these bytes were fetched, by this attempt, from
        -- this resource version, and here is what proved it. The idempotency key
        -- is what makes a repeated report the same fact rather than a new one.
        CREATE TABLE IF NOT EXISTS pawchive_acquisition_events (
            event_id TEXT PRIMARY KEY,
            idempotency_key TEXT NOT NULL UNIQUE,
            work_id TEXT NOT NULL,
            resource_version_id TEXT NOT NULL,
            attempt_id TEXT NOT NULL DEFAULT '',
            evidence_source TEXT NOT NULL,
            evidence_strength TEXT NOT NULL DEFAULT 'unproven',
            publish_job_id INTEGER,
            acquired_at TEXT NOT NULL,
            historical_path TEXT NOT NULL DEFAULT '',
            created_at TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_acquisition_resource
            ON pawchive_acquisition_events(resource_version_id);
        -- A second name, not a rewrite of the one above: `CREATE INDEX IF NOT
        -- EXISTS` skips an existing name, so an upgraded database would keep the
        -- old `work_id`-only index and never gain the covering one. The narrow
        -- index is then dropped, because the wider one answers everything it did.
        CREATE INDEX IF NOT EXISTS idx_pawchive_acquisition_work
            ON pawchive_acquisition_events(work_id);
        CREATE INDEX IF NOT EXISTS idx_pawchive_acquisition_work_evidence
            ON pawchive_acquisition_events(work_id, resource_version_id, evidence_strength);
        DROP INDEX IF EXISTS idx_pawchive_acquisition_work;

        -- What the user said, kept apart from what was fetched. `scope_kind`
        -- records whether the decision named an exact resource set or inherited
        -- the older, unrecorded one: a legacy confirmation said I have this
        -- work, not I have these files, and must not be widened later into
        -- covering a type the user never selected.
        CREATE TABLE IF NOT EXISTS pawchive_user_decisions (
            decision_id TEXT PRIMARY KEY,
            work_id TEXT NOT NULL,
            action TEXT NOT NULL,
            scope_kind TEXT NOT NULL DEFAULT 'legacy_unknown',
            manifest_id TEXT NOT NULL DEFAULT '',
            manifest_version INTEGER NOT NULL DEFAULT 0,
            reason TEXT NOT NULL DEFAULT '',
            bound_path TEXT NOT NULL DEFAULT '',
            legacy_post_id INTEGER,
            legacy_decision_id INTEGER UNIQUE,
            created_at TEXT NOT NULL,
            revoked_by TEXT NOT NULL DEFAULT ''
        );

        CREATE INDEX IF NOT EXISTS idx_pawchive_user_decisions_work
            ON pawchive_user_decisions(work_id, decision_id);

        CREATE TABLE IF NOT EXISTS pawchive_decision_members (
            decision_id TEXT NOT NULL,
            resource_version_id TEXT NOT NULL,
            PRIMARY KEY (decision_id, resource_version_id)
        );

        -- Migration bookkeeping. The high-water mark is what lets a long
        -- backfill stop and resume instead of holding one transaction over the
        -- whole working ledger.
        CREATE TABLE IF NOT EXISTS pawchive_ledger_migration (
            name TEXT PRIMARY KEY,
            version TEXT NOT NULL,
            high_water_mark INTEGER NOT NULL DEFAULT 0,
            status TEXT NOT NULL DEFAULT 'running',
            note TEXT NOT NULL DEFAULT '',
            updated_at TEXT NOT NULL
        );",
    )?;
    migrate_pawchive_evidence(conn)?;
    migrate_pawchive_ledger(conn)?;
    migrate_resource_versions_for_path_normalization(conn)?;
    backfill_subscription_profile_links(conn)?;
    backfill_published_local_date(conn)?;
    backfill_external_links(conn)?;
    Ok(())
}

/// Version recorded in `app_settings` for the derived external-link backfill.
const PAWCHIVE_EXTERNAL_LINKS_MIGRATION_KEY: &str = "pawchive_external_links_migration";
/// Bumped whenever `extract_external_links` returns something different for the
/// same body, so a corrected derivation reaches the rows the old one wrote.
const PAWCHIVE_EXTERNAL_LINKS_MIGRATION_VERSION: &str = "1";

/// Re-derive `kemono_posts.external_links` from the stored post body.
///
/// The column is derived from `content`, and it is only rewritten when a detail
/// response replaces the post — which never happens for a work whose manifest is
/// already complete (`posts_awaiting_detail` reads only incomplete ones). So a
/// fix to the extractor would leave every row it already wrote stale, and those
/// rows are what the netdisk dispatch sends to the downloader and what the work
/// list counts. The stored body is the source of truth, so the rows are simply
/// re-derived from it.
///
/// Versioned and idempotent: the marker stops the pass until the version moves,
/// and a row whose stored links already match is never written. Batched by id so
/// a library larger than one batch is walked without one transaction over every
/// post.
fn backfill_external_links(conn: &Connection) -> Result<()> {
    const BATCH: i64 = 2000;

    // A schema old enough to have lost either column has nothing to derive from
    // and nowhere to write; the pass simply does not apply there.
    if !column_present(conn, "kemono_posts", "content")
        || !column_present(conn, "kemono_posts", "external_links")
    {
        return Ok(());
    }

    let has_settings_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if has_settings_table {
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![PAWCHIVE_EXTERNAL_LINKS_MIGRATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if done.as_deref() == Some(PAWCHIVE_EXTERNAL_LINKS_MIGRATION_VERSION) {
            return Ok(());
        }
    }

    let mut last_id = 0i64;
    loop {
        let rows: Vec<(i64, String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT id, COALESCE(content, ''), COALESCE(external_links, '')
                 FROM kemono_posts
                 WHERE COALESCE(content, '') <> '' AND id > ?1
                 ORDER BY id ASC
                 LIMIT ?2",
            )?;
            let mapped = stmt
                .query_map(params![last_id, BATCH], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            mapped
        };
        if rows.is_empty() {
            break;
        }
        let batch_len = rows.len();
        last_id = rows.last().map(|row| row.0).unwrap_or(last_id);
        for (id, content, stored) in rows {
            let links = extract_external_links(&content);
            if links.is_empty() && stored.trim().is_empty() {
                continue;
            }
            if stored_links_match(&stored, &links) {
                continue;
            }
            conn.execute(
                "UPDATE kemono_posts SET external_links = ?1 WHERE id = ?2",
                params![serde_json::to_string(&links)?, id],
            )?;
        }
        if batch_len < BATCH as usize {
            break;
        }
    }

    if has_settings_table {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![
                PAWCHIVE_EXTERNAL_LINKS_MIGRATION_KEY,
                PAWCHIVE_EXTERNAL_LINKS_MIGRATION_VERSION
            ],
        )?;
    }
    Ok(())
}

/// Whether the stored JSON already holds exactly `links`.
///
/// Compared as JSON rather than as text so a row written with different spacing
/// still counts as matching, and order is compared too because the extractor
/// returns document order.
fn stored_links_match(stored: &str, links: &[String]) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(stored) else {
        return false;
    };
    match value.as_array() {
        Some(items) => {
            items.len() == links.len()
                && items
                    .iter()
                    .zip(links)
                    .all(|(item, link)| item.as_str() == Some(link.as_str()))
        }
        None => false,
    }
}

/// Version recorded in `app_settings` for the derived-date backfill.
const PAWCHIVE_LOCAL_DATE_MIGRATION_KEY: &str = "pawchive_local_date_migration";
const PAWCHIVE_LOCAL_DATE_MIGRATION_VERSION: &str = "1";

/// Fill `published_local_date` for rows written before the column existed.
///
/// The shift depends on the service's timezone (JST for fanbox/fantia, UTC for
/// patreon and friends, whatever offset an RFC 3339 stamp carries), which is
/// what `normalize_post_date` encodes and what a bare SQL expression cannot
/// reproduce. So the rows are read here and written back with the same function
/// the naming context uses — the folder name and the stored day come from one
/// implementation, which is the whole point of the exercise.
///
/// Bounded per call and idempotent: a library larger than one batch is filled
/// over successive startups rather than by holding one transaction over every
/// post. `published_local_date = ''` is the work list, so a row already filled
/// is never rewritten, and the version marker stops the read once there is
/// nothing left.
fn backfill_published_local_date(conn: &Connection) -> Result<()> {
    const BATCH: i64 = 2000;

    // Both halves of this migration have to survive a schema old enough to
    // have lost the source stamp: the read below would not resolve, and the
    // `UPDATE` would freeze a day derived from nothing. Substituting `''` for
    // the missing column keeps those rows on the reader's `created_at`
    // fallback, which is where they already were.
    let source_date = post_source_date_column(conn);
    // An id cursor, not an offset. Rows the write below refuses — an empty
    // `published_at`, or a schema whose source column is gone so `source_date`
    // is the literal `''` — keep `published_local_date = ''` and would be
    // selected again from the same offset forever. Advancing past the last id
    // read makes the loop terminate even when an entire batch is unfillable.
    let rows_sql = format!(
        "SELECT p.id, {source_date}, s.service
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         WHERE p.published_local_date = ''
           AND p.id > ?2
         ORDER BY p.id ASC
         LIMIT ?1"
    );
    let write_sql = format!(
        "UPDATE kemono_posts SET published_local_date = ?1 WHERE id = ?2 AND {source_date} <> ''"
    );

    let has_settings_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if has_settings_table {
        let done: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![PAWCHIVE_LOCAL_DATE_MIGRATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if done.as_deref() == Some(PAWCHIVE_LOCAL_DATE_MIGRATION_VERSION) {
            return Ok(());
        }
    }

    let mut after_id = 0_i64;
    loop {
        let rows: Vec<(i64, Option<String>, String)> = {
            let mut stmt = conn.prepare(&rows_sql)?;
            let mapped = stmt
                .query_map(params![BATCH, after_id], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            mapped
        };
        if rows.is_empty() {
            break;
        }
        let filled = rows.len();
        for (id, published_at, service) in rows {
            after_id = id;
            // A post whose `published_at` is empty keeps an empty derived date:
            // the SQL readers fall back to `created_at` for exactly that case,
            // and writing a value derived from nothing would freeze the wrong
            // day into the row.
            let date = normalize_post_date(published_at.as_deref(), Some(service.as_str()));
            conn.execute(&write_sql, params![date, id])?;
        }
        if filled < BATCH as usize {
            break;
        }
    }

    if has_settings_table {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![
                PAWCHIVE_LOCAL_DATE_MIGRATION_KEY,
                PAWCHIVE_LOCAL_DATE_MIGRATION_VERSION
            ],
        )?;
    }
    Ok(())
}

/// Add the columns a legacy database is missing.
///
/// `ALTER TABLE ADD COLUMN` has no `IF NOT EXISTS`, so an unconditional run
/// would fail startup on the second boot of an upgraded database.
fn ensure_columns(conn: &Connection, table: &str, columns: &[(&str, &str)]) -> Result<()> {
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
    Ok(())
}
/// Marker for the one-time move from the directory rule to delivery evidence.
const PAWCHIVE_EVIDENCE_MIGRATION_KEY: &str = "pawchive_evidence_migration";
const PAWCHIVE_EVIDENCE_MIGRATION_VERSION: &str = "1";

/// Marker for the one-time translation of `enabled` into a subscription mode.
const PAWCHIVE_MODE_MIGRATION_KEY: &str = "pawchive_mode_migration";
const PAWCHIVE_MODE_MIGRATION_VERSION: &str = "1";

/// Give subscriptions that existed before `mode` the intent they already had.
///
/// `mode` defaults to manual, which is the right answer for a subscription
/// created today: the user should see what the source has before anything is
/// fetched. It is not the right answer for a row that was already switched on —
/// `enabled = 1` is the user having asked for automatic rounds, and reading it
/// as manual would silently stop a running install. The translation runs once,
/// keyed on a marker, because after it a manual row is a decision the user made
/// and re-running would overwrite it.
fn migrate_subscription_modes(conn: &Connection) -> Result<()> {
    let has_settings_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if has_settings_table {
        let applied: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![PAWCHIVE_MODE_MIGRATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if applied.as_deref() == Some(PAWCHIVE_MODE_MIGRATION_VERSION) {
            return Ok(());
        }
    }
    conn.execute(
        "UPDATE kemono_subscriptions SET mode = 'auto'
         WHERE enabled = 1 AND (mode IS NULL OR mode = '' OR mode = 'manual')",
        [],
    )?;
    if has_settings_table {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![
                PAWCHIVE_MODE_MIGRATION_KEY,
                PAWCHIVE_MODE_MIGRATION_VERSION
            ],
        )?;
    }
    Ok(())
}

/// Move a pre-evidence ledger onto the evidence model, once.
///
/// Additive on purpose. The retired rule — "the post's folder holds a file, so
/// the post is downloaded" — cannot be re-run to check a verdict later, so the
/// migration records what it can no longer prove and marks the rest for review
/// instead of either trusting the old `done` or resetting it to `pending` (a
/// reset would re-download a library the user already has).
fn migrate_pawchive_evidence(conn: &Connection) -> Result<()> {
    // A legacy database handed to this function on its own (a fixture, or a
    // pre-`app_settings` install) has nowhere to store the marker. The steps
    // below are idempotent, so running them again is harmless; failing startup
    // over a missing marker table is not.
    let has_settings_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if has_settings_table {
        let applied: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![PAWCHIVE_EVIDENCE_MIGRATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if applied.as_deref() == Some(PAWCHIVE_EVIDENCE_MIGRATION_VERSION) {
            return Ok(());
        }
    }

    conn.execute(
        "UPDATE kemono_posts SET legacy_status = status WHERE legacy_status IS NULL",
        [],
    )?;

    // Resource identity used to be `{array index}_{remote path}`. The index is
    // a position, not an identity: inserting an attachment renumbered every
    // later row and inserted a second row for a resource already stored.
    // Collapse the duplicates onto the path, then rewrite the key to it.
    conn.execute(
        "DELETE FROM kemono_files
         WHERE id NOT IN (SELECT MIN(id) FROM kemono_files GROUP BY post_id, remote_path)",
        [],
    )?;
    conn.execute(
        "UPDATE kemono_files SET source_identity = remote_path WHERE source_identity != remote_path",
        [],
    )?;

    // An already-`done` row proves that something was settled once, not that
    // this install fetched it. Keep the task state and label the evidence so
    // the evaluator reports the post as 待核验 rather than verified.
    conn.execute(
        "UPDATE kemono_files SET delivery_source = 'legacy'
         WHERE status = 'done' AND delivery_source = ''",
        [],
    )?;

    if has_settings_table {
        conn.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![
                PAWCHIVE_EVIDENCE_MIGRATION_KEY,
                PAWCHIVE_EVIDENCE_MIGRATION_VERSION
            ],
        )?;
    }
    Ok(())
}

// ── Long-term ledger: identity, resource versions, acquisition ──────────────

/// Version recorded in `pawchive_ledger_migration` for the tables below.
const PAWCHIVE_LEDGER_VERSION: &str = "1";

/// Posts materialised per backfill batch.
const PAWCHIVE_LEDGER_BATCH: i64 = 500;

/// Batches one call may run. Startup is a bad place for an unbounded walk over
/// a large working ledger, so an unfinished backfill records where it stopped
/// and continues on the next call.
const PAWCHIVE_LEDGER_BATCHES_PER_CALL: usize = 16;

/// Separator inside a composite ledger key. A unit separator cannot appear in a
/// URL or in the JSON text these keys are built from, so a key cannot be forged
/// by embedding the separator in one of its parts.
const LEDGER_SEP: char = '\u{1f}';

/// Length-prefixed digest of a composite key.
///
/// The parts are length-prefixed rather than joined by a separator because a
/// remote ID is opaque text: `("a", "b\nc")` and `("a\nb", "c")` must not reach
/// the same key, and a truncated manifest must not collide with the full one.
fn ledger_digest(prefix: &str, parts: &[&str]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(parts.len() as u64).to_le_bytes());
    for part in parts {
        hasher.update(&(part.len() as u64).to_le_bytes());
        hasher.update(part.as_bytes());
    }
    format!("{prefix}_{}", hasher.finalize().to_hex())
}

/// The identity of a work: source, service, creator and post ID, exactly as the
/// source wrote them.
///
/// The IDs stay strings. A post ID is not a number — it is opaque text that can
/// exceed the range a client can hold exactly — so converting it to an integer
/// would silently merge two different posts onto one identity.
pub fn work_id_for(site_id: &str, service: &str, creator_id: &str, post_id: &str) -> String {
    ledger_digest("w", &[site_id, service, creator_id, post_id])
}

/// Which resource, in which version. The two are separate questions: a
/// replacement at the same path is a new version of the same resource.
pub fn resource_version_id_for(work_id: &str, resource_key: &str, version_key: &str) -> String {
    ledger_digest("rv", &[work_id, resource_key, version_key])
}

/// The `hash_source` value meaning the site published the digest, as opposed to
/// `local`, which means this install computed it from the bytes it received.
const HASH_SOURCE_SITE: &str = "source";

/// The version key of a resource whose bytes the source never characterised.
///
/// A real answer rather than a placeholder: it says the source offered nothing
/// that could detect a replacement at this path, so re-reading the path cannot
/// promise the content is unchanged.
const UNKNOWN_VERSION: &str = "unknown";

/// The resource key is the source's own stable attachment id.
const IDENTITY_BASIS_SOURCE_ID: &str = "source_id";
/// The resource key is the remote path, standing in for an attachment id that
/// was never recorded.
const IDENTITY_BASIS_PATH: &str = "path";

/// What a version key rests on.
const VERSION_BASIS_SOURCE_DIGEST: &str = "source_digest";
const VERSION_BASIS_STRONG_VALIDATOR: &str = "strong_validator";
const VERSION_BASIS_WEAK_VALIDATOR: &str = "weak_validator";
const VERSION_BASIS_LENGTH_ONLY: &str = "length_only";
const VERSION_BASIS_UNKNOWN: &str = "unknown";

/// Whether the server itself says this validator may be shared by different
/// bytes.
///
/// RFC 9110 writes a weak validator `W/"..."`, and that `W/` is the server
/// declining to promise that a change in the tag means a change in the bytes.
/// Using one as a version key would answer "the content is unchanged" for a
/// resource nobody promised that about.
fn is_weak_validator(value: &str) -> bool {
    value.len() >= 2 && value.as_bytes()[..2].eq_ignore_ascii_case(b"W/")
}

/// The version key for a resource, and the evidence it rests on.
///
/// A version answers one question — which bytes did the source hold — and only
/// a statement strong enough to *detect a replacement* may become a key. The
/// order is the plan's: a digest or revision the source published first, then a
/// strong validator.
///
/// A weak validator or a bare length is recorded as the basis but produces no
/// key. That is the whole point of keeping the distinction: those signals
/// cannot prove two reads saw the same bytes, so promoting one to a version
/// would let "the path did not change" read as "the content did not change".
/// The plan forbids promising to detect replacements the source never made
/// detectable, and `version_basis` is how the next bounded re-read knows which
/// signal to compare.
fn resource_version_evidence(
    expected_blake3: &str,
    hash_source: &str,
    resume_validator: &str,
    expected_length: Option<i64>,
) -> (String, &'static str) {
    // A digest the *site* published. `hash_source = 'local'` deliberately does
    // not count: it is this install's checksum of what it received, which
    // proves the local copy has not changed since, not which bytes the source
    // holds.
    let digest = expected_blake3.trim();
    if !digest.is_empty() && hash_source == HASH_SOURCE_SITE {
        return (digest.to_ascii_lowercase(), VERSION_BASIS_SOURCE_DIGEST);
    }
    if let Some(validator) = ResumeValidator::decode(resume_validator) {
        let value = validator.value.trim();
        if !value.is_empty() {
            if is_weak_validator(value) {
                return (UNKNOWN_VERSION.to_string(), VERSION_BASIS_WEAK_VALIDATOR);
            }
            return (value.to_string(), VERSION_BASIS_STRONG_VALIDATOR);
        }
        if validator.total_length > 0 {
            return (UNKNOWN_VERSION.to_string(), VERSION_BASIS_LENGTH_ONLY);
        }
    }
    if expected_length.unwrap_or(0) > 0 {
        return (UNKNOWN_VERSION.to_string(), VERSION_BASIS_LENGTH_ONLY);
    }
    (UNKNOWN_VERSION.to_string(), VERSION_BASIS_UNKNOWN)
}

/// Whether a resource key is the source's own id or a path standing in for one.
///
/// The working ledger defines `source_identity` as the remote path today, so
/// `path` is the honest answer for every row that exists now. Deriving it
/// rather than hardcoding it means the day discovery starts recording real
/// attachment ids, the ledger says so without a second change here.
fn resource_identity_basis(source_identity: &str, remote_path: &str) -> &'static str {
    if source_identity == remote_path || source_identity == normalize_remote_path(remote_path) {
        IDENTITY_BASIS_PATH
    } else {
        IDENTITY_BASIS_SOURCE_ID
    }
}

fn manifest_id_for(
    work_id: &str,
    fingerprint: &str,
    completeness: &str,
    source_revision: &str,
) -> String {
    ledger_digest("m", &[work_id, fingerprint, completeness, source_revision])
}

fn acquisition_event_id_for(idempotency_key: &str) -> String {
    ledger_digest("e", &[idempotency_key])
}

fn acquisition_idempotency_key(
    resource_version_id: &str,
    evidence_source: &str,
    publish_job_id: Option<i64>,
) -> String {
    let job = publish_job_id.map(|id| id.to_string()).unwrap_or_default();
    format!("{resource_version_id}{LEDGER_SEP}{evidence_source}{LEDGER_SEP}{job}")
}

/// The identity of a decision on the long-term ledger.
///
/// Keyed on the working ledger's own decision row, which is the only ordering
/// that stays stable as posts are re-discovered. Both write paths — the live
/// decision and the backfill that copies it later — derive the same identity
/// from it, so one working decision is one long-term decision whichever path
/// records it first. The middle part of the digest is a fixed domain tag, kept
/// exactly as the backfill first wrote it so identities an earlier version
/// recorded still resolve to the same row.
fn decision_id_for(work_id: &str, working_decision_id: i64) -> String {
    ledger_digest("d", &[work_id, "legacy", &working_decision_id.to_string()])
}

/// Canonical fingerprint of a manifest's member set.
///
/// Members are `(resource_version_id, required_role)` pairs, deduplicated and
/// sorted, so a re-ordered attachment list is the same manifest.
fn ledger_fingerprint(members: &mut Vec<String>) -> String {
    members.sort();
    members.dedup();
    let borrowed: Vec<&str> = members.iter().map(|member| member.as_str()).collect();
    ledger_digest("mf", &borrowed)
}

/// The idempotency key of an acquisition that no publish ledger row produced.
///
/// A publish-backed acquisition is keyed on its job, so two reports about the
/// same job are one fact. An external receipt has no publish job: the key is
/// the report itself — one settled delivery per resource version and task.
fn external_receipt_idempotency_key(resource_version_id: &str, task_id: &str) -> String {
    ledger_digest("acq", &["external_receipt", resource_version_id, task_id])
}

/// Insert one acquisition event, ignoring an exact repeat.
///
/// The long-term ledger is append-only and its idempotency key is the fact:
/// inserting the same key twice means the same thing happened twice on paper,
/// so the newer reading only refreshes when the bytes were seen.
#[allow(clippy::too_many_arguments)]
fn insert_acquisition_event(
    conn: &Connection,
    idempotency_key: &str,
    work_id: &str,
    resource_version_id: &str,
    attempt_id: &str,
    source: EvidenceSource,
    publish_job_id: Option<i64>,
    acquired_at: &str,
    historical_path: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pawchive_acquisition_events
             (event_id, idempotency_key, work_id, resource_version_id, attempt_id,
              evidence_source, evidence_strength, publish_job_id, acquired_at,
              historical_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
         ON CONFLICT(idempotency_key) DO UPDATE SET acquired_at = excluded.acquired_at",
        params![
            acquisition_event_id_for(idempotency_key),
            idempotency_key,
            work_id,
            resource_version_id,
            attempt_id,
            source.as_str(),
            if source.is_strong() {
                "strong"
            } else {
                "unproven"
            },
            publish_job_id,
            acquired_at,
            historical_path,
            now,
        ],
    )?;
    Ok(())
}

/// Record what one settled external receipt proved, on the long-term ledger.
///
/// The working receipt row is keyed on the post and dies with the
/// subscription's cascade, so on its own a verified delivery would be
/// forgettable: drop the subscription, re-add it, and the downloader's own
/// report of a delivered bundle would be gone. The acquisition events written
/// here are keyed on the work identity and the resource version instead, so
/// they stay, and a re-discovered post resolves to the same resource version
/// rather than to a second, unfetched one.
///
/// `covered` carries the resource version and the output path that proved it.
/// They are written empty for a receipt that settled nothing: the refusal is a
/// fact worth keeping, and every event carries the strength of the source that
/// produced it.
fn record_external_receipt_acquisition(
    conn: &Connection,
    work_id: &str,
    receipt: &ExternalReceipt,
    covered: &[(String, String)],
    settles: bool,
    now: &str,
) -> Result<usize> {
    let source = if settles {
        EvidenceSource::ExternalReceipt
    } else {
        EvidenceSource::ObservedOnDisk
    };
    // A settled receipt that named no resource version still has to leave a
    // trace on the work, or the work would read as never delivered. The marker
    // is a resource version derived from the task, never a claim about a file.
    let empty_marker = resource_version_id_for(work_id, "external_receipt", &receipt.task_id);
    let mut written = 0;
    if covered.is_empty() {
        insert_acquisition_event(
            conn,
            &external_receipt_idempotency_key(&empty_marker, &receipt.task_id),
            work_id,
            &empty_marker,
            "",
            source,
            None,
            now,
            "",
            now,
        )?;
        return Ok(1);
    }
    for (resource_version_id, path) in covered {
        insert_acquisition_event(
            conn,
            &external_receipt_idempotency_key(resource_version_id, &receipt.task_id),
            work_id,
            resource_version_id,
            "",
            source,
            None,
            now,
            path,
            now,
        )?;
        written += 1;
    }
    Ok(written)
}

/// The resource versions a receipt's output paths prove, and which required
/// resource versions the receipt covers without naming a path.
///
/// A path the receipt lists is matched to the direct resource whose target is
/// that exact path, so the event lands on the resource the file belongs to.
/// External bundles have no target path — the bridge delivers one bundle for
/// the whole link — so a settled receipt covers the work's external resources
/// once it has proven at least one real output, and covers nothing when it
/// proved none.
fn receipt_covered_resources(
    conn: &Connection,
    work_id: &str,
    post_db_id: i64,
    receipt: &ExternalReceipt,
    settlements: bool,
) -> Result<Vec<(String, String)>> {
    let mut covered: Vec<(String, String)> = Vec::new();
    if !settlements {
        return Ok(covered);
    }
    for path in &receipt.output_paths {
        let file_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM kemono_files
                 WHERE post_id = ?1 AND target_path = ?2 AND present_in_manifest = 1
                 ORDER BY id LIMIT 1",
                params![post_db_id, path],
                |row| row.get(0),
            )
            .optional()?;
        let Some(file_id) = file_id else {
            continue;
        };
        let resource: Option<(String, String, String, String, Option<i64>)> = conn
            .query_row(
                "SELECT source_identity, COALESCE(expected_blake3, ''), hash_source,
                        resume_validator, expected_length
                 FROM kemono_files WHERE id = ?1",
                params![file_id],
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
        if let Some((resource_key, digest, hash_source, validator, length)) = resource {
            let (version_key, _) =
                resource_version_evidence(&digest, &hash_source, &validator, length);
            let resource_version_id = resource_version_id_for(work_id, &resource_key, &version_key);
            covered.push((resource_version_id, path.clone()));
        }
    }
    // The external bundles the work requires. A bridge delivers one bundle per
    // link and reports the delivery for the whole task, so a settled receipt
    // covers every bundle the work still requires; that is what makes the work
    // read as delivered rather than as a bundle nobody ever reported.
    let links: Option<String> = conn
        .query_row(
            "SELECT external_links FROM kemono_posts WHERE id = ?1",
            params![post_db_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    let mut links: Vec<String> = links
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .unwrap_or_default();
    // A JD task can deliver only a selected subset of this post's shares.
    // Its archive must not settle other shares (or the post's image assets).
    let has_tasks: bool = conn.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='netdisk_bridge_tasks')",
        [], |r| r.get(0))?;
    if has_tasks {
        let selected: Option<String> = conn.query_row(
            "SELECT links FROM netdisk_bridge_tasks WHERE task_id=?1 AND bridge_id=?2 AND post_id=?3 AND manifest_version=?4",
            params![receipt.task_id,receipt.bridge_id,post_db_id,receipt.manifest_version], |r|r.get(0)).optional()?;
        if let Some(selected) = selected {
            let selected: Vec<String> = serde_json::from_str(&selected)?;
            links.retain(|link| selected.contains(link));
        }
    }
    for link in links {
        let resource_version_id =
            resource_version_id_for(work_id, &format!("external:{link}"), UNKNOWN_VERSION);
        covered.push((resource_version_id, String::new()));
    }
    Ok(covered)
}

/// How much of a work's resource set an observation actually saw.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManifestCompleteness {
    /// The source returned a response that lists the whole set — including an
    /// explicitly empty one.
    Complete,
    /// Part of the set was read and something in it could not be.
    Partial,
    /// Nothing in the observation says how much was seen.
    Unknown,
    /// Inherited from a ledger written before the source's field presence was
    /// recorded. Re-reading the row cannot upgrade it: the information was
    /// never stored, so this stays unknown until a fresh observation arrives.
    LegacyUnknown,
}

impl ManifestCompleteness {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::Partial => "partial",
            Self::Unknown => "unknown",
            Self::LegacyUnknown => "legacy_unknown",
        }
    }

    /// Read a stored completeness value back.
    ///
    /// Anything unrecognised is `Unknown`, never `Complete`: a value this build
    /// does not understand is a value it cannot claim saw the whole resource
    /// set, and the two mistakes are not symmetric — reading a partial
    /// observation as complete retires resources the source still lists.
    pub fn parse(stored: &str) -> Self {
        match stored {
            "complete" => Self::Complete,
            "partial" => Self::Partial,
            "legacy_unknown" => Self::LegacyUnknown,
            _ => Self::Unknown,
        }
    }

    /// Whether this observation may become the work's current manifest.
    ///
    /// Only a complete observation replaces the current one. Anything less is
    /// kept beside it: a summary that omitted `attachments` is not evidence
    /// that the attachments were withdrawn, and reading it as such would retire
    /// resources the source still lists.
    pub fn may_replace_current(self) -> bool {
        matches!(self, Self::Complete)
    }
}

/// What proved an acquisition, and how far it can be trusted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EvidenceSource {
    /// This install published the bytes and wrote the publish ledger row.
    PublishLedger,
    /// A verified publish recovered from the ledger after a restart.
    ReconcileRestore,
    /// A paired external-downloader bridge reported the delivery settled, with
    /// its outputs listed as real files again at record time.
    ExternalReceipt,
    /// Bytes seen on disk with no record of who put them there.
    ObservedOnDisk,
    /// Inherited from the retired "the folder is not empty, so it is done" rule.
    LegacyDirectoryRule,
}

impl EvidenceSource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::PublishLedger => "publish_ledger",
            Self::ReconcileRestore => "reconcile_restore",
            Self::ExternalReceipt => "external_receipt",
            Self::ObservedOnDisk => "observed_on_disk",
            Self::LegacyDirectoryRule => "legacy_directory_rule",
        }
    }

    /// Whether an event from this source counts as a fetch.
    ///
    /// Presence is not proof. A file on disk, a non-empty folder and a legacy
    /// `done` all say something is there; none of them says this install
    /// fetched it, and none of them may satisfy an automatic requirement.
    ///
    /// An external receipt does count, and only when it settled: by the time an
    /// event is written for one the receipt has already passed the pairing
    /// check and had every one of its outputs stat'd as a non-empty regular
    /// file inside the authorized roots. A receipt that settled nothing writes
    /// its event as unproven instead, so the ledger never reads a refused
    /// report as a fetch.
    pub fn is_strong(self) -> bool {
        matches!(
            self,
            Self::PublishLedger | Self::ReconcileRestore | Self::ExternalReceipt
        )
    }
}

/// How far the ledger backfill got.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerMigrationProgress {
    pub posts_migrated: usize,
    pub works_high_water_mark: i64,
    pub works_ready: bool,
    pub decisions_high_water_mark: i64,
    pub decisions_ready: bool,
}

impl LedgerMigrationProgress {
    pub fn is_ready(&self) -> bool {
        self.works_ready && self.decisions_ready
    }
}

/// A recorded position of the ledger backfill.
struct LedgerMarker {
    version: String,
    high_water_mark: i64,
    status: String,
}

fn read_ledger_marker(conn: &Connection, name: &str) -> Result<Option<LedgerMarker>> {
    Ok(conn
        .query_row(
            "SELECT version, high_water_mark, status FROM pawchive_ledger_migration
             WHERE name = ?1",
            params![name],
            |row| {
                Ok(LedgerMarker {
                    version: row.get(0)?,
                    high_water_mark: row.get(1)?,
                    status: row.get(2)?,
                })
            },
        )
        .optional()?)
}

fn write_ledger_marker(
    conn: &Connection,
    name: &str,
    high_water_mark: i64,
    status: &str,
    now: &str,
) -> Result<()> {
    conn.execute(
        "INSERT INTO pawchive_ledger_migration
             (name, version, high_water_mark, status, note, updated_at)
         VALUES (?1, ?2, ?3, ?4, '', ?5)
         ON CONFLICT(name) DO UPDATE SET
             version = excluded.version,
             high_water_mark = excluded.high_water_mark,
             status = excluded.status,
             updated_at = excluded.updated_at",
        params![name, PAWCHIVE_LEDGER_VERSION, high_water_mark, status, now],
    )?;
    Ok(())
}

/// The long-term identity of a working-ledger post.
fn work_id_for_post(conn: &Connection, post_db_id: i64) -> Result<Option<String>> {
    let row: Option<(String, String, String, String)> = conn
        .query_row(
            "SELECT s.site_id, s.service, s.user_id, p.post_id
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE p.id = ?1",
            params![post_db_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    Ok(row.map(|(site_id, service, creator_id, post_id)| {
        work_id_for(&site_id, &service, &creator_id, &post_id)
    }))
}

/// One working-ledger file row, read for materialisation.
struct LedgerFileRow {
    id: i64,
    source_identity: String,
    remote_path: String,
    file_type: String,
    expected_length: Option<i64>,
    expected_blake3: String,
    hash_source: String,
    delivery_source: String,
    integrity: String,
    target_path: String,
    publish_job_id: Option<i64>,
    present_in_manifest: bool,
    resume_validator: String,
    acquired_at: String,
}

/// Move one resource's acquisition history onto a new resource key.
///
/// The plan is explicit that an identity upgrade cannot be a key edit on its
/// own: the long-term ledger is keyed on the resource, and rewriting the key
/// while leaving the history on the old one would read every already-downloaded
/// resource as never fetched. So the events move with the key, in one
/// transaction, and the only thing that ever happens to the move is a rename —
/// never a re-fetch.
///
/// `Unchanged` is the ordinary answer for a resource whose identity did not
/// move. `Conflict` is the answer for a key that is already occupied: merging
/// would silently declare two resources one, which is the opposite of what the
/// identity work is for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IdentityMove {
    Moved { from: String, to: String },
    Unchanged,
    Conflict { occupied_by: i64 },
    NotFound,
}

/// Whether a working row's identity may be upgraded to the source's own id.
///
/// The proof required is small and checkable: the row is currently keyed by a
/// path, that path is exactly the location the source is serving now, and the
/// new key is not already taken. A row keyed by something that is not its path
/// is a real source id already — a different situation, and not this function's
/// business.
fn identity_move_allowed(
    conn: &Connection,
    file_id: i64,
    remote_path: &str,
    new_identity: &str,
) -> Result<Result<(), IdentityMove>> {
    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT source_identity, remote_path FROM kemono_files WHERE id = ?1",
            params![file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((source_identity, stored_path)) = row else {
        return Ok(Err(IdentityMove::NotFound));
    };
    let normalized = normalize_remote_path(remote_path);
    if source_identity != normalized || stored_path != normalized {
        // Either the row is already keyed by its source id, or its key and its
        // path disagree — in both cases this is not a path-key upgrade and a
        // move would be guesswork.
        return Ok(Err(IdentityMove::Unchanged));
    }
    if new_identity.is_empty() || new_identity == normalized {
        return Ok(Err(IdentityMove::Unchanged));
    }
    let occupant: Option<i64> = conn
        .query_row(
            "SELECT id FROM kemono_files
             WHERE post_id = (SELECT post_id FROM kemono_files WHERE id = ?1)
               AND source_identity = ?2",
            params![file_id, new_identity],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(occupied_by) = occupant {
        return Ok(Err(IdentityMove::Conflict { occupied_by }));
    }
    Ok(Ok(()))
}

fn move_resource_identity_exec(
    conn: &Connection,
    file_id: i64,
    remote_path: &str,
    new_identity: &str,
) -> Result<IdentityMove> {
    if let Err(outcome) = identity_move_allowed(conn, file_id, remote_path, new_identity)? {
        return Ok(outcome);
    }
    let normalized = normalize_remote_path(remote_path);
    let work_id: Option<String> = conn
        .query_row(
            "SELECT w.work_id FROM kemono_files f
         JOIN kemono_posts p ON p.id = f.post_id
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         JOIN pawchive_remote_works w
              ON w.site_id = s.site_id AND w.service = s.service
             AND w.creator_id = s.user_id AND w.post_id = p.post_id
         WHERE f.id = ?1",
            params![file_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(work_id) = work_id else {
        // No long-term identity means no events to carry; the working row is the
        // whole of the ledger for this resource, and re-keying it is still the
        // right move.
        conn.execute(
            "UPDATE kemono_files
             SET source_identity = ?1, identity_basis = ?2, updated_at = ?3
             WHERE id = ?4",
            params![
                new_identity,
                IDENTITY_BASIS_SOURCE_ID,
                Utc::now().to_rfc3339(),
                file_id
            ],
        )?;
        return Ok(IdentityMove::Moved {
            from: normalized,
            to: new_identity.to_string(),
        });
    };

    let old_version = resource_version_id_for(&work_id, &normalized, UNKNOWN_VERSION);
    let new_version = resource_version_id_for(&work_id, new_identity, UNKNOWN_VERSION);
    conn.execute(
        "UPDATE kemono_files
         SET source_identity = ?1, identity_basis = ?2, updated_at = ?3
         WHERE id = ?4",
        params![
            new_identity,
            IDENTITY_BASIS_SOURCE_ID,
            Utc::now().to_rfc3339(),
            file_id
        ],
    )?;
    // The resource version moves with its events. If the destination already
    // exists (a previous partial move, or the source listing the same resource
    // twice) the events are re-pointed at it and the duplicate row is dropped,
    // which is a merge of two rows that name the same resource and no more.
    let destination_exists: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pawchive_resource_versions WHERE resource_version_id = ?1",
        params![new_version],
        |row| row.get(0),
    )?;
    if destination_exists == 0 {
        conn.execute(
            "UPDATE pawchive_resource_versions
             SET resource_version_id = ?1, resource_key = ?2, identity_basis = ?3
             WHERE resource_version_id = ?4",
            params![
                new_version,
                new_identity,
                IDENTITY_BASIS_SOURCE_ID,
                old_version
            ],
        )?;
    } else {
        conn.execute(
            "DELETE FROM pawchive_resource_versions WHERE resource_version_id = ?1",
            params![old_version],
        )?;
    }
    for table in [
        "pawchive_acquisition_events",
        "pawchive_manifest_members",
        "pawchive_decision_members",
    ] {
        let column = "resource_version_id";
        let duplicate: i64 = conn.query_row(
            &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
            params![new_version],
            |row| row.get(0),
        )?;
        if duplicate > 0 && table != "pawchive_acquisition_events" {
            // A member row for the destination already exists, so the old row is
            // the duplicate and goes; a decision member keeps its decision.
            conn.execute(
                &format!("DELETE FROM {table} WHERE {column} = ?1"),
                params![old_version],
            )?;
            continue;
        }
        conn.execute(
            &format!("UPDATE {table} SET {column} = ?1 WHERE {column} = ?2"),
            params![new_version, old_version],
        )?;
    }
    Ok(IdentityMove::Moved {
        from: normalized,
        to: new_identity.to_string(),
    })
}

/// Perform the move: re-key the working row, and carry the resource version and
/// its events across with it.
pub fn move_resource_identity(
    conn: &Connection,
    file_id: i64,
    remote_path: &str,
    new_identity: &str,
) -> Result<IdentityMove> {
    if conn.is_autocommit() {
        let tx = conn.unchecked_transaction()?;
        let res = move_resource_identity_exec(&tx, file_id, remote_path, new_identity)?;
        tx.commit()?;
        Ok(res)
    } else {
        move_resource_identity_exec(conn, file_id, remote_path, new_identity)
    }
}

/// Promote one working-ledger post into the long-term ledger.
///
/// Idempotent: every statement is an upsert keyed on the fact it records, so a
/// resumed backfill, a repeated call at delete time and a live double write all
/// converge on the same rows. Returns the work ID, or `None` when the post has
/// no subscription to take its identity from.
///
/// This only writes the long-term tables. It deliberately does not change what
/// the working ledger contains, so a database that has been through it behaves
/// exactly as it did before.
pub fn materialize_work_ledger(conn: &Connection, post_db_id: i64) -> Result<Option<String>> {
    materialize_work_ledger_inner(conn, post_db_id, ManifestCompleteness::LegacyUnknown, false)
}

/// Record one discovery observation on the long-term ledger.
///
/// The live half of the ledger: it keeps the work's identity, its scheduling
/// binding and how much of the resource set this response actually listed. It
/// is gated, because a discovery round re-sees every post it already knows and
/// re-deriving each one would make the walk cost grow with the library instead
/// of with what changed.
pub fn record_work_observation(
    conn: &Connection,
    post_db_id: i64,
    completeness: ManifestCompleteness,
) -> Result<Option<String>> {
    materialize_work_ledger_inner(conn, post_db_id, completeness, true)
}

/// Whether the ledger already holds this work at this manifest revision.
///
/// `observation` is the completeness about to be recorded, or `None` to ask
/// only whether the work is on the ledger at all. The scheduling binding is
/// part of the answer: dropping a subscription removes it, and this call is
/// what puts it back, so a work whose binding is gone is never up to date.
fn ledger_holds_observation(
    conn: &Connection,
    subscription_id: i64,
    work_id: &str,
    source_revision: &str,
    observation: Option<ManifestCompleteness>,
) -> Result<bool> {
    let bound: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pawchive_subscription_works
         WHERE subscription_id = ?1 AND work_id = ?2",
        params![subscription_id, work_id],
        |row| row.get(0),
    )?;
    if bound == 0 {
        return Ok(false);
    }
    let current: Option<(String, String)> = conn
        .query_row(
            "SELECT source_revision, completeness FROM pawchive_work_manifests
             WHERE work_id = ?1 AND is_current = 1",
            params![work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_revision, stored_completeness)) = current else {
        return Ok(false);
    };
    if stored_revision != source_revision {
        return Ok(false);
    }
    match observation {
        None => Ok(true),
        Some(wanted) => Ok(stored_completeness == wanted.as_str()),
    }
}

struct PostManifestSourceRow {
    subscription_id: i64,
    site_id: String,
    service: String,
    creator_id: String,
    post_id: String,
    title: Option<String>,
    published_at: Option<String>,
    edited_at: Option<String>,
    content: Option<String>,
    manifest_version: i64,
    _stored_fingerprint: String,
    external_links: Option<String>,
}

fn materialize_work_ledger_inner(
    conn: &Connection,
    post_db_id: i64,
    completeness: ManifestCompleteness,
    gated: bool,
) -> Result<Option<String>> {
    let post: Option<PostManifestSourceRow> = conn
        .query_row(
            "SELECT p.subscription_id, s.site_id, s.service, s.user_id, p.post_id, p.title,
                    p.published_at, p.edited_at, p.content, p.manifest_version,
                    p.manifest_fingerprint, p.external_links
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE p.id = ?1",
            params![post_db_id],
            |row| {
                Ok(PostManifestSourceRow {
                    subscription_id: row.get(0)?,
                    site_id: row.get(1)?,
                    service: row.get(2)?,
                    creator_id: row.get(3)?,
                    post_id: row.get(4)?,
                    title: row.get(5)?,
                    published_at: row.get(6)?,
                    edited_at: row.get(7)?,
                    content: row.get(8)?,
                    manifest_version: row.get(9)?,
                    _stored_fingerprint: row.get(10)?,
                    external_links: row.get(11)?,
                })
            },
        )
        .optional()?;
    let Some(PostManifestSourceRow {
        subscription_id,
        site_id,
        service,
        creator_id,
        post_id,
        title,
        published_at,
        edited_at,
        content,
        manifest_version,
        _stored_fingerprint,
        external_links,
    }) = post
    else {
        return Ok(None);
    };

    let now = Utc::now().to_rfc3339();
    let work_id = work_id_for(&site_id, &service, &creator_id, &post_id);

    // Steady state: this observation is already on the ledger, so there is
    // nothing to derive. The caller that walks the whole library passes
    // `gated = false` instead, because it is bounded by its own high-water mark
    // and its call is the last chance to keep a work's history before a delete.
    if gated
        && ledger_holds_observation(
            conn,
            subscription_id,
            &work_id,
            &manifest_version.to_string(),
            Some(completeness),
        )?
    {
        return Ok(Some(work_id));
    }

    // Raw metadata, revisioned. The revision moves only when the source's own
    // fields change, so a re-discovery that saw the same post is not a new
    // revision and cannot invalidate a decision by itself.
    let raw_metadata = serde_json::json!({
        "post_id": post_id,
        "title": title,
        "published_at": published_at,
        "edited_at": edited_at,
        "content": content,
    })
    .to_string();
    let stored: Option<(String, i64)> = conn
        .query_row(
            "SELECT raw_metadata, metadata_revision FROM pawchive_remote_works WHERE work_id = ?1",
            params![work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    match stored {
        None => {
            conn.execute(
                "INSERT INTO pawchive_remote_works
                     (work_id, site_id, service, creator_id, post_id, raw_metadata,
                      metadata_revision, created_at, updated_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, 1, ?7, ?7)",
                params![
                    work_id,
                    site_id,
                    service,
                    creator_id,
                    post_id,
                    raw_metadata,
                    now
                ],
            )?;
        }
        Some((previous, revision)) => {
            if previous != raw_metadata {
                conn.execute(
                    "UPDATE pawchive_remote_works
                     SET raw_metadata = ?1, metadata_revision = ?2, updated_at = ?3
                     WHERE work_id = ?4",
                    params![raw_metadata, revision + 1, now, work_id],
                )?;
            }
        }
    }

    // A row here is the scheduling relation, not the work's existence.
    conn.execute(
        "INSERT INTO pawchive_subscription_works
             (subscription_id, work_id, discovery_state, created_at, updated_at)
         VALUES (?1, ?2, 'discovered', ?3, ?3)
         ON CONFLICT(subscription_id, work_id) DO UPDATE SET updated_at = excluded.updated_at",
        params![subscription_id, work_id, now],
    )?;

    let files: Vec<LedgerFileRow> = conn
        .prepare(
            "SELECT id, source_identity, remote_path, file_type, expected_length,
                    COALESCE(expected_blake3, ''), hash_source, delivery_source, integrity,
                    target_path, publish_job_id, present_in_manifest, resume_validator,
                    COALESCE(NULLIF(verified_at, ''), updated_at)
             FROM kemono_files WHERE post_id = ?1 ORDER BY id",
        )?
        .query_map(params![post_db_id], |row| {
            Ok(LedgerFileRow {
                id: row.get(0)?,
                source_identity: row.get(1)?,
                remote_path: row.get(2)?,
                file_type: row.get(3)?,
                expected_length: row.get(4)?,
                expected_blake3: row.get(5)?,
                hash_source: row.get(6)?,
                delivery_source: row.get(7)?,
                integrity: row.get(8)?,
                target_path: row.get(9)?,
                publish_job_id: row.get(10)?,
                present_in_manifest: row.get::<_, i64>(11)? != 0,
                resume_validator: row.get(12)?,
                acquired_at: row.get(13)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;

    let mut members: Vec<String> = Vec::new();
    for file in &files {
        // Identity and version are separate questions. The key says which
        // resource; the version says which bytes the source held. Only evidence
        // strong enough to detect a replacement becomes a version, so a site
        // that offers no revision and no strong digest is recorded as
        // `unknown` rather than having "the path is unchanged" read as "the
        // content is unchanged".
        let resource_key = file.source_identity.clone();
        let identity_basis = resource_identity_basis(&file.source_identity, &file.remote_path);
        let (version_key, version_basis) = resource_version_evidence(
            &file.expected_blake3,
            &file.hash_source,
            &file.resume_validator,
            file.expected_length,
        );
        let resource_version_id = resource_version_id_for(&work_id, &resource_key, &version_key);
        conn.execute(
            "INSERT INTO pawchive_resource_versions
                 (resource_version_id, work_id, resource_key, version_key, identity_basis,
                  version_basis, raw_ref, file_type, digest, digest_source, length,
                  validator, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13)
             ON CONFLICT(work_id, resource_key, version_key) DO UPDATE SET
                 raw_ref = excluded.raw_ref,
                 file_type = excluded.file_type,
                 length = COALESCE(excluded.length, pawchive_resource_versions.length),
                 validator = CASE WHEN excluded.validator != ''
                                  THEN excluded.validator
                                  ELSE pawchive_resource_versions.validator END,
                 -- A digest already recorded for this version is never replaced:
                 -- different bytes at the same path are a different version, not
                 -- a correction to this one.
                 digest = CASE WHEN pawchive_resource_versions.digest = ''
                               THEN excluded.digest
                               ELSE pawchive_resource_versions.digest END,
                 digest_source = CASE WHEN pawchive_resource_versions.digest_source = ''
                                      THEN excluded.digest_source
                                      ELSE pawchive_resource_versions.digest_source END",
            params![
                resource_version_id,
                work_id,
                resource_key,
                version_key,
                identity_basis,
                version_basis,
                file.remote_path,
                file.file_type,
                file.expected_blake3,
                file.hash_source,
                file.expected_length,
                file.resume_validator,
                now
            ],
        )?;

        // Only a verified publish proves a fetch. A recovered publish ledger row
        // is the same fact read back; anything else is presence.
        let evidence = match file.delivery_source.as_str() {
            "download" if file.integrity == "verified" && file.publish_job_id.is_some() => {
                Some(EvidenceSource::PublishLedger)
            }
            "download" if file.integrity == "verified" => Some(EvidenceSource::ObservedOnDisk),
            "present" => Some(EvidenceSource::ObservedOnDisk),
            "legacy" => Some(EvidenceSource::LegacyDirectoryRule),
            _ => None,
        };
        if let Some(evidence) = evidence {
            let key = acquisition_idempotency_key(
                &resource_version_id,
                evidence.as_str(),
                file.publish_job_id,
            );
            conn.execute(
                "INSERT INTO pawchive_acquisition_events
                     (event_id, idempotency_key, work_id, resource_version_id, attempt_id,
                      evidence_source, evidence_strength, publish_job_id, acquired_at,
                      historical_path, created_at)
                 VALUES (?1, ?2, ?3, ?4, '', ?5, ?6, ?7, ?8, ?9, ?10)
                 ON CONFLICT(idempotency_key) DO NOTHING",
                params![
                    acquisition_event_id_for(&key),
                    key,
                    work_id,
                    resource_version_id,
                    evidence.as_str(),
                    if evidence.is_strong() {
                        "strong"
                    } else {
                        "unproven"
                    },
                    file.publish_job_id,
                    file.acquired_at,
                    file.target_path,
                    now
                ],
            )?;
        }

        if file.present_in_manifest {
            members.push(format!("{resource_version_id}{LEDGER_SEP}direct"));
        }
        let _ = file.id;
    }

    // An external bundle is part of what the work needs delivered, and it has
    // its own identity: swapping the link for a different one is a different
    // requirement, not the same one re-observed.
    let links: Vec<String> = external_links
        .as_deref()
        .and_then(|json| serde_json::from_str::<Vec<String>>(json).ok())
        .unwrap_or_default();
    for link in links {
        let resource_key = format!("external:{link}");
        let resource_version_id = resource_version_id_for(&work_id, &resource_key, "unknown");
        conn.execute(
            "INSERT INTO pawchive_resource_versions
                 (resource_version_id, work_id, resource_key, version_key, identity_basis,
                  version_basis, raw_ref, file_type, digest, digest_source, length,
                  validator, created_at)
             VALUES (?1, ?2, ?3, 'unknown', 'external_link', 'unknown', ?3, 'external',
                     '', '', NULL, '', ?4)
             ON CONFLICT(work_id, resource_key, version_key) DO UPDATE SET
                 raw_ref = excluded.raw_ref",
            params![resource_version_id, work_id, resource_key, now],
        )?;
        members.push(format!("{resource_version_id}{LEDGER_SEP}external"));
    }

    let fingerprint = ledger_fingerprint(&mut members);
    let manifest_id = manifest_id_for(
        &work_id,
        &fingerprint,
        completeness.as_str(),
        &manifest_version.to_string(),
    );
    conn.execute(
        "INSERT INTO pawchive_work_manifests
             (manifest_id, work_id, member_fingerprint, completeness, source_revision,
              observed_at, is_current)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0)
         ON CONFLICT(manifest_id) DO UPDATE SET observed_at = excluded.observed_at",
        params![
            manifest_id,
            work_id,
            fingerprint,
            completeness.as_str(),
            manifest_version.to_string(),
            now
        ],
    )?;
    for member in &members {
        let (resource_version_id, role) =
            member.split_once(LEDGER_SEP).unwrap_or((member, "direct"));
        conn.execute(
            "INSERT INTO pawchive_manifest_members
                 (manifest_id, resource_version_id, required_role)
             VALUES (?1, ?2, ?3)
             ON CONFLICT DO NOTHING",
            params![manifest_id, resource_version_id, role],
        )?;
    }
    set_current_manifest(conn, &work_id, &manifest_id, completeness)?;

    materialize_legacy_decisions(conn, post_db_id, &work_id)?;
    Ok(Some(work_id))
}

/// Point a work at an observation, unless the current one is complete and this
/// one is not.
///
/// An incomplete observation is kept as evidence of what was seen; it never
/// becomes what the work requires.
fn set_current_manifest(
    conn: &Connection,
    work_id: &str,
    manifest_id: &str,
    completeness: ManifestCompleteness,
) -> Result<()> {
    let current_is_complete: i64 = conn.query_row(
        "SELECT COUNT(*) FROM pawchive_work_manifests
         WHERE work_id = ?1 AND is_current = 1 AND completeness = 'complete'",
        params![work_id],
        |row| row.get(0),
    )?;
    if current_is_complete > 0 && !completeness.may_replace_current() {
        return Ok(());
    }
    conn.execute(
        "UPDATE pawchive_work_manifests
         SET is_current = 0
         WHERE work_id = ?1 AND is_current = 1 AND manifest_id != ?2",
        params![work_id, manifest_id],
    )?;
    conn.execute(
        "UPDATE pawchive_work_manifests SET is_current = 1 WHERE manifest_id = ?1",
        params![manifest_id],
    )?;
    Ok(())
}

/// Copy the decisions the working ledger holds for one post.
///
/// The scope is recorded as `legacy_unknown`: an older confirmation said "I
/// have this work", not "I have these files", and the resource set it covered
/// was never stored. Reconstructing it from today's type switches would invent
/// a scope the user never chose, so the copy keeps the original action, time,
/// bound path and manifest version and claims no members.
fn materialize_legacy_decisions(
    conn: &Connection,
    post_db_id: i64,
    work_id: &str,
) -> Result<usize> {
    let has_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'pawchive_post_decisions'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if !has_table {
        return Ok(0);
    }
    let rows: Vec<(i64, i64, String, String, String, String)> = conn
        .prepare(
            "SELECT id, manifest_version, action, reason, bound_path, created_at
             FROM pawchive_post_decisions WHERE post_id = ?1 ORDER BY id",
        )?
        .query_map(params![post_db_id], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut copied = 0;
    for (legacy_id, manifest_version, action, reason, bound_path, created_at) in rows {
        // Target-less, because two uniqueness constraints can reject this row:
        // the decision id, and the working row id it is keyed on. A decision the
        // live write already recorded hits both, and either one being a no-op is
        // the right answer — the row is there, and the live write knows more
        // about it than this copy does.
        copied += conn.execute(
            "INSERT INTO pawchive_user_decisions
                 (decision_id, work_id, action, scope_kind, manifest_id, manifest_version,
                  reason, bound_path, legacy_post_id, legacy_decision_id, created_at, revoked_by)
             VALUES (?1, ?2, ?3, 'legacy_unknown', '', ?4, ?5, ?6, ?7, ?8, ?9, '')
             ON CONFLICT DO NOTHING",
            params![
                decision_id_for(work_id, legacy_id),
                work_id,
                action,
                manifest_version,
                reason,
                bound_path,
                post_db_id,
                legacy_id,
                created_at
            ],
        )?;
    }
    Ok(copied)
}

/// Bring the long-term ledger up to date from the working ledger.
///
/// Bounded and resumable. Each call advances at most
/// `PAWCHIVE_LEDGER_BATCHES_PER_CALL` batches of `PAWCHIVE_LEDGER_BATCH` posts
/// and records where it stopped, so a large working ledger is walked in steps
/// instead of one transaction. Nothing here changes how the working ledger is
/// read or written, so a partially migrated database still behaves exactly as
/// it did before, and the step is safe to run on every startup.
///
/// A startup that finds nothing to do writes nothing at all: an install whose
/// library has not changed must not rewrite its own bookkeeping on every boot.
pub fn migrate_pawchive_ledger(conn: &Connection) -> Result<LedgerMigrationProgress> {
    let now = Utc::now().to_rfc3339();

    let stored = read_ledger_marker(conn, "works")?;
    let mut works_mark = stored
        .as_ref()
        .map(|marker| marker.high_water_mark)
        .unwrap_or(0);
    let mut posts_migrated = 0usize;
    for _ in 0..PAWCHIVE_LEDGER_BATCHES_PER_CALL {
        let ids: Vec<i64> = conn
            .prepare("SELECT id FROM kemono_posts WHERE id > ?1 ORDER BY id LIMIT ?2")?
            .query_map(params![works_mark, PAWCHIVE_LEDGER_BATCH], |row| row.get(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(last) = ids.last().copied() else {
            break;
        };
        for post_db_id in &ids {
            materialize_work_ledger(conn, *post_db_id)?;
        }
        posts_migrated += ids.len();
        works_mark = last;
        write_ledger_marker(conn, "works", works_mark, "running", &now)?;
    }
    let works_ready = conn.query_row(
        "SELECT COUNT(*) FROM kemono_posts WHERE id > ?1",
        params![works_mark],
        |row| row.get::<_, i64>(0),
    )? == 0;
    let works_status = if works_ready { "ready" } else { "running" };
    let works_unchanged = stored.as_ref().is_some_and(|marker| {
        marker.version == PAWCHIVE_LEDGER_VERSION
            && marker.high_water_mark == works_mark
            && marker.status == works_status
    });
    if !works_unchanged {
        write_ledger_marker(conn, "works", works_mark, works_status, &now)?;
    }

    // Decisions carry their own mark. A decision made after its post was
    // migrated still has to be copied, and the working row's own ID is the only
    // ordering that stays stable as posts are re-discovered.
    let stored_decisions = read_ledger_marker(conn, "decisions")?;
    let mut decisions_mark = stored_decisions
        .as_ref()
        .map(|marker| marker.high_water_mark)
        .unwrap_or(0);
    for _ in 0..PAWCHIVE_LEDGER_BATCHES_PER_CALL {
        let rows: Vec<(i64, i64)> = conn
            .prepare(
                "SELECT id, post_id FROM pawchive_post_decisions
                 WHERE id > ?1 ORDER BY id LIMIT ?2",
            )?
            .query_map(params![decisions_mark, PAWCHIVE_LEDGER_BATCH], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        let Some(last) = rows.last().copied() else {
            break;
        };
        for (_, post_db_id) in &rows {
            if let Some(work_id) = work_id_for_post(conn, *post_db_id)? {
                materialize_legacy_decisions(conn, *post_db_id, &work_id)?;
            }
        }
        decisions_mark = last.0;
        write_ledger_marker(conn, "decisions", decisions_mark, "running", &now)?;
    }
    let decisions_ready = conn.query_row(
        "SELECT COUNT(*) FROM pawchive_post_decisions WHERE id > ?1",
        params![decisions_mark],
        |row| row.get::<_, i64>(0),
    )? == 0;
    let decisions_status = if decisions_ready { "ready" } else { "running" };
    let decisions_unchanged = stored_decisions.as_ref().is_some_and(|marker| {
        marker.version == PAWCHIVE_LEDGER_VERSION
            && marker.high_water_mark == decisions_mark
            && marker.status == decisions_status
    });
    if !decisions_unchanged {
        write_ledger_marker(conn, "decisions", decisions_mark, decisions_status, &now)?;
    }

    Ok(LedgerMigrationProgress {
        posts_migrated,
        works_high_water_mark: works_mark,
        works_ready,
        decisions_high_water_mark: decisions_mark,
        decisions_ready,
    })
}

const PAWCHIVE_PATH_NORM_MIGRATION_KEY: &str = "pawchive_path_normalization_migration";
const PAWCHIVE_PATH_NORM_MIGRATION_VERSION: &str = "1";

/// Re-key existing resource versions and working-ledger files to use normalized remote paths,
/// carrying acquisition events, manifest members, decision members, and claims across.
///
/// Running on an empty or already-migrated database performs zero writes.
pub fn migrate_resource_versions_for_path_normalization(conn: &Connection) -> Result<usize> {
    let has_settings_table = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'app_settings'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if has_settings_table {
        let applied: Option<String> = conn
            .query_row(
                "SELECT value FROM app_settings WHERE key = ?1",
                params![PAWCHIVE_PATH_NORM_MIGRATION_KEY],
                |row| row.get(0),
            )
            .optional()?;
        if applied.as_deref() == Some(PAWCHIVE_PATH_NORM_MIGRATION_VERSION) {
            return Ok(0);
        }
    }

    let unnormalized_resource_versions: Vec<(String, String, String, String)> = conn
        .prepare(
            "SELECT resource_version_id, work_id, resource_key, version_key
             FROM pawchive_resource_versions
             WHERE identity_basis = 'path'",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, _, resource_key, _)| {
            let normalized = normalize_remote_path(resource_key);
            normalized != *resource_key
        })
        .collect();

    let unnormalized_files: Vec<(i64, String, String, String)> = conn
        .prepare("SELECT id, source_identity, remote_path, identity_basis FROM kemono_files")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?
        .into_iter()
        .filter(|(_, source_identity, remote_path, identity_basis)| {
            let norm_remote = normalize_remote_path(remote_path);
            let norm_source = if identity_basis == "path" {
                normalize_remote_path(source_identity)
            } else {
                source_identity.to_string()
            };
            norm_remote != *remote_path || norm_source != *source_identity
        })
        .collect();

    if unnormalized_resource_versions.is_empty() && unnormalized_files.is_empty() {
        return Ok(0);
    }

    let tx = conn.unchecked_transaction()?;
    let mut migrated_count = 0;

    for (old_version_id, work_id, resource_key, version_key) in unnormalized_resource_versions {
        let normalized_key = normalize_remote_path(&resource_key);
        let new_version_id = resource_version_id_for(&work_id, &normalized_key, &version_key);
        if new_version_id == old_version_id {
            continue;
        }

        let destination_exists: i64 = tx.query_row(
            "SELECT COUNT(*) FROM pawchive_resource_versions WHERE resource_version_id = ?1",
            params![new_version_id],
            |row| row.get(0),
        )?;

        if destination_exists == 0 {
            tx.execute(
                "UPDATE pawchive_resource_versions
                 SET resource_version_id = ?1, resource_key = ?2, raw_ref = ?2
                 WHERE resource_version_id = ?3",
                params![new_version_id, normalized_key, old_version_id],
            )?;
        } else {
            tx.execute(
                "DELETE FROM pawchive_resource_versions WHERE resource_version_id = ?1",
                params![old_version_id],
            )?;
        }

        for table in ["pawchive_manifest_members", "pawchive_decision_members"] {
            let column = "resource_version_id";
            let duplicate: i64 = tx.query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE {column} = ?1"),
                params![new_version_id],
                |row| row.get(0),
            )?;
            if duplicate > 0 {
                tx.execute(
                    &format!("DELETE FROM {table} WHERE {column} = ?1"),
                    params![old_version_id],
                )?;
            } else {
                tx.execute(
                    &format!("UPDATE {table} SET {column} = ?1 WHERE {column} = ?2"),
                    params![new_version_id, old_version_id],
                )?;
            }
        }

        let _ = tx.execute(
            "UPDATE pawchive_selection_members SET resource_version_id = ?1 WHERE resource_version_id = ?2",
            params![new_version_id, old_version_id],
        );

        let events: Vec<(String, String, Option<i64>)> = tx
            .prepare(
                "SELECT event_id, evidence_source, publish_job_id
                 FROM pawchive_acquisition_events
                 WHERE resource_version_id = ?1",
            )?
            .query_map(params![old_version_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;

        for (event_id, evidence_source, publish_job_id) in events {
            let new_key =
                acquisition_idempotency_key(&new_version_id, &evidence_source, publish_job_id);
            let duplicate: i64 = tx.query_row(
                "SELECT COUNT(*) FROM pawchive_acquisition_events WHERE idempotency_key = ?1",
                params![new_key],
                |row| row.get(0),
            )?;
            if duplicate > 0 {
                tx.execute(
                    "DELETE FROM pawchive_acquisition_events WHERE event_id = ?1",
                    params![event_id],
                )?;
            } else {
                tx.execute(
                    "UPDATE pawchive_acquisition_events
                     SET resource_version_id = ?1, idempotency_key = ?2
                     WHERE event_id = ?3",
                    params![new_version_id, new_key, event_id],
                )?;
            }
        }

        migrated_count += 1;
    }

    for (file_id, source_identity, remote_path, identity_basis) in unnormalized_files {
        let norm_remote = normalize_remote_path(&remote_path);
        let norm_source = if identity_basis == "path" {
            normalize_remote_path(&source_identity)
        } else {
            source_identity
        };
        let duplicate: Option<i64> = tx
            .query_row(
                "SELECT id FROM kemono_files
                 WHERE post_id = (SELECT post_id FROM kemono_files WHERE id = ?1)
                   AND source_identity = ?2 AND id != ?1",
                params![file_id, norm_source],
                |row| row.get(0),
            )
            .optional()?;
        if duplicate.is_some() {
            tx.execute("DELETE FROM kemono_files WHERE id = ?1", params![file_id])?;
        } else {
            tx.execute(
                "UPDATE kemono_files SET remote_path = ?1, source_identity = ?2 WHERE id = ?3",
                params![norm_remote, norm_source, file_id],
            )?;
        }
        migrated_count += 1;
    }

    if has_settings_table {
        tx.execute(
            "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
             ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at",
            params![
                PAWCHIVE_PATH_NORM_MIGRATION_KEY,
                PAWCHIVE_PATH_NORM_MIGRATION_VERSION
            ],
        )?;
    }

    tx.commit()?;
    Ok(migrated_count)
}

/// What the evaluator concluded about one post.
///
/// This is a *derived* value. Nothing may treat it as the source of truth: it
/// is recomputed from the manifest, the current selection settings, the
/// delivery evidence and the user's decisions, and a post can move back to
/// `Pending` when the source adds a resource to it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostState {
    /// Every required resource of the current selection has valid evidence.
    Verified,
    /// The user confirmed this post's manifest and that decision still applies.
    Confirmed,
    /// The user decided this post is not wanted. A reversible decision, not a
    /// download of it.
    Ignored,
    /// Some required resources are proven, others are missing or deferred.
    Partial,
    /// Required resources exist and are claimable, none of them is proven.
    Pending,
    /// Something is on disk, but no receipt proves what it is.
    Unverified,
    /// Required resources failed with a class that is being retried later.
    Unavailable,
    /// Nothing to fetch directly; the post carries external links only.
    External,
    /// The current selection requires nothing from this post.
    NotRequired,
}

impl PostState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::Confirmed => "confirmed",
            Self::Ignored => "ignored",
            Self::Partial => "partial",
            Self::Pending => "pending",
            Self::Unverified => "unverified",
            Self::Unavailable => "unavailable",
            Self::External => "external",
            Self::NotRequired => "not_required",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "verified" => Some(Self::Verified),
            "confirmed" => Some(Self::Confirmed),
            "ignored" => Some(Self::Ignored),
            "partial" => Some(Self::Partial),
            "pending" => Some(Self::Pending),
            "unverified" => Some(Self::Unverified),
            "unavailable" => Some(Self::Unavailable),
            "external" => Some(Self::External),
            "not_required" => Some(Self::NotRequired),
            _ => None,
        }
    }

    /// Whether this counts towards a day reading "已齐". `External` does not:
    /// nothing has proven the external download finished.
    pub fn is_settled(self) -> bool {
        matches!(
            self,
            Self::Verified | Self::Confirmed | Self::Ignored | Self::NotRequired
        )
    }

    /// Whether a user decision, rather than a receipt, settled this post.
    pub fn is_manual(self) -> bool {
        matches!(self, Self::Confirmed | Self::Ignored)
    }

    /// Whether the download queue may claim this post's remaining resources.
    pub fn is_claimable(self) -> bool {
        matches!(self, Self::Pending | Self::Partial)
    }
}

/// The evaluator's verdict for one post.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostAssessment {
    pub state: PostState,
    pub reason: String,
    /// Whether this post is treated as satisfied by the date-based folder coverage rule.
    #[serde(default)]
    pub date_skipped: bool,
    /// Resources the current selection requires from this post.
    pub required_assets: usize,
    /// Required resources with a receipt this install produced.
    pub proven_assets: usize,
    /// Required resources present on disk without a receipt.
    pub unverified_assets: usize,
    /// Required resources that are neither proven nor deferred.
    pub missing_assets: usize,
    /// Required resources deferred to a later attempt by the retry rules.
    pub deferred_assets: usize,
    /// Required resources whose recorded content no longer matches. They are
    /// neither proof nor a gap to refill on its own.
    #[serde(default)]
    pub integrity_failed_assets: usize,
    /// Whether the post also carries external links that nothing has delivered.
    #[serde(default)]
    pub external_pending: bool,
}

/// Per-post evidence counts, read in one grouped query.
struct PostEvidence {
    post_db_id: i64,
    required_assets: i64,
    proven_assets: i64,
    unverified_assets: i64,
    deferred_assets: i64,
    has_external_links: bool,
    /// Whether the post falls inside the subscription's start date. Outside it
    /// the post is out of scope, exactly as it is for coverage.
    in_range: bool,
    /// A completed external-downloader receipt covers this post's manifest.
    external_delivered: bool,
    /// The post's folder held files that could be its media, last time it was
    /// read. A candidate, never a receipt.
    observed_content: bool,
    /// The folder could not be read, so its contents are unknown.
    observed_unreadable: bool,
    /// Required resources whose recorded content no longer matches.
    integrity_failed_assets: i64,
    /// Latest user decision that still covers this post's manifest version.
    latest_decision: Option<String>,
    /// How the folder that was read was chosen — see `FOLDER_FROM_DAY` and
    /// `FOLDER_FROM_SIBLING_DAY`.
    ///
    /// The folder's *state* says whether content is there; this says whether the
    /// folder that was read could have been the post's at all. The two are not
    /// the same question, and only the second one separates a work filed under a
    /// name this side cannot predict from a work that was never delivered.
    observed_source: String,
}

/// Whether a resource's recorded local hash matches the one the source gave.
///
/// A file found in place is only presence, unless the source supplied a hash of
/// its own and the local copy matches it: then the content is the resource, not
/// merely a file with a plausible name.
const MATCHING_HASH_SQL: &str = "f.local_hash != '' AND f.expected_blake3 IS NOT NULL \
                                 AND f.expected_blake3 != '' AND f.local_hash = f.expected_blake3";

/// A resource whose content is known not to be the source's any more — or is
/// known to be gone. It is not proof, and it is deliberately not re-downloaded:
/// the user's own edit or extraction is a local change to report, not a gap to
/// silently refill. Only an explicit re-fetch may replace it.
const INTEGRITY_FAILED_SQL: &str = "f.integrity IN ('mismatched', 'missing', 'unreadable')";

/// A resource whose content has not been contradicted by a check.
const INTEGRITY_OK_SQL: &str =
    "(f.integrity IS NULL OR f.integrity NOT IN ('mismatched', 'missing', 'unreadable'))";

/// The day a post belongs to, for every day-scoped read.
///
/// One expression, used by the coverage calendar, the post list, the evidence
/// query and the queue, because the day a work is counted on has to be the day
/// its folder is named after. `published_local_date` is that date, derived once
/// on write by `normalize_post_date` — the same call the naming context makes.
///
/// The fallbacks keep older rows readable before the backfill reaches them: an
/// un-migrated row has an empty `published_local_date`, and falling back to the
/// raw `published_at` is at worst the previous behaviour rather than a blank
/// day. `created_at` is the last resort, as it was before.
///
/// `alias` is the `kemono_posts` alias in the calling query, so the same text
/// serves a plain join and a correlated subquery.
fn post_day_sql(alias: &str, source_date: &str) -> String {
    format!(
        "COALESCE(NULLIF(substr({alias}.published_local_date, 1, 10), ''), \
                  NULLIF(substr(COALESCE(NULLIF({alias}.{source_date}, ''), {alias}.created_at, ''), 1, 10), ''), \
                  '')"
    )
}

/// The undated source stamp the derived day falls back to.
///
/// A database old enough to predate `published_at` has no such column at all,
/// and SQLite rejects the *whole statement* for naming a column that does not
/// exist — it does not stop at the `NULLIF`. So both the reader above and the
/// backfill that fills `published_local_date` substitute a constant `''` for a
/// missing column, and a test that migrates a deliberately stripped schema
/// exercises exactly that path.
const POST_SOURCE_DATE_COLUMN: &str = "published_at";

/// Which of the two date columns a statement may safely name.
///
/// `published_local_date` is added by `ensure_columns`, so it is always present
/// once the schema has been ensured; the source stamp is the one that legacy
/// databases may have lost.
fn post_source_date_column(conn: &Connection) -> &'static str {
    if column_present(conn, "kemono_posts", POST_SOURCE_DATE_COLUMN) {
        POST_SOURCE_DATE_COLUMN
    } else {
        "''"
    }
}

/// Does this work carry netdisk links? One definition, several readers.
///
/// The same question is asked in three places — the evidence read that decides
/// whether a work still owes an external delivery, the manual list's row badge,
/// and the `有网盘` filter that claims to show what that badge shows. Two
/// spellings used to answer it:
///
/// ```text
/// TRIM(COALESCE(external_links, '')) NOT IN ('', '[]')
/// json_valid(external_links) = 1 AND json_array_length(external_links) > 0
/// ```
///
/// They disagree in both directions. `'[ ]'` and `'  [  ]  '` are a non-empty
/// string but an empty array, so the first says "has links" and the second says
/// "none"; `'null'`, `'{}'`, `'0'` and non-JSON rubbish are a non-empty string
/// that is not an array at all, so the first says "has links" and the second
/// says "none". The first spelling let the evidence read call such a work
/// `external` while the list showed no `含网盘资源` badge and the filter matched
/// nothing — a row the UI describes one way and the query selects another.
///
/// The array reading is the one that survives: `external_links` is written by
/// `extract_external_links` and re-derived by `backfill_external_links`, both of
/// which produce a JSON array and normalize blank to `[]`. Asking
/// `json_array_length` is therefore both the stricter parse and the answer the
/// badge already gives, since the row reader decodes the array in Rust and
/// counts its elements. A malformed value is "no links", which is what the
/// badge shows, and this keeps the predicate and the badge one fact.
///
/// `alias` is the `kemono_posts` alias in the calling query, so the same text
/// serves the evidence join, the list query and the selection handler.
pub fn has_external_links_sql(alias: &str) -> String {
    format!(
        "CASE WHEN json_valid({alias}.external_links) = 1 \
              AND json_array_length({alias}.external_links) > 0 \
         THEN 1 ELSE 0 END"
    )
}

/// Does `table` already have `column`?
///
/// Shared by the day readers, which must agree with `ensure_columns`: adding a
/// column and reading it back with a different name is how a migration turns
/// into a startup crash.
fn column_present(conn: &Connection, table: &str, column: &str) -> bool {
    let sql = format!("PRAGMA table_info({table})");
    let Ok(mut stmt) = conn.prepare(&sql) else {
        return false;
    };
    stmt.query_map([], |row| row.get::<_, String>(1))
        .is_ok_and(|rows| {
            rows.filter_map(std::result::Result::ok)
                .any(|name| name == column)
        })
}

/// What counts as a delivered resource: this install's own verified fetch, or
/// content the source's own hash identifies. Either way the integrity check has
/// the last word — a delivery whose file was later replaced is not proof.
fn proven_asset_sql() -> String {
    format!("{INTEGRITY_OK_SQL} AND (f.delivery_source = 'download' OR ({MATCHING_HASH_SQL}))")
}

/// The `file_type IN (...)` half of the evidence query, built from the current
/// selection, restricted to the resources the source still lists.
///
/// Deselecting a type removes it from *this* query only: the asset rows stay in
/// the ledger, so re-enabling the type needs no re-discovery. An attachment the
/// source has withdrawn leaves the current requirement the same way — its row
/// and its delivered file stay, but it no longer counts towards what the work
/// needs, and it no longer holds a day open.
fn enabled_type_predicate(settings: &PawchiveSettings) -> String {
    let enabled = enabled_types(settings);
    if enabled.is_empty() {
        // No type selected: nothing is required, and an empty IN list would be
        // a syntax error.
        return "0".to_string();
    }
    let list = enabled
        .iter()
        .map(|name| format!("'{name}'"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("f.present_in_manifest = 1 AND f.file_type IN ({list})")
}

/// The resource types the current selection asks for.
///
/// One list, read by both the evidence query and the per-claim admission
/// check: a switch must not mean one thing to the evaluator and another to the
/// queue that fetches what the evaluator found.
fn enabled_types(settings: &PawchiveSettings) -> Vec<&'static str> {
    let mut enabled = Vec::new();
    if settings.download_images {
        enabled.push("image");
    }
    if settings.download_videos {
        enabled.push("video");
    }
    if settings.download_audio {
        enabled.push("audio");
    }
    if settings.download_archives {
        enabled.push("archive");
    }
    if settings.download_others {
        enabled.push("other");
    }
    enabled
}

/// Whether the current selection still asks for this resource type.
fn wants_type(settings: &PawchiveSettings, file_type: &str) -> bool {
    enabled_types(settings).contains(&file_type)
}

/// Read the evidence counts for the posts of one subscription (or one post).
///
/// One grouped statement, so a subscription with hundreds of posts and
/// thousands of resources does not run a query per post. The rule set lives in
/// `derive_assessment`; this only counts.
fn load_post_evidence(
    conn: &Connection,
    subscription_id: Option<i64>,
    post_db_id: Option<i64>,
    settings: &PawchiveSettings,
) -> Result<Vec<PostEvidence>> {
    let enabled = enabled_type_predicate(settings);
    let proven = proven_asset_sql();
    let source_date = post_source_date_column(conn);
    let post_day = post_day_sql("p", source_date);
    let has_external_links = has_external_links_sql("p");
    let scope = if post_db_id.is_some() {
        "p.id = ?1"
    } else {
        "p.subscription_id = ?1"
    };
    let scope_value: i64 = post_db_id.or(subscription_id).unwrap_or(0);
    let sql = format!(
        "SELECT p.id,
                SUM(CASE WHEN f.id IS NOT NULL AND {enabled} THEN 1 ELSE 0 END),
                SUM(CASE WHEN f.id IS NOT NULL AND {enabled}
                              AND ({proven})
                         THEN 1 ELSE 0 END),
                SUM(CASE WHEN f.id IS NOT NULL AND {enabled}
                              AND NOT ({proven}) AND f.delivery_source != ''
                         THEN 1 ELSE 0 END),
                SUM(CASE WHEN f.id IS NOT NULL AND {enabled}
                              AND NOT ({proven}) AND f.delivery_source = ''
                              AND f.next_attempt_at IS NOT NULL AND f.next_attempt_at > ?2
                         THEN 1 ELSE 0 END),
                {has_external_links},
                CASE WHEN sub.since_date IS NULL OR sub.since_date = ''
                          OR {post_day} >= sub.since_date
                     THEN 1 ELSE 0 END,
                CASE WHEN p.external_delivered_at IS NOT NULL
                          AND p.external_manifest_version >= p.manifest_version
                     THEN 1 ELSE 0 END,
                CASE WHEN p.observed_state = '{OBSERVED_CONTENT}' AND p.observed_files > 0
                     THEN 1 ELSE 0 END,
                CASE WHEN p.observed_state = '{OBSERVED_UNREADABLE}' THEN 1 ELSE 0 END,
                (SELECT d.action FROM pawchive_post_decisions d
                  WHERE d.post_id = p.id AND d.manifest_version >= p.manifest_version
                  ORDER BY d.id DESC LIMIT 1),
                SUM(CASE WHEN f.id IS NOT NULL AND {enabled} AND {INTEGRITY_FAILED_SQL}
                         THEN 1 ELSE 0 END),
                p.observed_source
         FROM kemono_posts p
         JOIN kemono_subscriptions sub ON sub.id = p.subscription_id
         LEFT JOIN kemono_files f ON f.post_id = p.id
         WHERE {scope}
         GROUP BY p.id"
    );
    let now = Utc::now().to_rfc3339();
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(params![scope_value, now], |row| {
            Ok(PostEvidence {
                post_db_id: row.get(0)?,
                required_assets: row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                proven_assets: row.get::<_, Option<i64>>(2)?.unwrap_or(0),
                unverified_assets: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                deferred_assets: row.get::<_, Option<i64>>(4)?.unwrap_or(0),
                has_external_links: row.get::<_, Option<i64>>(5)?.unwrap_or(0) == 1,
                in_range: row.get::<_, Option<i64>>(6)?.unwrap_or(1) == 1,
                external_delivered: row.get::<_, Option<i64>>(7)?.unwrap_or(0) == 1,
                observed_content: row.get::<_, Option<i64>>(8)?.unwrap_or(0) == 1,
                observed_unreadable: row.get::<_, Option<i64>>(9)?.unwrap_or(0) == 1,
                latest_decision: row.get(10)?,
                integrity_failed_assets: row.get::<_, Option<i64>>(11)?.unwrap_or(0),
                observed_source: row.get::<_, Option<String>>(12)?.unwrap_or_default(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The rule set: evidence counts in, one state and reason out.
///
/// Kept as a pure function of its input so coverage, the post list and the
/// download queue cannot drift into three different definitions of "已有".
fn derive_assessment(evidence: &PostEvidence) -> PostAssessment {
    let required = evidence.required_assets.max(0) as usize;
    let proven = evidence.proven_assets.max(0) as usize;
    let unverified = evidence.unverified_assets.max(0) as usize;
    let deferred = evidence.deferred_assets.max(0) as usize;
    let integrity_failed = evidence.integrity_failed_assets.max(0) as usize;
    let settled = proven + unverified;
    let claimable = required.saturating_sub(settled + deferred);
    let date_folder_present = evidence.observed_content && !evidence.observed_unreadable;
    let day_folder_belongs_to_another = evidence.observed_source == FOLDER_FROM_SIBLING_DAY;
    let external_pending = evidence.has_external_links && !evidence.external_delivered;
    let mut assessment = PostAssessment {
        state: PostState::Pending,
        reason: String::new(),
        date_skipped: false,
        required_assets: required,
        proven_assets: proven,
        unverified_assets: unverified,
        missing_assets: claimable,
        deferred_assets: deferred,
        integrity_failed_assets: integrity_failed,
        external_pending,
    };

    if evidence.latest_decision.as_deref() == Some("confirm") {
        assessment.state = PostState::Confirmed;
        assessment.reason = "已人工确认此作品的清单".to_string();
        return assessment;
    }
    if evidence.latest_decision.as_deref() == Some("ignore") {
        assessment.state = PostState::Ignored;
        assessment.reason = "已人工忽略此作品".to_string();
        return assessment;
    }

    if !evidence.in_range {
        assessment.reason = "超出订阅起始日期".to_string();
        assessment.state = PostState::NotRequired;
        return assessment;
    }

    // Date-based coverage policy confirmed by the user:
    // When the artist's date folder holds actual content on disk, this date is
    // treated as satisfied for automatic scheduling and coverage.
    if date_folder_present {
        assessment.reason = DAY_FOLDER_REASON.to_string();
        assessment.state = PostState::Verified;
        assessment.date_skipped = true;
        return assessment;
    }

    // Residual cases when no date content is present on disk:
    assessment.state = if day_folder_belongs_to_another {
        assessment.reason = SIBLING_FOLDER_REASON.to_string();
        PostState::Unverified
    } else if evidence.observed_source == FOLDER_SHARED_TEMPLATE {
        assessment.reason = "共享目录的内容归属未核验，暂停自动下载".to_string();
        PostState::Unverified
    } else if evidence.observed_unreadable {
        assessment.reason = "目录暂时无法读取，无法核验".to_string();
        PostState::Unverified
    } else if integrity_failed > 0 {
        assessment.reason = format!("{integrity_failed} 个文件与来源不一致，需重新获取");
        PostState::Unverified
    } else if required == 0 {
        if external_pending {
            assessment.reason = "仅有外部链接，等待下载器交付".to_string();
            PostState::External
        } else {
            assessment.reason = if evidence.external_delivered {
                "外部下载器已回执".to_string()
            } else {
                "当前范围不需要下载".to_string()
            };
            PostState::NotRequired
        }
    } else if claimable > 0 {
        assessment.reason = format!("缺 {claimable} 个文件");
        if settled == 0 {
            PostState::Pending
        } else {
            PostState::Partial
        }
    } else if deferred > 0 {
        assessment.reason = format!("{deferred} 个文件暂时取不到，稍后重试");
        if settled == 0 {
            PostState::Unavailable
        } else {
            PostState::Partial
        }
    } else if external_pending {
        assessment.reason = "直连文件已齐，外部链接尚未交付".to_string();
        PostState::External
    } else if unverified > 0 {
        assessment.reason = "本地有内容，但缺少完成凭据".to_string();
        PostState::Unverified
    } else {
        assessment.reason = "当前范围的文件已核验".to_string();
        PostState::Verified
    };
    assessment
}

/// Evaluate one post without touching the network or the filesystem.
pub fn assess_post(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
) -> Result<PostAssessment> {
    let evidence = load_post_evidence(conn, None, Some(post_db_id), settings)?;
    Ok(evidence.first().map(derive_assessment).unwrap_or_else(|| {
        derive_assessment(&PostEvidence {
            post_db_id,
            required_assets: 0,
            proven_assets: 0,
            unverified_assets: 0,
            deferred_assets: 0,
            has_external_links: false,
            in_range: true,
            external_delivered: false,
            observed_content: false,
            observed_unreadable: false,
            integrity_failed_assets: 0,
            latest_decision: None,
            observed_source: String::new(),
        })
    }))
}

/// What the long-term ledger knows about one work's resources.
///
/// This is the read half of the ledger work: it answers the acquisition
/// questions from `pawchive_acquisition_events`, `pawchive_work_manifests` and
/// `pawchive_user_decisions` alone, without consulting `kemono_posts` or
/// `kemono_files`. That matters because the working ledger's rows are reachable
/// from a subscription, and the whole point of the long-term ledger is to hold
/// the acquisition facts after the subscription that discovered them is gone.
///
/// It deliberately does not read `PawchiveSettings`. The requirement set here is
/// the work's whole current manifest; the caller intersects it with the selected
/// types when it computes automatic demand. Keeping the switches out is what
/// makes it structurally impossible to widen a legacy decision with today's
/// selection — the mistake the plan forbids by name.
///
/// Read-only: every statement is a `SELECT`, so a GET may call it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LedgerAssessment {
    pub work_id: String,
    /// The manifest the work is currently assessed against; empty when the
    /// ledger has no observation for it.
    pub manifest_id: String,
    /// How much of the resource set that observation saw.
    pub completeness: ManifestCompleteness,
    /// Whether the current manifest observed the whole set. When this is false
    /// `requires_fetch` is a lower bound, not the work's requirement: a summary
    /// that omitted `attachments` says nothing about the attachments.
    pub manifest_complete: bool,
    pub manifest_members: usize,
    /// Members a strong event proves this install fetched. Damage or deletion
    /// later does not remove them: acquisition is history, and integrity is a
    /// separate question asked about a located original.
    pub acquired_resource_versions: Vec<String>,
    /// Members seen only on disk, or inherited from the retired directory rule.
    /// Presence is not proof, and these may not satisfy an automatic demand.
    pub unproven_resource_versions: Vec<String>,
    /// Members the latest standing decision speaks for, when its scope is
    /// exact. Confirming and ignoring both land here: the demand set is
    /// `R − A − U − I`, so an ignored resource is subtracted for the opposite
    /// reason a fetched one is. Which of the two it was is the decision's
    /// `action`, not this list.
    pub covered_resource_versions: Vec<String>,
    /// Whether a decision stands but its scope is unknown. Nothing may be
    /// inferred from it, so its members are neither covered nor fetched.
    pub decision_scope_unknown: bool,
    /// Current members that are neither acquired nor covered by an exact
    /// decision.
    pub requires_fetch: Vec<String>,
    /// The work's old scope must be re-checked before it is used. A decision was
    /// inherited from the era when the covered set was not recorded, and its
    /// scope cannot be rebuilt from today's settings; the work's old range is
    /// frozen until a user looks at it.
    pub needs_confirmation: bool,
    /// Stable codes explaining the verdict, for logs and tests. Wording for a
    /// human belongs to the caller.
    pub reason_codes: Vec<String>,
}

/// The verdict that the ledger has no work with this identity.
///
/// Separate from "a work with no manifest": the first means nothing is known,
/// the second means the identity is established and its resources are not.
pub const LEDGER_REASON_UNKNOWN_WORK: &str = "unknown_work";
pub const LEDGER_REASON_NO_MANIFEST: &str = "no_manifest";
pub const LEDGER_REASON_MANIFEST_INCOMPLETE: &str = "manifest_incomplete";
pub const LEDGER_REASON_DECISION_SCOPE_UNKNOWN: &str = "decision_scope_unknown";
pub const LEDGER_REASON_UNPROVEN_PRESENCE: &str = "unproven_presence";
pub const LEDGER_REASON_REQUIRES_FETCH: &str = "requires_fetch";
/// Nothing in the current manifest is outstanding — whether because it was
/// fetched or because a standing decision covers it. The two are not the same
/// fact, which is why the counts above carry the distinction and this code only
/// says the work is not a gap.
pub const LEDGER_REASON_NOTHING_OUTSTANDING: &str = "nothing_outstanding";

/// Read the ledger's view of one work.
///
/// `Ok(None)` when the ledger has never seen the identity. That is not the same
/// answer as an empty assessment: "this work is not in the ledger" and "this
/// work is in the ledger and nothing has been fetched" are different facts, and
/// only the second may be treated as a gap.
/// The standing user decision for one work, with its member scope resolved.
///
/// Both `assess_work_from_ledger` and `demand_set_for_work` need "what did the
/// user last say, and which resources did that cover". They used to carry
/// byte-identical queries for both halves, which is how a change to the
/// ordering (or to what counts as `exact`) could silently apply to only one of
/// them and leave the two answers disagreeing about the same work.
struct UserDecisionScope {
    action: String,
    /// Resources an exact-scope decision covers. Always empty for a revocation:
    /// revoking says the user withdrew a claim, so it covers nothing.
    covered: Vec<String>,
    /// The decision was recorded under a scope this build cannot honour, so its
    /// coverage is unknown rather than empty. Only `assess_work_from_ledger`
    /// acts on this; `demand_set_for_work` subtracts only what it can name.
    scope_unknown: bool,
}

/// Read the latest standing decision for `work_id` and, when its scope is
/// exact, the resources it covers.
///
/// Ordering matches the working ledger's own: by the row id a legacy decision
/// was keyed on, so both ledgers pick the same decision for the same work
/// instead of disagreeing about which of several is "latest".
fn latest_user_decision_scope(
    conn: &Connection,
    work_id: &str,
) -> Result<Option<UserDecisionScope>> {
    let decision: Option<(String, String, String)> = conn
        .query_row(
            "SELECT decision_id, action, scope_kind FROM pawchive_user_decisions
             WHERE work_id = ?1 AND revoked_by = ''
             ORDER BY COALESCE(legacy_decision_id, 0) DESC, created_at DESC, decision_id DESC
             LIMIT 1",
            params![work_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let Some((decision_id, action, scope_kind)) = decision else {
        return Ok(None);
    };

    let mut covered: Vec<String> = Vec::new();
    let mut scope_unknown = false;
    // A revocation is a decision too, and it covers nothing.
    if scope_kind == "exact" && action != PostDecisionAction::Revoke.as_str() {
        let mut stmt = conn.prepare(
            "SELECT resource_version_id FROM pawchive_decision_members
             WHERE decision_id = ?1 ORDER BY resource_version_id",
        )?;
        let rows = stmt.query_map(params![decision_id], |row| row.get::<_, String>(0))?;
        covered = rows.collect::<rusqlite::Result<Vec<_>>>()?;
    } else if scope_kind != "exact" {
        scope_unknown = true;
    }

    Ok(Some(UserDecisionScope {
        action,
        covered,
        scope_unknown,
    }))
}

pub fn assess_work_from_ledger(
    conn: &Connection,
    work_id: &str,
) -> Result<Option<LedgerAssessment>> {
    let known = conn
        .query_row(
            "SELECT 1 FROM pawchive_remote_works WHERE work_id = ?1",
            params![work_id],
            |_| Ok(()),
        )
        .optional()?;
    if known.is_none() {
        return Ok(None);
    }

    let current: Option<(String, String)> = conn
        .query_row(
            "SELECT manifest_id, completeness FROM pawchive_work_manifests
             WHERE work_id = ?1 AND is_current = 1",
            params![work_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (manifest_id, completeness) = match current {
        Some((manifest_id, stored)) => (manifest_id, ManifestCompleteness::parse(&stored)),
        None => (String::new(), ManifestCompleteness::Unknown),
    };

    let members = if manifest_id.is_empty() {
        Vec::new()
    } else {
        let mut stmt = conn.prepare(
            "SELECT resource_version_id FROM pawchive_manifest_members
             WHERE manifest_id = ?1 ORDER BY resource_version_id",
        )?;
        let rows = stmt.query_map(params![manifest_id], |row| row.get::<_, String>(0))?;
        rows.collect::<rusqlite::Result<Vec<_>>>()?
    };

    // Strong evidence is a fetch; everything else is presence. They are kept
    // apart so "曾经下过" is never answered by "目录里有东西", and a member with
    // both is reported once, as acquired.
    let mut strong: HashSet<String> = HashSet::new();
    let mut weak: HashSet<String> = HashSet::new();
    {
        let mut stmt = conn.prepare(
            "SELECT resource_version_id, evidence_strength FROM pawchive_acquisition_events
             WHERE work_id = ?1",
        )?;
        let rows = stmt.query_map(params![work_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?;
        for row in rows {
            let (resource_version_id, strength) = row?;
            if strength == "strong" {
                strong.insert(resource_version_id);
            } else {
                weak.insert(resource_version_id);
            }
        }
    }
    weak.retain(|id| !strong.contains(id));

    // The latest standing decision, plus the resources it covers when its scope
    // is exact. `legacy_decision_id` is the working row id the decision was
    // keyed on, so the trailing keys only make the order total.
    let decision_scope = latest_user_decision_scope(conn, work_id)?;
    let mut covered: Vec<String> = Vec::new();
    let mut decision_scope_unknown = false;
    let mut decision_action = String::new();
    if let Some(scope) = decision_scope {
        decision_action = scope.action;
        covered = scope.covered;
        decision_scope_unknown = scope.scope_unknown;
    }
    let covered_set: HashSet<&String> = covered.iter().collect();

    // A member is a requirement unless something already answers for it. A
    // legacy decision answers for nothing, so it cannot subtract anything here —
    // it only freezes the work for review.
    let mut requires_fetch = Vec::new();
    for member in &members {
        if strong.contains(member) || covered_set.contains(member) {
            continue;
        }
        requires_fetch.push(member.clone());
    }

    let mut reason_codes = Vec::new();
    if manifest_id.is_empty() {
        reason_codes.push(LEDGER_REASON_NO_MANIFEST.to_string());
    } else if !completeness.may_replace_current() {
        reason_codes.push(LEDGER_REASON_MANIFEST_INCOMPLETE.to_string());
    }
    if decision_scope_unknown {
        reason_codes.push(LEDGER_REASON_DECISION_SCOPE_UNKNOWN.to_string());
    }
    if !weak.is_empty() {
        reason_codes.push(LEDGER_REASON_UNPROVEN_PRESENCE.to_string());
    }
    if !requires_fetch.is_empty() {
        reason_codes.push(LEDGER_REASON_REQUIRES_FETCH.to_string());
    } else if !members.is_empty() {
        reason_codes.push(LEDGER_REASON_NOTHING_OUTSTANDING.to_string());
    }

    let mut acquired_resource_versions: Vec<String> = strong.into_iter().collect();
    acquired_resource_versions.sort();
    let mut unproven_resource_versions: Vec<String> = weak.into_iter().collect();
    unproven_resource_versions.sort();

    Ok(Some(LedgerAssessment {
        work_id: work_id.to_string(),
        manifest_id,
        completeness,
        manifest_complete: completeness.may_replace_current(),
        manifest_members: members.len(),
        acquired_resource_versions,
        unproven_resource_versions,
        covered_resource_versions: covered,
        decision_scope_unknown,
        // An unknown scope only freezes a work that has something to re-check,
        // and only when the user had said they *had* the work. An inherited
        // confirmation claims a library the install may not be able to show; an
        // inherited ignore claims nothing, so there is no scope to protect and
        // no reason to stop the work. Freezing every work with an old decision
        // would halt the library over decisions that are not in question.
        needs_confirmation: decision_scope_unknown
            && decision_action == PostDecisionAction::Confirm.as_str()
            && (!requires_fetch.is_empty() || !completeness.may_replace_current()),
        requires_fetch,
        reason_codes,
    }))
}

/// The computed demand set for one work under an exact baseline and user type selection.
///
/// Formula: `Demand = R − A − U − I`
/// - R: Current manifest members intersecting the user's selected types (images, videos, etc.)
/// - A: Resource versions with proven acquisition events (strong evidence)
/// - U: User confirmed resources (accepted legacy scope or exact confirm decisions)
/// - I: User ignored / excluded resources (exact ignore decisions or baseline exclusions)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DemandSet {
    pub work_id: String,
    pub manifest_id: String,
    pub requested_resources: Vec<String>,
    pub acquired_resources: Vec<String>,
    pub user_confirmed_resources: Vec<String>,
    pub user_ignored_resources: Vec<String>,
    pub required_resources: Vec<String>,
    pub blocked_by_ambiguity: bool,
    pub blocked_reason: Option<String>,
}

impl DemandSet {
    pub fn is_claimable(&self) -> bool {
        !self.blocked_by_ambiguity && !self.required_resources.is_empty()
    }
}

pub fn demand_set(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
) -> Result<Option<DemandSet>> {
    let work_id = crate::pawchive_pairing_write::work_of_post(conn, post_db_id)?;
    let Some((work_id, _)) = work_id else {
        return Ok(None);
    };
    if let Some(set) = demand_set_for_work(conn, &work_id, settings)? {
        Ok(Some(set))
    } else {
        Ok(Some(DemandSet {
            work_id,
            manifest_id: String::new(),
            requested_resources: Vec::new(),
            acquired_resources: Vec::new(),
            user_confirmed_resources: Vec::new(),
            user_ignored_resources: Vec::new(),
            required_resources: Vec::new(),
            blocked_by_ambiguity: false,
            blocked_reason: None,
        }))
    }
}

pub fn demand_set_for_work(
    conn: &Connection,
    work_id: &str,
    settings: &PawchiveSettings,
) -> Result<Option<DemandSet>> {
    let Some(assessment) = assess_work_from_ledger(conn, work_id)? else {
        return Ok(None);
    };

    if assessment.manifest_id.is_empty() {
        return Ok(Some(DemandSet {
            work_id: work_id.to_string(),
            manifest_id: String::new(),
            requested_resources: Vec::new(),
            acquired_resources: Vec::new(),
            user_confirmed_resources: Vec::new(),
            user_ignored_resources: Vec::new(),
            required_resources: Vec::new(),
            blocked_by_ambiguity: false,
            blocked_reason: None,
        }));
    }

    let mut stmt = conn.prepare(
        "SELECT m.resource_version_id, v.file_type
         FROM pawchive_manifest_members m
         JOIN pawchive_resource_versions v ON v.resource_version_id = m.resource_version_id
         WHERE m.manifest_id = ?1
         ORDER BY m.resource_version_id",
    )?;
    let member_rows = stmt.query_map(params![assessment.manifest_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut requested = Vec::new();
    for row in member_rows {
        let (version_id, file_type) = row?;
        let effective_type = if file_type.is_empty() {
            "other"
        } else {
            &file_type
        };
        if wants_type(settings, effective_type) {
            requested.push(version_id);
        }
    }

    let acquired_set: HashSet<&str> = assessment
        .acquired_resource_versions
        .iter()
        .map(|s| s.as_str())
        .collect();
    let acquired: Vec<String> = requested
        .iter()
        .filter(|r| acquired_set.contains(r.as_str()))
        .cloned()
        .collect();

    let mut user_confirmed = Vec::new();
    let mut user_ignored = Vec::new();
    if let Some(scope) = latest_user_decision_scope(conn, work_id)? {
        let member_set: HashSet<&str> = scope.covered.iter().map(|s| s.as_str()).collect();

        if scope.action == PostDecisionAction::Confirm.as_str() {
            user_confirmed = requested
                .iter()
                .filter(|r| member_set.contains(r.as_str()))
                .cloned()
                .collect();
        } else if scope.action == PostDecisionAction::Ignore.as_str() {
            user_ignored = requested
                .iter()
                .filter(|r| member_set.contains(r.as_str()))
                .cloned()
                .collect();
        }
    }

    let mut blocked_by_ambiguity = false;
    let mut blocked_reason = None;

    if assessment.needs_confirmation {
        blocked_by_ambiguity = true;
        blocked_reason = Some("legacy_scope_needs_confirmation".to_string());
    } else {
        let has_groups_table = conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='content_groups'",
                [],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        let source_date = post_source_date_column(conn);
        if has_groups_table {
            let post_meta: Option<(i64, String, Option<String>, i64)> = conn
                .query_row(
                    &format!(
                        "SELECT p.id, {post_day}, p.published_at, p.subscription_id
                         FROM pawchive_remote_works w
                         JOIN kemono_posts p ON p.post_id = w.post_id
                         JOIN kemono_subscriptions s ON s.id = p.subscription_id
                         WHERE w.work_id = ?1 AND w.site_id = s.site_id AND w.service = s.service
                         LIMIT 1",
                        post_day = post_day_sql("p", source_date)
                    ),
                    params![work_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .optional()?;

            if let Some((post_id, day, _published_at, sub_id)) = post_meta {
                let paired_links =
                    crate::pawchive_pairing_write::list_work_group_links(conn, post_id)?;
                if paired_links.is_empty() {
                    // The group's own `date` is a folder date, so it is compared
                    // against the post's folder day, not the source's raw date.
                    if !day.is_empty() {
                        let mut stmt = conn.prepare(
                            "SELECT group_id, generation FROM content_groups
                                 WHERE date = ?1 AND artist_scope_id = (SELECT printf('artist:%d', artist_id) FROM kemono_subscriptions WHERE id = ?2)",
                        )?;
                        let rows = stmt.query_map(params![day, sub_id], |row| {
                            Ok((row.get(0)?, row.get(1)?))
                        })?;
                        let groups: Vec<(String, i64)> = rows
                            .collect::<rusqlite::Result<Vec<_>>>()
                            .unwrap_or_default();

                        let mut active_unpaired_groups = 0;
                        for (gid, gen) in groups {
                            let is_excluded =
                                crate::pawchive_pairing_write::group_is_excluded(conn, &gid, gen)
                                    .unwrap_or(false);
                            if !is_excluded {
                                active_unpaired_groups += 1;
                            }
                        }
                        if active_unpaired_groups > 0 {
                            blocked_by_ambiguity = true;
                            blocked_reason =
                                Some("unresolved_same_day_group_candidate".to_string());
                        }
                    }
                }
            }
        }
    }

    let confirmed_set: HashSet<&str> = user_confirmed.iter().map(|s| s.as_str()).collect();
    let ignored_set: HashSet<&str> = user_ignored.iter().map(|s| s.as_str()).collect();
    let mut required: Vec<String> = Vec::new();
    for r in &requested {
        if acquired_set.contains(r.as_str())
            || confirmed_set.contains(r.as_str())
            || ignored_set.contains(r.as_str())
        {
            continue;
        }
        required.push(r.clone());
    }

    Ok(Some(DemandSet {
        work_id: work_id.to_string(),
        manifest_id: assessment.manifest_id,
        requested_resources: requested,
        acquired_resources: acquired,
        user_confirmed_resources: user_confirmed,
        user_ignored_resources: user_ignored,
        required_resources: required,
        blocked_by_ambiguity,
        blocked_reason,
    }))
}

pub fn required_resources(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
) -> Result<Vec<String>> {
    let Some(ds) = demand_set(conn, post_db_id, settings)? else {
        return Ok(Vec::new());
    };
    if ds.blocked_by_ambiguity {
        return Ok(Vec::new());
    }
    Ok(ds.required_resources)
}

/// Re-evaluate a scope and persist the derived state.
///
/// Returns how many posts changed state. Only the derived columns are written:
/// the task ledger, the evidence and the user's decisions are untouched.
fn reassess_scope(
    conn: &Connection,
    subscription_id: Option<i64>,
    post_db_id: Option<i64>,
    settings: &PawchiveSettings,
) -> Result<usize> {
    let evidence = load_post_evidence(conn, subscription_id, post_db_id, settings)?;
    let now = Utc::now().to_rfc3339();
    let mut changed = 0;
    for row in &evidence {
        let assessment = derive_assessment(row);
        let updated = conn.execute(
            "UPDATE kemono_posts
             SET assessment_state = ?1, assessment_reason = ?2, assessment_at = ?3
             WHERE id = ?4 AND (assessment_state != ?1 OR assessment_reason != ?2)",
            params![
                assessment.state.as_str(),
                assessment.reason,
                now,
                row.post_db_id
            ],
        )?;
        changed += updated;
    }
    Ok(changed)
}

/// Re-evaluate every post of one subscription.
pub fn reassess_subscription(
    conn: &Connection,
    subscription_id: i64,
    settings: &PawchiveSettings,
) -> Result<usize> {
    reassess_scope(conn, Some(subscription_id), None, settings)
}

/// Re-evaluate one post.
pub fn reassess_post(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
) -> Result<PostAssessment> {
    let assessment = assess_post(conn, post_db_id, settings)?;
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE kemono_posts
         SET assessment_state = ?1, assessment_reason = ?2, assessment_at = ?3
         WHERE id = ?4",
        params![
            assessment.state.as_str(),
            assessment.reason,
            now,
            post_db_id
        ],
    )?;
    Ok(assessment)
}

/// What a full re-evaluation found, for the migration and settings preview.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AssessmentDigest {
    pub posts: usize,
    pub verified: usize,
    pub confirmed: usize,
    pub ignored: usize,
    pub partial: usize,
    pub pending: usize,
    pub unverified: usize,
    pub unavailable: usize,
    pub external: usize,
    pub not_required: usize,
}

impl AssessmentDigest {
    fn record(&mut self, state: PostState) {
        self.posts += 1;
        match state {
            PostState::Verified => self.verified += 1,
            PostState::Confirmed => self.confirmed += 1,
            PostState::Ignored => self.ignored += 1,
            PostState::Partial => self.partial += 1,
            PostState::Pending => self.pending += 1,
            PostState::Unverified => self.unverified += 1,
            PostState::Unavailable => self.unavailable += 1,
            PostState::External => self.external += 1,
            PostState::NotRequired => self.not_required += 1,
        }
    }

    /// Posts the download queue could still claim something for.
    pub fn claimable(&self) -> usize {
        self.pending + self.partial
    }
}

/// Evaluate every subscription and persist the results, reporting the totals.
///
/// Runs at startup after the evidence migration and whenever the selection
/// settings change, because a type switch changes what every post requires.
pub fn reassess_all_posts(conn: &Connection) -> Result<AssessmentDigest> {
    reassess_posts(conn, None)
}

/// Re-evaluate one subscription's posts and report its totals.
///
/// The per-subscription 核对本地 answers about one artist, so its digest has to
/// cover that subscription's posts only: a library-wide digest would report the
/// other subscriptions' counts as this one's result.
pub fn reassess_subscription_posts(
    conn: &Connection,
    subscription_id: i64,
) -> Result<AssessmentDigest> {
    reassess_posts(conn, Some(subscription_id))
}

fn reassess_posts(conn: &Connection, only_subscription: Option<i64>) -> Result<AssessmentDigest> {
    let settings = get_pawchive_settings(conn)?;
    let mut scope =
        conn.prepare("SELECT id FROM kemono_subscriptions WHERE ?1 IS NULL OR id = ?1")?;
    let subscription_ids: Vec<i64> = scope
        .query_map(params![only_subscription], |row| row.get(0))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let mut digest = AssessmentDigest::default();
    // This runs whenever the selection settings change and at startup, and it
    // writes one row per post. Without a transaction that was one commit per
    // post — a type switch on a 300k-post library was 300k WAL commits inside a
    // request handler — and a failure part-way left half the library assessed
    // against the old settings with no marker saying so.
    let tx = conn.unchecked_transaction()?;
    {
        // Prepared once: the loop below runs it per post, and `conn.execute`
        // re-prepares on every call.
        let mut update = tx.prepare_cached(
            "UPDATE kemono_posts
             SET assessment_state = ?1, assessment_reason = ?2, assessment_at = ?3
             WHERE id = ?4",
        )?;
        for subscription_id in subscription_ids {
            let evidence = load_post_evidence(&tx, Some(subscription_id), None, &settings)?;
            let now = Utc::now().to_rfc3339();
            for row in &evidence {
                let assessment = derive_assessment(row);
                update.execute(params![
                    assessment.state.as_str(),
                    assessment.reason,
                    now,
                    row.post_db_id
                ])?;
                digest.record(assessment.state);
            }
        }
    }
    tx.commit()?;
    Ok(digest)
}

/// Download coverage of one calendar day inside a subscription.
///
/// Counts posts, not file rows, and reads the evaluator's derived state rather
/// than a single `done` flag: one column used to mean download completion,
/// "already have it", type coverage and "stop retrying" at once, so any of them
/// could hide the others. A day reads complete only when no post on it is
/// unresolved; posts the current selection requires nothing from are counted
/// separately and never as downloaded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DayCoverage {
    pub day: String,
    pub total_posts: usize,
    /// Proven downloads plus user confirmations, never "nothing to do".
    pub downloaded_posts: usize,
    /// Of `downloaded_posts`, how many rest on a user decision.
    pub confirmed_posts: usize,
    /// Posts the current selection requires nothing from.
    pub not_required_posts: usize,
}

impl DayCoverage {
    /// Posts on this day that are neither proven nor out of range.
    pub fn missing_posts(&self) -> usize {
        self.total_posts
            .saturating_sub(self.downloaded_posts + self.not_required_posts)
    }

    pub fn complete(&self) -> bool {
        self.total_posts > 0 && self.missing_posts() == 0
    }
}

/// Mirror every bound subscription's creator URL into the artist's
/// `subscription` profile links, once per startup.
///
/// Subscriptions created before the link sync existed have no row to show on
/// the artist page. The profile table's own `(artist_id, kind, url)` unique
/// key makes the insert idempotent, and a best-effort insert must never fail
/// startup: a legacy row the URL normalizer rejects simply stays unmirrored.
fn backfill_subscription_profile_links(conn: &Connection) -> Result<()> {
    let table: Option<String> = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'artist_profile_links'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if table.is_none() {
        return Ok(());
    }
    let mut stmt = conn.prepare(
        "SELECT artist_id, service, service || '/user/' || user_id
         FROM kemono_subscriptions
         WHERE artist_id IS NOT NULL",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    drop(stmt);
    for (artist_id, service, creator_path) in rows {
        let url = format!("https://pawchive.pw/{creator_path}");
        let _ = crate::artist_profile_links::create_artist_profile_link(
            conn,
            artist_id,
            "subscription",
            &service,
            &url,
        );
    }
    Ok(())
}

/// How a subscription may be claimed.
///
/// `auto` is the scheduled behaviour the plan describes: a round takes the
/// resources the acquisition ledger does not cover for this subscription.
/// `manual` means no round takes anything on its own — the user picks works from
/// the full list and downloads them explicitly. It is not "disabled": discovery
/// still walks it, the panel still lists it, and an explicit task still runs
/// while the mode is manual. A new subscription starts manual so nothing is
/// fetched before the user has seen what is there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionMode {
    Manual,
    Auto,
}

impl SubscriptionMode {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Manual => "manual",
            Self::Auto => "auto",
        }
    }

    /// Whether an automatic round may claim this subscription's works.
    pub fn is_auto(self) -> bool {
        matches!(self, Self::Auto)
    }

    /// Parse a stored or submitted value. Anything unrecognised is manual: the
    /// answer that downloads less is the safe one for an unknown mode.
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "auto" => Self::Auto,
            _ => Self::Manual,
        }
    }
}

/// Stored subscription configuration record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscriptionRecord {
    pub id: i64,
    pub site_id: String,
    pub service: String,
    pub user_id: String,
    pub artist_id: Option<i64>,
    pub artist_name: Option<String>,
    pub target_dir: String,
    pub enabled: bool,
    /// Automatic or manual claiming. See [`SubscriptionMode`].
    #[serde(default = "default_subscription_mode")]
    pub mode: SubscriptionMode,
    pub since_date: Option<String>,
    pub check_interval_hours: i64,
    pub discovery_offset: i64,
    /// Whether the last discovery walk reached the end of the creator's list.
    /// Until it has, "no gap" only means "nothing found yet".
    #[serde(default)]
    pub discovery_complete: bool,
    pub last_discovery_at: Option<String>,
    pub last_error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
    /// Per-day download coverage, newest day first. Empty until the first
    /// round finishes; it is diagnostics for the panel, not part of the
    /// subscription's own columns.
    pub coverage: Vec<DayCoverage>,
}

fn default_subscription_mode() -> SubscriptionMode {
    SubscriptionMode::Manual
}

/// Refresh one subscription's coverage ledger after its files changed.
///
/// Coverage is recomputed from the ledger rather than incremented: retries and
/// type-filter changes can move rows between states, and the ledger is the
/// only single source of truth. Newer `published_at` values win over the
/// service-local date only in that both are always stored in the same
/// normalized `YYYY-MM-DD` shape.
fn read_subscription_coverage(conn: &Connection, subscription_id: i64) -> Result<Vec<DayCoverage>> {
    let post_day = post_day_sql("p", post_source_date_column(conn));
    let mut stmt = conn.prepare(&format!(
        // `since_date` also bounds what is reported. It used to filter only newly
        // discovered posts, so a subscription told to start in 2024 still counted
        // its older rows and reported them as missing no matter what the user had.
        //
        // The counts come from the evaluator's derived state, not from a single
        // `done` flag: a day may only read complete when no post on it is
        // unresolved, and a confirmation is counted separately from an automatic
        // verification instead of being folded into one "downloaded" number.
        //
        // The day is `post_day_sql`, so the calendar groups a work under the day
        // its folder is named after rather than under the source's raw date.
        "SELECT {post_day} AS day,
                COUNT(p.id) AS total_posts,
                SUM(CASE WHEN p.assessment_state IN ('verified', 'confirmed', 'ignored')
                         THEN 1 ELSE 0 END)
                    AS downloaded_posts,
                SUM(CASE WHEN p.assessment_state IN ('confirmed', 'ignored')
                         THEN 1 ELSE 0 END)
                    AS confirmed_posts,
                SUM(CASE WHEN p.assessment_state = 'not_required' THEN 1 ELSE 0 END)
                    AS not_required_posts
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         WHERE p.subscription_id = ?1
           AND (s.since_date IS NULL OR s.since_date = ''
                OR {post_day} >= s.since_date)
         GROUP BY day
         ORDER BY day DESC"
    ))?;
    let rows = stmt
        .query_map(params![subscription_id], |row| {
            Ok(DayCoverage {
                day: row.get(0)?,
                total_posts: row.get::<_, i64>(1)? as usize,
                downloaded_posts: row.get::<_, Option<i64>>(2)?.unwrap_or(0) as usize,
                confirmed_posts: row.get::<_, Option<i64>>(3)?.unwrap_or(0) as usize,
                not_required_posts: row.get::<_, Option<i64>>(4)?.unwrap_or(0) as usize,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

fn refresh_subscription_coverage(
    conn: &Connection,
    subscription_id: i64,
) -> Result<Vec<DayCoverage>> {
    let rows = read_subscription_coverage(conn, subscription_id)?;
    let now = Utc::now().to_rfc3339();
    for coverage in &rows {
        let status = if coverage.complete() {
            "complete"
        } else {
            "pending"
        };
        conn.execute(
            "INSERT INTO kemono_download_coverage (subscription_id, day, status, checked_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(subscription_id, day) DO UPDATE SET
                 status = excluded.status, checked_at = excluded.checked_at",
            params![subscription_id, coverage.day, status, now],
        )?;
    }
    Ok(rows)
}


/// Every day of a subscription that still needs posts, oldest first, over the
/// whole ledger.
///
/// Diagnostics only: the download phase does not consult this list, so a gap
/// day is re-fetched only when its posts come up in the ordinary pending queue.
pub fn pending_coverage_days(conn: &Connection, subscription_id: i64) -> Result<Vec<DayCoverage>> {
    let mut coverage = read_subscription_coverage(conn, subscription_id)?;
    coverage.retain(|day| !day.complete());
    coverage.reverse();
    Ok(coverage)
}

/// Exact number of days that still need posts, over the whole ledger.
///
/// Kept as its own aggregate even though `refresh_subscription_coverage` is no
/// longer windowed: this is one grouped scan with no list materialised, so the
/// round summary does not build a payload it does not need.
pub fn pending_coverage_day_count(conn: &Connection, subscription_id: i64) -> Result<usize> {
    let source_date = post_source_date_column(conn);
    let post_day = post_day_sql("p", source_date);
    let count: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM (
                 SELECT {post_day} AS day,
                        COUNT(p.id) AS total_posts,
                        SUM(CASE WHEN p.assessment_state IN ('verified', 'confirmed', 'ignored', 'not_required')
                                 THEN 1 ELSE 0 END) AS resolved_posts
                 FROM kemono_posts p
                 JOIN kemono_subscriptions s ON s.id = p.subscription_id
                 WHERE p.subscription_id = ?1
                   AND (s.since_date IS NULL OR s.since_date = ''
                        OR {post_day} >= s.since_date)
                 GROUP BY day
                 HAVING total_posts > 0 AND COALESCE(resolved_posts, 0) < total_posts
             )"
        ),
        params![subscription_id],
        |row| row.get(0),
    )?;
    Ok(count.max(0) as usize)
}

/// How many events the panel fetches per request when the caller does not say.
/// A display default for one response, not a bound on what is stored: nothing
/// trims `pawchive_events`, so the history is complete and the caller can ask
/// for any number of rows.
pub const PAWCHIVE_EVENT_PAGE: i64 = 50;

/// One line of Pawchive activity, surfaced by `GET /api/pawchive/events`.
///
/// Rounds, per-subscription outcomes and individual file failures used to leave
/// no durable trace at all (the `kemono_*` ledger records state, not history),
/// so the panel could not show what the last check actually did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PawchiveEvent {
    pub id: i64,
    pub created_at: String,
    pub level: String,
    pub kind: String,
    pub subscription_id: Option<i64>,
    pub artist_name: String,
    pub message: String,
    pub detail: String,
}

/// Record one Pawchive event, to the ledger and to the runtime log.
///
/// Best-effort by design: a diagnostics write must never fail a sync round, so
/// the insert drops its error. Nothing is trimmed, so the event history is
/// complete. Writing the runtime line here rather than at each call site keeps
/// the panel list and `gallery.log` from drifting apart.
pub fn record_pawchive_event(
    conn: &Connection,
    level: &str,
    kind: &str,
    subscription_id: Option<i64>,
    artist_name: &str,
    message: &str,
    detail: &str,
) {
    let now = Utc::now().to_rfc3339();
    let _ = conn.execute(
        "INSERT INTO pawchive_events
             (created_at, level, kind, subscription_id, artist_name, message, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            now,
            level,
            kind,
            subscription_id,
            artist_name,
            message,
            detail
        ],
    );
    let who = if artist_name.is_empty() {
        String::new()
    } else {
        format!(" {artist_name}")
    };
    let why = if detail.is_empty() {
        String::new()
    } else {
        format!(" ({detail})")
    };
    crate::logging::log_event(
        &level.to_ascii_uppercase(),
        &format!("pawchive[{kind}]{who}: {message}{why}"),
    );
}

/// Newest events first. `limit` is the caller's page size, defaulting to
/// `PAWCHIVE_EVENT_PAGE`; nothing is trimmed on the way in, so asking for more
/// rows reaches the whole history.
pub fn list_pawchive_events(conn: &Connection, limit: Option<i64>) -> Result<Vec<PawchiveEvent>> {
    let limit = limit.unwrap_or(PAWCHIVE_EVENT_PAGE).max(1);
    let mut stmt = conn.prepare(
        "SELECT id, created_at, level, kind, subscription_id, artist_name, message, detail
         FROM pawchive_events
         ORDER BY id DESC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit], |row| {
            Ok(PawchiveEvent {
                id: row.get(0)?,
                created_at: row.get(1)?,
                level: row.get(2)?,
                kind: row.get(3)?,
                subscription_id: row.get(4)?,
                artist_name: row.get(5)?,
                message: row.get(6)?,
                detail: row.get(7)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// `2026-09-11 缺 3 个，2026-09-13 缺 1 个` — the gap summary the panel shows.
///
/// Newest days win because `pending_coverage_days` returns oldest first and the
/// day nearest the failure is the one a user recognises. The count is of posts,
/// the same unit coverage is scored in.
pub fn coverage_gap_summary(days: &[DayCoverage]) -> String {
    days.iter()
        .rev()
        .take(3)
        .map(|day| format!("{} 缺 {} 个", day.day, day.missing_posts()))
        .collect::<Vec<_>>()
        .join("，")
}

/// Insert or update a subscription record.
///
/// The subscription stores no per-type filter of its own: which file types are
/// downloaded is the single `PawchiveSettings` decision, which the plan's UI
/// exposes globally. The former per-subscription `include_*` columns are gone
/// because nothing could set them, and their stale default made 压缩包 a no-op.
pub fn upsert_subscription(
    conn: &Connection,
    service: &str,
    user_id: &str,
    artist_id: Option<i64>,
    target_dir: &str,
    since_date: Option<&str>,
) -> Result<i64> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO kemono_subscriptions (
            site_id, service, user_id, artist_id, target_dir, enabled,
            since_date, check_interval_hours, created_at, updated_at
        ) VALUES (
            'pawchive', ?1, ?2, ?3, ?4, 0,
            ?5, 6, ?6, ?6
        ) ON CONFLICT(site_id, service, user_id) DO UPDATE SET
            artist_id = excluded.artist_id,
            target_dir = excluded.target_dir,
            since_date = excluded.since_date,
            updated_at = excluded.updated_at;",
        params![service, user_id, artist_id, target_dir, since_date, now],
    )?;

    let id: i64 = conn.query_row(
        "SELECT id FROM kemono_subscriptions WHERE site_id='pawchive' AND service=?1 AND user_id=?2",
        params![service, user_id],
        |row| row.get(0),
    )?;
    Ok(id)
}

/// Retrieve a subscription by ID.
pub fn get_subscription(conn: &Connection, id: i64) -> Result<Option<SubscriptionRecord>> {
    let record = conn
        .query_row(
            "SELECT s.id, s.site_id, s.service, s.user_id, s.artist_id, a.name, s.target_dir, s.enabled,
                    s.mode, s.since_date, s.check_interval_hours, s.discovery_offset,
                    s.discovery_complete, s.last_discovery_at, s.last_error, s.created_at, s.updated_at
             FROM kemono_subscriptions s
             LEFT JOIN artists a ON a.id = s.artist_id
             WHERE s.id = ?1",
            params![id],
            |row| {
                Ok(SubscriptionRecord {
                    id: row.get(0)?,
                    site_id: row.get(1)?,
                    service: row.get(2)?,
                    user_id: row.get(3)?,
                    artist_id: row.get(4)?,
                    artist_name: row.get(5)?,
                    target_dir: row.get(6)?,
                    enabled: row.get::<_, i64>(7)? != 0,
                    mode: SubscriptionMode::parse(&row.get::<_, String>(8)?),
                    since_date: row.get(9)?,
                    check_interval_hours: row.get(10)?,
                    discovery_offset: row.get(11)?,
                    discovery_complete: row.get::<_, i64>(12)? != 0,
                    last_discovery_at: row.get(13)?,
                    last_error: row.get(14)?,
                    created_at: row.get(15)?,
                    updated_at: row.get(16)?,
                    coverage: Vec::new(),
                })
            },
        )
        .optional()?;
    let mut record = match record {
        Some(record) => record,
        None => return Ok(None),
    };
    record.coverage = read_subscription_coverage(conn, record.id).unwrap_or_default();
    Ok(Some(record))
}

/// List all subscriptions.
///
/// Coverage is attached after the row read: a per-row subquery would couple a
/// plain listing to the file ledger, so each record recomputes its own
/// coverage from the ledger instead.
pub fn list_subscriptions(conn: &Connection) -> Result<Vec<SubscriptionRecord>> {
    let mut stmt = conn.prepare(
        "SELECT s.id, s.site_id, s.service, s.user_id, s.artist_id, a.name, s.target_dir, s.enabled,
                s.mode, s.since_date, s.check_interval_hours, s.discovery_offset,
                s.discovery_complete, s.last_discovery_at, s.last_error, s.created_at, s.updated_at
         FROM kemono_subscriptions s
         LEFT JOIN artists a ON a.id = s.artist_id
         ORDER BY s.id ASC",
    )?;

    let mut rows = stmt
        .query_map([], |row| {
            Ok(SubscriptionRecord {
                id: row.get(0)?,
                site_id: row.get(1)?,
                service: row.get(2)?,
                user_id: row.get(3)?,
                artist_id: row.get(4)?,
                artist_name: row.get(5)?,
                target_dir: row.get(6)?,
                enabled: row.get::<_, i64>(7)? != 0,
                mode: SubscriptionMode::parse(&row.get::<_, String>(8)?),
                since_date: row.get(9)?,
                check_interval_hours: row.get(10)?,
                discovery_offset: row.get(11)?,
                discovery_complete: row.get::<_, i64>(12)? != 0,
                last_discovery_at: row.get(13)?,
                last_error: row.get(14)?,
                created_at: row.get(15)?,
                updated_at: row.get(16)?,
                coverage: Vec::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for record in &mut rows {
        record.coverage = read_subscription_coverage(conn, record.id).unwrap_or_default();
    }
    Ok(rows)
}

/// Delete a subscription by ID.
///
/// The artist's `subscription` profile link for this creator goes with it, so
/// the artist page stops advertising a source that is no longer downloaded.
/// Links to other creators on other services are left untouched.
///
/// Note the delete runs through `ON DELETE CASCADE` for the posts and files
/// but asks for the creator first: the row is gone by the time the link
/// cleanup runs, so the artist/link match must be read before deleting.
pub fn delete_subscription(conn: &Connection, id: i64) -> Result<bool> {
    let creator: Option<(Option<i64>, String, String)> = conn
        .query_row(
            "SELECT artist_id, service, service || '/user/' || user_id
             FROM kemono_subscriptions WHERE id = ?1",
            params![id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let creator = match creator {
        Some(creator) => creator,
        None => return Ok(false),
    };
    // The working ledger cascades from this row, so the long-term ledger has to
    // be written before the delete. Without this, dropping a subscription and
    // adding it back would lose the record of what was already fetched, and the
    // next round would download the library again. The working rows themselves
    // are re-derived from the source on re-discovery; what is not re-derivable
    // is the acquisition history, and that is what survives here.
    // One transaction for the whole migration and the delete. The loop below
    // issues several statements per post, and without a transaction each of them
    // committed on its own: a subscription with 50k posts was ~1.5M WAL commits,
    // the delete request held the write lock for minutes, and a process that
    // died part-way left the ledger half-migrated with its watermark unmoved, so
    // the next startup repeated the work it had already paid for.
    // Keyset pagination bounds memory so a 50k-post creator is never materialized whole.
    let tx = conn.unchecked_transaction()?;
    let mut last_post_id = 0i64;
    const POST_BATCH_SIZE: usize = 500;
    loop {
        let mut stmt = tx.prepare(
            "SELECT id FROM kemono_posts WHERE subscription_id = ?1 AND id > ?2 ORDER BY id ASC LIMIT ?3",
        )?;
        let batch = stmt
            .query_map(params![id, last_post_id, POST_BATCH_SIZE as i64], |row| {
                row.get(0)
            })?
            .collect::<rusqlite::Result<Vec<i64>>>()?;
        let Some(&last_id) = batch.last() else {
            break;
        };
        for post_db_id in &batch {
            materialize_work_ledger(&tx, *post_db_id)?;
        }
        last_post_id = last_id;
    }
    // External receipts are historical facts that survive subscription deletion.
    // Ensure all receipts belonging to posts in this subscription have work_id populated.
    tx.execute(
        "UPDATE pawchive_external_receipts
         SET work_id = (
             SELECT w.work_id FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             JOIN pawchive_remote_works w
               ON w.site_id = s.site_id AND w.service = s.service
              AND w.creator_id = s.user_id AND w.post_id = p.post_id
             WHERE p.id = pawchive_external_receipts.post_id
         )
         WHERE post_id IN (SELECT id FROM kemono_posts WHERE subscription_id = ?1)
           AND work_id = ''",
        params![id],
    )?;
    // Removing the subscription unbinds scheduling. The work rows, their
    // resource versions, their manifests, their acquisition events, and their
    // external receipts stay: they are facts about the source, not about this subscription.
    tx.execute(
        "DELETE FROM pawchive_subscription_works WHERE subscription_id = ?1",
        params![id],
    )?;
    let rows = tx.execute(
        "DELETE FROM kemono_subscriptions WHERE id = ?1",
        params![id],
    )?;
    if rows > 0 {
        // Only subscriptions bound to an artist have a profile link to clean.
        if let (Some(artist_id), service, creator_path) = creator {
            let _ = tx.execute(
                // `instr`, not `LIKE '%' || ?2 || '%'`: the creator path
                // (`?2`) is an identifier that may legally contain `%` or
                // `_`. Bound into a LIKE pattern those are wildcards, so
                // deleting the subscription for creator `a_b` also deleted the
                // link for `axb`. `instr` compares the substring literally.
                "DELETE FROM artist_profile_links
                 WHERE artist_id = ?1 AND kind = 'subscription'
                   AND (instr(url, ?2) > 0 OR platform = ?3)",
                params![artist_id, creator_path, service],
            );
        }
    }
    tx.commit()?;
    Ok(rows > 0)
}

/// Toggle subscription enabled state.
pub fn toggle_subscription(conn: &Connection, id: i64, enabled: bool) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let rows = conn.execute(
        "UPDATE kemono_subscriptions SET enabled = ?1, updated_at = ?2 WHERE id = ?3",
        params![enabled as i64, now, id],
    )?;
    Ok(rows > 0)
}

/// Set whether the subscription's works may be claimed by an automatic round.
///
/// Changing the mode is not a download request and does not touch the ledger,
/// the acquisition history or any in-flight task; it only changes who decides
/// that this subscription's missing resources should be fetched. Switching to
/// manual therefore stops *new* automatic claims from the next one onwards,
/// while work already running finishes at its own safe boundary.
pub fn set_subscription_mode(conn: &Connection, id: i64, mode: SubscriptionMode) -> Result<bool> {
    let now = Utc::now().to_rfc3339();
    let rows = conn.execute(
        "UPDATE kemono_subscriptions SET mode = ?1, updated_at = ?2 WHERE id = ?3",
        params![mode.as_str(), now, id],
    )?;
    Ok(rows > 0)
}

pub const PAWCHIVE_SETTINGS_KEY: &str = "pawchive_settings_v1";

/// Settings for Pawchive background download and naming rules.
///
/// `#[serde(default)]` is load-bearing, not decoration: settings written by an
/// older build have no `download_audio` / `download_others` key, and without it
/// deserialization would fail and `get_pawchive_settings` would silently reset
/// every stored value back to the defaults.
const fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct PawchiveSettings {
    pub enabled: bool,
    #[serde(alias = "jdownloader_watch")]
    pub auto_netdisk: bool,
    pub auto_ingest: bool,
    pub interval_hours: u32,
    pub download_images: bool,
    pub download_videos: bool,
    pub download_audio: bool,
    pub download_archives: bool,
    pub download_others: bool,
    #[serde(default = "default_true")]
    pub download_content: bool,
    #[serde(default = "default_true")]
    pub download_links: bool,
    pub folder_template: String,
    pub image_template: String,
    pub attachment_template: String,
}

impl Default for PawchiveSettings {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_netdisk: true,
            auto_ingest: false,
            interval_hours: 6,
            download_images: true,
            download_videos: true,
            download_audio: true,
            // Archival bundles stay opt-in per the approved plan, and `other`
            // is a catch-all (psd/clip/sai/pdf/txt/...), so it is opt-in too.
            download_archives: false,
            download_others: false,
            download_content: true,
            download_links: true,
            folder_template: "{user}/{date} {title}/".to_string(),
            image_template: "{date} {index}.{ext}".to_string(),
            attachment_template: "{date} {name}".to_string(),
        }
    }
}

pub fn get_pawchive_settings(conn: &Connection) -> Result<PawchiveSettings> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT value FROM app_settings WHERE key = ?1",
            params![PAWCHIVE_SETTINGS_KEY],
            |row| row.get(0),
        )
        .optional()?;

    match raw {
        Some(s) => match serde_json::from_str::<PawchiveSettings>(&s) {
            Ok(settings) => Ok(settings),
            Err(_) => Ok(PawchiveSettings::default()),
        },
        None => Ok(PawchiveSettings::default()),
    }
}

pub fn save_pawchive_settings(conn: &Connection, settings: &PawchiveSettings) -> Result<()> {
    let serialized = serde_json::to_string(settings)?;
    conn.execute(
        "INSERT INTO app_settings (key, value, updated_at) VALUES (?1, ?2, strftime('%s','now'))
         ON CONFLICT(key) DO UPDATE SET value = excluded.value, updated_at = excluded.updated_at;",
        params![PAWCHIVE_SETTINGS_KEY, serialized],
    )?;
    // The type switches are part of the selection, so they change what every
    // post requires. Re-deriving here keeps the queue, the coverage and the
    // panel on one scope instead of leaving them on the previous one.
    let _ = reassess_all_posts(conn);
    Ok(())
}

/// Add subscription by parsing creator URL and resolving target folder.
///
/// When the subscription binds to an existing artist, the creator URL is also
/// recorded as that artist's `subscription` profile link, so the artist page's
/// 社交主页与订阅平台 section shows the source the download panel is reading
/// from. The link row is best-effort: URL normalization can reject an
/// exotic-but-downloadable creator address, and that must not fail the
/// subscription itself.
pub fn add_subscription_from_url(
    conn: &Connection,
    url: &str,
    target_dir: Option<&str>,
    since_date: Option<&str>,
    roots: &MediaRoots,
) -> Result<SubscriptionRecord> {
    let (service, user_id) = parse_creator_url(url)?;

    let resolved_target = if let Some(td) = target_dir.filter(|t| !t.trim().is_empty()) {
        let p = PathBuf::from(td);
        // A target that starts at the filesystem root is a folder address, not a
        // name. `Path::is_absolute` alone is not enough: on Windows a POSIX path
        // like `/pictures1/Artist` has no drive prefix and reads as relative.
        let addressed_path = p.is_absolute() || td.starts_with('/') || td.starts_with('\\');
        if addressed_path {
            td.to_string()
        } else {
            // A relative target names a folder under the first authorized root.
            // Only plain path components are accepted: `..` would place the
            // subscription outside every root, where no publish could ever
            // deliver into, so it is refused at the boundary instead of being
            // stored as a subscription that can only fail.
            if !p
                .components()
                .all(|c| matches!(c, std::path::Component::Normal(_)))
            {
                return Err(anyhow!("invalid folder name: {td}"));
            }
            if let Some(first_root) = roots.real_paths.first().or(roots.roots.first()) {
                PathBuf::from(first_root)
                    .join(td)
                    .to_string_lossy()
                    .replace('\\', "/")
            } else {
                td.to_string()
            }
        }
    } else if let Some(first_root) = roots.real_paths.first().or(roots.roots.first()) {
        PathBuf::from(first_root)
            .join(&user_id)
            .to_string_lossy()
            .replace('\\', "/")
    } else {
        user_id.clone()
    };

    let artist_id: Option<i64> = conn
        .query_row(
            "SELECT id FROM artists WHERE path = ?1 OR name = ?2 LIMIT 1",
            params![resolved_target, user_id],
            |row| row.get(0),
        )
        .optional()?;

    let id = upsert_subscription(
        conn,
        &service,
        &user_id,
        artist_id,
        &resolved_target,
        since_date,
    )?;

    if let Some(artist_id) = artist_id {
        let _ = crate::artist_profile_links::create_artist_profile_link(
            conn,
            artist_id,
            "subscription",
            &service,
            url.trim(),
        );
    }

    get_subscription(conn, id)?.ok_or_else(|| anyhow!("failed to retrieve added subscription"))
}

/// Canonical fingerprint of a post's resource set.
///
/// Sorted, so a re-ordered attachment list is the same manifest; the count is
/// mixed in so a truncated list cannot collide with the full one on the hash of
/// a shared prefix.
fn manifest_fingerprint(keys: &mut Vec<String>) -> String {
    keys.sort();
    keys.dedup();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(keys.len() as u64).to_le_bytes());
    for key in keys.iter() {
        hasher.update(key.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// Record discovered posts and their files into SQLite ledger.
/// Returns count of new posts stored.
///
/// Every attachment the source lists is stored, whether or not its type is
/// currently selected: the type switches decide what a post *requires*, and
/// dropping the rows here meant turning a type on again needed a re-discovery
/// and could still be blocked by an old post status.
pub fn record_discovered_posts(
    conn: &Connection,
    sub_id: i64,
    posts: &[PawchivePost],
    since_date: Option<&str>,
) -> Result<usize> {
    // One page is one unit of discovery: the loop in the inner function issues
    // several statements per post (upsert, file rows, manifest version,
    // long-term observation) and each one committed on its own, so a page that
    // failed or was killed half-way left posts with a partial file list and an
    // unmoved manifest version — a state the source never had.
    let tx = conn.unchecked_transaction()?;
    let count = record_discovered_posts_in(&tx, sub_id, posts, since_date)?;
    tx.commit()?;
    Ok(count)
}

fn record_discovered_posts_in(
    tx: &Connection,
    sub_id: i64,
    posts: &[PawchivePost],
    since_date: Option<&str>,
) -> Result<usize> {
    let now = Utc::now().to_rfc3339();
    let mut new_posts_count = 0;

    for post in posts {
        // Date boundary filter
        if let (Some(since), Some(pub_at)) = (since_date, post.published.as_deref()) {
            if pub_at < since {
                continue;
            }
        }

        let ext_links = post
            .content
            .as_deref()
            .map(extract_external_links)
            .unwrap_or_default();
        let ext_links_json = serde_json::to_string(&ext_links)?;

        // The date this post's folder is named after, derived from the same
        // function the naming context uses. Computed here rather than in SQL
        // because the shift depends on the service's own timezone, which a
        // pure-SQL expression cannot reproduce faithfully.
        let published_local_date =
            normalize_post_date(post.published.as_deref(), Some(post.service.as_str()));

        // What this response actually saw of the resource set. Everything below
        // that decides what the work *requires* is gated on it: a summary that
        // never mentioned the resource fields has not said the post is empty.
        let completeness = post.manifest_completeness();

        // An UPSERT reports one changed row whether it inserted or updated, so
        // `execute` cannot tell a freshly discovered post from one that is
        // already in the ledger. Ask first: only a post with no stored row is
        // new, otherwise every round would report the whole back catalogue as
        // 新增 and the sync summary would never drop to zero.
        let already_stored = tx
            .query_row(
                "SELECT 1 FROM kemono_posts WHERE subscription_id = ?1 AND post_id = ?2",
                params![sub_id, post.id],
                |_| Ok(()),
            )
            .optional()?
            .is_some();

        // A field the response omitted keeps whatever the ledger already had.
        // Overwriting it with the `None` that a missing key deserialises to
        // would erase the stored metadata — and, for `content`, the external
        // links derived from it, which are part of the manifest.
        tx.execute(
            "INSERT INTO kemono_posts (
                subscription_id, post_id, title, published_at, published_local_date, edited_at,
                content, external_links, status, created_at, updated_at,
                manifest_version, manifest_fingerprint
            ) VALUES (?1, ?2, ?3, ?4, ?10, ?5, ?6, ?7, 'pending', ?8, ?8, 1, '')
            ON CONFLICT(subscription_id, post_id) DO UPDATE SET
                title = COALESCE(excluded.title, kemono_posts.title),
                published_at = COALESCE(excluded.published_at, kemono_posts.published_at),
                -- Kept in step with `published_at` above: `excluded` already
                -- carries the date derived from whichever value won the COALESCE,
                -- because both are set from the same response here.
                published_local_date = CASE
                    WHEN COALESCE(excluded.published_at, kemono_posts.published_at)
                         = excluded.published_at
                    THEN excluded.published_local_date
                    ELSE kemono_posts.published_local_date END,
                edited_at = COALESCE(excluded.edited_at, kemono_posts.edited_at),
                content = COALESCE(excluded.content, kemono_posts.content),
                external_links = CASE WHEN ?9 THEN excluded.external_links
                                      ELSE kemono_posts.external_links END,
                updated_at = excluded.updated_at;",
            params![
                sub_id,
                post.id,
                post.title.as_deref(),
                post.published.as_deref(),
                post.edited.as_deref(),
                post.content.as_deref(),
                ext_links_json,
                now,
                post.content_present,
                published_local_date,
            ],
        )?;

        if !already_stored {
            new_posts_count += 1;
        }

        let post_db_id: i64 = tx.query_row(
            "SELECT id FROM kemono_posts WHERE subscription_id=?1 AND post_id=?2",
            params![sub_id, post.id],
            |row| row.get(0),
        )?;

        // Re-link preserved external receipts for this work if any, and restore
        // external delivery state if a settled receipt exists.
        if let Some(work_id) = work_id_for_post(tx, post_db_id)? {
            tx.execute(
                "UPDATE pawchive_external_receipts
                 SET post_id = ?1
                 WHERE work_id = ?2 AND (post_id IS NULL OR post_id != ?1)",
                params![post_db_id, work_id],
            )?;
            let settled_receipt: Option<(String, String, i64)> = tx
                .query_row(
                    "SELECT task_id, recorded_at, manifest_version
                     FROM pawchive_external_receipts
                     WHERE work_id = ?1 AND settled = 1
                     ORDER BY id DESC LIMIT 1",
                    params![work_id],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                )
                .optional()?;
            if let Some((task_id, recorded_at, m_version)) = settled_receipt {
                tx.execute(
                    "UPDATE kemono_posts
                     SET external_task_id = ?1, external_delivered_at = ?2, external_manifest_version = ?3
                     WHERE id = ?4 AND external_delivered_at IS NULL",
                    params![task_id, recorded_at, m_version, post_db_id],
                )?;
            }
        }

        // Collect all file refs (primary file + attachments)
        let mut file_refs = Vec::new();
        if let Some(primary) = &post.file {
            file_refs.push(primary);
        }
        for att in &post.attachments {
            file_refs.push(att);
        }

        let mut manifest_keys: Vec<String> = Vec::new();
        let mut current_identities: Vec<String> = Vec::new();
        for fref in file_refs {
            let remote_path = match &fref.path {
                Some(p) if !p.is_empty() => p,
                _ => continue,
            };
            let file_name = fref
                .name
                .as_deref()
                .unwrap_or_else(|| remote_path.rsplit('/').next().unwrap_or("unnamed"));

            let category = FileCategory::from_filename(file_name);
            // The resource key is the source's own identity for the attachment,
            // when it publishes one; otherwise the path it serves the file at.
            // It used to be prefixed with the attachment's array index, which is
            // a position, not an identity: inserting one attachment renumbered
            // every later one and inserted duplicates of resources already in
            // the ledger. The basis is recorded next to the row so a reader can
            // tell an identity from a location standing in for one.
            let normalized_path = normalize_remote_path(remote_path);
            let (source_identity, identity_basis) = fref.resource_identity();
            let source_identity = if source_identity.is_empty() {
                normalized_path.clone()
            } else {
                source_identity
            };
            manifest_keys.push(source_identity.clone());
            current_identities.push(source_identity.clone());

            // If the source published a real attachment id, check if an existing row
            // was previously recorded with identity_basis = 'path' for this same remote_path.
            // If so, upgrade it with move_resource_identity so its events are carried across!
            if identity_basis == IDENTITY_BASIS_SOURCE_ID {
                let existing_path_row: Option<i64> = tx
                    .query_row(
                        "SELECT id FROM kemono_files
                         WHERE post_id = ?1 AND remote_path = ?2 AND identity_basis = ?3",
                        params![post_db_id, normalized_path, IDENTITY_BASIS_PATH],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(existing_file_id) = existing_path_row {
                    move_resource_identity(
                        tx,
                        existing_file_id,
                        &normalized_path,
                        &source_identity,
                    )?;
                    continue;
                }
            }

            tx.execute(
                "INSERT INTO kemono_files (
                    post_id, source_identity, remote_path, file_name,
                    file_type, status, created_at, updated_at, identity_basis
                ) VALUES (?1, ?2, ?3, ?4, ?5, 'pending', ?6, ?6, ?7)
                ON CONFLICT(post_id, source_identity) DO NOTHING;",
                params![
                    post_db_id,
                    source_identity,
                    normalized_path,
                    file_name,
                    category.as_str(),
                    now,
                    identity_basis,
                ],
            )?;
        }

        // An attachment the source has withdrawn leaves the current
        // requirement. Its row, its downloaded file and its history stay; only
        // its participation in what this work still needs ends. Without this a
        // removed attachment kept counting forever, so a post could never read
        // as complete even after everything still listed had arrived.
        //
        // This runs only on an observation that actually listed the resource
        // fields. `attachments` is what a summary is allowed to leave out, and
        // reading an omitted key as an empty list retired every attachment the
        // post was known to have — after which the post read as requiring
        // nothing at all, which is the one answer that stops a download.
        if completeness == ManifestCompleteness::Complete {
            if current_identities.is_empty() {
                tx.execute(
                    "UPDATE kemono_files
                     SET present_in_manifest = 0, retired_at = ?1, updated_at = ?1
                     WHERE post_id = ?2 AND present_in_manifest = 1",
                    params![now, post_db_id],
                )?;
            } else {
                let placeholders = vec!["?"; current_identities.len()].join(", ");
                let mut values: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now.clone())];
                values.push(Box::new(post_db_id));
                for identity in &current_identities {
                    values.push(Box::new(identity.clone()));
                }
                let retired = tx.execute(
                    &format!(
                        "UPDATE kemono_files
                         SET present_in_manifest = 0, retired_at = ?1, updated_at = ?1
                         WHERE post_id = ?2 AND present_in_manifest = 1
                           AND source_identity NOT IN ({placeholders})"
                    ),
                    rusqlite::params_from_iter(values.iter().map(|value| value.as_ref())),
                )?;
                let _ = retired;
                // An attachment that came back is required again.
                tx.execute(
                    &format!(
                        "UPDATE kemono_files
                         SET present_in_manifest = 1, retired_at = NULL, updated_at = ?1
                         WHERE post_id = ?2 AND present_in_manifest = 0
                           AND source_identity IN ({placeholders})"
                    ),
                    rusqlite::params_from_iter(values.iter().map(|value| value.as_ref())),
                )?;
            }

            // The manifest is what the work needs delivered, and an external
            // bundle is part of that: swapping the netdisk link for a different
            // one changes the requirement, so the version has to move. It did
            // not before, which let a confirmation or a receipt for the old
            // bundle keep covering the new one.
            //
            // The version moves only for a complete observation, because only a
            // complete one can say the requirement changed. Bumping it on a
            // summary would invalidate a confirmation without any resource
            // having changed.
            for link in &ext_links {
                manifest_keys.push(format!("external:{link}"));
            }
            let fingerprint = manifest_fingerprint(&mut manifest_keys);
            let stored: Option<(i64, String)> = tx
                .query_row(
                    "SELECT manifest_version, manifest_fingerprint FROM kemono_posts WHERE id = ?1",
                    params![post_db_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let (version, stored_fingerprint) = stored.unwrap_or((0, String::new()));
            if stored_fingerprint != fingerprint {
                tx.execute(
                    "UPDATE kemono_posts
                     SET manifest_version = ?1, manifest_fingerprint = ?2, updated_at = ?3
                     WHERE id = ?4",
                    params![version + 1, fingerprint, now, post_db_id],
                )?;
            }
        }

        // Record the observation itself. The working ledger holds the latest
        // reading; the long-term ledger holds what was read and how much of it
        // was seen, so a later summary cannot pass for a complete listing.
        record_work_observation(tx, post_db_id, completeness)?;
    }

    Ok(new_posts_count)
}

/// Link delivered files to their media-library rows, and follow a moved item.
///
/// Tags and every other library fact bind to `items.id`, and a file the user
/// later moves inside the library keeps that id while its path changes. Storing
/// both means a moved delivery still resolves to the work it belongs to, instead
/// of reading as a file that vanished.
pub fn link_evidence_to_items(conn: &Connection) -> Result<usize> {
    // Candidate identity and the binding must observe one database snapshot.
    let tx = conn.unchecked_transaction()?;
    let conn = &tx;
    let has_items = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'items'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if !has_items {
        return Ok(0);
    }
    // A path that moved is followed first: the identity survives the move.
    let moved = conn.execute(
        "UPDATE kemono_files
         SET target_path = (SELECT i.file_path FROM items i WHERE i.id = kemono_files.evidence_item_id),
             updated_at = ?1
         WHERE evidence_item_id IS NOT NULL
           AND EXISTS (SELECT 1 FROM items i
                        WHERE i.id = kemono_files.evidence_item_id
                          AND i.missing = 0
                          AND i.file_path != kemono_files.target_path)",
        params![Utc::now().to_rfc3339()],
    )?;
    // For unlinked deliveries, resolve candidate items with identity and move-chain constraints.
    let has_move_history = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'move_history'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    let has_jobs = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'download_publish_jobs'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();

    // Check optional columns
    let has_item_file_size = conn
        .prepare("PRAGMA table_info(items)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .any(|col| col.as_deref() == Ok("file_size"));
    let item_columns = conn
        .prepare("PRAGMA table_info(items)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let has_item_hash = item_columns.iter().any(|name| name == "content_hash");
    let has_hash_status = item_columns.iter().any(|name| name == "hash_status");

    let has_mh_created = if has_move_history {
        conn.prepare("PRAGMA table_info(move_history)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .any(|col| col.as_deref() == Ok("created_at"))
    } else {
        false
    };

    let has_expected_length = conn
        .prepare("PRAGMA table_info(kemono_files)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .any(|col| col.as_deref() == Ok("expected_length"));
    let has_publish_job_id = conn
        .prepare("PRAGMA table_info(kemono_files)")?
        .query_map([], |row| row.get::<_, String>(1))?
        .any(|col| col.as_deref() == Ok("publish_job_id"));

    // A path (even with the right size and a recent move) only finds a
    // candidate. A published artifact must also match the indexed BLAKE3.
    // Missing/pending hashes stay unbound until the normal hash worker finishes.
    let matches_published_artifact = |file_id: i64, item_id: i64| -> Result<bool> {
        if !has_publish_job_id {
            return Ok(true); // Legacy delivery without a publish ledger.
        }
        let job_id: Option<i64> = conn.query_row(
            "SELECT publish_job_id FROM kemono_files WHERE id = ?1",
            params![file_id],
            |row| row.get(0),
        )?;
        let Some(job_id) = job_id else {
            return Ok(true);
        };
        if !has_jobs || !has_item_hash || !has_item_file_size {
            return Ok(false);
        }
        let hash_ready = if has_hash_status {
            "AND i.hash_status = 'done'"
        } else {
            ""
        };
        Ok(conn
            .query_row(
                &format!(
                    "SELECT 1 FROM download_publish_jobs j JOIN items i ON i.id = ?2
                     WHERE j.id = ?1 AND j.stage IN ('published', 'ingested')
                       AND i.missing = 0 AND i.file_size = j.expected_length
                       AND length(j.expected_blake3) = 64
                       AND lower(i.content_hash) = lower(j.expected_blake3) {hash_ready}"
                ),
                params![job_id, item_id],
                |_| Ok(()),
            )
            .optional()?
            .is_some())
    };

    struct UnlinkedFile {
        id: i64,
        target_path: String,
        expected_length: Option<u64>,
        job_created_at: Option<f64>,
    }

    let unlinked_sql = match (has_publish_job_id, has_jobs, has_expected_length) {
        (true, true, true) => {
            "SELECT f.id, f.target_path, COALESCE(j.expected_length, f.expected_length), j.created_at
             FROM kemono_files f
             LEFT JOIN download_publish_jobs j ON j.id = f.publish_job_id
             WHERE f.evidence_item_id IS NULL AND f.target_path != '' AND f.id > ?1
             ORDER BY f.id ASC LIMIT ?2"
        }
        (true, true, false) => {
            "SELECT f.id, f.target_path, j.expected_length, j.created_at
             FROM kemono_files f
             LEFT JOIN download_publish_jobs j ON j.id = f.publish_job_id
             WHERE f.evidence_item_id IS NULL AND f.target_path != '' AND f.id > ?1
             ORDER BY f.id ASC LIMIT ?2"
        }
        (false, _, true) => {
            "SELECT f.id, f.target_path, f.expected_length, NULL
             FROM kemono_files f
             WHERE f.evidence_item_id IS NULL AND f.target_path != '' AND f.id > ?1
             ORDER BY f.id ASC LIMIT ?2"
        }
        _ => {
            "SELECT f.id, f.target_path, NULL, NULL
             FROM kemono_files f
             WHERE f.evidence_item_id IS NULL AND f.target_path != '' AND f.id > ?1
             ORDER BY f.id ASC LIMIT ?2"
        }
    };

    let now_str = Utc::now().to_rfc3339();
    let mut resolved_links = 0usize;
    let mut last_unlinked_id = 0i64;
    const UNLINKED_BATCH_LIMIT: usize = 500;

    loop {
        let unlinked_rows: Vec<UnlinkedFile> = {
            let mut stmt = conn.prepare(unlinked_sql)?;
            let rows = stmt.query_map(
                params![last_unlinked_id, UNLINKED_BATCH_LIMIT as i64],
                |row| {
                    let id: i64 = row.get(0)?;
                    let target_path: String = row.get(1)?;
                    let expected_length: Option<u64> =
                        row.get::<_, Option<i64>>(2)?.map(|l| l as u64);
                    let job_created_at: Option<f64> =
                        match row.get::<_, rusqlite::types::Value>(3)? {
                            rusqlite::types::Value::Real(r) => Some(r),
                            rusqlite::types::Value::Integer(i) => Some(i as f64),
                            rusqlite::types::Value::Text(s) => s.parse::<f64>().ok(),
                            _ => None,
                        };
                    Ok(UnlinkedFile {
                        id,
                        target_path,
                        expected_length,
                        job_created_at,
                    })
                },
            )?;
            rows.collect::<rusqlite::Result<Vec<_>>>()?
        };
        let Some(last) = unlinked_rows.last() else {
            break;
        };
        last_unlinked_id = last.id;

        for unlinked in unlinked_rows {
            let mut move_resolved = false;
            if has_move_history {
                let mh_sql = if has_mh_created && has_item_file_size {
                    "SELECT mh.item_id, i.file_path, mh.created_at, i.file_size
                 FROM move_history mh
                 JOIN items i ON i.id = mh.item_id
                 WHERE mh.old_path = ?1 AND mh.status = 'applied' AND i.missing = 0
                 ORDER BY mh.id DESC LIMIT 1"
                } else {
                    "SELECT mh.item_id, i.file_path, 0.0, 0
                 FROM move_history mh
                 JOIN items i ON i.id = mh.item_id
                 WHERE mh.old_path = ?1 AND mh.status = 'applied' AND i.missing = 0
                 ORDER BY mh.id DESC LIMIT 1"
                };
                let mut mh_stmt = conn.prepare(mh_sql)?;
                let mh_match = mh_stmt
                    .query_row(params![unlinked.target_path], |row| {
                        let mh_created = match row.get::<_, rusqlite::types::Value>(2)? {
                            rusqlite::types::Value::Real(r) => r,
                            rusqlite::types::Value::Integer(i) => i as f64,
                            rusqlite::types::Value::Text(s) => s.parse::<f64>().unwrap_or(0.0),
                            _ => 0.0,
                        };
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            mh_created,
                            row.get::<_, i64>(3)? as u64,
                        ))
                    })
                    .optional()?;

                if let Some((mh_item_id, new_path, mh_created, item_size)) = mh_match {
                    let is_stale_pre_publish_move = match (unlinked.job_created_at, has_mh_created)
                    {
                        (Some(job_created), true) if mh_created > 0.0 => {
                            mh_created < job_created - 60.0
                        }
                        _ => false,
                    };
                    let is_size_mismatch = match (unlinked.expected_length, has_item_file_size) {
                        (Some(expected), true) if item_size > 0 && expected > 0 => {
                            item_size != expected
                        }
                        _ => false,
                    };

                    if !is_stale_pre_publish_move
                        && !is_size_mismatch
                        && matches_published_artifact(unlinked.id, mh_item_id)?
                    {
                        conn.execute(
                            "UPDATE kemono_files
                         SET evidence_item_id = ?1, target_path = ?2, updated_at = ?3
                         WHERE id = ?4",
                            params![mh_item_id, new_path, now_str, unlinked.id],
                        )?;
                        resolved_links += 1;
                        move_resolved = true;
                    }
                }
            }

            if move_resolved {
                continue;
            }

            // Second, check direct path in items:
            let item_sql = if has_item_file_size {
                "SELECT i.id, i.file_path, i.file_size FROM items i WHERE i.file_path = ?1 AND i.missing = 0"
            } else {
                "SELECT i.id, i.file_path, 0 FROM items i WHERE i.file_path = ?1 AND i.missing = 0"
            };
            let mut item_stmt = conn.prepare(item_sql)?;
            let item_match = item_stmt
                .query_row(params![unlinked.target_path], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, i64>(2)? as u64,
                    ))
                })
                .optional()?;

            if let Some((item_id, _, item_size)) = item_match {
                let is_size_mismatch = match (unlinked.expected_length, has_item_file_size) {
                    (Some(expected), true) if item_size > 0 && expected > 0 => {
                        item_size != expected
                    }
                    _ => false,
                };

                let has_active_move = if has_move_history {
                    if let (true, Some(created_at)) = (has_mh_created, unlinked.job_created_at) {
                        conn.query_row(
                            "SELECT 1 FROM move_history
                         WHERE old_path = ?1 AND status = 'applied' AND created_at >= ?2",
                            params![unlinked.target_path, created_at - 60.0],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some()
                    } else {
                        conn.query_row(
                            "SELECT 1 FROM move_history WHERE old_path = ?1 AND status = 'applied'",
                            params![unlinked.target_path],
                            |_| Ok(()),
                        )
                        .optional()?
                        .is_some()
                    }
                } else {
                    false
                };

                if !is_size_mismatch
                    && !has_active_move
                    && matches_published_artifact(unlinked.id, item_id)?
                {
                    conn.execute(
                    "UPDATE kemono_files SET evidence_item_id = ?1, updated_at = ?2 WHERE id = ?3",
                    params![item_id, now_str, unlinked.id],
                )?;
                    resolved_links += 1;
                }
            }
        }
    }
    // This is the ingest handoff the publish ledger was waiting for: the scan
    // has now turned the artifact into a library row, so the job that published
    // it can say so. Without this the job stayed at `published` forever and
    // `mark_job_ingested` had no caller outside its own test, which meant the
    // ledger could not tell a published file from an indexed one.
    let mut ingested = 0;
    if has_jobs && has_publish_job_id {
        let mut last_seen_id = 0i64;
        const INGEST_BATCH_LIMIT: usize = 500;
        loop {
            let mut stmt = conn.prepare(
                "SELECT j.id, f.evidence_item_id
                 FROM download_publish_jobs j
                 JOIN kemono_files f ON f.publish_job_id = j.id
                 WHERE j.stage = 'published' AND f.evidence_item_id IS NOT NULL AND j.id > ?1
                 ORDER BY j.id ASC
                 LIMIT ?2",
            )?;
            let batch = stmt
                .query_map(params![last_seen_id, INGEST_BATCH_LIMIT as i64], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })?
                .collect::<rusqlite::Result<Vec<(i64, i64)>>>()?;
            let Some(&(last_job_id, _)) = batch.last() else {
                break;
            };
            for (job_id, item_id) in &batch {
                if crate::ingest_publish::mark_job_ingested(conn, *job_id, *item_id).is_ok() {
                    ingested += 1;
                }
            }
            last_seen_id = last_job_id;
        }
    }
    tx.commit()?;
    Ok(moved + resolved_links + ingested)
}

/// One library item that may already hold a post's content.
///
/// Candidates come from the existing media index rather than a walk of the
/// artist's directory: the index already knows every file, its size, its date
/// and its hash, so listing candidates is a query and not a scan.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PawchiveCandidate {
    pub item_id: i64,
    pub file_path: String,
    pub file_name: String,
    pub file_size: i64,
    pub content_hash: String,
    /// Which rule matched: the post's own folder, or the same artist and day.
    pub reason: String,
}

/// Candidate items for one post, strongest evidence first.
pub fn list_post_candidates(
    conn: &Connection,
    post_db_id: i64,
    limit: Option<i64>,
) -> Result<Vec<PawchiveCandidate>> {
    let has_items = conn
        .query_row(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'items'",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .is_some();
    if !has_items {
        return Ok(Vec::new());
    }
    let limit = limit
        .unwrap_or(crate::DEFAULT_ITEM_PAGE_LIMIT)
        .clamp(1, crate::MAX_ITEM_PAGE_LIMIT);
    // The scope carries the rendered prefix, the day and the title, because the
    // "is this the post's own folder" decision cannot be made in SQL: it has to
    // tolerate the older naming rules, which SQL cannot express. So the query
    // selects the raw fields and the classification happens below.
    let scope = post_folder_prefix(conn, post_db_id)?;
    let day_prefix = if scope.day.is_empty() {
        String::new()
    } else {
        scope.day.clone()
    };
    let mut stmt = conn.prepare(
        "SELECT i.id, i.file_path, i.file_name, i.file_size, i.content_hash
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         JOIN items i ON i.artist_id = s.artist_id AND i.missing = 0
         WHERE p.id = ?1
           AND (
                (?2 != '' AND substr(i.file_path, 1, length(?2)) = ?2)
             OR (
                  COALESCE(NULLIF(i.manual_date, ''), NULLIF(i.detected_date, ''), i.date) != ''
              AND abs(julianday(COALESCE(NULLIF(i.manual_date, ''), NULLIF(i.detected_date, ''), i.date))
                    - julianday(substr(COALESCE(NULLIF(p.published_at, ''), p.created_at), 1, 10))) <= 3
             )
           )
         ORDER BY i.id ASC
         LIMIT ?3",
    )?;
    let mut rows = stmt
        .query_map(params![post_db_id, day_prefix, limit], |row| {
            // A NULL in any of these must not silently drop a candidate: the
            // library's own columns are NOT NULL, but this query is also read
            // against older or hand-built databases.
            Ok(PawchiveCandidate {
                item_id: row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                file_path: row.get::<_, Option<String>>(1)?.unwrap_or_default(),
                file_name: row.get::<_, Option<String>>(2)?.unwrap_or_default(),
                file_size: row.get::<_, Option<i64>>(3)?.unwrap_or(0),
                content_hash: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                reason: String::new(),
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for candidate in &mut rows {
        candidate.reason = scope.classify(&candidate.file_path).to_string();
    }
    // The post's own folder ranks above a same-day neighbour. Sorting in Rust
    // rather than SQL because the rank now depends on the tolerant match.
    rows.sort_by_key(|candidate| (u8::from(candidate.reason != "folder"), candidate.item_id));
    Ok(rows)
}

/// Whether a directory name belongs to a post, tolerating how the naming rules
/// have changed over time.
///
/// The template is user-editable and its output has changed across releases, so
/// a folder written by an older rule must still be recognised as this work's.
/// A literal prefix comparison does not survive that: the historical name
/// `2026-07-21 17-302026／7／21 木偶2 8P 差分` carries a `HH-MM` segment the
/// current `{date}` never produces, and its title keeps a full-width `／` that
/// `sanitize_path_component` rewrites to `-`. Comparing those two strings
/// rejects a folder that is plainly the same work, which sends a downloaded
/// post back to the download queue.
///
/// So the comparison is deliberately narrow, not loose:
/// - the day must match — the folder is named after the post's day, and this is
///   the one part both rules agree on;
/// - the titles must match after normalisation.
///
/// What is *not* relaxed is the title itself. Numbers, sequence markers and
/// version words stay significant, so `維琳娜2 9P` never matches `維琳娜3 9P`:
/// two works by one artist can share a day, and the folder name is the only
/// thing that tells them apart.
/// Returns date representation variants for a given `YYYY-MM-DD` day string,
/// covering hyphens, underscores, dots, compact 8-digit format, unpadded months/days,
/// and Chinese date characters.
fn date_variants(day: &str) -> Vec<String> {
    let mut variants = Vec::new();
    if day.is_empty() {
        return variants;
    }
    variants.push(day.to_string());
    let parts: Vec<&str> = day.split('-').collect();
    if parts.len() == 3 {
        let (y, m, d) = (parts[0], parts[1], parts[2]);
        let m_unpadded = m.trim_start_matches('0');
        let m_unpadded = if m_unpadded.is_empty() {
            "0"
        } else {
            m_unpadded
        };
        let d_unpadded = d.trim_start_matches('0');
        let d_unpadded = if d_unpadded.is_empty() {
            "0"
        } else {
            d_unpadded
        };

        let months = if m_unpadded != m {
            vec![m, m_unpadded]
        } else {
            vec![m]
        };
        let days = if d_unpadded != d {
            vec![d, d_unpadded]
        } else {
            vec![d]
        };

        for &mo in &months {
            for &da in &days {
                variants.push(format!("{y}_{mo}_{da}"));
                variants.push(format!("{y}.{mo}.{da}"));
                variants.push(format!("{y}-{mo}-{da}"));
                variants.push(format!("{y}年{mo}月{da}日"));
                variants.push(format!("{y}年{mo}月{da}"));
            }
        }
        variants.push(format!("{y}{m}{d}"));
    }
    let mut seen = std::collections::HashSet::new();
    variants.retain(|v| seen.insert(v.clone()));
    variants.sort_by_key(|v| std::cmp::Reverse(v.len()));
    variants
}

/// Strip a date prefix matching `day` or its variants from `folder_name`.
///
/// Supports bracketed dates `[2026-04-21]` or `(2026-04-21)`, and various date delimiters.
/// Returns the remainder of `folder_name` after the date prefix if matched.
fn strip_day_prefix<'a>(folder_name: &'a str, day: &str) -> Option<&'a str> {
    if folder_name.is_empty() || day.is_empty() {
        return None;
    }
    let trimmed = folder_name.trim();
    let (prefix_bracket, inner) = if let Some(rest) = trimmed.strip_prefix('[') {
        (Some(']'), rest)
    } else if let Some(rest) = trimmed.strip_prefix('(') {
        (Some(')'), rest)
    } else {
        (None, trimmed)
    };

    let variants = date_variants(day);
    for var in variants {
        if let Some(rest) = inner.strip_prefix(&var) {
            let after_bracket = if let Some(closing) = prefix_bracket {
                if let Some(after_closing) = rest.strip_prefix(closing) {
                    after_closing
                } else {
                    continue;
                }
            } else {
                rest
            };

            let Some(first) = after_bracket.chars().next() else {
                return Some("");
            };
            // What follows the day must NOT be an ASCII digit or ASCII letter (which would mean a longer date or word)
            if first.is_ascii_digit() || first.is_ascii_alphabetic() {
                continue;
            }
            return Some(after_bracket);
        }
    }
    None
}

/// Whether `folder_name` contains the post's unique ID bounded by non-alphanumeric characters.
fn folder_name_matches_post_id(folder_name: &str, post_id: &str) -> bool {
    let pid = post_id.trim();
    if pid.len() < 4 {
        return false;
    }
    for (pos, _) in folder_name.match_indices(pid) {
        let before_ok = folder_name[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        let after_idx = pos + pid.len();
        let after_ok = folder_name[after_idx..]
            .chars()
            .next()
            .is_none_or(|c| !c.is_ascii_alphanumeric());
        if before_ok && after_ok {
            return true;
        }
    }
    false
}

fn folder_name_matches_post(folder_name: &str, day: &str, title: &str) -> bool {
    if folder_name.is_empty() || day.is_empty() {
        return false;
    }
    let Some(after_day) = strip_day_prefix(folder_name, day) else {
        return false;
    };
    if !after_day.is_empty() {
        let first = after_day.chars().next().unwrap_or(' ');
        if first.is_alphanumeric() {
            return false;
        }
    }
    let want = normalize_folder_title(title);
    let found = normalize_folder_title(after_day);
    if want.is_empty() {
        // A post with no usable title is settled by its day alone; the day
        // prefix already carried the whole comparison.
        return true;
    }
    found == want
}

/// The title compared between a post and a directory name.
///
/// Both sides are reduced to a comparable title with the naming artifacts of
/// every rule this install has used removed:
/// - the leading `HH-MM` the historical `{date} {HH-MM}{title}` rule inserted
///   after the day, which the current `{date} {title}` rule never writes;
/// - the date prefix the source title likes to carry (`2026／7／21 木偶2`), which
///   the folder name may or may not have kept, in whatever width and separator
///   the rule that wrote it used (`2026-7-21`, `2026/07/21`, `20260914`);
/// - full-width `／` and half-width `-`, which the two rules disagree on.
///
/// `pawchive_pairing::normalize_title_for_pairing` already folds case, collapses
/// whitespace and strips a leading date, so this only adds the `HH-MM` segment
/// and the separator fold, then lets that shared normaliser do the rest. The
/// shared function is deliberately reused rather than reimplemented: the pairing
/// rules and the folder rules must agree on what "the same title" means.
fn normalize_folder_title(raw: &str) -> String {
    let trimmed = raw.trim();
    let without_time = strip_leading_clock_segment(trimmed);
    // Full-width separators are folded before anything looks for a date: the
    // source titles write `2026／7／21`, and `sanitize_path_component` turns that
    // into `2026-7-21` in one rule and leaves it alone in another.
    let folded: String = without_time
        .chars()
        .map(|ch| match ch {
            '／' | '⁄' | '∕' => '/',
            _ => ch,
        })
        .collect();
    // The leading date is a naming artifact on both sides, and it is not always
    // there on both; `normalize_title_for_pairing` removes whichever one remains.
    crate::pawchive_pairing::normalize_title_for_pairing(&folded)
}

/// Drop a leading `HH-MM` / `HH:MM` clock segment.
///
/// Only the shape the historical naming rule produced is removed. The rule wrote
/// `{date} {HH-MM}{title}` with no separator between the clock and the title, so
/// `17-30` followed by `木偶2` or by the title's own `2026／7／21` are both the
/// clock. A title that genuinely begins with a number keeps it only when it is
/// not shaped like a clock: `4K 修正版` has no `HH-MM` boundary, so nothing is
/// cut. No separator is required after the clock, because the historical rule
/// never wrote one.
fn strip_leading_clock_segment(value: &str) -> &str {
    let bytes = value.as_bytes();
    if bytes.len() < 5 {
        return value;
    }
    let digits = |range: std::ops::Range<usize>| {
        range.end <= bytes.len() && bytes[range].iter().all(|byte| byte.is_ascii_digit())
    };
    if !(digits(0..2) && (bytes[2] == b'-' || bytes[2] == b':') && digits(3..5)) {
        return value;
    }
    let rest = &value[5..];
    // `17-302026／…` is the clock then the title's own date; `17-30 木偶2` is the
    // clock then a separated title. Both drop the clock. `17302` has no
    // separator at index 2 and was already refused above.
    rest.trim_start_matches([' ', '-', '_', '.'])
}

/// Whether `child` is this post's folder, read from the artist directory.
///
/// Shared by the observation read and the candidate lookup so both agree on
/// which directory is the post's own; a disagreement is what made a folder
/// that exists read as missing.
fn folder_entry_matches_post(
    folder_name: &str,
    day: &str,
    title: &str,
    post_id: Option<&str>,
    derived_names: &[String],
) -> bool {
    // A name that is exactly one of this post's generated text files is not a
    // folder at all.
    if derived_names.iter().any(|derived| derived == folder_name) {
        return false;
    }
    if let Some(pid) = post_id {
        if folder_name_matches_post_id(folder_name, pid) {
            return true;
        }
    }
    folder_name_matches_post(folder_name, day, title)
}

/// What a candidate path is compared against: the post's rendered folder, its
/// day and its title.
struct PostFolderScope {
    /// The exact prefix the current template renders. A path under it is the
    /// post's own folder beyond doubt.
    rendered_prefix: String,
    /// The subscription's own directory, which is where a post's folder lives —
    /// possibly one `{user}` level deeper, because the saved template starts
    /// with `{user}/` and `target_dir` already ends with the artist name.
    artist_dir: String,
    day: String,
    title: String,
    post_id: Option<String>,
    derived: Vec<String>,
}

impl PostFolderScope {
    /// How this item relates to the post.
    ///
    /// `folder` is the post's own directory — the rendered prefix, or a
    /// directory under the artist that is the same work under an older naming
    /// rule. `same_day` is a candidate only: it may be this work's file, and the
    /// evaluator must not read it as a receipt.
    fn classify(&self, file_path: &str) -> &'static str {
        let normalized = file_path.replace('\\', "/");
        if !self.rendered_prefix.is_empty()
            && normalized.starts_with(&format!("{}/", self.rendered_prefix.trim_end_matches('/')))
        {
            return "folder";
        }
        // A folder name is what an older naming rule is recognised by, so every
        // directory between the artist and the file is offered for comparison.
        // `target_dir` already ends with the artist name while the saved
        // template starts with `{user}/`, so how many levels sit between the two
        // depends on whether this install's library has that extra layer — the
        // measured one has it for the fanbox artist and not for the patreon
        // artist, and both are legitimate.
        for name in self.folder_names_of(&normalized) {
            if folder_entry_matches_post(
                name,
                &self.day,
                &self.title,
                self.post_id.as_deref(),
                &self.derived,
            ) {
                return "folder";
            }
        }
        "same_day"
    }

    /// Every directory name between the artist directory and this path, nearest
    /// the file last.
    fn folder_names_of<'a>(&'a self, normalized_path: &'a str) -> Vec<&'a str> {
        if self.artist_dir.is_empty() {
            return Vec::new();
        }
        let Some(rest) = normalized_path.strip_prefix(&self.artist_dir) else {
            return Vec::new();
        };
        let rest = rest.trim_start_matches('/');
        // The file itself is the last segment and is not a folder name.
        let mut parts: Vec<&str> = rest.split('/').filter(|part| !part.is_empty()).collect();
        parts.pop();
        parts
    }
}

/// The folder this post's files live in, as the saved template renders it,
/// together with what a candidate is classified against.
fn post_folder_prefix(conn: &Connection, post_db_id: i64) -> Result<PostFolderScope> {
    let template = get_pawchive_settings(conn)
        .map(|stored| stored.folder_template)
        .unwrap_or_default();
    let (post_id, service, user_id, artist_name, title, published_at, target_dir): (
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        String,
    ) = conn.query_row(
        "SELECT p.post_id, s.service, s.user_id, a.name, p.title, p.published_at, s.target_dir
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         LEFT JOIN artists a ON a.id = s.artist_id
         WHERE p.id = ?1",
        params![post_db_id],
        |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
                row.get(5)?,
                row.get(6)?,
            ))
        },
    )?;
    let user = artist_name
        .as_deref()
        .filter(|name| !name.trim().is_empty())
        .unwrap_or(&user_id);
    let naming = PostNamingContext::new(
        user,
        published_at.as_deref(),
        title.as_deref(),
        &post_id,
        Some(&service),
    )
    .with_identity(&user_id, &service);
    let folder = naming.format_folder_path_with_template(&template);
    let derived = derived_text_names(&naming);
    Ok(PostFolderScope {
        rendered_prefix: PathBuf::from(&target_dir)
            .join(folder)
            .to_string_lossy()
            .replace('\\', "/"),
        artist_dir: target_dir.trim_end_matches('/').to_string(),
        day: naming.date.clone(),
        title: naming.title.to_string(),
        post_id: Some(post_id),
        derived,
    })
}

/// Verify a delivered file against the hash the source published.
///
/// Only a comparable hash proves anything: hashing a file this side computed no
/// expectation for would record what is on disk, not what it should be. A match
/// turns evidence that was merely "found in place" into proof of that resource;
/// a mismatch keeps the resource unproven and reports it.
/// Compare one delivered resource with the digest recorded for it.
///
/// The verdict is written down, not just returned: a file whose content is no
/// longer the resource's must stop counting as proof, and the evaluator reads
/// that from the row. The delivery itself is not erased — the record that this
/// install fetched the work stays — so a mismatch reports a local change
/// instead of queueing the file again.
pub fn verify_file_hash(conn: &Connection, file_id: i64) -> Result<Option<bool>> {
    let row: Option<(String, Option<String>)> = conn
        .query_row(
            "SELECT target_path, expected_blake3 FROM kemono_files WHERE id = ?1",
            params![file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((target_path, expected)) = row else {
        return Ok(None);
    };
    if target_path.trim().is_empty() {
        return Ok(None);
    }
    let Some(expected) = expected.filter(|value| !value.trim().is_empty()) else {
        // Nothing to compare against: the resource is left unjudged rather
        // than reported as verified.
        return Ok(None);
    };
    let now = Utc::now().to_rfc3339();
    // A file that has since gone, become a directory, turned into a symlink or
    // become unreadable is not the resource either. It used to make the whole
    // pass fail and leave the old proof standing.
    let local = match read_file_digest(Path::new(&target_path)) {
        Ok(digest) => digest,
        Err(error) => {
            let verdict = if Path::new(&target_path).symlink_metadata().is_err() {
                "missing"
            } else {
                "unreadable"
            };
            conn.execute(
                "UPDATE kemono_files SET integrity = ?1, verified_at = ?2, updated_at = ?2
                 WHERE id = ?3",
                params![verdict, now, file_id],
            )?;
            let _ = error;
            return Ok(Some(false));
        }
    };
    let matches = local.eq_ignore_ascii_case(expected.trim());
    conn.execute(
        "UPDATE kemono_files
         SET local_hash = ?1, integrity = ?2, verified_at = ?3, updated_at = ?3
         WHERE id = ?4",
        params![
            local,
            if matches { "verified" } else { "mismatched" },
            now,
            file_id
        ],
    )?;
    Ok(Some(matches))
}

/// The BLAKE3 digest of a regular file, rejecting anything that is not one.
fn read_file_digest(path: &Path) -> Result<String> {
    let metadata = fs::symlink_metadata(path)
        .with_context(|| format!("cannot read delivered file metadata: {}", path.display()))?;
    if metadata.file_type().is_symlink() {
        return Err(anyhow!("delivered file is a symlink: {}", path.display()));
    }
    if !metadata.is_file() {
        return Err(anyhow!("delivered path is not a file: {}", path.display()));
    }
    let mut file = File::open(path)
        .with_context(|| format!("cannot open delivered file: {}", path.display()))?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hasher.finalize().to_hex().to_string())
}

/// What a verification pass over one post found.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct VerifyReport {
    /// Resources that could be compared with the source's own hash.
    pub checked: usize,
    /// Of those, how many matched.
    pub matched: usize,
    /// Resources with no source hash or no target path: nothing to compare.
    pub skipped: usize,
    /// Resources whose recorded content is not the source's.
    pub mismatched: usize,
}

/// Hash every resource of a post that can be compared with its source hash.
///
/// Records the result on each resource, so a match upgrades evidence that was
/// only "found in place" to proof. A resource that cannot be compared is counted
/// as skipped, never as verified.
pub fn verify_post_files(conn: &Connection, post_db_id: i64) -> Result<VerifyReport> {
    let file_ids: Vec<i64> = {
        let mut stmt = conn.prepare(
            "SELECT id FROM kemono_files WHERE post_id = ?1 AND target_path != '' ORDER BY id ASC",
        )?;
        let collected: Vec<i64> = stmt
            .query_map(params![post_db_id], |row| row.get::<_, i64>(0))?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        collected
    };
    let mut report = VerifyReport::default();
    for file_id in file_ids {
        match verify_file_hash(conn, file_id)? {
            Some(true) => {
                report.checked += 1;
                report.matched += 1;
            }
            Some(false) => {
                report.checked += 1;
                report.mismatched += 1;
            }
            None => report.skipped += 1,
        }
    }
    let settings = get_pawchive_settings(conn)?;
    reassess_post(conn, post_db_id, &settings)?;
    Ok(report)
}

/// What an external downloader reports about one delivered bundle.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExternalReceipt {
    pub post_id: i64,
    /// The manifest the delivery was made for.
    pub manifest_version: i64,
    /// The downloader's own task id. Required: a receipt with no task cannot be
    /// matched to anything or de-duplicated.
    pub task_id: String,
    /// `completed` or `failed`.
    pub result: String,
    #[serde(default)]
    pub output_paths: Vec<String>,
    #[serde(default)]
    pub note: String,
    /// The bridge that produced this report. Empty means the caller is not a
    /// paired bridge, and the receipt then settles nothing.
    #[serde(default)]
    pub bridge_id: String,
}

/// A paired downloader bridge.
///
/// Settlement requires one. While no bridge is paired the receipt route records
/// history and settles nothing: a caller that merely names a file inside the
/// media roots must not be able to declare a work delivered.
#[derive(Debug, Clone)]
pub struct ExternalBridge {
    pub bridge_id: String,
}

/// Result of recording an external receipt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReceiptOutcome {
    /// The delivery was recorded for the first time.
    Recorded,
    /// The same task reported the same version again; nothing changed.
    Duplicate,
    /// The receipt describes a manifest the post has moved past.
    Stale,
    /// Kept as a fact, but it settled nothing. The reason is carried so the
    /// caller can explain the refusal instead of reporting a silent success.
    Rejected(String),
    NotFound,
}

/// Why a `completed` receipt may not settle its post.
///
/// `None` means every condition holds. The list is ordered from the cheapest
/// check to the filesystem ones.
fn completed_receipt_rejection(
    receipt: &ExternalReceipt,
    bridge: Option<&ExternalBridge>,
) -> Option<String> {
    let Some(bridge) = bridge else {
        return Some("下载器桥尚未配对，回执只作记录".to_string());
    };
    if receipt.bridge_id.trim().is_empty() || receipt.bridge_id.trim() != bridge.bridge_id.trim() {
        return Some("回执来源与已配对的下载器桥不一致".to_string());
    }
    if receipt.output_paths.is_empty() {
        return Some("回执未列出任何输出文件".to_string());
    }
    for path in &receipt.output_paths {
        let candidate = PathBuf::from(path);
        match fs::symlink_metadata(&candidate) {
            Err(error) => return Some(format!("回执列出的输出无法读取：{path}（{error}）")),
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Some(format!("回执列出的输出是符号链接：{path}"))
            }
            Ok(metadata) if !metadata.is_file() => {
                return Some(format!("回执列出的输出不是普通文件：{path}"))
            }
            Ok(metadata) if metadata.len() == 0 => {
                return Some(format!("回执列出的输出是空文件：{path}"))
            }
            Ok(_) => {}
        }
    }
    None
}

/// The digest of what one report said.
///
/// Deliberately order-preserving and not de-duplicating: a receipt that lists
/// the same path twice said something different from one that lists it once.
fn receipt_fingerprint(receipt: &ExternalReceipt) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(receipt.result.as_bytes());
    hasher.update(b"\n");
    hasher.update(receipt.bridge_id.as_bytes());
    hasher.update(b"\n");
    hasher.update(receipt.note.as_bytes());
    hasher.update(b"\n");
    for path in &receipt.output_paths {
        hasher.update(path.as_bytes());
        hasher.update(b"\n");
    }
    hasher.finalize().to_hex().to_string()
}

/// Update the working row that carries a settled delivery.
///
/// Kept separate from the ledger half because a receipt whose work has no
/// long-term identity — a post with no subscription row to take one from —
/// still has a working row to mark.
fn mark_external_post_settled(
    conn: &Connection,
    receipt: &ExternalReceipt,
    now: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE kemono_posts
         SET external_task_id = ?1, external_delivered_at = ?2, external_manifest_version = ?3
         WHERE id = ?4",
        params![
            receipt.task_id,
            now,
            receipt.manifest_version,
            receipt.post_id
        ],
    )?;
    Ok(())
}

/// Write the settlement this post's manifest is covered by.
///
/// The working-row update and the acquisition events commit together: a crash
/// between them would either lose the proven delivery or leave the ledger
/// claiming one the working row never recorded.
fn settle_external_post(
    conn: &Connection,
    work_id: &str,
    receipt: &ExternalReceipt,
    covered: &[(String, String)],
    now: &str,
) -> Result<()> {
    mark_external_post_settled(conn, receipt, now)?;
    record_external_receipt_acquisition(conn, work_id, receipt, covered, true, now)?;
    Ok(())
}

/// The verdict for a stored receipt row.
///
/// A `failed` report is a fact, never a refusal: the distinction matters because
/// a refusal has to be shown to the caller as "kept, settled nothing".
fn receipt_outcome(
    stale: bool,
    settles: bool,
    repeated: bool,
    is_failed: bool,
    reason: &str,
) -> ReceiptOutcome {
    if stale {
        ReceiptOutcome::Stale
    } else if settles || is_failed {
        if repeated {
            ReceiptOutcome::Duplicate
        } else {
            ReceiptOutcome::Recorded
        }
    } else {
        ReceiptOutcome::Rejected(reason.to_string())
    }
}

/// Record what an external downloader reported, once per task and version.
///
/// The row is keyed by the manifest the *report* was made for, never by the
/// post's current version: an older receipt used to take the current version's
/// key, so the valid receipt that arrived afterwards was de-duplicated away.
/// Recording and settling share one transaction, so a crash cannot leave a
/// receipt that claims to have settled without having done it.
///
/// Only a `completed` result reported by a paired bridge, listing files that
/// exist as non-empty regular files inside the media roots, settles anything.
/// A `failed` report is a fact worth keeping and never a settlement, and a
/// later `completed` report for the same task and version replaces it.
pub fn record_external_receipt(
    conn: &Connection,
    receipt: &ExternalReceipt,
    roots: &MediaRoots,
    bridge: Option<&ExternalBridge>,
) -> Result<ReceiptOutcome> {
    let current: Option<i64> = conn
        .query_row(
            "SELECT manifest_version FROM kemono_posts WHERE id = ?1",
            params![receipt.post_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(current_version) = current else {
        return Ok(ReceiptOutcome::NotFound);
    };
    if receipt.task_id.trim().is_empty() {
        return Err(anyhow!("an external receipt must carry a task id"));
    }
    if receipt.result != "completed" && receipt.result != "failed" {
        return Err(anyhow!("unsupported receipt result: {}", receipt.result));
    }
    // Paths are still checked even though the caller is internal: this is the
    // boundary where a downloader could otherwise name any file on the host.
    for path in &receipt.output_paths {
        let candidate = PathBuf::from(path);
        if !path_under_authorized_roots(&candidate, roots) {
            return Err(anyhow!(
                "output path is outside authorized media roots: {path}"
            ));
        }
    }

    // A receipt for a manifest the post has moved past never settles, whatever
    // it says. It is still stored under its own version as history.
    let stale = receipt.manifest_version < current_version;
    let rejection = if receipt.result != "completed" || stale {
        None
    } else {
        completed_receipt_rejection(receipt, bridge)
    };
    let settles = receipt.result == "completed" && !stale && rejection.is_none();
    let reason = if stale {
        "回执针对的清单版本已过期".to_string()
    } else {
        rejection.unwrap_or_default()
    };

    let fingerprint = receipt_fingerprint(receipt);
    let now = Utc::now().to_rfc3339();
    let output_paths = receipt.output_paths.join("\n");
    // The long-term identity of the work this receipt is about. It is resolved
    // before the transaction, and an unresolvable one leaves the working row
    // update as the whole of the settlement: a receipt whose work has no
    // subscription identity has no ledger row to attach a fact to.
    let work_id = work_id_for_post(conn, receipt.post_id)?;
    let covered = match work_id.as_deref() {
        Some(work_id) => {
            receipt_covered_resources(conn, work_id, receipt.post_id, receipt, settles)?
        }
        None => Vec::new(),
    };
    let work_id_str = work_id.as_deref().unwrap_or("");
    let tx = conn.unchecked_transaction()?;
    let existing: Option<(String, i64, String)> = tx
        .query_row(
            "SELECT result, settled, content_fingerprint FROM pawchive_external_receipts
             WHERE ((work_id != '' AND work_id = ?1) OR post_id = ?2)
               AND manifest_version = ?3 AND task_id = ?4",
            params![
                work_id_str,
                receipt.post_id,
                receipt.manifest_version,
                receipt.task_id
            ],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;

    let is_failed = receipt.result != "completed";
    let outcome = match existing {
        Some((stored_result, stored_settled, stored_fingerprint)) => {
            if stored_result == receipt.result && stored_fingerprint == fingerprint {
                if settles && stored_settled == 0 {
                    // The same report, re-sent after whatever blocked it has
                    // been resolved: settle it now instead of leaving the work
                    // permanently unclaimed.
                    if let Some(work_id) = work_id.as_deref() {
                        settle_external_post(&tx, work_id, receipt, &covered, &now)?;
                    } else {
                        mark_external_post_settled(&tx, receipt, &now)?;
                    }
                    tx.execute(
                        "UPDATE pawchive_external_receipts
                         SET settled = 1, rejected_reason = '', recorded_at = ?1,
                             work_id = CASE WHEN work_id = '' THEN ?2 ELSE work_id END,
                             post_id = ?3
                         WHERE ((work_id != '' AND work_id = ?2) OR post_id = ?3)
                           AND manifest_version = ?4 AND task_id = ?5",
                        params![
                            now,
                            work_id_str,
                            receipt.post_id,
                            receipt.manifest_version,
                            receipt.task_id
                        ],
                    )?;
                    ReceiptOutcome::Recorded
                } else {
                    receipt_outcome(stale, settles, true, is_failed, &reason)
                }
            } else if stored_result == "completed" && is_failed {
                // A completed delivery is never downgraded by a later failure
                // report about the same attempt.
                ReceiptOutcome::Duplicate
            } else {
                // A corrected report, or a failure that has since completed:
                // the newer fact replaces the older one for this attempt.
                tx.execute(
                    "UPDATE pawchive_external_receipts
                     SET result = ?1, output_paths = ?2, note = ?3, bridge_id = ?4,
                         content_fingerprint = ?5, settled = ?6, rejected_reason = ?7,
                         recorded_at = ?8,
                         work_id = CASE WHEN work_id = '' THEN ?9 ELSE work_id END,
                         post_id = ?10
                     WHERE ((work_id != '' AND work_id = ?9) OR post_id = ?10)
                       AND manifest_version = ?11 AND task_id = ?12",
                    params![
                        receipt.result,
                        output_paths,
                        receipt.note,
                        receipt.bridge_id,
                        fingerprint,
                        settles as i64,
                        reason,
                        now,
                        work_id_str,
                        receipt.post_id,
                        receipt.manifest_version,
                        receipt.task_id,
                    ],
                )?;
                if settles {
                    match work_id.as_deref() {
                        Some(work_id) => {
                            settle_external_post(&tx, work_id, receipt, &covered, &now)?
                        }
                        None => mark_external_post_settled(&tx, receipt, &now)?,
                    }
                }
                receipt_outcome(stale, settles, false, is_failed, &reason)
            }
        }
        None => {
            tx.execute(
                "INSERT INTO pawchive_external_receipts
                     (post_id, manifest_version, task_id, result, output_paths, note, bridge_id,
                      content_fingerprint, settled, rejected_reason, recorded_at, work_id)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12)",
                params![
                    receipt.post_id,
                    receipt.manifest_version,
                    receipt.task_id,
                    receipt.result,
                    output_paths,
                    receipt.note,
                    receipt.bridge_id,
                    fingerprint,
                    settles as i64,
                    reason,
                    now,
                    work_id_str,
                ],
            )?;
            if settles {
                match work_id.as_deref() {
                    Some(work_id) => settle_external_post(&tx, work_id, receipt, &covered, &now)?,
                    None => mark_external_post_settled(&tx, receipt, &now)?,
                }
            }
            receipt_outcome(stale, settles, false, is_failed, &reason)
        }
    };
    tx.commit()?;

    let settings = get_pawchive_settings(conn)?;
    reassess_post(conn, receipt.post_id, &settings)?;
    Ok(outcome)
}

/// The private part file a resource is received into.
/// Deterministic per target so an interrupted receive resumes into the same
/// file, and dot-prefixed so the scanner and the media index ignore it. The
/// final name is only ever created by the publish step.
fn staging_path_for(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let prefix = ".gallery_pawchive_";
    let suffix = ".part";
    let max_stem_bytes = MAX_RENDERED_NAME_BYTES.saturating_sub(prefix.len() + suffix.len());
    // Truncating alone makes different long targets share a resumable part.
    // Keep short names compatible; bind a shortened name to its full target.
    let clamped = if name.len() > max_stem_bytes {
        let hash = blake3::hash(name.as_bytes()).to_hex();
        format!(
            "{}_{}",
            clamp_name_bytes(name, max_stem_bytes - 17),
            &hash[..16]
        )
    } else {
        name.to_string()
    };
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    parent.join(format!("{prefix}{clamped}{suffix}"))
}

/// The validator recorded for a resource's partial receive, if any.
fn load_resume_validator(conn: &Connection, file_id: i64) -> Result<Option<ResumeValidator>> {
    let raw: Option<String> = conn
        .query_row(
            "SELECT resume_validator FROM kemono_files WHERE id = ?1",
            params![file_id],
            |row| row.get(0),
        )
        .optional()?;
    Ok(raw.as_deref().and_then(ResumeValidator::decode))
}

/// How many times this resource has been attempted, for the backoff curve.
fn current_attempt(conn: &Connection, file_id: i64) -> i64 {
    conn.query_row(
        "SELECT attempt_count FROM kemono_files WHERE id = ?1",
        params![file_id],
        |row| row.get(0),
    )
    .unwrap_or(1)
}

/// Which version of a remote artifact a partial file belongs to.
///
/// A `206` can only be appended to a partial file when the server proves it is
/// still serving the same version. Without that proof two different versions get
/// concatenated and the result is a file that never existed upstream, so a
/// resume without one of these is refused and the receive starts over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResumeValidator {
    /// `ETag` (preferred) or `Last-Modified`, exactly as the server sent it.
    pub value: String,
    /// Full artifact length the server declared.
    pub total_length: u64,
}

impl ResumeValidator {
    fn from_headers(headers: &reqwest::header::HeaderMap, total_length: u64) -> Option<Self> {
        let pick = |name: reqwest::header::HeaderName| {
            headers
                .get(name)
                .and_then(|value| value.to_str().ok())
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string)
        };
        let value = pick(reqwest::header::ETAG).or_else(|| pick(reqwest::header::LAST_MODIFIED))?;
        Some(Self {
            value,
            total_length,
        })
    }

    /// Ledger form: the declared length and the validator on two lines, so the
    /// stored value can be compared without a second column.
    fn encode(&self) -> String {
        format!("{}\n{}", self.total_length, self.value)
    }

    fn decode(raw: &str) -> Option<Self> {
        let (length, value) = raw.split_once('\n')?;
        Some(Self {
            value: value.to_string(),
            total_length: length.trim().parse().ok()?,
        })
    }
}

/// Outcome of a resumable receive. The bytes are staged, not published: the
/// share is only touched by the shared publish protocol.
#[derive(Debug, Clone)]
pub struct DownloadResult {
    pub staged_path: PathBuf,
    pub bytes_written: u64,
    pub blake3_hash: String,
    pub resumed: bool,
    /// What to resume with next time, when the server offers a validator.
    pub validator: Option<ResumeValidator>,
}

/// `Content-Range: bytes <start>-<end>/<total>`.
fn parse_content_range(raw: &str) -> Option<(u64, u64, Option<u64>)> {
    let spec = raw.trim().strip_prefix("bytes ")?;
    let (range_part, total_part) = spec.split_once('/')?;
    let (start, end) = range_part.split_once('-')?;
    let start: u64 = start.trim().parse().ok()?;
    let end: u64 = end.trim().parse().ok()?;
    let total = match total_part.trim() {
        "*" => None,
        value => Some(value.parse().ok()?),
    };
    Some((start, end, total))
}

fn declared_length(headers: &reqwest::header::HeaderMap) -> Option<u64> {
    headers
        .get(reqwest::header::CONTENT_LENGTH)
        .and_then(|value| value.to_str().ok())
        .and_then(|value| value.trim().parse().ok())
}

/// Download a remote file into a private part file, verified.
///
/// One attempt at resuming is made, and only with a validator from the earlier
/// response. A `206` whose declared start, length or total does not line up with
/// what is already on disk is not appended to: the partial file is discarded and
/// the artifact is received again from zero. The result is staged; publishing it
/// is the caller's job.
///
/// `on_validator` is called with the version tags and declared length as soon as
/// they are known and *before* the body is read. That is the only moment at
/// which they can still be recorded: a first response that disconnects halfway
/// through used to leave a part file with no validator, so the next round could
/// not resume and fetched the whole artifact again.
pub async fn download_resumable_stream(
    client: &reqwest::Client,
    file_url: &str,
    part_path: &Path,
    roots: &MediaRoots,
    expected_blake3: Option<&str>,
    resume: Option<&ResumeValidator>,
    on_validator: &mut (dyn FnMut(&ResumeValidator) + Send),
) -> Result<DownloadResult> {
    if !path_under_authorized_roots(part_path, roots) {
        return Err(anyhow!(
            "staging path is outside authorized media roots: {:?}",
            part_path
        ));
    }
    let parent = part_path
        .parent()
        .ok_or_else(|| anyhow!("staging path has no parent: {:?}", part_path))?;
    fs::create_dir_all(parent)
        .with_context(|| format!("cannot create parent directory {:?}", parent))?;

    let mut attempt = 0;
    loop {
        attempt += 1;
        let existing_len = match fs::metadata(part_path) {
            Ok(metadata) if metadata.is_file() => metadata.len(),
            _ => 0,
        };
        // Resume only when a validator pins the version already on disk.
        let resuming = existing_len > 0 && resume.is_some();
        let mut request = client.get(file_url);
        if resuming {
            request = request.header(reqwest::header::RANGE, format!("bytes={existing_len}-"));
            request = request.header(
                reqwest::header::IF_RANGE,
                resume.map(|known| known.value.clone()).unwrap_or_default(),
            );
        }
        let restart_from_zero = |part_path: &Path| {
            let _ = fs::remove_file(part_path);
        };

        let response = request
            .send()
            .await
            .context("send download request failed")?;
        let status = response.status();

        if status == reqwest::StatusCode::RANGE_NOT_SATISFIABLE {
            // The part is at or past the artifact's length: whatever it is, it
            // cannot be appended to.
            restart_from_zero(part_path);
            if attempt > 1 {
                return Err(anyhow!(
                    "download failed with HTTP status: {}",
                    reqwest::StatusCode::RANGE_NOT_SATISFIABLE
                ));
            }
            continue;
        }
        if status != reqwest::StatusCode::PARTIAL_CONTENT && status != reqwest::StatusCode::OK {
            // A source that states how long to wait is obeyed. Dropping the
            // header made the caller fall back to a fixed delay, which is
            // exactly the hammering a rate limiter is asking to stop.
            let suffix = match retry_after_seconds(response.headers()) {
                Some(seconds) => format!(" (retry-after: {seconds})"),
                None => String::new(),
            };
            return Err(anyhow!(
                "download failed with HTTP status: {status}{suffix}"
            ));
        }

        let resuming_response = status == reqwest::StatusCode::PARTIAL_CONTENT;

        // Work out the artifact's true length before writing anything.
        let (total_length, open_offset) = if resuming_response {
            let header = response
                .headers()
                .get(reqwest::header::CONTENT_RANGE)
                .and_then(|value| value.to_str().ok())
                .ok_or_else(|| anyhow!("206 response carried no Content-Range header"))?;
            let (start, end, total) = parse_content_range(header)
                .ok_or_else(|| anyhow!("cannot parse Content-Range header: {header}"))?;
            if start != existing_len {
                restart_from_zero(part_path);
                if attempt > 1 {
                    return Err(anyhow!(
                        "server range start {start} does not match staged length {existing_len}"
                    ));
                }
                continue;
            }
            if end < start {
                return Err(anyhow!("Content-Range end {end} precedes start {start}"));
            }
            let total = total.ok_or_else(|| {
                anyhow!("server did not declare the artifact length in Content-Range")
            })?;
            // The resource changed under us: the staged bytes belong to another
            // version and must never be joined to this one.
            if resume.is_some_and(|known| known.total_length != total) {
                restart_from_zero(part_path);
                if attempt > 1 {
                    return Err(anyhow!(
                        "artifact length changed from {} to {total} while resuming",
                        resume.map(|known| known.total_length).unwrap_or(0)
                    ));
                }
                continue;
            }
            if let Some(declared) = declared_length(response.headers()) {
                if declared != end - start + 1 {
                    return Err(anyhow!(
                        "Content-Length {declared} does not match the range {start}-{end}"
                    ));
                }
            }
            (total, existing_len)
        } else {
            let declared = declared_length(response.headers()).ok_or_else(|| {
                anyhow!("server did not declare the artifact length (no Content-Length)")
            })?;
            (declared, 0)
        };

        if open_offset == 0 {
            restart_from_zero(part_path);
        }
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(part_path)
            .with_context(|| format!("cannot open staging file: {:?}", part_path))?;
        // Read the version tags before the body is consumed by the stream, and
        // record them there and then: everything below this line can fail with
        // the part file already on disk.
        let mut validator = ResumeValidator::from_headers(response.headers(), total_length);
        if let Some(known) = validator.as_ref() {
            on_validator(known);
        }
        let mut stream = response.bytes_stream();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("error reading download stream")?;
            file.write_all(&chunk)
                .context("error writing to staging file")?;
        }
        file.sync_all().context("error syncing staging file")?;
        drop(file);

        let received = fs::metadata(part_path)?.len();
        // The declared length is the only proof the artifact is whole; a
        // truncated transfer that stops early must not be published.
        if received != total_length {
            return Err(anyhow!(
                "received {received} bytes but the server declared {total_length}"
            ));
        }

        let mut staged = File::open(part_path)
            .with_context(|| format!("cannot open staging file for hashing: {:?}", part_path))?;
        let mut hasher = blake3::Hasher::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            let read = staged.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            hasher.update(&buffer[..read]);
        }
        let computed_blake3 = hasher.finalize().to_hex().to_string();
        if let Some(expected) = expected_blake3 {
            if !computed_blake3.eq_ignore_ascii_case(expected) {
                restart_from_zero(part_path);
                return Err(anyhow!(
                    "blake3 hash mismatch: computed {computed_blake3}, expected {expected}"
                ));
            }
        }

        // A resumed response can omit the validator; keep the one that proved
        // the version instead of losing it.
        if validator.is_none() && resuming_response {
            validator = resume.cloned();
        }
        return Ok(DownloadResult {
            staged_path: part_path.to_path_buf(),
            bytes_written: received,
            blake3_hash: computed_blake3,
            resumed: resuming_response,
            validator,
        });
    }
}

/// The directory a post's media already occupies, from its first reserved path.
///
/// A resource's target is reserved once and reused on every retry, so it also
/// records where the work was being published when the artist was moved or when
/// the folder template changed. The generated documents belong beside the media,
/// and writing them to the freshly rendered folder instead is what split one
/// work across two directories.
///
/// Only a reservation inside the artist's own tree is followed: a path outside
/// it is a stale reservation the artist move has not reached, and putting the
/// documents there would file them outside the artist.
fn reserved_media_dir(
    conn: &Connection,
    post_db_id: i64,
    artist_dir: &Path,
) -> Result<Option<PathBuf>> {
    let reserved: Option<String> = conn
        .query_row(
            "SELECT target_path FROM kemono_files
             WHERE post_id = ?1 AND TRIM(COALESCE(target_path, '')) <> ''
             ORDER BY id ASC
             LIMIT 1",
            params![post_db_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(reserved) = reserved else {
        return Ok(None);
    };
    let Some(parent) = Path::new(&reserved).parent() else {
        return Ok(None);
    };
    if parent != artist_dir && !parent.starts_with(artist_dir) {
        return Ok(None);
    }
    Ok(Some(parent.to_path_buf()))
}

/// Whether `path` already holds exactly `body`.
///
/// Used by the generated text files to decide whether there is anything to
/// write: a read error answers `false`, and the caller then rewrites the file,
/// which is the safe direction for a document derived from the post body.
fn generated_file_matches(path: &Path, body: &[u8]) -> bool {
    fs::read(path)
        .map(|existing| existing == body)
        .unwrap_or(false)
}

/// Write `content {id}.txt` and `links {title} {id}.txt` atomically into post target directory.
pub fn publish_post_text_files(
    target_dir: &Path,
    post: &PawchivePost,
    naming: &PostNamingContext,
    roots: &MediaRoots,
    download_content: bool,
    download_links: bool,
) -> Result<Vec<PathBuf>> {
    if !download_content && !download_links {
        return Ok(Vec::new());
    }
    if !path_under_authorized_roots(target_dir, roots) {
        return Err(anyhow!(
            "target directory is outside authorized media roots: {:?}",
            target_dir
        ));
    }
    fs::create_dir_all(target_dir)
        .with_context(|| format!("cannot create target directory: {:?}", target_dir))?;

    let mut published = Vec::new();
    let (local_pub, orig_pub) =
        format_post_datetime_display(post.published.as_deref(), Some(&post.service));

    let append_metadata_header = |formatted: &mut String| {
        formatted.push_str(&format!(
            "Title: {}\n",
            post.title.as_deref().unwrap_or("Untitled")
        ));
        formatted.push_str(&format!("Post ID: {}\n", post.id));
        if let Some(local) = &local_pub {
            formatted.push_str(&format!("Published (Local): {}\n", local));
        }
        if let Some(orig) = &orig_pub {
            formatted.push_str(&format!("Published (Original): {}\n", orig));
        }
        formatted.push_str(&format!("User ID: {}\n", post.user));
        formatted.push_str(&format!("Service: {}\n", post.service));
    };

    // 1. Content file
    if download_content {
        if let Some(content) = post.content.as_deref() {
            let trimmed = content.trim();
            if !trimmed.is_empty() {
                let content_filename = naming.format_content_filename();
                let content_target = target_dir.join(&content_filename);
                // Rendered again rather than skipped when the file exists: this
                // file is derived from the body, so a corrected body or a fixed
                // renderer has to be able to supersede what is already there.
                // `generated_file_matches` keeps a matching file untouched, which
                // is what makes an unchanged re-run write nothing.
                let mut formatted = String::new();
                append_metadata_header(&mut formatted);
                formatted.push_str("\n=== Content ===\n");
                formatted.push_str(trimmed);
                formatted.push('\n');
                if !generated_file_matches(&content_target, formatted.as_bytes()) {
                    let temp_name =
                        format!(".gallery_publish_{}.part", uuid::Uuid::new_v4().simple());
                    let temp_path = target_dir.join(&temp_name);
                    fs::write(&temp_path, formatted.as_bytes()).with_context(|| {
                        format!("cannot write content part file: {:?}", temp_path)
                    })?;
                    crate::ingest_publish::publish_generated_file(&temp_path, &content_target)
                        .with_context(|| {
                            format!("cannot rename to content file: {:?}", content_target)
                        })?;
                    published.push(content_target);
                }
            }
        }
    }

    // 2. Links file
    if download_links {
        let ext_links = post
            .content
            .as_deref()
            .map(extract_external_links)
            .unwrap_or_default();
        if !ext_links.is_empty() {
            let links_filename = naming.format_links_filename();
            let links_target = target_dir.join(&links_filename);
            // Same as the content file: regenerated from the body, so a fixed
            // extractor reaches a work whose links file already exists — an
            // earlier build wrote the keyless form of a MEGA link, and a file
            // that is merely present would keep it forever.
            let mut formatted = String::new();
            append_metadata_header(&mut formatted);
            formatted.push_str(&format!("\n=== External Links ({}) ===\n", ext_links.len()));
            for (idx, link_url) in ext_links.iter().enumerate() {
                formatted.push_str(&format!("{}. {}\n", idx + 1, link_url));
            }
            if !generated_file_matches(&links_target, formatted.as_bytes()) {
                let temp_name = format!(".gallery_publish_{}.part", uuid::Uuid::new_v4().simple());
                let temp_path = target_dir.join(&temp_name);
                fs::write(&temp_path, formatted.as_bytes())
                    .with_context(|| format!("cannot write links part file: {:?}", temp_path))?;
                crate::ingest_publish::publish_generated_file(&temp_path, &links_target)
                    .with_context(|| format!("cannot rename to links file: {:?}", links_target))?;
                published.push(links_target);
            }
        }
    }

    Ok(published)
}

/// Result of reading a post's folder once.
///
/// An observation is a *candidate*, never a verdict: it can say "something is
/// here that might be this post", not "this post is downloaded". Every
/// conclusion drawn from it still has to pass the evaluator.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct FolderObservation {
    state: &'static str,
    files: usize,
}

/// The folder does not exist yet. Nothing to observe, and the post is free to
/// be claimed.
const OBSERVED_MISSING: &str = "missing";
/// The folder exists and holds no content that could be this post's media.
const OBSERVED_EMPTY: &str = "empty";
/// The folder holds files that could be this post's media.
const OBSERVED_CONTENT: &str = "content";
/// The folder could not be read (permissions, an unmounted share, ...). Not the
/// same as "the content is absent", so it must not read as a missing post.
const OBSERVED_UNREADABLE: &str = "unreadable";
const OBSERVED_AMBIGUOUS: &str = "ambiguous";
const FOLDER_SHARED_TEMPLATE: &str = "shared_template";

/// The folder that was read is the name the saved template renders.
const FOLDER_FROM_TEMPLATE: &str = "template_scoped";
/// The folder was found as a sibling directory matching the post by day and
/// title — a name the historical rule wrote.
const FOLDER_FROM_NAME: &str = "named";
/// The folder was found by the day-only fallback: the post is alone on its day
/// and the day holds exactly one directory with content.
const FOLDER_FROM_DAY: &str = "day";
/// The day-only fallback was refused because the day's only directory with
/// content is named after a different post of the same subscription, so the
/// rendered path was read instead. This post's own folder is not on disk.
///
/// Recorded separately from `day` because it is the opposite finding about the
/// same day, and the evaluator has to report it: a post whose day holds only a
/// neighbour's work is neither "had" nor "missing".
const FOLDER_FROM_SIBLING_DAY: &str = "sibling_day";

/// The reason the evaluator records when the day-folder rule settles a post.
///
/// Named rather than spelled out at each site because the observation sweep has
/// to recognise the rows this rule decided: a database written before
/// `observed_source` existed carries no other trace of how a folder was found.
/// Changing the wording therefore moves both the message and the reach of the
/// sweep, which is what keeps the two from drifting apart.
const DAY_FOLDER_REASON: &str = "日期目录存在，按已下载计入";

/// The reason recorded when the day's only folder belongs to another post.
///
/// The post is not "had" — the folder on its day is somebody else's — and it is
/// not "missing" either: the work is usually on disk, filed under a name this
/// side cannot predict, and fetching it again would leave a second copy. Only a
/// human can say which it is, so the day is handed over instead of answered.
const SIBLING_FOLDER_REASON: &str = "当天目录属于同订阅的另一篇作品，需人工核对";

/// Bounded number of posts whose folder one round will read.
///
/// A check must terminate, and an observation is a hint rather than a
/// correctness requirement: the download path re-observes the post it is about
/// to claim. A round that stops here says so instead of reporting the rest as
/// empty.
const PAWCHIVE_OBSERVE_MAX_POSTS: usize = 2000;

struct ContentScan {
    files: usize,
    unreadable: bool,
}

fn is_archive_file(path: &Path) -> bool {
    let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
        return false;
    };
    let lower = ext.to_ascii_lowercase();
    matches!(lower.as_str(), "zip" | "7z" | "rar" | "tar" | "gz")
}

fn observe_post_folder_with_mode(
    dir: &Path,
    derived_names: &[String],
    stop_on_first: bool,
) -> FolderObservation {
    match fs::metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => FolderObservation {
            state: OBSERVED_MISSING,
            files: 0,
        },
        // A share that is not mounted or permission denied:
        Err(_) => FolderObservation {
            state: OBSERVED_UNREADABLE,
            files: 0,
        },
        Ok(metadata) if metadata.is_file() => {
            if is_archive_file(dir) && metadata.len() > 0 {
                FolderObservation {
                    state: OBSERVED_CONTENT,
                    files: 1,
                }
            } else {
                FolderObservation {
                    state: OBSERVED_UNREADABLE,
                    files: 0,
                }
            }
        }
        Ok(metadata) if !metadata.is_dir() => FolderObservation {
            state: OBSERVED_UNREADABLE,
            files: 0,
        },
        Ok(_) => {
            let scan = if stop_on_first {
                has_content_file(dir, derived_names)
            } else {
                count_content_files(dir, derived_names, 0)
            };
            let state = if scan.files > 0 {
                OBSERVED_CONTENT
            } else if scan.unreadable {
                OBSERVED_UNREADABLE
            } else {
                OBSERVED_EMPTY
            };
            FolderObservation {
                state,
                files: scan.files,
            }
        }
    }
}

/// Existence-only folder probe (L5-A4): stops at the first qualifying non-helper
/// content file (`files = 1` when present, not an exact count).
fn observe_post_folder_existence(dir: &Path, derived_names: &[String]) -> FolderObservation {
    observe_post_folder_with_mode(dir, derived_names, true)
}

/// Read a post's folder with bounded exact content-file counting for display (`observed_files`).
fn observe_post_folder(dir: &Path, derived_names: &[String]) -> FolderObservation {
    observe_post_folder_with_mode(dir, derived_names, false)
}

/// A file name that is a download in progress, not a delivered file.
fn has_transient_suffix(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".part") || lower.ends_with(".crdownload") || lower.ends_with(".download")
}

/// Whether `name` is a helper or temporary file that cannot count as delivered media.
fn is_helper_file(path: &Path, name: &str, derived_names: &[String]) -> bool {
    let lower = name.to_ascii_lowercase();
    if name.starts_with('.') || has_transient_suffix(name) || lower.ends_with(".crawljob") {
        return true;
    }
    if !derived_names.iter().any(|derived| derived == name)
        && !(lower.ends_with(".txt")
            && (lower.starts_with("content ") || lower.starts_with("links ")))
    {
        return false;
    }
    // Names alone are not provenance: real attachments can be called
    // "content draft.txt". Recognize the legacy generated metadata header.
    // Bounded reads also keep unrelated large text attachments cheap.
    if !fs::symlink_metadata(path).is_ok_and(|m| m.is_file() && !m.file_type().is_symlink()) {
        return false;
    }
    let Ok(file) = fs::File::open(path) else {
        return false;
    };
    let mut bytes = Vec::new();
    if std::io::Read::read_to_end(&mut std::io::Read::take(file, 16384), &mut bytes).is_err() {
        return false;
    }
    let header = String::from_utf8_lossy(&bytes).replace("\r\n", "\n");
    let Some((metadata, body)) = header.split_once("\n\n") else {
        return false;
    };
    metadata.starts_with("Title: ")
        && ["Post ID: ", "User ID: ", "Service: "]
            .iter()
            .all(|prefix| {
                metadata.lines().any(|line| {
                    line.strip_prefix(prefix)
                        .is_some_and(|v| !v.trim().is_empty())
                })
            })
        && (body.starts_with("=== Content ===") || body.starts_with("=== External Links ("))
}

const MAX_FOLDER_DEPTH: usize = 6;
const MAX_FOLDER_ENTRIES: usize = 2000;

fn scan_content_files_bounded(
    dir: &Path,
    derived_names: &[String],
    depth: usize,
    budget: &mut usize,
    stop_on_first: bool,
) -> ContentScan {
    if depth > MAX_FOLDER_DEPTH || *budget == 0 {
        return ContentScan {
            files: 0,
            unreadable: true,
        };
    }
    let entries = match fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => {
            return ContentScan {
                files: 0,
                unreadable: true,
            };
        }
    };
    let mut scan = ContentScan {
        files: 0,
        unreadable: false,
    };
    for entry_res in entries {
        if *budget == 0 {
            scan.unreadable = true;
            break;
        }
        *budget = budget.saturating_sub(1);
        let entry = match entry_res {
            Ok(e) => e,
            Err(_) => {
                scan.unreadable = true;
                continue;
            }
        };
        let name = entry.file_name();
        let name_str = name.to_string_lossy();
        if is_helper_file(&entry.path(), &name_str, derived_names) {
            continue;
        }
        let Ok(kind) = entry.file_type() else {
            scan.unreadable = true;
            continue;
        };
        if kind.is_file() {
            match entry.metadata() {
                Ok(meta) => {
                    if meta.len() > 0 {
                        scan.files += 1;
                        if stop_on_first {
                            scan.unreadable = false;
                            break;
                        }
                    }
                }
                Err(_) => {
                    scan.unreadable = true;
                }
            }
        } else if kind.is_dir() {
            let child = scan_content_files_bounded(
                &entry.path(),
                derived_names,
                depth + 1,
                budget,
                stop_on_first,
            );
            scan.files += child.files;
            if stop_on_first && scan.files > 0 {
                scan.unreadable = false;
                break;
            }
            if child.unreadable {
                scan.unreadable = true;
            }
        }
    }
    scan
}

/// Short-circuit existence scan: stops as soon as the first qualifying content file is found.
fn has_content_file(dir: &Path, derived_names: &[String]) -> ContentScan {
    let mut budget = MAX_FOLDER_ENTRIES;
    scan_content_files_bounded(dir, derived_names, 0, &mut budget, true)
}

fn count_content_files_bounded(
    dir: &Path,
    derived_names: &[String],
    depth: usize,
    budget: &mut usize,
) -> ContentScan {
    scan_content_files_bounded(dir, derived_names, depth, budget, false)
}

/// Bounded exact count of qualifying files under `dir` for display statistics.
fn count_content_files(dir: &Path, derived_names: &[String], depth: usize) -> ContentScan {
    let mut budget = MAX_FOLDER_ENTRIES;
    count_content_files_bounded(dir, derived_names, depth, &mut budget)
}

/// The generated text files of one post, by name.
fn derived_text_names(naming: &PostNamingContext<'_>) -> Vec<String> {
    vec![
        naming.format_content_filename(),
        naming.format_links_filename(),
    ]
}

/// Persist what one read of a post's folder found.
fn record_folder_observation(
    conn: &Connection,
    post_db_id: i64,
    observation: FolderObservation,
    folder_source: &str,
) -> Result<()> {
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "UPDATE kemono_posts
         SET observed_state = ?1, observed_files = ?2, observed_at = ?3, observed_source = ?4
         WHERE id = ?5",
        params![
            observation.state,
            observation.files as i64,
            now,
            folder_source,
            post_db_id
        ],
    )?;
    Ok(())
}

/// One post row joined with the fields its folder path depends on.
struct PostObserveRow {
    post_db_id: i64,
    /// Which subscription the post belongs to: the rows a round touched are
    /// the subscriptions whose aggregates have to be recomputed afterwards.
    subscription_id: i64,
    post_id: String,
    service: String,
    user_id: String,
    artist_name: Option<String>,
    title: Option<String>,
    published_at: Option<String>,
    target_dir: String,
}

/// What one folder search concluded.
///
/// `incomplete` is the member that changes decisions. A search that could not
/// enumerate a directory it needed has not shown that the post's folder is
/// absent — it has shown nothing. Answering "missing" from an unfinished search
/// is what lets an automatic pass re-download a library that is mounted but
/// unreadable, so the hole has to travel with the path and the caller decides
/// what it means: `observation_after_search` turns a `missing` reading taken
/// that way into `unreadable`.
struct ResolvedFolder {
    /// Where the post's folder is. Production reads `observation` and `source`;
    /// the resolver regressions assert which candidate won, so the path stays
    /// available to tests without being read dead weight in the product binary.
    #[allow(dead_code)]
    path: PathBuf,
    source: &'static str,
    incomplete: bool,
    /// What the search already read at `path`.
    ///
    /// The search has to open each candidate to classify it, so the reading
    /// exists by the time a path is chosen. Returning it is the difference
    /// between one traversal and two: callers used to re-`observe_post_folder`
    /// the path they were just handed, which is the same directory read twice
    /// per post per round. The reading is the raw one — the caller still runs it
    /// through `observation_after_search`, because whether an unfinished search
    /// may report absence is a decision about the search, not about the folder.
    observation: FolderObservation,
}

impl ResolvedFolder {
    fn new(
        path: PathBuf,
        source: &'static str,
        incomplete: bool,
        observation: FolderObservation,
    ) -> Self {
        Self {
            path,
            source,
            incomplete,
            observation,
        }
    }
}

/// One directory a folder search is looking at.
struct CandidateDir {
    path: PathBuf,
    name: String,
}

/// The directories directly under `dir`.
///
/// Returns `(directories, complete)`. A directory that could not be enumerated,
/// or an entry that could not be inspected, leaves the listing *incomplete*
/// rather than shorter: an entry this side could not see is not an entry that
/// is not there, and collapsing the two is what made an unreadable share read
/// as a work that had never been delivered.
fn list_subdirectories(dir: &Path) -> (Vec<CandidateDir>, bool) {
    // Three different answers and only one of them is a hole:
    //
    //   - the path is absent, so there is genuinely nothing in it;
    //   - the path is not a directory, so it holds no subdirectories;
    //   - the path is a directory this process cannot read.
    //
    // Calling the first two holes would stop the first download of every artist
    // whose directory has not been created yet, which is the common case, not
    // the exceptional one.
    match fs::metadata(dir) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return (Vec::new(), true),
        Err(_) => return (Vec::new(), false),
        Ok(metadata) if !metadata.is_dir() => return (Vec::new(), true),
        Ok(_) => {}
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return (Vec::new(), false);
    };
    let mut found = Vec::new();
    let mut complete = true;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        let Ok(kind) = entry.file_type() else {
            complete = false;
            continue;
        };
        if kind.is_dir() {
            found.push(CandidateDir {
                path: entry.path(),
                name: entry.file_name().to_string_lossy().into_owned(),
            });
        } else if kind.is_file() {
            let path = entry.path();
            if is_archive_file(&path) {
                found.push(CandidateDir {
                    path,
                    name: entry.file_name().to_string_lossy().into_owned(),
                });
            }
        }
    }
    (found, complete)
}

/// A `missing` reading taken from a search that could not finish is not a
/// finding, it is an unknown.
///
/// Unknown has to stay distinct from absent all the way to the decision: an
/// unknown folder may hold the work, so an automatic pass must pause rather
/// than fetch, where an absent one is exactly what a pass is for.
/// `derive_assessment` already turns an unknown folder into `Unverified` instead
/// of a gap; this is what makes the reading that feeds it honest.
fn observation_after_search(
    observation: FolderObservation,
    resolved: &ResolvedFolder,
) -> FolderObservation {
    if resolved.incomplete && observation.state == OBSERVED_MISSING {
        return FolderObservation {
            state: OBSERVED_UNREADABLE,
            files: 0,
        };
    }
    observation
}

/// Whether `name` has the shape of a post directory (starts with full date YYYY-MM-DD/YYYY_MM_DD/etc.,
/// or compact 8-digit date, or post ID in brackets).
/// Used to avoid traversing into individual post folders when discovering container search bases.
fn looks_like_post_dir(name: &str) -> bool {
    let trimmed = name.trim();
    if trimmed.is_empty() || trimmed.starts_with('.') {
        return false;
    }
    let test_str = if (trimmed.starts_with('[') || trimmed.starts_with('(')) && trimmed.len() > 1 {
        &trimmed[1..]
    } else {
        trimmed
    };

    let bytes = test_str.as_bytes();
    if bytes.len() >= 8 {
        // YYYYMMDD (8 consecutive digits)
        if bytes[0..8].iter().all(|b| b.is_ascii_digit()) {
            return true;
        }
        // YYYY-MM-DD, YYYY_MM_DD, or YYYY.MM.DD
        if bytes[0..4].iter().all(|b| b.is_ascii_digit()) {
            let sep = bytes[4];
            if sep == b'-' || sep == b'_' || sep == b'.' {
                if let Some(second_sep_pos) = bytes[5..].iter().position(|&b| b == sep) {
                    let second_idx = 5 + second_sep_pos;
                    let month = &bytes[5..second_idx];
                    if !month.is_empty()
                        && (1..=2).contains(&month.len())
                        && month.iter().all(|b| b.is_ascii_digit())
                        && second_idx + 1 < bytes.len()
                        && bytes[second_idx + 1].is_ascii_digit()
                    {
                        return true;
                    }
                }
            }
        }
    }
    // Chinese dates: e.g. "2026年4月21日" or "2026年04月21日"
    if test_str.contains('年')
        && test_str.contains('月')
        && (test_str.contains('日') || test_str.contains('号'))
    {
        return true;
    }

    false
}

/// Traverses container directories under `target_dir` (e.g. year folders, categories, archives, artist subfolders)
/// up to depth 3, skipping post directories so search bases are container roots.
fn collect_search_bases(
    target_dir: &Path,
    rendered_first: Option<&str>,
    year_hint: Option<&str>,
) -> (Vec<PathBuf>, bool) {
    let mut bases = Vec::new();
    let mut complete = true;
    let mut visited = std::collections::HashSet::new();

    let root = target_dir.to_path_buf();
    match fs::metadata(&root) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return (vec![root], true);
        }
        Err(_) => return (vec![root], false),
        Ok(metadata) if !metadata.is_dir() => return (vec![root], true),
        Ok(_) => {}
    }

    let mut queue = std::collections::VecDeque::new();
    queue.push_back((root.clone(), 0usize));
    visited.insert(root.clone());
    bases.push(root.clone());

    if let Some(first) = rendered_first {
        if !first.trim().is_empty() {
            let nested = root.join(first.trim());
            if nested.is_dir() && visited.insert(nested.clone()) {
                bases.push(nested.clone());
                queue.push_back((nested, 1usize));
            }
        }
    }

    const MAX_CONTAINER_DEPTH: usize = 3;
    const MAX_CONTAINER_BASES: usize = 64;

    while let Some((dir, depth)) = queue.pop_front() {
        if bases.len() >= MAX_CONTAINER_BASES {
            complete = false;
            break;
        }
        if depth >= MAX_CONTAINER_DEPTH {
            match fs::read_dir(&dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(_) => {
                                complete = false;
                                continue;
                            }
                        };
                        let is_dir = match entry.file_type() {
                            Ok(kind) => kind.is_dir(),
                            Err(_) => {
                                complete = false;
                                continue;
                            }
                        };
                        if is_dir {
                            let name = entry.file_name().to_string_lossy().into_owned();
                            if !name.starts_with('.') && !looks_like_post_dir(&name) {
                                complete = false;
                                break;
                            }
                        }
                    }
                }
                Err(_) => complete = false,
            }
            continue;
        }
        let entries = match fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => {
                complete = false;
                continue;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let Ok(kind) = entry.file_type() else {
                complete = false;
                continue;
            };
            if !kind.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if name.starts_with('.') {
                continue;
            }
            if looks_like_post_dir(&name) {
                continue;
            }
            let child_path = entry.path();
            if visited.insert(child_path.clone()) {
                if bases.len() >= MAX_CONTAINER_BASES {
                    complete = false;
                    break;
                }
                bases.push(child_path.clone());
                queue.push_back((child_path, depth + 1));
            }
        }
        if bases.len() >= MAX_CONTAINER_BASES {
            complete = false;
            break;
        }
    }

    if !queue.is_empty() {
        complete = false;
    }

    if let Some(year) = year_hint {
        if !year.is_empty() {
            bases.sort_by_key(|b| {
                let s = b.to_string_lossy();
                if s.contains(year) {
                    0
                } else if b == &root {
                    1
                } else {
                    2
                }
            });
        }
    }

    (bases, complete)
}

/// The directory a post's files live in, found rather than assumed.
fn resolve_post_folder(
    target_dir: &str,
    naming: &PostNamingContext<'_>,
    template: &str,
    day: &str,
    derived: &[String],
) -> ResolvedFolder {
    resolve_post_folder_with_verified(target_dir, naming, template, day, derived, None)
}

/// A rendered path must distinguish a whole date or a post identity before
/// arbitrary existing content at that path can establish this work's coverage.
fn template_scopes_post(naming: &PostNamingContext<'_>, template: &str) -> bool {
    let rendered = naming.format_folder_path_with_template(template);
    let mut other = naming.clone();
    let alternate_id = format!("{}_other", naming.post_id);
    other.post_id = &alternate_id;
    if !naming.post_id.is_empty() && other.format_folder_path_with_template(template) != rendered {
        return true;
    }
    let parts: Vec<_> = naming.date.split('-').collect();
    if parts.len() != 3 || parts[0].len() != 4 || parts[1].len() != 2 || parts[2].len() != 2 {
        return false;
    }
    (0..3).all(|i| {
        let mut date = parts.clone();
        date[i] = match i {
            0 => {
                if parts[0] == "2000" {
                    "2001"
                } else {
                    "2000"
                }
            }
            _ => {
                if parts[i] == "01" {
                    "02"
                } else {
                    "01"
                }
            }
        };
        let mut other = naming.clone();
        other.date = date.join("-");
        other.format_folder_path_with_template(template) != rendered
    })
}

fn resolve_post_folder_with_verified(
    target_dir: &str,
    naming: &PostNamingContext<'_>,
    template: &str,
    day: &str,
    derived: &[String],
    verified_location: Option<&Path>,
) -> ResolvedFolder {
    if let Some(loc) = verified_location {
        let loc_exists = loc.is_dir() || (loc.is_file() && is_archive_file(loc));
        if loc_exists {
            let obs = observe_post_folder(loc, derived);
            if obs.state == OBSERVED_CONTENT || obs.state == OBSERVED_UNREADABLE {
                return ResolvedFolder::new(loc.to_path_buf(), FOLDER_FROM_NAME, false, obs);
            }
        }
    }

    let base = PathBuf::from(target_dir);
    let rendered = naming.format_folder_path_with_template(template);
    let expected = base.join(&rendered);
    let scoped_template = template_scopes_post(naming, template);
    let expected_exists = expected.is_dir() || (expected.is_file() && is_archive_file(&expected));
    // A rendered path that exists but holds nothing is still the answer this
    // search reaches when no candidate wins, and "empty" is a different reading
    // from "missing": an empty folder is a real directory that was read. Keep
    // what was measured here instead of re-deriving it at the fallback, and
    // instead of the caller re-reading the same directory to find out.
    let mut expected_observation: Option<FolderObservation> = None;
    if expected_exists {
        let obs = observe_post_folder(&expected, derived);
        if (scoped_template && obs.state == OBSERVED_CONTENT) || obs.state == OBSERVED_UNREADABLE {
            return ResolvedFolder::new(expected, FOLDER_FROM_TEMPLATE, false, obs);
        }
        expected_observation = Some(obs);
    }

    let rendered_first = rendered.split('/').find(|part| !part.is_empty());
    let year_hint = day.split('-').next().filter(|y| y.len() == 4);
    let (bases, bases_complete) = collect_search_bases(&base, rendered_first, year_hint);

    let mut incomplete = !bases_complete;
    let mut name_matched: Option<(PathBuf, FolderObservation)> = None;
    let mut day_content_folders: Vec<(PathBuf, FolderObservation)> = Vec::new();
    let mut day_unreadable_folders: Vec<(PathBuf, FolderObservation)> = Vec::new();
    let mut observed_cache: std::collections::HashMap<PathBuf, FolderObservation> =
        std::collections::HashMap::new();

    // One listing per base carrying both rules with cached observation to
    // eliminate redundant I/O on identical candidates.
    for candidate_base in &bases {
        let (candidates, complete) = list_subdirectories(candidate_base);
        if !complete {
            incomplete = true;
        }
        for candidate in &candidates {
            let matches_name = folder_entry_matches_post(
                &candidate.name,
                day,
                naming.title,
                Some(naming.post_id),
                derived,
            );
            let matches_day = folder_name_starts_with_day(&candidate.name, day);
            if !matches_name && !matches_day {
                continue;
            }
            let obs = if let Some(cached) = observed_cache.get(&candidate.path) {
                *cached
            } else {
                let fresh = observe_post_folder_existence(&candidate.path, derived);
                observed_cache.insert(candidate.path.clone(), fresh);
                fresh
            };
            if matches_name {
                if obs.state == OBSERVED_CONTENT {
                    let display_obs = observe_post_folder(&candidate.path, derived);
                    return ResolvedFolder::new(
                        candidate.path.clone(),
                        FOLDER_FROM_NAME,
                        incomplete,
                        display_obs,
                    );
                }
                if obs.state == OBSERVED_UNREADABLE {
                    return ResolvedFolder::new(
                        candidate.path.clone(),
                        FOLDER_FROM_NAME,
                        incomplete,
                        obs,
                    );
                }
                if name_matched.is_none() {
                    name_matched = Some((candidate.path.clone(), obs));
                }
            }
            if matches_day {
                if obs.state == OBSERVED_CONTENT {
                    day_content_folders.push((candidate.path.clone(), obs));
                } else if obs.state == OBSERVED_UNREADABLE {
                    day_unreadable_folders.push((candidate.path.clone(), obs));
                }
            }
        }
    }

    if !day_content_folders.is_empty() {
        day_content_folders.sort_by(|a, b| a.0.cmp(&b.0));
        let (path, _) = day_content_folders.remove(0);
        let display_obs = observe_post_folder(&path, derived);
        return ResolvedFolder::new(path, FOLDER_FROM_DAY, incomplete, display_obs);
    }

    if !day_unreadable_folders.is_empty() {
        day_unreadable_folders.sort_by(|a, b| a.0.cmp(&b.0));
        let (path, observation) = day_unreadable_folders.remove(0);
        return ResolvedFolder::new(path, FOLDER_FROM_DAY, incomplete, observation);
    }

    if let Some((folder, observation)) = name_matched {
        return ResolvedFolder::new(folder, FOLDER_FROM_NAME, incomplete, observation);
    }

    if !scoped_template && expected_observation.is_some_and(|o| o.state == OBSERVED_CONTENT) {
        return ResolvedFolder::new(
            expected,
            FOLDER_SHARED_TEMPLATE,
            incomplete,
            FolderObservation {
                state: OBSERVED_AMBIGUOUS,
                files: 0,
            },
        );
    }

    // Nothing matched. The rendered path is where a future download would land;
    // either it was already read above, or it does not exist. Report what was
    // actually measured rather than making the caller stat it again.
    ResolvedFolder::new(
        expected,
        FOLDER_FROM_TEMPLATE,
        incomplete,
        expected_observation.unwrap_or(FolderObservation {
            state: OBSERVED_MISSING,
            files: 0,
        }),
    )
}

/// Whether `folder_name` is `day`, or `day` followed by a separator.
fn folder_name_starts_with_day(folder_name: &str, day: &str) -> bool {
    strip_day_prefix(folder_name, day).is_some()
}

/// The posts whose folder one round reads.
///
/// Two kinds. The first is the download queue's own scope: a post that is still
/// waiting on something. The second is what keeps a folder search honest — a
/// post whose verdict rests on the day-only fallback.
///
/// That fallback answers "does my day hold a folder that could be mine", and
/// the answer depends on a rule this side can change and on a day the source
/// can move under it. A verdict produced that way must stay reachable, or a fix
/// to the rule can never reach the rows it was written for. That is exactly
/// what happened to the 1.0.362 sibling guard: it was correct, and it settled
/// nothing, because every post it was written for had already been marked had
/// and was therefore no longer read.
///
/// The last arm reads a row's `assessment_reason`, which is the only trace a
/// verdict left before `observed_source` was recorded: `''` means the row was
/// observed before that column existed, so how its folder was found is unknown
/// and the reason is what says whether the day rule decided it. Both arms empty
/// out by themselves — a row read once records its source, and a row the day
/// rule stops settling stops carrying that reason — so this costs one extra
/// read per day-found post and nothing at all for the rest.
///
/// The day arm is deliberately *not* gated on a rule version that the next fix
/// has to remember to bump. That gate would make the re-read self-emptying too,
/// but it would fail in exactly the way this release is fixing: a corrected
/// search that forgets to bump the version would leave the rows it was written
/// for unread, silently. It is bounded instead — the arm can hold no more rows
/// than the library has, and the whole measured library (599 posts) sits well
/// under `PAWCHIVE_OBSERVE_MAX_POSTS` — so the extra reads are a handful of
/// directory stats per round, not a starved queue.
fn posts_to_observe(
    conn: &Connection,
    limit: usize,
    only_subscription: Option<i64>,
) -> Result<Vec<PostObserveRow>> {
    let sql = format!(
        "SELECT p.id, p.subscription_id, p.post_id, s.service, s.user_id, a.name, p.title,
                p.published_at, s.target_dir
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         LEFT JOIN artists a ON a.id = s.artist_id
         WHERE (?3 IS NULL OR p.subscription_id = ?3)
           AND (p.assessment_state IN ('pending', 'partial', 'unverified', 'unavailable', 'unassessed', 'external')
            OR p.observed_source IN ('{FOLDER_FROM_DAY}', '{FOLDER_FROM_SIBLING_DAY}', 'template', '{FOLDER_SHARED_TEMPLATE}')
            OR (p.observed_source = '' AND p.assessment_reason = ?2))
         ORDER BY COALESCE(NULLIF(p.observed_at, ''), '1970-01-01T00:00:00Z') ASC, p.id ASC
         LIMIT ?1"
    );
    let mut stmt = conn.prepare(&sql)?;
    // A row-level read error is a failed observation, not a shorter list of
    // posts: reporting the rows that happened to parse would mark the rest as
    // unobserved while looking like a complete pass. The caller reports a
    // failed connection or query as `ObserveRound::Failed`, so a failed read
    // answers the same way.
    let rows = stmt
        .query_map(
            params![limit as i64, DAY_FOLDER_REASON, only_subscription],
            |row| {
                Ok(PostObserveRow {
                    post_db_id: row.get(0)?,
                    subscription_id: row.get(1)?,
                    post_id: row.get(2)?,
                    service: row.get(3)?,
                    user_id: row.get(4)?,
                    artist_name: row.get(5)?,
                    title: row.get(6)?,
                    published_at: row.get(7)?,
                    target_dir: row.get(8)?,
                })
            },
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Read every post whose verdict still depends on a folder search, and persist
/// what was found.
///
/// Offline: it touches the filesystem and the ledger, never the network. This
/// replaces the retired rule that marked a post downloaded as soon as its
/// folder held any file: the observation records *what the folder holds* (empty,
/// content, or unreadable) and *how the folder was found*, and the evaluator
/// decides from that. Today a folder that holds content is `Verified`, which
/// makes an automatic pass skip the post — see `resolve_post_folder` for what
/// that day-level rule can and cannot distinguish.
///
/// How one round ended.
///
/// `Failed` is deliberately not `Completed { observed: 0 }`. A round whose query
/// never ran, or which could not persist what it read, has not shown that the
/// library's folders are empty — it has shown nothing. A caller that reassesses
/// subscriptions off `Completed { 0 }` would be reassessing off a silence, and
/// would report the library as checked when a directory was never opened.
enum ObserveRound {
    Completed { observed: usize, truncated: bool },
    Failed,
}

/// Returns how the round ended. A round whose query or connection failed, or
/// which could not persist a reading it took, reports `Failed`: no rows were
/// read into the ledger, so nothing may be treated as newly observed.
fn observe_post_folders(pool: &Arc<DbPool>, only_subscription: Option<i64>) -> ObserveRound {
    let (rows, template) = {
        let conn = match pool.get() {
            Ok(c) => c,
            Err(err) => {
                eprintln!("warning: observe_post_folders: failed to acquire connection: {err}");
                return ObserveRound::Failed;
            }
        };
        let template = get_pawchive_settings(&conn)
            .map(|settings| settings.folder_template)
            .unwrap_or_default();
        let rows = match posts_to_observe(&conn, PAWCHIVE_OBSERVE_MAX_POSTS + 1, only_subscription)
        {
            Ok(r) => r,
            Err(err) => {
                eprintln!("warning: observe_post_folders: query failed: {err}");
                return ObserveRound::Failed;
            }
        };
        (rows, template)
    };

    let truncated = rows.len() > PAWCHIVE_OBSERVE_MAX_POSTS;
    let mut observed = 0;
    let mut failed = false;
    let mut touched_subscriptions: Vec<i64> = Vec::new();
    for row in rows.iter().take(PAWCHIVE_OBSERVE_MAX_POSTS) {
        let user = row
            .artist_name
            .as_deref()
            .filter(|name| !name.trim().is_empty())
            .unwrap_or(&row.user_id);
        let naming = PostNamingContext::new(
            user,
            row.published_at.as_deref(),
            row.title.as_deref(),
            &row.post_id,
            Some(&row.service),
        )
        .with_identity(&row.user_id, &row.service);
        let derived = derived_text_names(&naming);
        let resolved =
            resolve_post_folder(&row.target_dir, &naming, &template, &naming.date, &derived);
        // The search already read the chosen path, so the reading travels with
        // it: re-observing here would traverse the same directory a second time
        // for every post in the round.
        let observation = observation_after_search(resolved.observation, &resolved);
        let Ok(conn) = pool.get() else {
            failed = true;
            break;
        };
        // A row whose reading could not be written keeps its previous one, so
        // treating this as a shorter list would leave a stale verdict presented
        // as a fresh check.
        match record_folder_observation(&conn, row.post_db_id, observation, resolved.source) {
            Ok(()) => {
                observed += 1;
                if !touched_subscriptions.contains(&row.subscription_id) {
                    touched_subscriptions.push(row.subscription_id);
                }
            }
            Err(err) => {
                eprintln!("warning: observe_post_folders: could not record observation: {err}");
                failed = true;
                break;
            }
        }
    }

    // Recomputing the aggregates is the other half of the round: a subscription
    // whose post was read but whose summary was not recomputed keeps reporting
    // the old answer. It is logged and not fatal, because the reading itself is
    // already persisted and `reassess_all_posts` recomputes the whole library
    // in the reconcile path — but it is not silently swallowed either.
    if !touched_subscriptions.is_empty() {
        match pool.get() {
            Ok(conn) => {
                let settings = get_pawchive_settings(&conn).unwrap_or_default();
                for subscription_id in touched_subscriptions {
                    if let Err(err) = reassess_subscription(&conn, subscription_id, &settings) {
                        eprintln!(
                            "warning: observe_post_folders: reassess failed for \
                             subscription {subscription_id}: {err}"
                        );
                    }
                    if let Err(err) = refresh_subscription_coverage(&conn, subscription_id) {
                        eprintln!(
                            "warning: observe_post_folders: coverage refresh failed for \
                             subscription {subscription_id}: {err}"
                        );
                    }
                }
            }
            Err(err) => eprintln!("warning: observe_post_folders: reassess skipped: {err}"),
        }
    }
    if failed {
        return ObserveRound::Failed;
    }
    ObserveRound::Completed {
        observed,
        truncated,
    }
}

/// One `kemono_files` row read for a download pass.
struct PostFileRow {
    id: i64,
    remote_path: String,
    file_name: String,
    file_type: String,
    expected_blake3: Option<String>,
    status: String,
    /// Reserved when the task was first claimed and reused on every retry, so
    /// a later run cannot renumber a resource onto a name a sibling occupies.
    target_path: String,
    /// Set while the retry rules hold this resource back.
    next_attempt_at: Option<String>,
    /// Whether the source still lists this resource. A withdrawn attachment
    /// keeps its row and its delivered file but is no longer fetched.
    present_in_manifest: bool,
}

/// Stable creation order of a post's file rows.
///
/// The order is what the default `{date} {index}.{ext}` image template renders,
/// so it must not be derived from a status-filtered subset: renumbering a
/// retried file would land it on a name an already downloaded sibling occupies
/// and it could never succeed on a later round.
fn list_post_files(conn: &Connection, post_db_id: i64) -> Result<Vec<PostFileRow>> {
    let mut stmt = conn.prepare(
        "SELECT id, remote_path, file_name, file_type, expected_blake3, status,
                target_path, next_attempt_at, present_in_manifest
         FROM kemono_files
         WHERE post_id = ?1
         ORDER BY id ASC",
    )?;
    let rows = stmt
        .query_map(params![post_db_id], |row| {
            Ok(PostFileRow {
                id: row.get(0)?,
                remote_path: row.get(1)?,
                file_name: row.get(2)?,
                file_type: row.get(3)?,
                expected_blake3: row.get(4)?,
                status: row.get(5)?,
                target_path: row.get(6)?,
                next_attempt_at: row.get(7)?,
                present_in_manifest: row.get::<_, Option<i64>>(8)?.unwrap_or(1) != 0,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}


/// Why a delivery attempt failed.
///
/// Kept separate from the message so the retry policy can act on the cause:
/// a 404 is a resource to re-probe later, a 403 is a permission or access
/// problem, and neither may be recorded the same way as a broken stream.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DeliveryErrorClass {
    Network,
    RateLimited,
    AccessDenied,
    NotFound,
    PathConflict,
    Verification,
    Unknown,
}

impl DeliveryErrorClass {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Network => "network",
            Self::RateLimited => "rate_limited",
            Self::AccessDenied => "access_denied",
            Self::NotFound => "not_found",
            Self::PathConflict => "path_conflict",
            Self::Verification => "verification",
            Self::Unknown => "unknown",
        }
    }

    /// How long to hold this resource back, given how many times it failed.
    ///
    /// Every class defers rather than retiring the resource: a resource that is
    /// gone today can come back, and silently dropping it would turn an outage
    /// into a permanent gap. Repeated failures grow the wait so a round does not
    /// spend itself on the same broken resource.
    fn backoff_minutes(self, attempt: i64) -> i64 {
        let attempt = attempt.clamp(1, 10);
        let exponential = 1i64 << (attempt - 1);
        let minutes = match self {
            Self::Network => 2 * exponential,
            Self::RateLimited => 60,
            Self::AccessDenied => 720,
            // A resource the source no longer lists is re-probed daily, never
            // treated as permanently gone.
            Self::NotFound => 1440,
            Self::PathConflict => 60,
            Self::Verification => 1440,
            Self::Unknown => 30,
        };
        minutes.min(1440)
    }
}

/// Classify a download failure from its message.
///
/// The transport reports over HTTP, so the status code is the strongest signal
/// available; the remaining cases are the protocol failures this side raises
/// itself (a bad `Content-Range`, a hash mismatch, a refused target).
pub fn classify_download_error(message: &str) -> DeliveryErrorClass {
    let lower = message.to_lowercase();
    if let Some(code) = http_status_from_message(&lower) {
        return match code {
            401 | 403 => DeliveryErrorClass::AccessDenied,
            404 | 410 => DeliveryErrorClass::NotFound,
            408 | 429 => DeliveryErrorClass::RateLimited,
            500..=599 => DeliveryErrorClass::Network,
            _ => DeliveryErrorClass::Unknown,
        };
    }
    if lower.contains("blake3 hash mismatch")
        || lower.contains("does not match")
        || lower.contains("content-range")
    {
        return DeliveryErrorClass::Verification;
    }
    if lower.contains("already exists")
        || lower.contains("outside authorized media roots")
        || lower.contains("has no parent")
        || lower.contains("cannot create parent directory")
    {
        return DeliveryErrorClass::PathConflict;
    }
    if lower.contains("send download request failed")
        || lower.contains("error reading download stream")
        || lower.contains("timed out")
        || lower.contains("failed to read metadata")
    {
        return DeliveryErrorClass::Network;
    }
    DeliveryErrorClass::Unknown
}

fn http_status_from_message(lower: &str) -> Option<u16> {
    let rest = lower.split("http status: ").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// The wait a rate-limiting response asked for, in seconds.
///
/// `Retry-After` is either a delay in seconds or an HTTP date. Only the delay
/// form is honoured: converting a date would make the wait depend on how far
/// this host's clock is from the source's, and a skew must not be able to turn
/// a short wait into a long one or the reverse. The value is clamped to a day,
/// so a source cannot hold a resource back indefinitely.
fn retry_after_seconds(headers: &reqwest::header::HeaderMap) -> Option<i64> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let seconds = raw.trim().parse::<i64>().ok()?;
    if seconds <= 0 {
        return None;
    }
    Some(seconds.min(24 * 60 * 60))
}

/// The wait a failure message carried, if the transport recorded one.
fn retry_after_from_message(message: &str) -> Option<i64> {
    let rest = message.split("retry-after: ").nth(1)?;
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

/// How long to hold a failed resource back.
///
/// A wait the source stated itself wins over the class default: the retry rules
/// exist so this side does not hammer a source, and ignoring a declared
/// `Retry-After` is exactly that. Everything else keeps the per-class curve.
fn backoff_minutes_for(message: &str, class: DeliveryErrorClass, attempt: i64) -> i64 {
    match retry_after_from_message(message) {
        // Rounded up: a 90 second wait must not be retried after 60. Capped at
        // a day here as well as at the header, so no message can park a
        // resource for longer than the class curve itself allows.
        Some(seconds) => (seconds.clamp(1, 24 * 60 * 60) + 59) / 60,
        None => class.backoff_minutes(attempt),
    }
}

/// Reserve the path one resource will be published to, once.
///
/// The template can map two works onto one directory or one file name; taking
/// the name anyway would either overwrite the other work or make one work look
/// like another's. A target another resource already holds gets a suffix, and
/// the result is persisted so every later retry reuses it rather than
/// re-rendering whatever the template says today.
fn reserve_target_path(conn: &Connection, file_id: i64, desired: &Path) -> Result<PathBuf> {
    let mut target = desired.to_path_buf();
    if target_reserved_by_another(conn, file_id, &target)? {
        let (post_db_id, source_identity): (i64, String) = conn.query_row(
            "SELECT post_id, source_identity FROM kemono_files WHERE id = ?1",
            params![file_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        target = with_stable_suffix(&target, &target_suffix(post_db_id, &source_identity));
    }
    conn.execute(
        "UPDATE kemono_files SET target_path = ?1, updated_at = ?2 WHERE id = ?3",
        params![target.to_string_lossy(), Utc::now().to_rfc3339(), file_id],
    )?;
    Ok(target)
}

/// A stable suffix for a target that another resource already holds.
///
/// Derived from the work and the resource, so the retry of this resource
/// renders the same name again instead of walking to a new one.
fn target_suffix(post_db_id: i64, source_identity: &str) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(format!("{post_db_id}\n{source_identity}").as_bytes());
    hasher.finalize().to_hex().to_string()[..8].to_string()
}

pub(crate) fn with_stable_suffix(target: &Path, suffix: &str) -> PathBuf {
    let stem = target
        .file_stem()
        .and_then(|value| value.to_str())
        .unwrap_or("file");
    let extension = target
        .extension()
        .and_then(|value| value.to_str())
        .filter(|s| !s.is_empty())
        .map(|s| format!(".{s}"))
        .unwrap_or_default();
    let stem = clamp_name_bytes(
        stem,
        MAX_RENDERED_NAME_BYTES.saturating_sub(suffix.len() + 1 + extension.len()),
    );
    let name = format!("{stem}_{suffix}{extension}");
    target.parent().unwrap_or_else(|| Path::new(".")).join(name)
}

/// Whether another resource of the ledger already reserved this target.
///
/// A file on disk that no resource reserved is deliberately *not* a conflict:
/// the per-resource check records it as content found in place rather than
/// fetching it again.
fn target_reserved_by_another(conn: &Connection, file_id: i64, target: &Path) -> Result<bool> {
    let path = target.to_string_lossy().to_string();
    let found = conn
        .query_row(
            "SELECT 1 FROM kemono_files
             WHERE target_path = ?1 AND target_path != '' AND id != ?2",
            params![path, file_id],
            |_| Ok(()),
        )
        .optional()?;
    Ok(found.is_some())
}

/// Whether a file claim may still start for this post.
///
/// Re-read on every claim rather than once per round: a round can run for a long
/// time, and the settings it started with are not the settings the user has now.
/// Stopping new work has to reach each claim, not just the head of the round.
/// What a claim may still do, read at the moment of the claim.
///
/// The round's settings snapshot and the stored assessment were both written
/// before the first file was fetched, and adding a subscription with a start
/// date does not re-derive the posts already in the ledger. Re-reading the
/// admission facts here is what stops a round that sat queued behind a long
/// backlog from downloading a post the current selection no longer asks for.
struct ClaimScope {
    /// The master switch is on, this subscription is still enabled, and it is
    /// in a mode that allows an automatic round to take work from it.
    enabled: bool,
    /// The post's publication day is not before the subscription's start date.
    in_range: bool,
    /// The selection as it stands now, for the type check that follows.
    settings: PawchiveSettings,
}

/// The admission facts for one post, or `None` when the post is gone.
fn claim_scope(conn: &Connection, post_db_id: i64) -> Result<Option<ClaimScope>> {
    let settings = get_pawchive_settings(conn)?;
    let source_date = post_source_date_column(conn);
    let day_sql = post_day_sql("p", source_date);
    let query = format!(
        "SELECT s.enabled, s.mode, s.since_date, {day_sql}
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         WHERE p.id = ?1"
    );
    let row: Option<(i64, String, Option<String>, String)> = conn
        .query_row(&query, params![post_db_id], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .optional()?;
    let Some((subscription_enabled, mode, since_date, day)) = row else {
        return Ok(None);
    };
    // A post whose date could not be read is not silently in scope: an empty
    // day compares below any start date, which is what the coverage query does
    // with the same expression.
    let in_range = match since_date.as_deref().map(str::trim) {
        Some(since) if !since.is_empty() => day.as_str() >= since,
        _ => true,
    };
    Ok(Some(ClaimScope {
        enabled: settings.enabled
            && subscription_enabled != 0
            && SubscriptionMode::parse(&mode).is_auto(),
        in_range,
        settings,
    }))
}

/// The date a post is sorted by: its own publication date when the source gave
/// one, else the time this side first saw the row, and the empty string when
/// neither is readable.
///
/// Every queue query projects this as `sort_key`, so the three of them sort by
/// the same expression instead of three hand-copied ones.
const POST_SORT_KEY_SQL: &str = "COALESCE(NULLIF(p.published_at, ''), p.created_at, '')";

/// The queue order, written against a scope that has `sort_key` and `id`.
///
/// The oldest gap first; a post whose date could not be read sorts after every
/// dated one rather than ahead of them; the row id keeps the order total, so two
/// rounds walk the queue the same way.
const ROUND_QUEUE_ORDER_SQL: &str =
    "CASE WHEN sort_key = '' THEN 1 ELSE 0 END, substr(sort_key, 1, 10) ASC, id ASC";

/// The admission predicate, written against a scope that has the `kemono_posts`
/// row as `p` and the `kemono_subscriptions` row as `s`, with `?1` set to 1 for
/// an `auto`-only round.
///
/// A post is claimable when the evaluator found something missing that it may
/// fetch, and its subscription is on: discovery already honoured the
/// per-subscription switch but this query once did not, so a round downloaded
/// the pending posts of a subscription the user had switched off. A scheduled
/// round additionally requires `auto` mode, because `manual` means the user
/// takes works from the list themselves and the round must not decide for them.
///
/// It is one piece of text on purpose. Three queries use it — the flat
/// reference queue, the bounded round queue, and the count that says how much
/// the round left behind — and the count has to describe the same set the round
/// walked. When each query carried its own copy, a change to admission could
/// land in one of them and not the others.
const ROUND_ADMISSION_SQL: &str = "p.assessment_state IN ('pending', 'partial') AND s.enabled = 1
           AND (?1 = 0 OR s.mode = 'auto')";

/// How much of one subscription's backlog a single round takes.
///
/// The round used to build its queue from every outstanding post in the
/// database, so a creator with a 200k backlog was materialized in full before
/// the first file was fetched. The cut is per subscription and deliberately not
/// a global `LIMIT`: a global one takes the globally oldest posts, so a creator
/// whose backlog is all recent would drop out of the round entirely, which is
/// the unfairness the rotation exists to remove. What a subscription leaves
/// behind is reported, not silently skipped.
const PAWCHIVE_ROUND_MAX_POSTS_PER_SUBSCRIPTION: usize = 2000;


/// Where a piece of the automatic round's work came from.
///
/// The round is interleaved by subscription, so the entry has to carry the
/// subscription it was taken from and, when the subscription is bound to one,
/// the artist. `post_id` is the local row id — the source's own id is opaque
/// text and lives on the post row, not in the queue's key.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScheduledPost {
    /// `kemono_posts.id`.
    pub post_id: i64,
    pub subscription_id: i64,
    /// The artist the subscription is bound to, when one is bound.
    pub artist_id: Option<i64>,
    /// The normalized publication day, `None` when the source gave no date.
    pub day: Option<String>,
}

/// The automatic round's queue, and how much of the backlog it does not cover.
///
/// `posts.len()` against `outstanding` is the round's own answer to "processed N
/// of M"; the difference is what the per-subscription cap left for the next
/// round. The cut is derived rather than carried as a `truncated` flag because a
/// flag can say `true` over a difference of zero, and then two numbers that are
/// supposed to describe the same thing disagree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoundQueue {
    /// The posts this round will walk, in the order it walks them.
    pub posts: Vec<ScheduledPost>,
    /// How many posts the admission predicate matched, before the cap.
    pub outstanding: usize,
}

impl RoundQueue {
    /// Whether the cap left work behind for the next round.
    pub fn truncated(&self) -> bool {
        self.posts.len() < self.outstanding
    }
}

/// The automatic round's queue, interleaved across subscriptions.
///
/// Plan §7.2: 自动公平调度按画师轮转，再按规范化日期/稳定 work ID，每轮有资源/
/// 字节预算和退避，不让一个大作者或失败文件占满队列。 The complaint that
/// motivated it is a round that spends every claim on the one creator who happens
/// to sit at the head of a global date order, so the other creators' gaps never
/// close while that backlog is chewed through.
///
/// One post is taken from each subscription per turn, round-robin, and the
/// rotation only contains subscriptions that actually have outstanding work: a
/// subscription whose queue is empty is not a turn, so it cannot shift where the
/// other subscriptions' posts land. (A turn that yields nothing would be a
/// silent extra round for every remaining post.)
///
/// Order inside one subscription is left alone: the same rule as
/// [`claimable_posts`] — the oldest gap first, a post with no readable date
/// after every dated one, and the row id to make the order total. Both queues
/// share one admission predicate and one order expression, so a change to either
/// cannot land in only one of them.
///
/// Determinism: the queue query is totally ordered, groups are collected in
/// first-appearance order (that is, the subscription owning the globally oldest
/// outstanding gap goes first), and a `HashMap` is used only to find a group's
/// slot. No iteration order of a hash container reaches the result, so the same
/// database state always produces the same round.
///
/// The queue is bounded ([`PAWCHIVE_ROUND_MAX_POSTS_PER_SUBSCRIPTION`]) and the
/// result says how much it left behind. The cut is applied inside the query, per
/// subscription, and what comes back is exactly the first turns of the order an
/// unbounded round would have walked — the same posts in the same sequence, just
/// stopping earlier — so bounding the queue can never reorder or skip work.
pub fn schedule_round_order(conn: &Connection, auto_only: bool) -> Result<RoundQueue> {
    let queued = {
        let mut stmt = conn.prepare(&format!(
            "SELECT id, subscription_id, artist_id, day
             FROM (
                 SELECT id, subscription_id, artist_id, day, sort_key,
                        ROW_NUMBER() OVER (PARTITION BY subscription_id
                                           ORDER BY {ROUND_QUEUE_ORDER_SQL}) AS rank
                 FROM (
                     SELECT p.id AS id, p.subscription_id AS subscription_id,
                            s.artist_id AS artist_id,
                            NULLIF(substr({POST_SORT_KEY_SQL}, 1, 10), '') AS day,
                            {POST_SORT_KEY_SQL} AS sort_key
                     FROM kemono_posts p
                     JOIN kemono_subscriptions s ON s.id = p.subscription_id
                     WHERE {ROUND_ADMISSION_SQL}
                 )
             )
             WHERE rank <= ?2
             ORDER BY {ROUND_QUEUE_ORDER_SQL}"
        ))?;
        let collected = stmt
            .query_map(
                params![
                    auto_only as i64,
                    PAWCHIVE_ROUND_MAX_POSTS_PER_SUBSCRIPTION as i64
                ],
                |row| {
                    Ok(ScheduledPost {
                        post_id: row.get(0)?,
                        subscription_id: row.get(1)?,
                        artist_id: row.get(2)?,
                        day: row.get(3)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<ScheduledPost>>>()?;
        collected
    };

    // Groups keyed by subscription, in first-appearance order. The map is a
    // lookup for that order, never the order itself.
    let mut groups: Vec<Vec<ScheduledPost>> = Vec::new();
    let mut slot: std::collections::HashMap<i64, usize> = std::collections::HashMap::new();
    for post in queued {
        let index = match slot.get(&post.subscription_id) {
            Some(index) => *index,
            None => {
                groups.push(Vec::new());
                slot.insert(post.subscription_id, groups.len() - 1);
                groups.len() - 1
            }
        };
        groups[index].push(post);
    }

    let turns = groups.iter().map(Vec::len).max().unwrap_or(0);
    let mut posts = Vec::new();
    for turn in 0..turns {
        for group in &groups {
            if let Some(post) = group.get(turn) {
                posts.push(post.clone());
            }
        }
    }

    // Counted over the same predicate the queue was built from, so the two can
    // differ only by what the cap removed. A failed count fails the queue rather
    // than returning a queue that claims to be complete: `outstanding` would
    // come back as a number this side made up, and the round would report that
    // it left nothing behind.
    let outstanding: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*)
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE {ROUND_ADMISSION_SQL}"
        ),
        params![auto_only as i64],
        |row| row.get(0),
    )?;

    Ok(RoundQueue {
        posts,
        outstanding: outstanding.max(0) as usize,
    })
}

/// Restore evidence for resources that were published but not recorded.
///
/// A crash between the publish and its ledger write would otherwise leave a
/// resource whose file is on the share and whose row still says `downloading`.
/// The publish ledger is the authority for that pairing, so the row is completed
/// from it rather than the file being fetched a second time.
///
/// The pairing is only accepted when it is the same delivery: the ledger row has
/// to name this resource and the manifest it was published for, the target has
/// to be a regular file under the authorized roots, and its bytes have to still
/// hash to what the publish recorded. Length alone used to be the whole test,
/// which restored a file somebody had replaced with a same-size one as a
/// completed download.
fn reconcile_published_files(conn: &Connection, roots: &MediaRoots) -> Result<usize> {
    let rows: Vec<(i64, String, i64, i64, String, String)> = {
        let mut stmt = conn.prepare(
            "SELECT f.id, j.target_path, j.expected_length, j.id, j.expected_blake3,
                    j.source_identity
             FROM kemono_files f
             JOIN kemono_posts p ON p.id = f.post_id
             JOIN download_publish_jobs j
                  ON j.engine = 'pawchive'
                 AND j.source_job_id = p.post_id || ':' || f.id
                 AND j.source_identity = f.source_identity
                 AND j.manifest_version = p.manifest_version
                 AND j.stage IN ('published', 'ingested')
             WHERE f.status != 'done'",
        )?;
        let collected = stmt
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        collected
    };
    let mut restored = 0;
    for (file_id, target_path, expected_length, job_id, expected_blake3, source_identity) in rows {
        let target = PathBuf::from(&target_path);
        if !path_under_authorized_roots(&target, roots) {
            continue;
        }
        // `metadata` follows a symlink, so a link pointing at any same-size file
        // used to satisfy this. The publish only ever creates a regular file.
        let regular = fs::symlink_metadata(&target)
            .map(|metadata| !metadata.file_type().is_symlink() && metadata.is_file())
            .unwrap_or(false);
        if !regular {
            continue;
        }
        let matches = read_file_digest(&target)
            .map(|digest| digest.eq_ignore_ascii_case(expected_blake3.trim()))
            .unwrap_or(false);
        if !matches {
            continue;
        }
        conn.execute(
            "UPDATE kemono_files
             SET status = 'done', delivery_source = 'download', publish_job_id = ?1,
                 target_path = ?2, expected_length = ?3, expected_blake3 = ?6, local_hash = ?6,
                 integrity = 'verified', hash_source = 'local', verified_at = ?4,
                 claimed_at = NULL, error_message = NULL, error_class = '',
                 next_attempt_at = NULL, updated_at = ?4
             WHERE id = ?5 AND source_identity = ?7",
            params![
                job_id,
                target_path,
                expected_length,
                Utc::now().to_rfc3339(),
                file_id,
                expected_blake3,
                source_identity
            ],
        )?;
        restored += 1;
    }
    Ok(restored)
}

/// The claim one download pass holds on a `kemono_files` row.
///
/// One owner per pass rather than per row: the lease is per row, but the worker
/// that loses it is the whole pass, so a single owner token revokes all of it at
/// once.
#[derive(Clone, Debug)]
struct DownloadClaim {
    owner: String,
}

impl DownloadClaim {
    fn new() -> Self {
        Self {
            owner: format!("dl_{}", uuid::Uuid::new_v4().simple()),
        }
    }

    fn owner(&self) -> &str {
        &self.owner
    }

    /// Take the row for this pass and return the fencing token it now carries.
    ///
    /// The claim increments the token itself, so the value returned here is the
    /// only one this pass can ever write with: a reclaim moves the row past it,
    /// and every guarded write then matches nothing.
    fn claim(&self, conn: &Connection, file_id: i64) -> Result<Option<i64>> {
        let token = conn
            .query_row(
                "UPDATE kemono_files
                 SET status = 'downloading', claimed_at = ?1, claimed_by = ?2,
                     claim_token = claim_token + 1, attempt_count = attempt_count + 1,
                     updated_at = ?1
                 WHERE id = ?3
                 RETURNING claim_token",
                params![Utc::now().to_rfc3339(), self.owner, file_id],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        Ok(token)
    }
}

/// Whether this pass still holds `file_id` with the token it was granted.
///
/// Asked immediately before publishing, which is the step the plan names. A
/// reclaim returns the row to the queue and bumps its token, so the answer turns
/// false the moment the pass loses its lease. Publishing is create-if-absent and
/// what was already published stays, so the check's job is to stop this worker
/// from naming a file the row no longer points at, and from recording a result
/// against the row its successor now holds.
fn still_claimed(
    conn: &Connection,
    file_id: i64,
    claim: &DownloadClaim,
    token: i64,
) -> Result<bool> {
    let held: Option<i64> = conn
        .query_row(
            "SELECT claim_token FROM kemono_files WHERE id = ?1 AND claimed_by = ?2",
            params![file_id, claim.owner()],
            |row| row.get(0),
        )
        .optional()?;
    Ok(held == Some(token))
}

/// Settle one file row, but only while this pass still holds the claim it was
/// granted, and release the claim in the same statement.
///
/// Both halves are one statement on purpose. A settle that released a claim it no
/// longer held would clear the new owner's, and a settle that kept the claim
/// would leave a finished row looking like somebody is still working on it. The
/// `claimed_by`/`claim_token` predicate is the same one the pre-publish check
/// reads, so a pass that lost its lease is refused by the write itself rather
/// than by the check having been recent enough.
///
/// `assignments` is the caller's own `SET` fragment, using unnumbered `?`
/// placeholders that bind before the guard's.
fn settle_claimed_file(
    conn: &Connection,
    file_id: i64,
    claim: &DownloadClaim,
    token: i64,
    assignments: &str,
    values: Vec<rusqlite::types::Value>,
) -> Result<bool> {
    let sql = format!(
        "UPDATE kemono_files
         SET {assignments}, claimed_at = NULL, claimed_by = ''
         WHERE id = ? AND claimed_by = ? AND claim_token = ?"
    );
    let mut bound = values;
    bound.push(file_id.into());
    bound.push(claim.owner().to_string().into());
    bound.push(token.into());
    Ok(conn.execute(&sql, rusqlite::params_from_iter(bound))? == 1)
}

/// Release download claims a crashed or killed round left behind.
///
/// A row written as `downloading` and then abandoned would never be picked up
/// again: the queue only claims `pending` and `failed`, so the resource would
/// stay invisible forever.
///
/// The reclaim also clears the owner and bumps the fencing token. It cannot know
/// whether the worker it is superseding is still writing, so it does not try to
/// stop it: it takes the row past the token that worker holds, and every write
/// that worker makes is conditional on that token. That is what makes "the lease
/// expired" mean "the old worker can no longer commit" instead of "the old
/// worker is assumed to have stopped".
fn reset_stale_download_claims(conn: &Connection, lease_minutes: i64) -> Result<usize> {
    let cutoff = (Utc::now() - Duration::minutes(lease_minutes.max(1))).to_rfc3339();
    let recovered = conn.execute(
        "UPDATE kemono_files
         SET status = 'pending', claimed_at = NULL, claimed_by = '',
             claim_token = claim_token + 1, updated_at = ?1
         WHERE status = 'downloading'
           AND (claimed_at IS NULL OR claimed_at < ?2)",
        params![Utc::now().to_rfc3339(), cutoff],
    )?;
    Ok(recovered)
}

/// The state of one explicit request, or `None` when no such attempt exists.
fn attempt_state(conn: &Connection, attempt_id: &str) -> Result<Option<String>> {
    let state = conn
        .query_row(
            "SELECT state FROM pawchive_attempts WHERE attempt_id = ?1",
            params![attempt_id],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    Ok(state)
}

/// What a cancel request found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CancelOutcome {
    /// The request was open and is now cancelled; `posts` of its works were
    /// still outstanding.
    Cancelled {
        posts: i64,
    },
    /// Already cancelled; repeating a cancel is not an error.
    AlreadyCancelled,
    /// Finished before the cancel arrived. Reporting it as cancelled would
    /// misdescribe a completed task, and there is nothing left to stop.
    AlreadyFinished,
    Unknown,
}

/// Cancel one explicit request.
///
/// Cancelling stops *subsequent* claims: the resource already receiving is
/// allowed to reach its own safe point — it writes to its own part file and only
/// names the final file after verifying it — and anything already published
/// stays where it is. The works it never reached are recorded as cancelled
/// rather than left queued, so the panel does not report outstanding work for a
/// task the user stopped.
pub fn cancel_attempt(conn: &Connection, attempt_id: &str) -> Result<CancelOutcome> {
    let Some(state) = attempt_state(conn, attempt_id)? else {
        return Ok(CancelOutcome::Unknown);
    };
    match state.as_str() {
        "cancelled" => Ok(CancelOutcome::AlreadyCancelled),
        "done" => Ok(CancelOutcome::AlreadyFinished),
        _ => {
            let now = Utc::now().to_rfc3339();
            let changed = conn.execute(
                "UPDATE pawchive_attempts SET state = 'cancelled', updated_at = ?1
                 WHERE attempt_id = ?2 AND state NOT IN ('done', 'cancelled')",
                params![now, attempt_id],
            )?;
            if changed == 0 {
                // The state moved between the read and the write; report what it
                // is now rather than claiming a cancel that did not happen.
                return Ok(match attempt_state(conn, attempt_id)?.as_deref() {
                    Some("cancelled") => CancelOutcome::AlreadyCancelled,
                    Some("done") => CancelOutcome::AlreadyFinished,
                    _ => CancelOutcome::Unknown,
                });
            }
            let posts = conn.execute(
                "UPDATE pawchive_attempt_posts SET state = 'cancelled', updated_at = ?1
                 WHERE attempt_id = ?2 AND state IN ('queued', 'failed', 'conflict')",
                params![now, attempt_id],
            )? as i64;
            Ok(CancelOutcome::Cancelled { posts })
        }
    }
}

/// Download a post's missing files, publish them, and record what was proven.
pub async fn download_post_files(
    pool: &Arc<DbPool>,
    client: &reqwest::Client,
    post_db_id: i64,
    artist_dir: &Path,
    roots: &MediaRoots,
) -> Result<usize> {
    let settings = {
        let conn = pool.get()?;
        get_pawchive_settings(&conn).unwrap_or_default()
    };
    download_post_files_with_settings(pool, client, post_db_id, artist_dir, roots, &settings).await
}

/// Download the files this post still needs, with specific settings.
///
/// Every resource is judged on its own: it is downloaded, or it is already at
/// its own reserved target, or it failed — and only the first two produce
/// evidence. Per-resource judgement decides what this pass fetches; it does not
/// decide whether the pass runs at all.
///
/// An automatic pass stops before the network when the work's folder already
/// holds content, because the date-level admission treats that as had. What that
/// costs is deliberate and documented on `resolve_post_folder`: a second work on
/// the same day, or an attachment added after the day's folder was filled, is
/// settled by that folder too. A manual attempt is not filtered by admission
/// (`pass_admission_settings`), so the same partial delivery can still be
/// supplemented by hand, under the authorized-root and no-overwrite rules.
///
/// The pooled connection is checked out per database phase and released before
/// each network await. Borrowing a checked-out guard across an await would make
/// this future `!Send` (a `rusqlite::Connection` is `Send` but not `Sync`), and
/// holding it for a whole download would occupy a pool slot for minutes.
pub async fn download_post_files_with_settings(
    pool: &Arc<DbPool>,
    client: &reqwest::Client,
    post_db_id: i64,
    artist_dir: &Path,
    roots: &MediaRoots,
    settings: &PawchiveSettings,
) -> Result<usize> {
    download_post_files_scoped(
        pool,
        client,
        post_db_id,
        artist_dir,
        roots,
        settings,
        DownloadPostPolicy::Automatic,
    )
    .await
}

/// What caused a sync round. The per-subscription `enabled` flag only gates
/// automatic rounds; plan section 3.1 lets a manual round cover every
/// subscription once without changing any subscription's state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SyncTrigger {
    /// The user pressed 立即同步全部.
    Manual,
    /// The background interval round.
    Scheduled,
    /// The user pressed 检查缺哪些: discover and recompute coverage, then stop.
    /// Nothing is downloaded and no ingest is claimed.
    Check,
}

impl SyncTrigger {
    /// Stable token for the status payload and the event log.
    pub fn as_str(self) -> &'static str {
        match self {
            SyncTrigger::Manual => "manual",
            SyncTrigger::Scheduled => "scheduled",
            SyncTrigger::Check => "check",
        }
    }

    /// A check never downloads, so it must not claim the pending posts.
    pub(super) fn downloads(self) -> bool {
        self != SyncTrigger::Check
    }
}

