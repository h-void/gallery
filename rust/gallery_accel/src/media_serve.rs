//! Native media serve (file/stream/text/delete + video frame via ffmpeg).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex as StdMutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;

use std::collections::HashSet;

use rusqlite::OptionalExtension;

use crate::fs_util::safe_canonicalize;
use crate::image_preview::{clamp_max_edge, image_preview_bytes};
#[cfg(not(target_os = "linux"))]
use crate::media_roots::path_under_authorized_roots;
use crate::media_roots::MediaRoots;
use crate::media_type::media_type_for_file;
use crate::recycle::{capture_item_snapshot, ensure_recycle_schema};

const TEXT_PREVIEW_MAX_BYTES: u64 = 512 * 1024;
const TRANSCODE_MARKER_STALE_AFTER: Duration = Duration::from_secs(60 * 60);
/// Single-frame extraction is a seek + one decode; anything longer means the
/// input is broken and the request should fail bounded instead of hanging.
const VIDEO_FRAME_EXTRACT_TIMEOUT: Duration = Duration::from_secs(30);
const VIDEO_FRAME_CACHE_VERSION: u32 = 1;
const DEFAULT_VIDEO_FRAME_CACHE_MAX_BYTES: u64 = 2_000_000_000;
const DEFAULT_VIDEO_TRANSCODE_CACHE_MAX_BYTES: u64 = 900_000_000;
const VIDEO_FRAME_CACHE_CLEANUP_INTERVAL: u64 = 300;
static VIDEO_FRAME_CACHE_LAST_CLEANUP: AtomicU64 = AtomicU64::new(0);

fn ffmpeg_gate() -> &'static (StdMutex<usize>, Condvar) {
    static GATE: OnceLock<(StdMutex<usize>, Condvar)> = OnceLock::new();
    GATE.get_or_init(|| (StdMutex::new(0), Condvar::new()))
}

fn ffmpeg_max_concurrency() -> usize {
    static LIMIT: OnceLock<usize> = OnceLock::new();
    *LIMIT.get_or_init(|| {
        std::env::var("FFMPEG_MAX_CONCURRENCY")
            .ok()
            .and_then(|value| value.trim().parse::<usize>().ok())
            .filter(|value| *value > 0)
            .unwrap_or(2)
    })
}

/// Bounded wait for a free ffmpeg slot. Queueing is intentional, but an
/// unbounded wait would pile blocked threads onto the blocking pool when a
/// wedged ffmpeg process never releases its slot.
const FFMPEG_SLOT_WAIT: Duration = Duration::from_secs(60);
/// Hard total limit for one HLS transcode (`-codec: copy` is I/O bound and
/// even very large files finish far inside this). A wedged ffmpeg must
/// release its slot instead of holding it for the process lifetime.
const TRANSCODE_TOTAL_LIMIT: Duration = Duration::from_secs(2 * 60 * 60);

/// Bounds concurrent ffmpeg child processes across every spawn site. Requests
/// queue for a free slot instead of being rejected, so a burst of uncached
/// videos cannot fan out into unbounded transcode/frame processes.
pub(crate) struct FfmpegSlotGuard;

impl FfmpegSlotGuard {
    pub(crate) fn acquire_blocking() -> Result<Self> {
        let (lock, condvar) = ffmpeg_gate();
        let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = std::time::Instant::now() + FFMPEG_SLOT_WAIT;
        while *active >= ffmpeg_max_concurrency() {
            let now = std::time::Instant::now();
            if now >= deadline {
                anyhow::bail!(
                    "ffmpeg concurrency slot busy for over {FFMPEG_SLOT_WAIT:?}; request rejected"
                );
            }
            let (guard, _timeout) = condvar
                .wait_timeout(active, deadline - now)
                .unwrap_or_else(|e| e.into_inner());
            active = guard;
        }
        *active += 1;
        Ok(FfmpegSlotGuard)
    }
}

impl Drop for FfmpegSlotGuard {
    fn drop(&mut self) {
        let (lock, condvar) = ffmpeg_gate();
        let mut active = lock.lock().unwrap_or_else(|e| e.into_inner());
        *active -= 1;
        condvar.notify_one();
    }
}

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
}

/// Comparison key for "is this walked path the same stored path?".
///
/// Two differences between a stored path and one the OS walk renders have to be
/// folded away, and a byte comparison missed the same file because of both: the
/// verbatim prefix `WalkDir` yields on Windows (`//?/d:/…`) and the case of the
/// drive letter (`D:/…` in the database, `d:/…` from the process). Missing either
/// one made the loose-file walk move a file the item pass had just refused.
/// Windows paths fold fully (NTFS is case-insensitive); on Linux nothing is
/// case-folded, because there two paths that differ in case are two files.
fn path_key(path: &str) -> String {
    let normalized = normalize_slashes(path);
    let normalized = normalized
        .strip_prefix("//?/")
        .unwrap_or(&normalized)
        .to_string();
    #[cfg(windows)]
    {
        normalized.to_lowercase()
    }
    #[cfg(not(windows))]
    {
        normalized
    }
}

fn capture_character_ref_snapshot(
    conn: &rusqlite::Connection,
    item_id: i64,
) -> Result<(String, String)> {
    let mut tag_refs = Vec::new();
    let mut non_tag_ids = Vec::new();
    let mut stmt = conn.prepare("SELECT id, character_id, embedding, embedding_dim, embedding_model_repo_id, embedding_model_variant, embedding_model_file, embedding_updated_at, source_type, created_at FROM character_references WHERE item_id=? ORDER BY id")?;
    let rows = stmt.query_map([item_id], |row| {
        let id: i64 = row.get(0)?;
        let source: String = row.get(8)?;
        let value = json!({
            "character_id": row.get::<_, i64>(1)?,
            "embedding": row.get::<_, Vec<u8>>(2)?,
            "embedding_dim": row.get::<_, i64>(3)?,
            "embedding_model_repo_id": row.get::<_, String>(4)?,
            "embedding_model_variant": row.get::<_, String>(5)?,
            "embedding_model_file": row.get::<_, String>(6)?,
            "embedding_updated_at": row.get::<_, Option<f64>>(7)?,
            "source_type": source,
            "created_at": row.get::<_, f64>(9)?,
        });
        Ok((id, value))
    })?;
    for row in rows {
        let (id, value) = row?;
        if value["source_type"] == "tag_single" {
            tag_refs.push(value);
        } else {
            non_tag_ids.push(id);
        }
    }
    Ok((
        serde_json::to_string(&tag_refs)?,
        serde_json::to_string(&non_tag_ids)?,
    ))
}

fn real_media_roots(roots: &MediaRoots) -> Vec<String> {
    roots.allowed_roots()
}

fn is_under_allowed_root(path: &Path, allowed: &[String]) -> bool {
    let Ok(canon) = safe_canonicalize(path) else {
        // If file does not exist yet, check logical path only.
        let logical = normalize_slashes(&path.to_string_lossy());
        return allowed.iter().any(|root| {
            let root = root.trim_end_matches(['/', '\\']);
            logical == root || logical.starts_with(&format!("{root}/"))
        });
    };
    let logical = normalize_slashes(&canon.to_string_lossy());
    // A canonical Windows path carries the `\\?\` verbatim prefix. Roots that
    // answer the final-path query are canonicalized the same way and compare with
    // the prefix; a root that cannot be canonicalized - a virtual alias, or any
    // root on a volume that refuses the query, such as the ImDisk RamDisk - is
    // configured in plain form, so that case compares without it.
    let logical_plain = logical.strip_prefix("//?/").unwrap_or(&logical);
    allowed.iter().any(|root| {
        let root_path = PathBuf::from(root);
        if let Ok(root_canon) = safe_canonicalize(&root_path) {
            let root_s = normalize_slashes(&root_canon.to_string_lossy());
            logical == root_s
                || logical.starts_with(&format!("{root_s}/"))
                || logical.starts_with(&format!("{root_s}\\"))
        } else {
            let root_s = root.trim_end_matches(['/', '\\']);
            logical_plain == root_s || logical_plain.starts_with(&format!("{root_s}/"))
        }
    })
}

/// Resolve a media path only if it is under configured media roots / real mappings.
///
/// Mirrors Python `_is_path_allowed` / `_resolve_allowed_path` safety: never serve
/// arbitrary host files (e.g. `/etc/hosts`, `C:\\Windows\\...`).
pub fn resolve_allowed_path(path: &str, roots: &MediaRoots) -> Result<PathBuf> {
    let cleaned = normalize_slashes(path.trim());
    if cleaned.is_empty() || cleaned.split('/').any(|part| part == "..") {
        return Err(anyhow!("path not allowed"));
    }
    let allowed = real_media_roots(roots);

    let mut candidates: Vec<PathBuf> = Vec::new();
    // Map virtual roots to real paths via MediaRoots (single env parse at startup).
    if let Ok(mapped) = roots.map_to_real(&cleaned) {
        candidates.push(mapped);
    }
    candidates.push(PathBuf::from(&cleaned));

    for cand in candidates {
        let Ok(canonical) = safe_canonicalize(&cand) else {
            continue;
        };
        if is_under_allowed_root(&canonical, &allowed) && canonical.is_file() {
            // Callers must use this verified path rather than resolving the
            // user-supplied path a second time after the allowlist check.
            return Ok(canonical);
        }
    }
    Err(anyhow!("file not found or not allowed"))
}

/// Content types the browser may render inline. Media directories are
/// user-writable, so only whitelisted image/video/audio extensions keep a
/// renderable type; everything else (HTML family, SVG, XML, unknown binaries)
/// is served as `application/octet-stream` with `Content-Disposition:
/// attachment`, so a planted `.html`/`.svg` can never execute script in this
/// app's origin.
fn inline_media_headers(full: &Path) -> (String, Option<String>) {
    const INLINE_AUDIO: &[&str] = &["mp3", "m4a", "aac", "ogg", "oga", "opus", "wav", "flac"];
    let name = full
        .file_name()
        .map(|value| value.to_string_lossy().to_string())
        .unwrap_or_default();
    let ext = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let category = media_type_for_file(&name);
    if category == Some("image")
        || category == Some("video")
        || INLINE_AUDIO.contains(&ext.as_str())
    {
        let mime = mime_guess::from_path(full)
            .first_or_octet_stream()
            .essence_str()
            .to_string();
        (mime, None)
    } else {
        ("application/octet-stream".to_string(), Some(name))
    }
}

fn attachment_disposition(name: &str) -> String {
    let fallback: String = name
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '-' | '_' | '(' | ')' | ' ') {
                ch
            } else {
                '_'
            }
        })
        .collect();
    format!("attachment; filename=\"{fallback}\"")
}

/// One open handle together with the metadata read *from that handle*.
///
/// The length and the bytes have to come from the same `open`. Taking the
/// length from a `metadata(path)` call and then opening the path again lets a
/// replacement land between the two calls, and the response then describes a
/// file whose bytes it is not reading: a `Content-Length` that does not match
/// the body, or a `206` that streams the tail of a different version. The
/// validator has the same requirement — an `ETag` derived from path metadata
/// would be a claim about a file that may already be gone.
struct OpenedMedia {
    file: File,
    len: u64,
    modified: Option<SystemTime>,
}

impl OpenedMedia {
    async fn open(path: &Path) -> std::io::Result<Self> {
        let file = File::open(path).await?;
        let meta = file.metadata().await?;
        Ok(Self {
            file,
            len: meta.len(),
            modified: meta.modified().ok(),
        })
    }
}

/// A strong validator over the length and the modification time, so a
/// same-size replacement at a different time is still a different version.
///
/// The frontend's own cache key is `size-mtime` at one-second resolution, which
/// is why a same-size replacement inside the same second produces a
/// byte-identical URL. The validator is what lets the client revalidate that
/// URL instead of trusting it, and `If-Range` is what stops a range request
/// from splicing two versions together.
fn media_etag(len: u64, modified: Option<SystemTime>) -> String {
    let stamp = modified
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|delta| delta.as_nanos())
        .unwrap_or(0);
    format!("\"{:x}-{:x}\"", len, stamp)
}

/// Format an instant as an HTTP-date (RFC 9110 §5.6.7 IMF-fixdate).
fn http_date(time: SystemTime) -> String {
    let moment: chrono::DateTime<chrono::Utc> = time.into();
    moment.format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn parse_http_date(value: &str) -> Option<SystemTime> {
    chrono::DateTime::parse_from_rfc2822(value.trim())
        .ok()
        .map(|parsed| parsed.with_timezone(&chrono::Utc).into())
}

/// Whether the request's conditional headers say the client already holds this
/// version, which is answered `304` rather than with a body.
///
/// `If-None-Match` wins when both are present, because it is the field that can
/// tell two same-second revisions apart; `If-Modified-Since` is only consulted
/// on its own, and a date it cannot parse is not a match.
fn not_modified_since(
    headers: &HeaderMap,
    etag: &str,
    last_modified: Option<&str>,
    modified: Option<SystemTime>,
) -> bool {
    if let Some(value) = headers
        .get(header::IF_NONE_MATCH)
        .and_then(|value| value.to_str().ok())
    {
        let value = value.trim();
        if value == "*" {
            return true;
        }
        // Weak comparison is the correct one here: `If-None-Match` only asks
        // whether the representation is the same one.
        return value.split(',').any(|candidate| {
            let candidate = candidate.trim();
            candidate.strip_prefix("W/").unwrap_or(candidate) == etag
        });
    }
    let (Some(value), Some(modified)) = (
        headers
            .get(header::IF_MODIFIED_SINCE)
            .and_then(|value| value.to_str().ok()),
        modified,
    ) else {
        return false;
    };
    // The comparison is at one-second resolution because that is the resolution
    // of the field, so "not newer than" is the condition.
    let _ = last_modified;
    parse_http_date(value).is_some_and(|since| modified <= since + Duration::from_secs(1))
}

/// Whether an `If-Range` still names the current version.
///
/// A range is only answered when this holds; otherwise the whole body is sent,
/// because a `206` built from a validator that no longer matches would stitch
/// the client's old copy to bytes from the new one.
fn if_range_allows(value: &str, etag: &str, modified: Option<SystemTime>) -> bool {
    let value = value.trim();
    if value.starts_with('"') || value.starts_with("W/") {
        // The field requires a strong comparison, so a weak validator never
        // authorises a partial response.
        return !value.starts_with("W/") && value == etag;
    }
    match (parse_http_date(value), modified) {
        (Some(since), Some(modified)) => modified <= since + Duration::from_secs(1),
        _ => false,
    }
}

/// Build the response for an already-open media file.
///
/// One constructor decides `200` / `206` / `304` for every media route, and the
/// validator it emits and the range it answers both come from the same handle,
/// so a response cannot describe one version of a file while streaming another.
async fn file_response(
    opened: OpenedMedia,
    mime: &str,
    disposition: Option<&str>,
    headers: &HeaderMap,
    allow_ranges: bool,
) -> Result<Response, (StatusCode, Value)> {
    let OpenedMedia {
        file,
        len,
        modified,
    } = opened;
    let etag = media_etag(len, modified);
    let last_modified = modified.map(http_date);

    if not_modified_since(headers, &etag, last_modified.as_deref(), modified) {
        let mut builder = Response::builder()
            .status(StatusCode::NOT_MODIFIED)
            .header(header::ETAG, &etag);
        if let Some(value) = &last_modified {
            builder = builder.header(header::LAST_MODIFIED, value);
        }
        return builder.body(Body::empty()).map_err(internal);
    }

    // A range is honoured only when the client is not also asking us to check
    // it first, or when the check still passes.
    let range = match headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        Some(value) if allow_ranges => {
            match headers
                .get(header::IF_RANGE)
                .and_then(|value| value.to_str().ok())
            {
                Some(if_range) if !if_range_allows(if_range, &etag, modified) => None,
                _ => Some(value),
            }
        }
        _ => None,
    };

    if let Some(value) = range {
        match parse_bytes_range(value, len) {
            ParsedRange::Satisfiable(start, end) => {
                let mut file = file;
                use tokio::io::{AsyncSeekExt, SeekFrom};
                file.seek(SeekFrom::Start(start)).await.map_err(internal)?;
                let take = end - start + 1;
                let mut builder = Response::builder()
                    .status(StatusCode::PARTIAL_CONTENT)
                    .header(header::CONTENT_TYPE, mime)
                    .header(header::ACCEPT_RANGES, "bytes")
                    .header(header::ETAG, &etag)
                    .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                    .header(header::CONTENT_LENGTH, take);
                if let Some(value) = &last_modified {
                    builder = builder.header(header::LAST_MODIFIED, value);
                }
                if let Some(name) = disposition {
                    builder =
                        builder.header(header::CONTENT_DISPOSITION, attachment_disposition(name));
                }
                return builder
                    .body(Body::from_stream(ReaderStream::new(file.take(take))))
                    .map_err(internal);
            }
            ParsedRange::Unsatisfiable => {
                return Response::builder()
                    .status(StatusCode::RANGE_NOT_SATISFIABLE)
                    .header(header::CONTENT_TYPE, "application/json")
                    .header(header::CONTENT_RANGE, format!("bytes */{len}"))
                    .body(Body::from(
                        json!({"error": "requested range not satisfiable"}).to_string(),
                    ))
                    .map_err(internal);
            }
            // RFC 9110 §14.2: a `Range` the server does not understand, or does
            // not support, is *ignored* — so this falls through to the full
            // body rather than answering 416 for a request we could serve.
            ParsedRange::Ignored => {}
        }
    }

    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::ETAG, &etag)
        .header(header::CONTENT_LENGTH, len);
    if let Some(value) = &last_modified {
        builder = builder.header(header::LAST_MODIFIED, value);
    }
    if let Some(name) = disposition {
        builder = builder.header(header::CONTENT_DISPOSITION, attachment_disposition(name));
    }
    builder
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(internal)
}

pub async fn serve_file_response(
    path: &str,
    roots: &MediaRoots,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let opened = OpenedMedia::open(&full)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let (mime, disposition) = inline_media_headers(&full);
    file_response(opened, &mime, disposition.as_deref(), headers, true).await
}

#[derive(Debug, PartialEq, Eq)]
enum ParsedRange {
    /// Inclusive (start, end) byte range inside the body.
    Satisfiable(u64, u64),
    /// A well-formed `bytes=` range this representation cannot satisfy:
    /// out of bounds, or a suffix of zero. Answered `416` with
    /// `Content-Range: bytes */len` — the only case RFC 9110 §14.4 reserves
    /// that status for.
    Unsatisfiable,
    /// A `Range` the server does not understand or does not support: another
    /// unit, a malformed field, or a multi-part set. RFC 9110 §14.2 requires
    /// these to be *ignored*, so the request is answered with the full body.
    /// A client that asks for something we cannot parse is still asking for the
    /// file, and 416 would deny it the bytes it could have had.
    Ignored,
}

fn parse_bytes_range(header: &str, len: u64) -> ParsedRange {
    let header = header.trim();
    // An empty field, or one naming a unit we do not implement, is ignored.
    let Some(rest) = header.strip_prefix("bytes=") else {
        return ParsedRange::Ignored;
    };
    // Multi-range responses are not supported. A set of ranges is still a
    // request this server can answer in full, so it is ignored rather than
    // refused.
    if rest.contains(',') {
        return ParsedRange::Ignored;
    }
    let Some((first, last)) = rest.split_once('-') else {
        return ParsedRange::Ignored;
    };
    if first.is_empty() {
        // Suffix form `bytes=-N`: the final N bytes. A suffix of zero is a
        // well-formed request for nothing, so it is unsatisfiable rather than
        // malformed.
        let Ok(suffix) = last.parse::<u64>() else {
            return ParsedRange::Ignored;
        };
        if suffix == 0 || len == 0 {
            return ParsedRange::Unsatisfiable;
        }
        return ParsedRange::Satisfiable(len.saturating_sub(suffix), len - 1);
    }
    let Ok(start) = first.parse::<u64>() else {
        return ParsedRange::Ignored;
    };
    let end = if last.is_empty() {
        len.saturating_sub(1)
    } else {
        let Ok(end) = last.parse::<u64>() else {
            return ParsedRange::Ignored;
        };
        end
    };
    // The bounds check comes first: an open-ended range past the end of the
    // file has `last` empty, so `end` is already clamped to the final byte and
    // the reversal test below would misread it as malformed.
    if len == 0 || start >= len {
        return ParsedRange::Unsatisfiable;
    }
    // A reversed pair is malformed, not unsatisfiable.
    if start > end {
        return ParsedRange::Ignored;
    }
    ParsedRange::Satisfiable(start, end.min(len - 1))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, Value) {
    // Details (including server paths) stay in the process log; the response
    // body is generic so absolute paths never leak to clients.
    log_error!("media request failed: {e}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": "internal server error"}),
    )
}

pub async fn serve_text(path: &str, roots: &MediaRoots) -> Result<Value, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let name = full
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();
    if media_type_for_file(&name) != Some("text") {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "not a text media file"}),
        ));
    }
    let size = tokio::fs::metadata(&full).await.map_err(internal)?.len();
    let mut body = Vec::with_capacity(size.min(TEXT_PREVIEW_MAX_BYTES + 1) as usize);
    File::open(&full)
        .await
        .map_err(internal)?
        .take(TEXT_PREVIEW_MAX_BYTES + 1)
        .read_to_end(&mut body)
        .await
        .map_err(internal)?;
    let truncated = body.len() as u64 > TEXT_PREVIEW_MAX_BYTES;
    body.truncate(TEXT_PREVIEW_MAX_BYTES as usize);
    Ok(json!({
        "content": String::from_utf8_lossy(&body),
        "truncated": truncated,
        "size": size,
    }))
}

fn path_variants(path: &str, full: &Path) -> Vec<String> {
    let mut out = Vec::new();
    for candidate in [
        path.to_string(),
        full.to_string_lossy().to_string(),
        normalize_slashes(path),
        normalize_slashes(&full.to_string_lossy()),
    ] {
        if !candidate.is_empty() && !out.iter().any(|v| v == &candidate) {
            out.push(candidate);
        }
    }
    out
}

fn lookup_active_item_id(conn: &rusqlite::Connection, variants: &[String]) -> Result<Option<i64>> {
    if variants.is_empty() {
        return Ok(None);
    }
    let placeholders = variants.iter().map(|_| "?").collect::<Vec<_>>().join(",");
    let sql =
        format!("SELECT id FROM items WHERE missing=0 AND file_path IN ({placeholders}) LIMIT 1");
    let mut stmt = conn.prepare(&sql)?;
    let params: Vec<&dyn rusqlite::types::ToSql> = variants
        .iter()
        .map(|v| v as &dyn rusqlite::types::ToSql)
        .collect();
    match stmt.query_row(params.as_slice(), |r| r.get::<_, i64>(0)) {
        Ok(id) => Ok(Some(id)),
        Err(rusqlite::Error::QueryReturnedNoRows) => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// True when a path segment names an fnOS volume root (`vol1`, `volume2`, ...).
pub(crate) fn is_fnos_volume_segment(part: &str) -> bool {
    let lower = part.to_ascii_lowercase();
    (lower.starts_with("vol") || lower.starts_with("volume"))
        && lower
            .chars()
            .skip_while(|c| c.is_ascii_alphabetic())
            .all(|c| c.is_ascii_digit())
        && lower.chars().any(|c| c.is_ascii_digit())
}

/// True when a directory is a `.@#local/trash` namespace in any space shape.
///
/// The reserved system trash is `<space>/.@#local/trash`; a plain move into it
/// would bypass trashbind and leave an entry the OS trash index does not know
/// about, so such a directory is never used as an app-recycle destination.
pub(crate) fn is_fnos_trash_namespace(root: &Path) -> bool {
    normalize_slashes(&root.to_string_lossy()).ends_with("/.@#local/trash")
}

/// fnOS trash root + relative path for a path inside a space whose trash layout
/// is verified: `/volN/<uid>/...` and `/volN/@team/<name>/...`.
///
/// The reserved trash directories measured on fnOS 6.18 are exactly
/// `<volume>/.@#local/trash`, `<volume>/<uid>/.@#local/trash` and
/// `<volume>/@team/<name>/.@#local/trash`. A media root may also sit in a space
/// of another shape (a share such as `/volN/photos`), where the kernel's
/// destination is unverified: predicting a directory the kernel does not use
/// would delete the file and then refuse to record where it went. Those paths
/// return `None` and fall back to the app-managed recycle instead.
pub(crate) fn fnos_recycle_target(full: &Path) -> Option<(PathBuf, PathBuf)> {
    let logical = normalize_slashes(&full.to_string_lossy());
    let parts: Vec<&str> = logical.split('/').filter(|p| !p.is_empty()).collect();
    for (idx, part) in parts.iter().enumerate() {
        if !is_fnos_volume_segment(part) {
            continue;
        }
        // The first volume-like segment decides the space; an unrecognized
        // shape is not a "try again deeper" case.
        let space_len = if parts.get(idx + 1).copied() == Some("@team")
            && parts.get(idx + 2).is_some_and(|name| !name.is_empty())
        {
            2
        } else if parts
            .get(idx + 1)
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
        {
            1
        } else {
            return None;
        };
        if idx + space_len >= parts.len() {
            return None;
        }
        let mut trash = PathBuf::from("/");
        for p in &parts[..=idx + space_len] {
            trash.push(p);
        }
        trash.push(".@#local");
        trash.push("trash");
        let rel: PathBuf = parts[idx + space_len + 1..].iter().collect();
        return Some((trash, rel));
    }
    None
}

/// Candidate recycle directories and relative paths on the same volume/space as `full`.
/// Prioritizes same-volume targets so file moves are fast O(1) links/renames without crossing disks:
/// 1. fnOS volume and user system trash folders (.@#local/trash, .Recycle_bin, .recycle)
/// 2. Media root same-volume stores (#recycle, .Recycle_bin, .recycle, .gallery_recycle)
/// 3. Same-volume DATA_DIR/recycle if DATA_DIR shares the device/volume
pub(crate) fn candidate_recycle_targets(
    full: &Path,
    roots: Option<&MediaRoots>,
) -> Vec<(PathBuf, PathBuf)> {
    let mut candidates = Vec::new();
    let logical = normalize_slashes(&full.to_string_lossy());
    let parts: Vec<&str> = logical.split('/').filter(|p| !p.is_empty()).collect();

    // 1. Volume-based trash candidates
    for (idx, part) in parts.iter().enumerate() {
        if !is_fnos_volume_segment(part) {
            continue;
        }

        let vol_root = {
            let mut p = PathBuf::from("/");
            for segment in &parts[..=idx] {
                p.push(segment);
            }
            p
        };

        if idx + 1 < parts.len() {
            let mut user_space = PathBuf::from("/");
            for segment in &parts[..=idx + 1] {
                user_space.push(segment);
            }
            let rel_from_user: PathBuf = parts[idx + 2..].iter().collect();

            candidates.push((
                user_space.join(".@#local").join("trash"),
                rel_from_user.clone(),
            ));
            candidates.push((user_space.join(".Recycle_bin"), rel_from_user.clone()));
            candidates.push((user_space.join(".recycle"), rel_from_user.clone()));

            if idx + 2 < parts.len() {
                let mut user_home = PathBuf::from("/");
                for segment in &parts[..=idx + 2] {
                    user_home.push(segment);
                }
                let rel_from_home: PathBuf = parts[idx + 3..].iter().collect();

                candidates.push((
                    user_home.join(".@#local").join("trash"),
                    rel_from_home.clone(),
                ));
                candidates.push((user_home.join(".Recycle_bin"), rel_from_home.clone()));
                candidates.push((user_home.join(".recycle"), rel_from_home.clone()));
                candidates.push((user_home.join(".gallery_recycle"), rel_from_home));
            }

            candidates.push((user_space.join(".gallery_recycle"), rel_from_user));
        }

        let rel_from_vol: PathBuf = parts[idx + 1..].iter().collect();
        candidates.push((
            vol_root.join(".@#local").join("trash"),
            rel_from_vol.clone(),
        ));
        candidates.push((vol_root.join(".Recycle_bin"), rel_from_vol.clone()));
        candidates.push((vol_root.join(".gallery_recycle"), rel_from_vol));
        break;
    }

    // 2. Media root same-volume locations
    if let Some(roots) = roots {
        let canonical_full = safe_canonicalize(full)
            .ok()
            .unwrap_or_else(|| full.to_path_buf());
        for root_str in roots.allowed_roots() {
            let root_path = PathBuf::from(&root_str);
            let canonical_root = safe_canonicalize(&root_path)
                .ok()
                .unwrap_or_else(|| root_path.clone());
            let matched_root = if canonical_full.starts_with(&canonical_root) {
                let rel = canonical_full
                    .strip_prefix(&canonical_root)
                    .ok()
                    .map(|p| p.to_path_buf());
                Some((canonical_root, rel))
            } else if full.starts_with(&root_path) {
                let rel = full.strip_prefix(&root_path).ok().map(|p| p.to_path_buf());
                Some((root_path, rel))
            } else {
                None
            };

            if let Some((root, Some(rel))) = matched_root {
                candidates.push((root.join(".Recycle_bin"), rel.clone()));
                candidates.push((root.join("#recycle"), rel.clone()));
                candidates.push((root.join(".recycle"), rel.clone()));
                candidates.push((root.join(".gallery_recycle"), rel.clone()));

                if let Some(parent) = root.parent() {
                    if let Some(root_name) = root.file_name() {
                        let parent_rel = PathBuf::from(root_name).join(&rel);
                        candidates.push((parent.join(".gallery_recycle"), parent_rel));
                    }
                }
                break;
            }
        }
    }

    // 3. Same-volume check with DATA_DIR
    let data_recycle = gallery_recycle_dir();
    let same_volume_as_data = {
        let data_dir = data_recycle.parent().unwrap_or(&data_recycle);
        #[cfg(unix)]
        {
            use std::os::unix::fs::MetadataExt;
            match (std::fs::metadata(full), std::fs::metadata(data_dir)) {
                (Ok(fm), Ok(dm)) => fm.dev() == dm.dev(),
                _ => false,
            }
        }
        #[cfg(not(unix))]
        {
            full.components().next() == data_dir.components().next()
        }
    };

    if same_volume_as_data {
        let rel_for_data: PathBuf = if let Some((_, rel)) = fnos_recycle_target(full) {
            rel
        } else {
            parts.iter().collect()
        };
        candidates.push((data_recycle, rel_for_data));
    }

    let mut deduped = Vec::new();
    for (root, rel) in candidates {
        if !deduped.iter().any(|(r, _)| r == &root) {
            deduped.push((root, rel));
        }
    }
    deduped
}

pub(crate) fn gallery_recycle_dir() -> PathBuf {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    PathBuf::from(data_dir).join("recycle")
}

/// `uuid::Uuid::new_v4().simple()` renders as 32 lowercase hex digits. Both the
/// collision suffix in [`move_file_no_overwrite`] and the flat gallery recycle
/// prefix are built from it, so recognising the token lets a recovery scan tell
/// a file this code stored from an unrelated file that merely shares a name.
fn is_simple_uuid(token: &str) -> bool {
    token.len() == 32
        && token
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// True when `candidate` is a file name [`move_file_no_overwrite`] could have
/// produced for an original named `base`. Four shapes are reachable: the name
/// unchanged, the same name with a `__<uuid>` collision suffix inserted before
/// the extension, and each of those behind the flat fallback's `<uuid>_`
/// prefix. Anything else is a different file.
pub(crate) fn recycle_name_matches_base(base: &str, candidate: &str) -> bool {
    if candidate == base {
        return true;
    }
    let name = match candidate.split_once('_') {
        Some((prefix, tail)) if is_simple_uuid(prefix) => tail,
        _ => candidate,
    };
    if name == base {
        return true;
    }
    let (stem, ext) = match base.rsplit_once('.') {
        Some((stem, ext)) => (stem, ext),
        None => (base, ""),
    };
    let Some(token) = name.strip_prefix(&format!("{stem}__")) else {
        return false;
    };
    if ext.is_empty() {
        is_simple_uuid(token)
    } else {
        token
            .strip_suffix(&format!(".{ext}"))
            .is_some_and(is_simple_uuid)
    }
}

/// Persist the recycle destination as soon as the move returns.
///
/// The delete records `'moving'` before it touches the filesystem and only
/// names the destination in the finalizing transaction, so a process that dies
/// in between leaves a row that cannot say where the bytes went. Writing the
/// destination here closes that window to a single statement. Callers treat a
/// failure as best effort: the finalizing update rewrites the same value, and
/// an early-write failure must not abandon a delete that can still complete.
pub(crate) fn record_recycled_path(
    conn: &rusqlite::Connection,
    id: i64,
    recycled: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE recycle_entries SET recycled_path=? WHERE id=? AND status='moving'",
        rusqlite::params![recycled, id],
    )?;
    Ok(())
}

/// Move `src` to `dest` without overwriting, adding a UUID suffix on collision.
pub(crate) fn move_file_no_overwrite(src: &Path, dest: &Path) -> Result<(PathBuf, Option<String>)> {
    let final_dest = if dest.exists() {
        let stem = dest
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let ext = dest
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let parent = dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        parent.join(format!("{stem}__{}{ext}", uuid::Uuid::new_v4().simple()))
    } else {
        dest.to_path_buf()
    };
    match move_file_exact_impl(src, &final_dest, false) {
        Ok(warning) => Ok((final_dest, warning)),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            let stem = dest
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());
            let ext = dest
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            let parent = dest.parent().unwrap_or_else(|| Path::new("."));
            let retry = parent.join(format!("{stem}__{}{ext}", uuid::Uuid::new_v4().simple()));
            move_file_exact_impl(src, &retry, false)
                .map(|warning| (retry, warning))
                .map_err(anyhow::Error::from)
        }
        Err(error) => Err(error.into()),
    }
}

fn path_is_within_existing_root(path: &Path, root: &Path) -> bool {
    safe_canonicalize(path)
        .ok()
        .zip(safe_canonicalize(root).ok())
        .is_some_and(|(path, root)| path == root || path.starts_with(root))
}

pub(crate) fn recycle_source_is_trusted(
    recycled: &Path,
    original: &Path,
    roots: Option<&MediaRoots>,
) -> bool {
    if path_is_within_existing_root(recycled, &gallery_recycle_dir()) {
        return true;
    }
    for (trash, _) in candidate_recycle_targets(original, roots) {
        if path_is_within_existing_root(recycled, &trash) {
            return true;
        }
    }
    if let Some(roots) = roots {
        for root_str in roots.allowed_roots() {
            let root = PathBuf::from(root_str);
            for folder in [".gallery_recycle", ".recycle", "#recycle", ".Recycle_bin"] {
                if path_is_within_existing_root(recycled, &root.join(folder)) {
                    return true;
                }
            }
        }
    }
    if let (Some(rec_str), Some(orig_str)) = (recycled.to_str(), original.to_str()) {
        let rec_norm = normalize_slashes(rec_str);
        let orig_norm = normalize_slashes(orig_str);
        for marker in [
            "/.gallery_recycle/",
            "/.recycle/",
            "/.Recycle_bin/",
            "/#recycle/",
        ] {
            if let Some((prefix, _)) = rec_norm.split_once(marker) {
                if orig_norm.starts_with(prefix) {
                    return true;
                }
            }
        }
    }
    false
}

pub(crate) fn move_file_exact_no_overwrite(src: &Path, dest: &Path) -> std::io::Result<()> {
    move_file_exact_impl(src, dest, false).map(|warning| {
        if let Some(warning) = warning {
            log_error!("media: move completed with source cleanup warning: {warning}");
        }
    })
}

/// Move into an authorized path without re-resolving a potentially replaced
/// parent directory. Recycle restore uses this for its media-root destination.
pub(crate) fn move_file_to_authorized_path_no_overwrite(
    src: &Path,
    dest: &Path,
    roots: &MediaRoots,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let dest = authorized_file_path(dest, roots, true)?;
        return move_file_exact_no_overwrite(src, &dest.path);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !path_under_authorized_roots(dest, roots) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        move_file_exact_no_overwrite(src, dest)
    }
}

/// Read the source through an authorized directory descriptor before returning
/// a failed restore to recycle storage.
pub(crate) fn move_file_from_authorized_path_no_overwrite(
    src: &Path,
    dest: &Path,
    roots: &MediaRoots,
) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        let src = authorized_file_path(src, roots, false)?;
        return move_file_exact_no_overwrite(&src.path, dest);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !path_under_authorized_roots(src, roots) {
            return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
        }
        move_file_exact_no_overwrite(src, dest)
    }
}

#[cfg(target_os = "linux")]
struct AuthorizedFilePath {
    _parent: std::os::fd::OwnedFd,
    path: PathBuf,
}

#[cfg(target_os = "linux")]
fn authorized_file_path(
    path: &Path,
    roots: &MediaRoots,
    create_parent: bool,
) -> std::io::Result<AuthorizedFilePath> {
    use std::ffi::CString;
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    const O_PATH: c_int = 0o10000000;
    let root = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|root| PathBuf::from(root).canonicalize().ok())
        .filter(|root| path.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let relative = path
        .strip_prefix(&root)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let file_name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let slash = CString::new("/").unwrap();
    let root_fd = unsafe {
        open(
            slash.as_ptr(),
            O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
        )
    };
    if root_fd < 0 {
        return Err(std::io::Error::last_os_error());
    }
    let mut current = unsafe { OwnedFd::from_raw_fd(root_fd) };
    for component in root.strip_prefix("/").unwrap_or(&root).components().chain(
        relative
            .parent()
            .unwrap_or_else(|| Path::new(""))
            .components(),
    ) {
        let std::path::Component::Normal(component) = component else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        };
        let name = CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let next = unsafe {
            openat(
                current.as_raw_fd(),
                name.as_ptr(),
                O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
            )
        };
        let next = if next >= 0 {
            next
        } else if create_parent
            && std::io::Error::last_os_error().kind() == std::io::ErrorKind::NotFound
        {
            if unsafe { mkdirat(current.as_raw_fd(), name.as_ptr(), 0o755) } != 0
                && std::io::Error::last_os_error().kind() != std::io::ErrorKind::AlreadyExists
            {
                return Err(std::io::Error::last_os_error());
            }
            unsafe {
                openat(
                    current.as_raw_fd(),
                    name.as_ptr(),
                    O_PATH | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
                )
            }
        } else {
            return Err(std::io::Error::last_os_error());
        };
        if next < 0 {
            return Err(std::io::Error::last_os_error());
        }
        current = unsafe { OwnedFd::from_raw_fd(next) };
    }
    let fd_path = PathBuf::from("/proc/self/fd")
        .join(current.as_raw_fd().to_string())
        .join(file_name);
    Ok(AuthorizedFilePath {
        _parent: current,
        path: fd_path,
    })
}

/// `force_copy` skips the hard-link attempt so tests can exercise the copy
/// fallback deterministically on filesystems where links always succeed.
/// Move `src` to `dest` exactly once. `Ok(Some(warning))` means the move
/// succeeded but source-side cleanup failed (best-effort retire); callers
/// surface the warning rather than claiming complete success.
fn move_file_exact_impl(
    src: &Path,
    dest: &Path,
    force_copy: bool,
) -> std::io::Result<Option<String>> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source_file = open_source_file(src)?;
    let source_meta = source_file.metadata()?;
    let mut copy_reason: Option<String> = None;
    if !force_copy {
        match std::fs::hard_link(src, dest) {
            Ok(()) => {
                let created_identity = std::fs::metadata(dest)
                    .ok()
                    .and_then(|metadata| file_identity(&metadata));
                if !same_path_identity(src, &source_meta) {
                    let _ = remove_created_file(dest, created_identity);
                    return Err(std::io::Error::other("source changed during move"));
                }
                let cleanup_warning = match retire_source_if_unchanged(src, &source_meta) {
                    Ok(warning) => warning,
                    Err(error) => {
                        let _ = remove_created_file(dest, created_identity);
                        return Err(error);
                    }
                };
                fsync_parent_dir_best_effort(dest);
                fsync_parent_dir_best_effort(src);
                return Ok(cleanup_warning);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(error),
            Err(error) => {
                // A link fails across filesystems (EXDEV) and on mounts that
                // refuse links. Copying is still the right fallback, but it is
                // not equivalent: until the source retires, the bytes exist
                // twice, so an interruption in that window leaves two copies
                // for the caller to choose between. Report why the move
                // degraded rather than hiding it behind a silent success.
                copy_reason = Some(format!("hard link unavailable ({error}); copied instead"));
            }
        }
    }

    let copy_result = (|| -> std::io::Result<Option<(u64, u64)>> {
        let mut from = source_file.try_clone()?;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(dest)?;
        std::io::copy(&mut from, &mut to)?;
        to.sync_all()?;
        if from.metadata()?.len() != to.metadata()?.len() {
            return Err(std::io::Error::other("copied file size mismatch"));
        }
        Ok(std::fs::metadata(dest)
            .ok()
            .and_then(|metadata| file_identity(&metadata)))
    })();
    let created_identity = match copy_result {
        Ok(identity) => identity,
        Err(error) => {
            let _ = remove_created_file(dest, None);
            return Err(error);
        }
    };
    if !same_path_identity(src, &source_meta) {
        let _ = remove_created_file(dest, created_identity);
        return Err(std::io::Error::other("source changed during move"));
    }
    let cleanup_warning = match retire_source_if_unchanged(src, &source_meta) {
        Ok(warning) => warning,
        Err(error) => {
            let _ = remove_created_file(dest, created_identity);
            return Err(error);
        }
    };
    fsync_parent_dir_best_effort(dest);
    fsync_parent_dir_best_effort(src);
    Ok(merge_move_warnings(copy_reason, cleanup_warning))
}

/// Fold the reason a move had to copy into the warning it returns, so a
/// degraded (cross-volume) move is reported instead of looking like a clean
/// link-based one. Either input alone is still a warning worth surfacing.
fn merge_move_warnings(
    copy_reason: Option<String>,
    cleanup_warning: Option<String>,
) -> Option<String> {
    match (copy_reason, cleanup_warning) {
        (Some(reason), Some(warning)) => Some(format!("{reason}; {warning}")),
        (Some(reason), None) => Some(reason),
        (None, warning) => warning,
    }
}

#[cfg(unix)]
fn file_identity(metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    use std::os::unix::fs::MetadataExt;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_metadata: &std::fs::Metadata) -> Option<(u64, u64)> {
    None
}

fn same_path_identity(path: &Path, expected: &std::fs::Metadata) -> bool {
    let Ok(actual) = std::fs::metadata(path) else {
        return false;
    };
    match (file_identity(expected), file_identity(&actual)) {
        (Some(expected), Some(actual)) => expected == actual,
        _ => actual.len() == expected.len() && actual.modified().ok() == expected.modified().ok(),
    }
}

#[cfg(target_os = "linux")]
fn open_source_file(path: &Path) -> std::io::Result<std::fs::File> {
    use std::ffi::CString;
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn open(pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_RDONLY: c_int = 0;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_NOFOLLOW: c_int = 0o400000;
    let path = CString::new(path.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let fd = unsafe { open(path.as_ptr(), O_RDONLY | O_CLOEXEC | O_NOFOLLOW) };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::fs::File::from_raw_fd(fd) })
    }
}

#[cfg(not(target_os = "linux"))]
fn open_source_file(path: &Path) -> std::io::Result<std::fs::File> {
    std::fs::File::open(path)
}

/// Retire exactly the file version that was opened before publication. Linux
/// cannot unlink by inode, so exchange the pathname with an operation-owned
/// placeholder first. If another process replaced the source, the exchange is
/// reversed before returning an error and the replacement remains in place.
///
/// `Ok(Some(warning))` reports a best-effort cleanup failure after the retire
/// itself succeeded (e.g. ZFS trimacl leaving a 0-byte placeholder behind);
/// callers must surface it instead of reporting complete success.
fn retire_source_if_unchanged(
    src: &Path,
    expected: &std::fs::Metadata,
) -> std::io::Result<Option<String>> {
    #[cfg(target_os = "linux")]
    {
        return retire_source_if_unchanged_linux(src, expected);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !same_path_identity(src, expected) {
            return Err(std::io::Error::other("source changed during move"));
        }
        std::fs::remove_file(src).map(|()| None)
    }
}

#[cfg(target_os = "linux")]
fn retire_source_if_unchanged_linux(
    src: &Path,
    expected: &std::fs::Metadata,
) -> std::io::Result<Option<String>> {
    use std::ffi::CString;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    const AT_FDCWD: c_int = -100;
    const RENAME_EXCHANGE: c_uint = 2;
    let parent = src
        .parent()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let placeholder = parent.join(format!(".gallery-unlink-{}", uuid::Uuid::new_v4().simple()));
    let placeholder_meta = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&placeholder)?
        .metadata()?;
    let placeholder_identity = file_identity(&placeholder_meta);
    let source = CString::new(src.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let temporary = CString::new(placeholder.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let exchange = || {
        if unsafe {
            renameat2(
                AT_FDCWD,
                source.as_ptr(),
                AT_FDCWD,
                temporary.as_ptr(),
                RENAME_EXCHANGE,
            )
        } == 0
        {
            Ok(())
        } else {
            Err(std::io::Error::last_os_error())
        }
    };

    if let Err(error) = exchange() {
        let _ = remove_created_file(&placeholder, placeholder_identity);
        return Err(error);
    }
    if !same_path_identity(&placeholder, expected) {
        let rollback = exchange();
        let _ = remove_created_file(&placeholder, placeholder_identity);
        return match rollback {
            Ok(()) => Err(std::io::Error::other("source changed during move")),
            Err(rollback_error) => Err(std::io::Error::other(format!(
                "source changed during move; source exchange rollback failed: {rollback_error}"
            ))),
        };
    }

    // The source name now refers only to our placeholder. Best-effort cleanup
    // deliberately checks identities again, preserving any concurrent rewrite.
    // A failure leaves the retired content or a 0-byte placeholder behind:
    // report it instead of letting the operation claim complete success.
    let mut cleanup_warnings: Vec<String> = Vec::new();
    if let Err(error) = remove_created_file(&placeholder, file_identity(expected)) {
        cleanup_warnings.push(format!("retired file cleanup failed: {error}"));
    }
    if let Err(error) = remove_created_file(src, placeholder_identity) {
        cleanup_warnings.push(format!("source placeholder cleanup failed: {error}"));
    }
    if cleanup_warnings.is_empty() {
        Ok(None)
    } else {
        Ok(Some(cleanup_warnings.join("; ")))
    }
}

fn remove_created_file(path: &Path, expected: Option<(u64, u64)>) -> std::io::Result<()> {
    if let Some(expected) = expected {
        let actual = std::fs::metadata(path)
            .ok()
            .and_then(|metadata| file_identity(&metadata));
        if actual != Some(expected) {
            return Ok(());
        }
    }
    std::fs::remove_file(path)
}

/// Best-effort fsync of the directory containing `path` so a just-completed
/// publish (link, create, rename, unlink) survives power loss. File data is
/// already fsynced by the move paths and SQLite by its own commit; the
/// directory entry is the remaining durability gap. Fails open because some
/// filesystems refuse directory fsync after the move already succeeded.
pub(crate) fn fsync_parent_dir_best_effort(path: &Path) {
    if let Some(parent) = path.parent() {
        if let Ok(dir) = std::fs::File::open(parent) {
            let _ = dir.sync_all();
        }
    }
}

/// True when `path` lies inside fnOS's reserved system trash, which is always
/// `<volume>/.@#local/trash`, `<volume>/<uid>/.@#local/trash`, or
/// `<volume>/@team/<name>/.@#local/trash`.
///
/// Matched by path components anchored to a volume root rather than by
/// substring: a media folder that merely happens to be named `.@#local/trash`
/// deeper in the library must not be treated as the reserved directory. The
/// reserved directory counts only when it sits directly under the volume, a
/// numeric user space, or a team space.
pub(crate) fn is_fnos_system_trash_path(path: &Path) -> bool {
    let logical = normalize_slashes(&path.to_string_lossy());
    let parts: Vec<&str> = logical.split('/').filter(|part| !part.is_empty()).collect();
    let Some(local_idx) = parts.iter().position(|part| *part == ".@#local") else {
        return false;
    };
    if parts.get(local_idx + 1).copied() != Some("trash") {
        return false;
    }
    let Some(vol_idx) = parts[..local_idx]
        .iter()
        .position(|part| is_fnos_volume_segment(part))
    else {
        return false;
    };
    match &parts[vol_idx + 1..local_idx] {
        // <volume>/.@#local/trash
        [] => true,
        // <volume>/<uid>/.@#local/trash
        [user] => !user.is_empty() && user.bytes().all(|b| b.is_ascii_digit()),
        // <volume>/@team/<name>/.@#local/trash
        [team, name] => *team == "@team" && !name.is_empty(),
        _ => false,
    }
}

/// Marker inside the error message of [`try_fnos_system_recycle`] when the
/// magic `unlinkat` consumed the source file but the predicted destination is
/// not there: the bytes are in the system trash at a location this build cannot
/// name. The delete path must keep its record for reconciliation instead of
/// dropping it, which would leave the file untracked while the item row keeps
/// pointing at a path that no longer exists.
pub(crate) const FNOS_TRASH_DESTINATION_UNKNOWN: &str = "fnos-system-trash-destination-unknown";

#[cfg(target_os = "linux")]
pub(crate) fn try_fnos_system_recycle(
    full: &Path,
    trash_root: &Path,
    rel: &Path,
) -> std::io::Result<PathBuf> {
    // 1. Pre-check: fnOS kernel magic device /dev/trim-trashbin MUST exist.
    // POSIX defines that unlinkat ignores dirfd when the path is absolute.
    // On any standard Linux kernel (WSL, vanilla distribution, Docker, unpatched kernel),
    // calling libc::unlinkat(-7126, absolute_path, flags) falls back to a normal POSIX
    // unlink, permanently deleting the file and returning 0!
    // Checking /dev/trim-trashbin ensures we only invoke this on a patched fnOS kernel.
    if !Path::new("/dev/trim-trashbin").exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "/dev/trim-trashbin device node not found; kernel does not support fnOS trim_do_trashbin",
        ));
    }

    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(full.as_os_str().as_bytes())
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    let flags = if full.is_dir() { libc::AT_REMOVEDIR } else { 0 };
    let dest = trash_root.join(rel);
    // 1b. Post-check anchor: fnOS trashbind renames a colliding destination
    // (verified on fnOS 6.18: `pic.jpg` reappears as `pic_1.jpg`), so a name
    // that already exists in the system trash means the kernel would store this
    // file under a name we cannot predict. Refuse *before* unlinking anything so
    // the caller can fall back to the app-managed recycle while the source file
    // still exists — a post-hoc fallback cannot work once the source is gone.
    if dest.exists() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            format!(
                "fnOS system trash already holds '{}'; deferring to app recycle",
                dest.display()
            ),
        ));
    }
    // -7126 (0xffffe42a) is fnOS custom kernel magic dfd for trim_do_trashbin.
    // The kernel diverts unlinkat to /dev/trim-trashbin, where trashbind
    // moves the file into .@#local/trash and attaches user.trim-trashbin-record xattr.
    let ret = unsafe { libc::unlinkat(-7126, c_path.as_ptr(), flags) };
    if ret == 0 {
        fsync_parent_dir_best_effort(full);
        // 2. Post-check: The file MUST now exist at the predicted system trash
        // destination. If the magic unlinkat failed to divert and instead
        // hard-unlinked the file, dest will NOT exist.
        if !dest.exists() {
            return Err(std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!(
                    "{FNOS_TRASH_DESTINATION_UNKNOWN}: unlinkat(-7126) returned 0 but target '{}' does not exist",
                    dest.display()
                ),
            ));
        }
        Ok(dest)
    } else {
        Err(std::io::Error::last_os_error())
    }
}

pub(crate) fn move_into_recycle(
    full: &Path,
    roots: Option<&MediaRoots>,
    prefer_system: bool,
) -> Result<(PathBuf, Option<String>)> {
    let _ = prefer_system;
    let base = full
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());

    // 0. On fnOS Linux: if prefer_system is enabled, try kernel trim_do_trashbin via magic dfd -7126 directly into system recycle bin.
    #[cfg(target_os = "linux")]
    if prefer_system {
        if let Some((trash_root, rel)) = fnos_recycle_target(full) {
            if !rel.as_os_str().is_empty() {
                match try_fnos_system_recycle(full, &trash_root, &rel) {
                    Ok(dest) => return Ok((dest, None)),
                    Err(err) => {
                        log_info!(
                            "media: fnOS system recycle via magic dfd failed for {}: {err}; falling back to standard recycle",
                            full.display()
                        );
                    }
                }
            }
        }
    }

    let candidates = candidate_recycle_targets(full, roots);
    for (trash_root, rel) in &candidates {
        // Reaching this loop means the kernel system-recycle path did not take
        // the file (unsupported kernel, a trash-name collision refused above, or
        // a space shape this build does not predict). A plain move into a
        // `.@#local/trash` directory would bypass trashbind and leave an entry
        // the OS trash index does not know about, so every `.@#local/trash`
        // candidate is skipped — not only the ones whose space shape is
        // recognized — and the file falls through to the app-managed recycle.
        if is_fnos_trash_namespace(trash_root) {
            continue;
        }
        if !rel.as_os_str().is_empty() {
            let nested = trash_root.join(rel);
            if let Ok((dest, warning)) = move_file_no_overwrite(full, &nested) {
                return Ok((dest, warning));
            }
        }
        let flat = trash_root.join(&base);
        if let Ok((dest, warning)) = move_file_no_overwrite(full, &flat) {
            return Ok((dest, warning));
        }
    }

    // Gallery-owned DATA_DIR/recycle fallback (global fallback)
    let recycle = gallery_recycle_dir();
    std::fs::create_dir_all(&recycle)?;
    if let Some((_, rel)) = fnos_recycle_target(full) {
        if !rel.as_os_str().is_empty() {
            let nested = recycle.join(&rel);
            if let Ok((dest, warning)) = move_file_no_overwrite(full, &nested) {
                return Ok((dest, warning));
            }
        }
    }
    let flat = recycle.join(format!("{}_{base}", uuid::Uuid::new_v4().simple()));
    move_file_no_overwrite(full, &flat)
}


pub fn delete_item_to_recycle(
    conn: &rusqlite::Connection,
    path: &str,
    roots: &MediaRoots,
) -> Result<Value, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    if !full.is_file() {
        return Err((StatusCode::NOT_FOUND, json!({"error": "file not found"})));
    }
    let variants = path_variants(path, &full);
    let item_id = match lookup_active_item_id(conn, &variants) {
        Ok(Some(id)) => id,
        Ok(None) => {
            return Err((
                StatusCode::NOT_FOUND,
                json!({"error": "file is not an active library item"}),
            ))
        }
        Err(e) => return Err(internal(e)),
    };

    let original = full.clone();
    ensure_recycle_schema(conn).map_err(internal)?;
    let (mut item_snapshot, tag_ids, favorite) =
        capture_item_snapshot(conn, item_id).map_err(internal)?;
    let (tag_single_refs, non_tag_single_ref_ids) =
        capture_character_ref_snapshot(conn, item_id).map_err(internal)?;
    if let Some(object) = item_snapshot.as_object_mut() {
        object.insert("favorite".to_string(), Value::Bool(favorite));
    }
    let original_s = original.display().to_string();
    let artist_id: i64 = conn
        .query_row("SELECT artist_id FROM items WHERE id=?", [item_id], |row| {
            row.get(0)
        })
        .map_err(internal)?;

    // Phase 1: record the delete intent before touching the filesystem. A
    // crash between the file move and the DB commit then leaves a recoverable
    // 'moving' marker (reconciled at next startup) instead of an untracked
    // file in trash that fnOS retention could purge silently.
    conn.execute(
        "INSERT INTO recycle_entries
         (original_item_id, artist_id, original_path, recycled_path, item_snapshot,
          tag_ids_snapshot, tag_single_refs_snapshot, non_tag_single_ref_ids, status)
         VALUES (?,?,?,?,?,?,?,?, 'moving')",
        rusqlite::params![
            item_id,
            artist_id,
            original_s,
            "",
            serde_json::to_string(&item_snapshot).map_err(internal)?,
            serde_json::to_string(&tag_ids).map_err(internal)?,
            &tag_single_refs,
            &non_tag_single_ref_ids,
        ],
    )
    .map_err(internal)?;
    let moving_id = conn.last_insert_rowid();

    // Phase 2: move the file into recycle storage (dir fsyncs inside). A
    // source-side cleanup warning must reach the result: it means the move
    // succeeded but a placeholder may linger at the source path.
    let recycle_target = crate::recycle::get_recycle_target_setting(conn);
    let prefer_system = recycle_target != "app";
    let (recycled, move_cleanup_warning) = match move_into_recycle(
        &full,
        Some(roots),
        prefer_system,
    ) {
        Ok((recycled, warning)) => (recycled, warning),
        Err(error) => {
            // The kernel may have consumed the file without leaving it where
            // the measured layout predicts. Dropping the 'moving' row here
            // would leave the item active in the library while the bytes sit
            // in the system trash with no record of where they went, so that
            // one case keeps the row and lets reconciliation settle it.
            if error
                .to_string()
                .contains(crate::media_serve::FNOS_TRASH_DESTINATION_UNKNOWN)
            {
                log_error!(
                        "media: delete of {original_s} left the fnOS system trash destination unknown: {error}"
                    );
                crate::recycle::reconcile_moving_recycle_entries(conn);
                return Err(internal(anyhow!(
                    "file was moved into the fnOS system trash at an unknown destination; \
                         the recycle record was kept for reconciliation"
                )));
            }
            let _ = conn.execute(
                "DELETE FROM recycle_entries WHERE id=? AND status='moving'",
                rusqlite::params![moving_id],
            );
            return Err(internal(error));
        }
    };
    let recycled_s = recycled.display().to_string();

    // Phase 2b: name the destination before anything else can fail. A crash
    // from here on leaves a 'moving' row that points at a real file, so the
    // startup reconciliation finalizes the delete instead of declaring the
    // bytes lost. Best effort — phase 3 rewrites the same value, so a failure
    // here must not abandon a delete that can still complete.
    if let Err(error) = record_recycled_path(conn, moving_id, &recycled_s) {
        log_error!("media: could not record recycle destination for entry {moving_id}: {error}");
    }

    if let Some(warning) = &move_cleanup_warning {
        log_error!("media: delete of {original_s} completed with cleanup warning: {warning}");
    }


    // Phase 3: finalize the record and remove the item row atomically.
    let db_result = (|| -> Result<(i64, i64)> {
        conn.execute("BEGIN IMMEDIATE", [])?;
        let tx = (|| -> Result<(i64, i64)> {
            let changed = conn.execute(
                "UPDATE recycle_entries SET status='recycled', recycled_path=?, last_error=''
                 WHERE id=? AND status='moving'",
                rusqlite::params![recycled_s, moving_id],
            )?;
            if changed != 1 {
                return Err(anyhow!("recycle record changed during delete"));
            }
            let deleted_refs = conn.execute(
                "DELETE FROM character_references WHERE item_id=? AND source_type='tag_single'",
                rusqlite::params![item_id],
            )? as i64;
            let deleted_items =
                conn.execute("DELETE FROM items WHERE id=?", rusqlite::params![item_id])? as i64;
            if deleted_items == 0 {
                return Err(anyhow!("item disappeared during delete"));
            }
            Ok((deleted_refs, deleted_items))
        })();
        match tx {
            Ok(v) => {
                conn.execute("COMMIT", [])?;
                Ok(v)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    })();

    match db_result {
        Ok((deleted_refs, _)) => {
            if deleted_refs > 0 {
                // Best-effort index refresh path (no-op metadata rebuild today).
                let _ = crate::product_ui::rebuild_character_index(conn);
            }
            Ok(json!({
                "ok": true,
                "item_id": item_id,
                "recycled_to": recycled_s,
                "deleted_auto_character_refs": deleted_refs,
                // Null when the source retired cleanly; a string means the
                // move completed but source-side cleanup failed and a
                // placeholder may remain at the original path.
                "cleanup_warning": move_cleanup_warning,
            }))
        }
        Err(db_err) => match move_file_exact_no_overwrite(&recycled, &original) {
            Ok(()) => {
                // File is back at its original path and the item row survived
                // the rolled-back transaction; the moving marker is void.
                let _ = conn.execute(
                    "DELETE FROM recycle_entries WHERE id=? AND status='moving'",
                    rusqlite::params![moving_id],
                );
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({
                        "error": format!("database delete failed; file restored: {db_err}"),
                        "item_id": item_id,
                        "original_path": original_s,
                    }),
                ))
            }
            Err(restore_err) => {
                let error = format!(
                    "database delete failed and restore failed: db={db_err}; restore={restore_err}"
                );
                // The file stays in trash under the phase-1 record: promote it
                // to a visible 'recycled' entry carrying the failure reason.
                let recorded = conn
                    .execute(
                        "UPDATE recycle_entries SET status='recycled', recycled_path=?, last_error=?
                         WHERE id=? AND status='moving'",
                        rusqlite::params![recycled_s, error, moving_id],
                    )
                    .is_ok();
                Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({
                        "error": error,
                        "item_id": item_id,
                        "original_path": original_s,
                        "recycled_to": recycled_s,
                        "needs_reconciliation": true,
                        "reconciliation_recorded": recorded,
                    }),
                ))
            }
        },
    }
}

/// Blocking delete used from the HTTP route's `spawn_blocking` task.
pub fn delete_to_recycle(
    path: &str,
    roots: &MediaRoots,
    conn: &rusqlite::Connection,
) -> Result<Value, (StatusCode, Value)> {
    // Blocking FS + SQLite: caller should already be on spawn_blocking or short path.
    delete_item_to_recycle(conn, path, roots)
}

/// Recycle all items and loose files in an artist's subfolder, then remove the folder.
pub fn delete_folder_to_recycle(
    conn: &rusqlite::Connection,
    roots: &MediaRoots,
    artist_id: i64,
    folder: &str,
) -> Result<Value, (StatusCode, Value)> {
    let norm_folder = folder
        .trim()
        .replace('\\', "/")
        .trim_matches('/')
        .to_string();
    if norm_folder.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "cannot delete artist root folder"}),
        ));
    }
    if norm_folder
        .split('/')
        .any(|part| part == ".." || part == "." || part.is_empty())
    {
        return Err((
            StatusCode::BAD_REQUEST,
            json!({"error": "invalid folder path"}),
        ));
    }

    let artist_path: Option<String> = conn
        .query_row(
            "SELECT path FROM artists WHERE id=?",
            rusqlite::params![artist_id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        })?;

    let Some(artist_path) = artist_path else {
        return Err((StatusCode::NOT_FOUND, json!({"error": "artist not found"})));
    };

    let disk_folder_result =
        crate::scan::resolve_scan_scope(&artist_path, Some(&norm_folder), roots);

    // Look for items in the database under this folder
    let base_virtual = artist_path
        .replace('\\', "/")
        .trim_end_matches('/')
        .to_string();
    let prefix_virtual = format!("{base_virtual}/{norm_folder}/");
    let mut prefixes = vec![prefix_virtual];
    if let Ok(real) = roots.map_to_real(&artist_path) {
        let base_real = real
            .to_string_lossy()
            .replace('\\', "/")
            .trim_end_matches('/')
            .to_string();
        let prefix_real = format!("{base_real}/{norm_folder}/");
        if prefix_real != prefixes[0] {
            prefixes.push(prefix_real);
        }
    }

    // One pass over every item under the folder, partitioned by owner: rows
    // belonging to this artist are deleted as items below; rows owned by a
    // different artist (or by none) must stay on disk. Those files can sit
    // nested here after a manual move or an interrupted artist move, and the
    // loose-file walk's own refusal bookkeeping only knows this artist's rows,
    // so the foreign keys are collected up front.
    let mut item_paths: Vec<(i64, String)> = Vec::new();
    let mut foreign_item_keys: HashSet<String> = HashSet::new();
    for prefix in &prefixes {
        let mut stmt = conn
            .prepare(
                "SELECT id, artist_id, file_path FROM items
                 WHERE substr(replace(file_path,'\\','/'), 1, length(?)) = ? COLLATE BINARY",
            )
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": e.to_string()}),
                )
            })?;
        let rows = stmt
            .query_map(rusqlite::params![prefix, prefix], |r| {
                Ok((
                    r.get::<_, i64>(0)?,
                    r.get::<_, Option<i64>>(1)?,
                    r.get::<_, String>(2)?,
                ))
            })
            .map_err(|e| {
                (
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({"error": e.to_string()}),
                )
            })?;
        for (id, row_artist_id, path) in rows.into_iter().flatten() {
            if row_artist_id == Some(artist_id) {
                if !item_paths.iter().any(|(existing_id, _)| *existing_id == id) {
                    item_paths.push((id, path));
                }
            } else {
                foreign_item_keys.insert(normalize_slashes(&path));
                foreign_item_keys.insert(path_key(&path));
            }
        }
    }

    if disk_folder_result.is_err() && item_paths.is_empty() {
        return Err((StatusCode::NOT_FOUND, json!({"error": "folder not found"})));
    }

    let mut recycled_items: usize = 0;
    let mut recycled_loose_files: usize = 0;
    let mut failed: usize = 0;
    // Paths whose item row went to recycle successfully, in the same normalized
    // form the walk below produces. The walk must not touch them: the per-item
    // delete already committed them, and moving them again would leave a second,
    // untracked copy in recycle storage.
    let mut handled_paths: HashSet<String> = HashSet::new();
    // Non-item files that stay put, so the response and the toast can name them
    // instead of only reporting a count.
    let mut failed_paths: Vec<String> = Vec::new();

    for (_id, file_path) in &item_paths {
        match delete_item_to_recycle(conn, file_path, roots) {
            Ok(_) => {
                recycled_items += 1;
                handled_paths.insert(normalize_slashes(file_path));
                handled_paths.insert(path_key(file_path));
            }
            Err((_status, _err)) => {
                failed += 1;
                failed_paths.push(file_path.clone());
            }
        }
    }

    if let Ok(ref disk_dir) = disk_folder_result {
        if disk_dir.is_dir() {
            for entry in walkdir::WalkDir::new(disk_dir)
                .into_iter()
                .filter_map(|e| e.ok())
            {
                if entry.file_type().is_file() {
                    let loose_path = entry.path();
                    let loose_text = loose_path.to_string_lossy();
                    let loose_key = path_key(&loose_text);
                    // A file this call could not delete as an item — a row
                    // computed as `missing`, or one owned by another artist —
                    // is still on disk here. Moving it would take the bytes out
                    // of the library with the item row left active and no
                    // recycle record to restore from, so it is left where it is
                    // and reported as a failure.
                    let was_refused = failed_paths.iter().any(|path| {
                        normalize_slashes(path) == normalize_slashes(&loose_text)
                            || path_key(path) == loose_key
                    });
                    if handled_paths.contains(&loose_key) || was_refused {
                        continue;
                    }
                    if foreign_item_keys.contains(&normalize_slashes(&loose_text))
                        || foreign_item_keys.contains(&loose_key)
                    {
                        failed += 1;
                        failed_paths.push(loose_text.to_string());
                        continue;
                    }
                    let recycle_target = crate::recycle::get_recycle_target_setting(conn);
                    let prefer_system = recycle_target != "app";
                    match move_into_recycle(loose_path, Some(roots), prefer_system) {
                        Ok((_recycled_path, _warning)) => {
                            recycled_loose_files += 1;
                        }
                        Err(_e) => {
                            failed += 1;
                            failed_paths.push(loose_path.to_string_lossy().to_string());
                        }
                    }
                }
            }
        }
    }

    let folder_removed = if let Ok(ref disk_dir) = disk_folder_result {
        if disk_dir.is_dir() {
            let mut dirs: Vec<PathBuf> = walkdir::WalkDir::new(disk_dir)
                .into_iter()
                .filter_map(|e| e.ok())
                .filter(|e| e.file_type().is_dir())
                .map(|e| e.into_path())
                .collect();
            dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
            for dir in dirs {
                let _ = std::fs::remove_dir(dir);
            }
            !disk_dir.exists()
        } else {
            true
        }
    } else {
        true
    };

    if recycled_items > 0 {
        let _ = crate::product_ui::rebuild_character_index(conn);
    }

    Ok(json!({
        "ok": true,
        "recycled_items": recycled_items,
        "recycled_loose_files": recycled_loose_files,
        "failed": failed,
        "failed_paths": failed_paths,
        "folder_removed": folder_removed,
    }))
}

/// Maximum JPEG payload accepted from a single ffmpeg frame extraction (32 MiB).
const MAX_VIDEO_FRAME_JPEG_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum VideoFrameExtractError {
    SlotBusy(String),
    Unavailable(String),
    StdoutMissing,
    TimedOut,
    OutputTooLarge,
    Io(String),
    Failed(String),
}

impl std::fmt::Display for VideoFrameExtractError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SlotBusy(msg) => f.write_str(msg),
            Self::Unavailable(msg) => write!(f, "ffmpeg unavailable: {msg}"),
            Self::StdoutMissing => f.write_str("ffmpeg stdout missing"),
            Self::TimedOut => f.write_str("ffmpeg frame extraction timed out"),
            Self::OutputTooLarge => f.write_str("ffmpeg frame output exceeded size limit"),
            Self::Io(msg) => f.write_str(msg),
            Self::Failed(msg) => f.write_str(msg),
        }
    }
}

impl std::error::Error for VideoFrameExtractError {}

/// Shared controlled executor for single-frame JPEG extraction via `ffmpeg`.
///
/// Acquires `FfmpegSlotGuard`, enforces `VIDEO_FRAME_EXTRACT_TIMEOUT` and
/// `MAX_VIDEO_FRAME_JPEG_BYTES`, and kills + reaps the `ffmpeg` child process
/// on timeout, read failure, or output overflow so no orphan process lingers.
pub(crate) fn extract_video_frame_jpeg_blocking(
    full_path: &Path,
    t: f64,
) -> std::result::Result<Vec<u8>, VideoFrameExtractError> {
    let _ffmpeg_slot = FfmpegSlotGuard::acquire_blocking()
        .map_err(|e| VideoFrameExtractError::SlotBusy(e.to_string()))?;
    let mut child = std::process::Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{t:.3}"),
            "-i",
            &full_path.to_string_lossy(),
            "-frames:v",
            "1",
            "-f",
            "image2pipe",
            "-vcodec",
            "mjpeg",
            "pipe:1",
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| VideoFrameExtractError::Unavailable(e.to_string()))?;
    let stdout = child
        .stdout
        .take()
        .ok_or(VideoFrameExtractError::StdoutMissing)?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut buf = Vec::new();
        let mut limited = std::io::Read::take(stdout, MAX_VIDEO_FRAME_JPEG_BYTES + 1);
        let read_result = std::io::Read::read_to_end(&mut limited, &mut buf);
        let _ = sender.send((buf, read_result));
    });
    match receiver.recv_timeout(VIDEO_FRAME_EXTRACT_TIMEOUT) {
        Ok((buf, Ok(_))) => {
            if buf.len() as u64 > MAX_VIDEO_FRAME_JPEG_BYTES {
                let _ = child.kill();
                let _ = child.wait();
                return Err(VideoFrameExtractError::OutputTooLarge);
            }
            let status = child
                .wait()
                .map_err(|e| VideoFrameExtractError::Io(format!("wait for ffmpeg: {e}")))?;
            if !status.success() || buf.is_empty() {
                return Err(VideoFrameExtractError::Failed(
                    "ffmpeg failed to extract frame".to_string(),
                ));
            }
            Ok(buf)
        }
        Ok((_, Err(error))) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(VideoFrameExtractError::Io(format!(
                "read ffmpeg output: {error}"
            )))
        }
        Err(_) => {
            let _ = child.kill();
            let _ = child.wait();
            Err(VideoFrameExtractError::TimedOut)
        }
    }
}

pub async fn video_frame_jpeg(
    path: &str,
    roots: &MediaRoots,
    t: f64,
) -> Result<Vec<u8>, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let (cache_path, cached) = tokio::task::spawn_blocking({
        let full = full.clone();
        move || {
            let cache_path = video_frame_cache_path(&full, t);
            let cached = cache_path
                .as_ref()
                .and_then(|cache| std::fs::read(cache).ok().filter(|bytes| !bytes.is_empty()));
            (cache_path, cached)
        }
    })
    .await
    .map_err(|error| internal(error.to_string()))?;
    if let Some(bytes) = cached {
        return Ok(bytes);
    }
    tokio::task::spawn_blocking(move || {
        let buf = extract_video_frame_jpeg_blocking(&full, t).map_err(|err| match err {
            VideoFrameExtractError::SlotBusy(msg) => {
                (StatusCode::SERVICE_UNAVAILABLE, json!({"error": msg}))
            }
            VideoFrameExtractError::Unavailable(msg) => (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": format!("ffmpeg unavailable: {msg}")}),
            ),
            VideoFrameExtractError::StdoutMissing => (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": "ffmpeg stdout missing"}),
            ),
            VideoFrameExtractError::TimedOut => (
                StatusCode::BAD_GATEWAY,
                json!({"error": "ffmpeg frame extraction timed out"}),
            ),
            VideoFrameExtractError::OutputTooLarge => (
                StatusCode::BAD_GATEWAY,
                json!({"error": "ffmpeg frame output exceeded size limit"}),
            ),
            VideoFrameExtractError::Io(msg) => internal(msg),
            VideoFrameExtractError::Failed(msg) => (StatusCode::BAD_GATEWAY, json!({"error": msg})),
        })?;
        if let Some(cache) = cache_path {
            write_video_frame_cache(&cache, &buf);
        }
        Ok(buf)
    })
    .await
    .map_err(|error| internal(error.to_string()))?
}

fn video_frame_cache_max_bytes() -> u64 {
    std::env::var("VIDEO_FRAME_CACHE_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_VIDEO_FRAME_CACHE_MAX_BYTES)
}

fn video_frame_cache_root() -> Option<PathBuf> {
    if video_frame_cache_max_bytes() == 0 {
        return None;
    }
    let configured = std::env::var("VIDEO_FRAME_CACHE_DIR").ok();
    let root = match configured.as_deref().map(str::trim) {
        Some(value) if !value.is_empty() => PathBuf::from(value),
        _ => {
            let preview = std::env::var("IMAGE_PREVIEW_CACHE_DIR").ok()?;
            let preview = preview.trim();
            if preview.is_empty() {
                return None;
            }
            PathBuf::from(preview).join("video-frames")
        }
    };
    let _ = std::fs::create_dir_all(&root);
    Some(root)
}

fn video_frame_cache_path(full: &Path, t: f64) -> Option<PathBuf> {
    let root = video_frame_cache_root()?;
    let full = safe_canonicalize(full).ok()?;
    let metadata = std::fs::metadata(&full).ok()?;
    let modified = metadata
        .modified()
        .ok()?
        .duration_since(UNIX_EPOCH)
        .ok()?
        .as_nanos();
    let mut hasher = blake3::Hasher::new();
    hasher.update(&VIDEO_FRAME_CACHE_VERSION.to_le_bytes());
    hasher.update(full.to_string_lossy().as_bytes());
    hasher.update(&metadata.len().to_le_bytes());
    hasher.update(&modified.to_le_bytes());
    hasher.update(&t.to_bits().to_le_bytes());
    let key = hasher.finalize().to_hex().to_string();
    Some(
        root.join(&key[..2])
            .join(&key[2..4])
            .join(format!("{key}.jpg")),
    )
}

fn write_video_frame_cache(cache: &Path, body: &[u8]) {
    if let Some(root) = video_frame_cache_root() {
        maybe_cleanup_video_frame_cache(&root, body.len() as u64);
    }
    crate::image_preview::write_exclusive_jpeg_cache(cache, body);
}

fn maybe_cleanup_video_frame_cache(root: &Path, reserve_bytes: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = VIDEO_FRAME_CACHE_LAST_CLEANUP.load(Ordering::Relaxed);
    if last > 0 && now.saturating_sub(last) < VIDEO_FRAME_CACHE_CLEANUP_INTERVAL {
        return;
    }
    if VIDEO_FRAME_CACHE_LAST_CLEANUP
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let _ = cleanup_video_frame_cache(root, video_frame_cache_max_bytes(), reserve_bytes);
}

fn cleanup_video_frame_cache(
    root: &Path,
    max_bytes: u64,
    reserve_bytes: u64,
) -> std::io::Result<usize> {
    let mut total = 0u64;
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
    {
        if !entry.file_type().is_file() {
            continue;
        }
        let extension = entry.path().extension().and_then(|ext| ext.to_str());
        if extension == Some("part") {
            // Reclaim crash leftovers from interrupted writes.
            let stale = entry
                .metadata()
                .ok()
                .and_then(|meta| meta.modified().ok())
                .and_then(|modified| SystemTime::now().duration_since(modified).ok())
                .map(|age| age >= Duration::from_secs(300))
                .unwrap_or(false);
            if stale {
                let _ = std::fs::remove_file(entry.path());
            }
            continue;
        }
        if extension != Some("jpg") {
            continue;
        }
        let metadata = entry.metadata()?;
        let size = metadata.len();
        let modified = metadata.modified().unwrap_or(UNIX_EPOCH);
        total = total.saturating_add(size);
        entries.push((modified, size, entry.path().to_path_buf()));
    }
    let target = max_bytes
        .saturating_mul(9)
        .checked_div(10)
        .unwrap_or(0)
        .saturating_sub(reserve_bytes);
    entries.sort_by_key(|(modified, _, _)| *modified);
    let mut removed = 0usize;
    for (_, size, path) in entries {
        if total <= target {
            break;
        }
        if std::fs::remove_file(path).is_ok() {
            total = total.saturating_sub(size);
            removed += 1;
        }
    }
    Ok(removed)
}

/// HTTP choke-point for image previews: always allowlist first, then open.
///
/// Routes must call this (or `resolve_allowed_path` then a Path-based opener),
/// never pass client `path` strings straight into `image_preview_bytes`.
pub fn preview_jpeg_allowed(
    path: &str,
    roots: &MediaRoots,
    max: Option<u32>,
) -> Result<Vec<u8>, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let max_edge = clamp_max_edge(max);
    image_preview_bytes(&full.to_string_lossy(), max_edge)
        .map_err(|e| (StatusCode::BAD_REQUEST, json!({"error": e.to_string()})))
}

/// HTTP choke-point for content-hash of a client-supplied path.
pub fn content_hash_allowed(path: &str, roots: &MediaRoots) -> Result<Value> {
    use crate::content_hash::hash_file;
    let full = resolve_allowed_path(path, roots)?;
    let metadata = std::fs::metadata(&full)?;
    let content_hash = hash_file(&full, 1024 * 1024)?;
    // The resolved absolute path stays server-side only; responses must not
    // leak host paths to the client.
    log_info!("media: content hash resolved {path} -> {}", full.display());
    Ok(json!({
        "path": path,
        "content_hash": content_hash,
        "file_size": metadata.len(),
    }))
}

fn transcode_cache_root() -> PathBuf {
    std::env::var("VIDEO_TRANSCODE_CACHE_DIR")
        .or_else(|_| std::env::var("IMAGE_PREVIEW_CACHE_DIR"))
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("data/transcode-cache"))
}

fn transcode_cache_max_bytes() -> u64 {
    std::env::var("VIDEO_TRANSCODE_CACHE_MAX_BYTES")
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .unwrap_or(DEFAULT_VIDEO_TRANSCODE_CACHE_MAX_BYTES)
}

fn cleanup_transcode_cache(
    root: &Path,
    max_bytes: u64,
    reserve_bytes: u64,
) -> std::io::Result<usize> {
    if !root.is_dir() {
        return Ok(0);
    }
    let mut total = 0u64;
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        if !path.is_dir() || marker_is_fresh(&path.join(".running")) {
            continue;
        }
        let mut size = 0u64;
        let mut modified = UNIX_EPOCH;
        for child in walkdir::WalkDir::new(&path)
            .follow_links(false)
            .into_iter()
            .filter_map(|entry| entry.ok())
        {
            if !child.file_type().is_file() {
                continue;
            }
            let metadata = child.metadata()?;
            size = size.saturating_add(metadata.len());
            modified = modified.max(metadata.modified().unwrap_or(UNIX_EPOCH));
        }
        total = total.saturating_add(size);
        entries.push((modified, size, path));
    }
    let target = max_bytes
        .saturating_mul(9)
        .checked_div(10)
        .unwrap_or(0)
        .saturating_sub(reserve_bytes);
    entries.sort_by_key(|(modified, _, _)| *modified);
    let mut removed = 0usize;
    for (_, size, path) in entries {
        if total <= target {
            break;
        }
        if std::fs::remove_dir_all(path).is_ok() {
            total = total.saturating_sub(size);
            removed += 1;
        }
    }
    Ok(removed)
}

fn transcode_paths(path: &str, roots: &MediaRoots) -> Result<(String, PathBuf, PathBuf)> {
    let full = safe_canonicalize(resolve_allowed_path(path, roots)?)?;
    let metadata = std::fs::metadata(&full)?;
    if !metadata.is_file() {
        anyhow::bail!("video source is not a file");
    }
    let modified = metadata
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos()
        .to_le_bytes();
    let mut hasher = blake3::Hasher::new();
    hasher.update(full.to_string_lossy().as_bytes());
    hasher.update(&metadata.len().to_le_bytes());
    hasher.update(&modified);
    let key = hasher.finalize().to_hex().to_string();
    let dir = transcode_cache_root().join(&key);
    Ok((key, dir.join("index.m3u8"), dir.join(".running")))
}

fn marker_is_fresh(marker: &Path) -> bool {
    let Ok(metadata) = marker.metadata() else {
        return false;
    };
    let Ok(modified) = metadata.modified() else {
        return false;
    };
    let Ok(age) = SystemTime::now().duration_since(modified) else {
        return false;
    };
    age < TRANSCODE_MARKER_STALE_AFTER
}

fn claim_transcode_marker(marker: &Path) -> Result<bool> {
    // Bounded retries: a stale marker whose removal keeps failing (read-only
    // moment on the NAS) must degrade to "not claimed" instead of spinning
    // forever on the request thread.
    for _ in 0..3 {
        match OpenOptions::new().write(true).create_new(true).open(marker) {
            Ok(_) => return Ok(true),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && marker_is_fresh(marker) =>
            {
                return Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                if std::fs::remove_file(marker).is_err() {
                    return Ok(false);
                }
            }
            Err(error) => return Err(error.into()),
        }
    }
    Ok(false)
}

fn transcode_error_marker(marker: &Path) -> PathBuf {
    marker.with_file_name(".error")
}

fn finish_video_transcode(
    result: std::io::Result<std::process::ExitStatus>,
    playlist: &Path,
    marker: &Path,
) {
    let error = match result {
        Ok(status) if status.success() && playlist.is_file() => None,
        Ok(status) if status.success() => Some("ffmpeg produced no HLS playlist".to_string()),
        Ok(status) => Some(format!("ffmpeg exited with {status}")),
        Err(error) => Some(format!("ffmpeg wait failed: {error}")),
    };
    let error_marker = transcode_error_marker(marker);
    if let Some(message) = error {
        let _ = std::fs::remove_file(playlist);
        let _ = std::fs::write(error_marker, message);
    } else {
        let _ = std::fs::remove_file(error_marker);
    }
    let _ = std::fs::remove_file(marker);
}

/// Report whether HLS playlist exists for this source path.
///
/// Responses never include server-absolute paths; clients address the playlist
/// through its cache `key` via the segment/playlist routes.
pub fn video_transcode_status(path: &str, roots: &MediaRoots) -> Value {
    match transcode_paths(path, roots) {
        Ok((key, playlist, _)) if playlist.is_file() => json!({
            "status": "ready",
            "ready": true,
            "key": key,
            "path": path,
        }),
        Ok((key, _playlist, marker)) => {
            if marker.is_file() && marker_is_fresh(&marker) {
                json!({"status": "processing", "ready": false, "key": key, "path": path})
            } else {
                let _ = std::fs::remove_file(&marker);
                match std::fs::read_to_string(transcode_error_marker(&marker)) {
                    Ok(message) => {
                        json!({"status": "error", "ready": false, "key": key, "path": path, "message": message})
                    }
                    Err(_) => {
                        json!({"status": "pending", "ready": false, "key": key, "path": path, "message": "transcode_pending_or_not_started"})
                    }
                }
            }
        }
        Err(err) => json!({
            "status": "error",
            "ready": false,
            "message": err.to_string(),
            "path": path,
        }),
    }
}

pub fn start_video_transcode(path: &str, roots: &MediaRoots) -> Result<Value> {
    let full = resolve_allowed_path(path, roots)?;
    let (key, playlist, marker) = transcode_paths(path, roots)?;
    let reserve = std::fs::metadata(&full)
        .map(|metadata| metadata.len())
        .unwrap_or(0);
    let _ = cleanup_transcode_cache(
        &transcode_cache_root(),
        transcode_cache_max_bytes(),
        reserve,
    );
    if let Some(parent) = playlist.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    if playlist.is_file() {
        return Ok(json!({
            "ok": true,
            "status": "ready",
            "ready": true,
            "key": key
        }));
    }
    let _ = std::fs::remove_file(transcode_error_marker(&marker));
    if !claim_transcode_marker(&marker)? {
        return Ok(json!({"ok": true, "key": key, "status": "processing", "ready": false}));
    }
    // Hold the ffmpeg slot from spawn until the waiter thread reaps the child.
    let ffmpeg_slot = FfmpegSlotGuard::acquire_blocking()?;
    let spawned = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            &full.to_string_lossy(),
            "-codec:",
            "copy",
            "-start_number",
            "0",
            "-hls_time",
            "4",
            "-hls_list_size",
            "0",
            "-f",
            "hls",
            &playlist.to_string_lossy(),
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn();
    let mut child = match spawned {
        Ok(child) => child,
        Err(error) => {
            drop(ffmpeg_slot);
            let _ = std::fs::remove_file(&marker);
            return Err(error.into());
        }
    };
    let waiter_marker = marker.clone();
    let waiter_playlist = playlist.clone();
    std::thread::spawn(move || {
        // Keep the slot until the transcode finishes, not just until this
        // function returns. Poll so a wedged ffmpeg hits the total limit and
        // releases the slot instead of holding it forever.
        let _slot = ffmpeg_slot;
        let deadline = std::time::Instant::now() + TRANSCODE_TOTAL_LIMIT;
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break Ok(status),
                Ok(None) => {
                    if std::time::Instant::now() >= deadline {
                        let _ = child.kill();
                        let _ = child.wait();
                        break Err(std::io::Error::new(
                            std::io::ErrorKind::TimedOut,
                            "ffmpeg transcode exceeded the total time limit",
                        ));
                    }
                    std::thread::sleep(Duration::from_secs(1));
                }
                Err(error) => break Err(error),
            }
        };
        finish_video_transcode(status, &waiter_playlist, &waiter_marker);
    });
    // ponytail: stale markers permit a duplicate only if a broken ffmpeg process outlives one hour; add PID tracking if observed.
    Ok(json!({
        "ok": true,
        "key": key,
        "status": "started",
        "ready": false
    }))
}

pub async fn serve_transcoded_hls(
    path: &str,
    roots: &MediaRoots,
    _headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    let (key, playlist, _) = transcode_paths(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    if !playlist.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "transcoded playlist not ready"}),
        ));
    }
    let raw = tokio::fs::read(&playlist).await.map_err(internal)?;
    let rewritten = rewrite_transcoded_playlist(&key, &raw).map_err(internal)?;
    let len = rewritten.len();
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/vnd.apple.mpegurl")
        .header(header::CACHE_CONTROL, "no-cache")
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from(rewritten))
        .map_err(internal)
}

fn rewrite_transcoded_playlist(key: &str, body: &[u8]) -> Result<Vec<u8>> {
    let text = std::str::from_utf8(body)?;
    let mut rewritten = String::with_capacity(text.len() + 128);
    for line in text.lines() {
        if line.is_empty() || line.starts_with('#') {
            rewritten.push_str(line);
        } else {
            if !safe_transcode_segment_name(line) {
                anyhow::bail!("unsafe HLS segment name");
            }
            rewritten.push_str("/api/file/video-transcoded-segment/");
            rewritten.push_str(key);
            rewritten.push('/');
            rewritten.push_str(line);
        }
        rewritten.push('\n');
    }
    Ok(rewritten.into_bytes())
}

fn safe_transcode_segment_name(value: &str) -> bool {
    !value.is_empty()
        && value != "."
        && value != ".."
        && !value.contains(['/', '\\'])
        && value
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

pub async fn serve_transcoded_hls_segment(
    key: &str,
    segment: &str,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    if key.len() != 64
        || !key.bytes().all(|byte| byte.is_ascii_hexdigit())
        || !safe_transcode_segment_name(segment)
    {
        return Err((StatusCode::NOT_FOUND, json!({"error": "segment not found"})));
    }
    let root = transcode_cache_root();
    let key_dir = root.join(key);
    let full = key_dir.join(segment);
    let key_dir = safe_canonicalize(&key_dir)
        .map_err(|_| (StatusCode::NOT_FOUND, json!({"error": "segment not found"})))?;
    let full = safe_canonicalize(&full)
        .map_err(|_| (StatusCode::NOT_FOUND, json!({"error": "segment not found"})))?;
    if !full.starts_with(&key_dir) || !full.is_file() {
        return Err((StatusCode::NOT_FOUND, json!({"error": "segment not found"})));
    }
    // Same constructor as every other media route: one open, one validator,
    // one place that decides 200 / 206 / 304. This route used to carry its own
    // copy of the range logic, which is how it drifted into answering 416 for a
    // `Range` the media route would have ignored.
    let opened = OpenedMedia::open(&full).await.map_err(internal)?;
    let (mime, disposition) = inline_media_headers(&full);
    file_response(opened, &mime, disposition.as_deref(), headers, true).await
}

/// Compatible progressive stream: serve original with Range (ffmpeg filter optional later).
pub async fn serve_video_compatible(
    path: &str,
    roots: &MediaRoots,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    serve_file_response(path, roots, headers).await
}

pub async fn serve_video_hls(
    path: &str,
    roots: &MediaRoots,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    // Prefer transcoded playlist when ready; else 404 so UI falls back.
    match video_transcode_status(path, roots) {
        status if status.get("ready") == Some(&json!(true)) => {
            serve_transcoded_hls(path, roots, headers).await
        }
        _ => Err((
            StatusCode::NOT_FOUND,
            json!({"error": "hls_not_ready", "hint": "POST /api/file/video-transcode first"}),
        )),
    }
}
