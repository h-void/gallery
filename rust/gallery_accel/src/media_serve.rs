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
pub fn inline_media_headers(full: &Path) -> (String, Option<String>) {
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

pub fn attachment_disposition(name: &str) -> String {
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

fn path_variants(path: &str, full: &Path, roots: &MediaRoots) -> Vec<String> {
    let mut out = Vec::new();
    let full_norm = normalize_slashes(&full.to_string_lossy());
    let path_norm = normalize_slashes(path);
    for candidate in [
        path.to_string(),
        full.to_string_lossy().to_string(),
        path_norm.clone(),
        full_norm.clone(),
    ] {
        if !candidate.is_empty() && !out.iter().any(|v| v == &candidate) {
            out.push(candidate);
        }
    }
    for (i, virtual_root) in roots.roots.iter().enumerate() {
        let virt = normalize_slashes(virtual_root)
            .trim_end_matches('/')
            .to_string();
        let Some(real_raw) = roots.real_root_at(i) else {
            continue;
        };
        let real = normalize_slashes(real_raw)
            .trim_end_matches('/')
            .to_string();
        if virt.is_empty() || real.is_empty() || virt == real {
            continue;
        }
        if let Some(suffix) = full_norm.strip_prefix(&real).filter(|s| s.starts_with('/')) {
            let mapped_virt = format!("{virt}{suffix}");
            if !out.iter().any(|v| v == &mapped_virt) {
                out.push(mapped_virt);
            }
        }
        if let Some(suffix) = path_norm.strip_prefix(&virt).filter(|s| s.starts_with('/')) {
            let mapped_real = format!("{real}{suffix}");
            if !out.iter().any(|v| v == &mapped_real) {
                out.push(mapped_real);
            }
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

#[path = "media_serve_recycle.rs"]
mod recycle_ops;

#[path = "media_serve_video.rs"]
mod video_ops;

// Re-exports keep the pre-split `crate::media_serve::…` names unchanged.
pub(crate) use recycle_ops::*;
pub(crate) use video_ops::*;
// The library root re-exports these as part of the crate API (lib.rs), so they
// must stay `pub` across the split instead of stopping at the crate boundary.
pub use recycle_ops::{delete_folder_to_recycle, delete_item_to_recycle, delete_to_recycle};
pub use video_ops::{
    content_hash_allowed, preview_jpeg_allowed, serve_transcoded_hls, serve_transcoded_hls_segment,
    serve_video_compatible, serve_video_hls, start_video_transcode, video_frame_jpeg,
    video_transcode_status,
};
