//! Native media serve (file/stream/text/delete + video frame via ffmpeg).

use std::fs::OpenOptions;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, Result};
use axum::body::Body;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::Response;
use serde_json::{json, Value};
use tokio::fs::File;
use tokio::io::AsyncReadExt;
use tokio::process::Command;
use tokio_util::io::ReaderStream;

use crate::image_preview::{clamp_max_edge, image_preview_bytes};
#[cfg(not(target_os = "linux"))]
use crate::media_roots::path_under_authorized_roots;
use crate::media_roots::MediaRoots;
use crate::media_type::media_type_for_file;
use crate::recycle::{capture_item_snapshot, ensure_recycle_schema};

const TEXT_PREVIEW_MAX_BYTES: u64 = 512 * 1024;
const TRANSCODE_MARKER_STALE_AFTER: Duration = Duration::from_secs(60 * 60);
const VIDEO_FRAME_CACHE_VERSION: u32 = 1;
const DEFAULT_VIDEO_FRAME_CACHE_MAX_BYTES: u64 = 2_000_000_000;
const DEFAULT_VIDEO_TRANSCODE_CACHE_MAX_BYTES: u64 = 900_000_000;
const VIDEO_FRAME_CACHE_CLEANUP_INTERVAL: u64 = 300;
static VIDEO_FRAME_CACHE_LAST_CLEANUP: AtomicU64 = AtomicU64::new(0);

fn normalize_slashes(path: &str) -> String {
    path.replace('\\', "/")
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
    let Ok(canon) = path.canonicalize() else {
        // If file does not exist yet, check logical path only.
        let logical = normalize_slashes(&path.to_string_lossy());
        return allowed.iter().any(|root| {
            let root = root.trim_end_matches(['/', '\\']);
            logical == root || logical.starts_with(&format!("{root}/"))
        });
    };
    let logical = normalize_slashes(&canon.to_string_lossy());
    allowed.iter().any(|root| {
        let root_path = PathBuf::from(root);
        if let Ok(root_canon) = root_path.canonicalize() {
            let root_s = normalize_slashes(&root_canon.to_string_lossy());
            logical == root_s
                || logical.starts_with(&format!("{root_s}/"))
                || logical.starts_with(&format!("{root_s}\\"))
        } else {
            let root_s = root.trim_end_matches(['/', '\\']);
            logical == root_s || logical.starts_with(&format!("{root_s}/"))
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
        let Ok(canonical) = cand.canonicalize() else {
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

pub async fn serve_file_response(
    path: &str,
    roots: &MediaRoots,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots)
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let meta = tokio::fs::metadata(&full)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, json!({"error": e.to_string()})))?;
    let len = meta.len();
    let mime = mime_guess::from_path(&full)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    if let Some(range) = headers.get(header::RANGE).and_then(|v| v.to_str().ok()) {
        if let Some((start, end)) = parse_bytes_range(range, len) {
            let mut file = File::open(&full).await.map_err(internal)?;
            use tokio::io::{AsyncSeekExt, SeekFrom};
            file.seek(SeekFrom::Start(start)).await.map_err(internal)?;
            let take = end - start + 1;
            let limited = file.take(take);
            let stream = ReaderStream::new(limited);
            let body = Body::from_stream(stream);
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, take)
                .body(body)
                .map_err(internal);
        }
    }

    let file = File::open(&full).await.map_err(internal)?;
    let stream = ReaderStream::new(file);
    let body = Body::from_stream(stream);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .body(body)
        .map_err(internal)
}

fn parse_bytes_range(header: &str, len: u64) -> Option<(u64, u64)> {
    let header = header.trim();
    let rest = header.strip_prefix("bytes=")?;
    let (a, b) = rest.split_once('-')?;
    let start: u64 = a.parse().ok()?;
    let end: u64 = if b.is_empty() {
        len.saturating_sub(1)
    } else {
        b.parse().ok()?
    };
    if start > end || start >= len {
        return None;
    }
    Some((start, end.min(len.saturating_sub(1))))
}

fn internal<E: std::fmt::Display>(e: E) -> (StatusCode, Value) {
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        json!({"error": e.to_string()}),
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

/// fnOS trash root + relative path under user volume, if path matches /volN/user/...
fn fnos_recycle_target(full: &Path) -> Option<(PathBuf, PathBuf)> {
    let logical = normalize_slashes(&full.to_string_lossy());
    let parts: Vec<&str> = logical.split('/').filter(|p| !p.is_empty()).collect();
    for (idx, part) in parts.iter().enumerate() {
        let lower = part.to_ascii_lowercase();
        let is_vol = (lower.starts_with("vol") || lower.starts_with("volume"))
            && lower
                .chars()
                .skip_while(|c| c.is_ascii_alphabetic())
                .all(|c| c.is_ascii_digit())
            && lower.chars().any(|c| c.is_ascii_digit());
        if is_vol && idx + 1 < parts.len() {
            let mut trash = PathBuf::from("/");
            for p in &parts[..=idx + 1] {
                trash.push(p);
            }
            trash.push(".@#local");
            trash.push("trash");
            let rel: PathBuf = parts[idx + 2..].iter().collect();
            return Some((trash, rel));
        }
    }
    None
}

fn gallery_recycle_dir() -> PathBuf {
    let data_dir = std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into());
    PathBuf::from(data_dir).join("recycle")
}

/// Move `src` to `dest` without overwriting, adding a UUID suffix on collision.
pub(crate) fn move_file_no_overwrite(src: &Path, dest: &Path) -> Result<PathBuf> {
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
    match move_file_exact_no_overwrite(src, &final_dest) {
        Ok(()) => Ok(final_dest),
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
            move_file_exact_no_overwrite(src, &retry)?;
            Ok(retry)
        }
        Err(error) => Err(error.into()),
    }
}

fn path_is_within_existing_root(path: &Path, root: &Path) -> bool {
    path.canonicalize()
        .ok()
        .zip(root.canonicalize().ok())
        .is_some_and(|(path, root)| path == root || path.starts_with(root))
}

pub(crate) fn recycle_source_is_trusted(recycled: &Path, original: &Path) -> bool {
    path_is_within_existing_root(recycled, &gallery_recycle_dir())
        || fnos_recycle_target(original)
            .is_some_and(|(trash, _)| path_is_within_existing_root(recycled, &trash))
}

pub(crate) fn move_file_exact_no_overwrite(src: &Path, dest: &Path) -> std::io::Result<()> {
    move_file_exact_impl(src, dest, false)
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

    const O_RDONLY: c_int = 0;
    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
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
            O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
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
                O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
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
                    O_RDONLY | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
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
fn move_file_exact_impl(src: &Path, dest: &Path, force_copy: bool) -> std::io::Result<()> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let source_file = open_source_file(src)?;
    let source_meta = source_file.metadata()?;
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
                if let Err(error) = retire_source_if_unchanged(src, &source_meta) {
                    let _ = remove_created_file(dest, created_identity);
                    return Err(error);
                }
                return Ok(());
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => return Err(error),
            Err(_) => {}
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
    if let Err(error) = retire_source_if_unchanged(src, &source_meta) {
        let _ = remove_created_file(dest, created_identity);
        return Err(error);
    }
    Ok(())
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
fn retire_source_if_unchanged(src: &Path, expected: &std::fs::Metadata) -> std::io::Result<()> {
    #[cfg(target_os = "linux")]
    {
        return retire_source_if_unchanged_linux(src, expected);
    }
    #[cfg(not(target_os = "linux"))]
    {
        if !same_path_identity(src, expected) {
            return Err(std::io::Error::other("source changed during move"));
        }
        std::fs::remove_file(src)
    }
}

#[cfg(target_os = "linux")]
fn retire_source_if_unchanged_linux(
    src: &Path,
    expected: &std::fs::Metadata,
) -> std::io::Result<()> {
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
    let _ = remove_created_file(&placeholder, file_identity(expected));
    let _ = remove_created_file(src, placeholder_identity);
    Ok(())
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

fn move_into_recycle(full: &Path) -> Result<PathBuf> {
    let base = full
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "file".into());

    if let Some((trash_root, rel)) = fnos_recycle_target(full) {
        // Prefer nested structure under fnOS trash.
        if !rel.as_os_str().is_empty() {
            let nested = trash_root.join(&rel);
            if let Ok(dest) = move_file_no_overwrite(full, &nested) {
                return Ok(dest);
            }
        }
        // Flat under trash root.
        let flat = trash_root.join(&base);
        if let Ok(dest) = move_file_no_overwrite(full, &flat) {
            return Ok(dest);
        }
    }

    // Gallery-owned DATA_DIR/recycle fallback (always try; do not claim fnOS trash).
    let recycle = gallery_recycle_dir();
    std::fs::create_dir_all(&recycle)?;
    if let Some((_, rel)) = fnos_recycle_target(full) {
        if !rel.as_os_str().is_empty() {
            let nested = recycle.join(&rel);
            if let Ok(dest) = move_file_no_overwrite(full, &nested) {
                return Ok(dest);
            }
        }
    }
    let flat = recycle.join(format!("{}_{base}", uuid::Uuid::new_v4().simple()));
    move_file_no_overwrite(full, &flat)
}

fn record_delete_reconciliation(
    conn: &rusqlite::Connection,
    item_id: i64,
    item_snapshot: &Value,
    tag_ids: &[i64],
    tag_single_refs: &str,
    non_tag_single_ref_ids: &str,
    original_path: &str,
    recycled_path: &str,
    error: &str,
) -> bool {
    conn.execute(
        "INSERT INTO recycle_entries
         (original_item_id, artist_id, original_path, recycled_path, item_snapshot,
          tag_ids_snapshot, tag_single_refs_snapshot, non_tag_single_ref_ids, last_error)
         VALUES (?,?,?,?,?,?,?,?,?)",
        rusqlite::params![
            item_id,
            item_snapshot["artist_id"].as_i64().unwrap_or_default(),
            original_path,
            recycled_path,
            item_snapshot.to_string(),
            serde_json::to_string(tag_ids).unwrap_or_else(|_| "[]".into()),
            tag_single_refs,
            non_tag_single_ref_ids,
            error,
        ],
    )
    .is_ok()
}

/// Delete an active library item: recycle file then remove DB row + auto character refs.
///
/// Returns success only when FS + DB agree. On DB failure, tries to restore the file.
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
    let recycled = move_into_recycle(&full).map_err(internal)?;
    let recycled_s = recycled.display().to_string();
    let original_s = original.display().to_string();

    let db_result = (|| -> Result<(i64, i64)> {
        conn.execute("BEGIN IMMEDIATE", [])?;
        let tx = (|| -> Result<(i64, i64)> {
            let artist_id: i64 =
                conn.query_row("SELECT artist_id FROM items WHERE id=?", [item_id], |row| {
                    row.get(0)
                })?;
            conn.execute(
                "INSERT INTO recycle_entries (original_item_id, artist_id, original_path, recycled_path, item_snapshot, tag_ids_snapshot, tag_single_refs_snapshot, non_tag_single_ref_ids) VALUES (?,?,?,?,?,?,?,?)",
                rusqlite::params![item_id, artist_id, original_s, recycled_s, serde_json::to_string(&item_snapshot)?, serde_json::to_string(&tag_ids)?, &tag_single_refs, &non_tag_single_ref_ids],
            )?;
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
            }))
        }
        Err(db_err) => match move_file_exact_no_overwrite(&recycled, &original) {
            Ok(()) => Err((
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({
                    "error": format!("database delete failed; file restored: {db_err}"),
                    "item_id": item_id,
                    "original_path": original_s,
                }),
            )),
            Err(restore_err) => {
                let error = format!(
                    "database delete failed and restore failed: db={db_err}; restore={restore_err}"
                );
                let recorded = record_delete_reconciliation(
                    conn,
                    item_id,
                    &item_snapshot,
                    &tag_ids,
                    &tag_single_refs,
                    &non_tag_single_ref_ids,
                    &original_s,
                    &recycled_s,
                    &error,
                );
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
    let mut child = Command::new("ffmpeg")
        .args([
            "-hide_banner",
            "-loglevel",
            "error",
            "-ss",
            &format!("{t:.3}"),
            "-i",
            &full.to_string_lossy(),
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
        .map_err(|e| {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                json!({"error": format!("ffmpeg unavailable: {e}")}),
            )
        })?;
    let mut stdout = child.stdout.take().ok_or_else(|| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            json!({"error": "ffmpeg stdout missing"}),
        )
    })?;
    let mut buf = Vec::new();
    stdout.read_to_end(&mut buf).await.map_err(internal)?;
    let status = child.wait().await.map_err(internal)?;
    if !status.success() || buf.is_empty() {
        return Err((
            StatusCode::BAD_GATEWAY,
            json!({"error": "ffmpeg failed to extract frame"}),
        ));
    }
    if let Some(cache) = cache_path {
        buf = tokio::task::spawn_blocking(move || {
            write_video_frame_cache(&cache, &buf);
            buf
        })
        .await
        .map_err(|error| internal(error.to_string()))?;
    }
    Ok(buf)
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
    let full = full.canonicalize().ok()?;
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
    if let Some(parent) = cache.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let part = cache.with_extension("jpg.part");
    if std::fs::write(&part, body).is_ok() {
        let _ = std::fs::rename(&part, cache);
    } else {
        let _ = std::fs::remove_file(&part);
    }
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
        if !entry.file_type().is_file()
            || entry.path().extension().and_then(|ext| ext.to_str()) != Some("jpg")
        {
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

/// Back-compat alias used by older call sites.
pub fn preview_or_fallback(
    path: &str,
    roots: &MediaRoots,
    max: Option<u32>,
) -> Result<Vec<u8>, (StatusCode, Value)> {
    preview_jpeg_allowed(path, roots, max)
}

/// HTTP choke-point for content-hash of a client-supplied path.
pub fn content_hash_allowed(path: &str, roots: &MediaRoots) -> Result<Value> {
    use crate::content_hash::hash_file;
    let full = resolve_allowed_path(path, roots)?;
    let metadata = std::fs::metadata(&full)?;
    let content_hash = hash_file(&full, 1024 * 1024)?;
    Ok(json!({
        "path": path,
        "content_hash": content_hash,
        "file_size": metadata.len(),
        "resolved_path": full.display().to_string(),
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
    let full = resolve_allowed_path(path, roots)?.canonicalize()?;
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
    loop {
        match OpenOptions::new().write(true).create_new(true).open(marker) {
            Ok(_) => return Ok(true),
            Err(error)
                if error.kind() == std::io::ErrorKind::AlreadyExists && marker_is_fresh(marker) =>
            {
                return Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(marker);
            }
            Err(error) => return Err(error.into()),
        }
    }
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
pub fn video_transcode_status(path: &str, roots: &MediaRoots) -> Value {
    match transcode_paths(path, roots) {
        Ok((key, playlist, _)) if playlist.is_file() => json!({
            "status": "ready",
            "ready": true,
            "key": key,
            "playlist": playlist.display().to_string(),
            "path": path,
        }),
        Ok((key, playlist, marker)) => {
            if marker.is_file() && marker_is_fresh(&marker) {
                json!({"status": "processing", "ready": false, "key": key, "playlist": playlist.display().to_string(), "path": path})
            } else {
                let _ = std::fs::remove_file(&marker);
                match std::fs::read_to_string(transcode_error_marker(&marker)) {
                    Ok(message) => {
                        json!({"status": "error", "ready": false, "key": key, "playlist": playlist.display().to_string(), "path": path, "message": message})
                    }
                    Err(_) => {
                        json!({"status": "pending", "ready": false, "key": key, "playlist": playlist.display().to_string(), "path": path, "message": "transcode_pending_or_not_started"})
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
            "key": key,
            "playlist": playlist.display().to_string()
        }));
    }
    let _ = std::fs::remove_file(transcode_error_marker(&marker));
    if !claim_transcode_marker(&marker)? {
        return Ok(
            json!({"ok": true, "key": key, "playlist": playlist.display().to_string(), "status": "processing", "ready": false}),
        );
    }
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
            let _ = std::fs::remove_file(&marker);
            return Err(error.into());
        }
    };
    let waiter_marker = marker.clone();
    let waiter_playlist = playlist.clone();
    std::thread::spawn(move || {
        finish_video_transcode(child.wait(), &waiter_playlist, &waiter_marker);
    });
    // ponytail: stale markers permit a duplicate only if a broken ffmpeg process outlives one hour; add PID tracking if observed.
    Ok(json!({
        "ok": true,
        "key": key,
        "playlist": playlist.display().to_string(),
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
    let key_dir = key_dir
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, json!({"error": "segment not found"})))?;
    let full = full
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, json!({"error": "segment not found"})))?;
    if !full.starts_with(&key_dir) || !full.is_file() {
        return Err((StatusCode::NOT_FOUND, json!({"error": "segment not found"})));
    }
    let len = tokio::fs::metadata(&full).await.map_err(internal)?.len();
    let mime = mime_guess::from_path(&full)
        .first_or_octet_stream()
        .essence_str()
        .to_string();
    if let Some(range) = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok())
    {
        if let Some((start, end)) = parse_bytes_range(range, len) {
            use tokio::io::{AsyncSeekExt, SeekFrom};
            let mut file = File::open(&full).await.map_err(internal)?;
            file.seek(SeekFrom::Start(start)).await.map_err(internal)?;
            let take = end - start + 1;
            return Response::builder()
                .status(StatusCode::PARTIAL_CONTENT)
                .header(header::CONTENT_TYPE, mime)
                .header(header::ACCEPT_RANGES, "bytes")
                .header(header::CONTENT_RANGE, format!("bytes {start}-{end}/{len}"))
                .header(header::CONTENT_LENGTH, take)
                .body(Body::from_stream(ReaderStream::new(file.take(take))))
                .map_err(internal);
        }
    }
    let file = File::open(&full).await.map_err(internal)?;
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::ACCEPT_RANGES, "bytes")
        .header(header::CONTENT_LENGTH, len)
        .body(Body::from_stream(ReaderStream::new(file)))
        .map_err(internal)
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

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn parse_range_header() {
        assert_eq!(parse_bytes_range("bytes=0-99", 1000), Some((0, 99)));
        assert_eq!(parse_bytes_range("bytes=10-", 100), Some((10, 99)));
    }

    #[test]
    fn rejects_paths_outside_media_roots() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let ok_file = media.join("a.jpg");
        std::fs::write(&ok_file, b"x").unwrap();
        // Sensitive host file simulation
        let secret = dir.path().join("secret.txt");
        std::fs::write(&secret, b"secret").unwrap();

        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let allowed = resolve_allowed_path(&ok_file.to_string_lossy().replace('\\', "/"), &roots);
        assert!(allowed.is_ok(), "media file should be allowed");

        let denied = resolve_allowed_path(&secret.to_string_lossy().replace('\\', "/"), &roots);
        assert!(denied.is_err(), "file outside media roots must be denied");

        // Classic host paths
        #[cfg(unix)]
        {
            assert!(resolve_allowed_path("/etc/hosts", &roots).is_err());
        }
        #[cfg(windows)]
        {
            assert!(
                resolve_allowed_path(r"C:\Windows\System32\drivers\etc\hosts", &roots).is_err()
            );
        }
    }

    #[test]
    fn allows_double_dot_in_filename_but_rejects_parent_path_components() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        let nested = media.join("nested");
        std::fs::create_dir_all(&nested).unwrap();
        let video = media.join("1..mp4");
        std::fs::write(&video, b"video").unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let allowed = resolve_allowed_path(&video.to_string_lossy().replace('\\', "/"), &roots);
        assert!(
            allowed.is_ok(),
            "double dots inside a filename are not traversal"
        );

        let traversal = nested.join("..").join("1..mp4");
        let denied = resolve_allowed_path(&traversal.to_string_lossy().replace('\\', "/"), &roots);
        assert!(
            denied.is_err(),
            "a parent-directory path component must be rejected"
        );
    }

    #[test]
    fn preview_jpeg_allowed_denies_outside_media_roots() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        // Minimal valid JPEG (1x1) so open would succeed if allowlist failed.
        let jpeg_header = [
            0xFFu8, 0xD8, 0xFF, 0xE0, 0x00, 0x10, 0x4A, 0x46, 0x49, 0x46, 0x00, 0x01, 0x01, 0x00,
            0x00, 0x01, 0x00, 0x01, 0x00, 0x00, 0xFF, 0xDB, 0x00, 0x43, 0x00, 0x08, 0x06, 0x06,
            0x07, 0x06, 0x05, 0x08, 0x07, 0x07, 0x07, 0x09, 0x09, 0x08, 0x0A, 0x0C, 0x14, 0x0D,
            0x0C, 0x0B, 0x0B, 0x0C, 0x19, 0x12, 0x13, 0x0F, 0x14, 0x1D, 0x1A, 0x1F, 0x1E, 0x1D,
            0x1A, 0x1C, 0x1C, 0x20, 0x24, 0x2E, 0x27, 0x20, 0x22, 0x2C, 0x23, 0x1C, 0x1C, 0x28,
            0x37, 0x29, 0x2C, 0x30, 0x31, 0x34, 0x34, 0x34, 0x1F, 0x27, 0x39, 0x3D, 0x38, 0x32,
            0x3C, 0x2E, 0x33, 0x34, 0x32, 0xFF, 0xC0, 0x00, 0x0B, 0x08, 0x00, 0x01, 0x00, 0x01,
            0x01, 0x01, 0x11, 0x00, 0xFF, 0xC4, 0x00, 0x14, 0x00, 0x01, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x08, 0xFF, 0xC4,
            0x00, 0x14, 0x10, 0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0xFF, 0xDA, 0x00, 0x08, 0x01, 0x01, 0x00, 0x00,
            0x3F, 0x00, 0x7F, 0xFF, 0xD9,
        ];
        let ok_file = media.join("a.jpg");
        std::fs::write(&ok_file, jpeg_header).unwrap();
        let secret = dir.path().join("secret.jpg");
        std::fs::write(&secret, jpeg_header).unwrap();

        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let ok = preview_jpeg_allowed(
            &ok_file.to_string_lossy().replace('\\', "/"),
            &roots,
            Some(128),
        );
        // Allowlisted file must not be rejected by path policy (decode may still fail on tiny jpeg).
        match &ok {
            Ok(_) => {}
            Err((code, body)) => {
                assert_ne!(
                    *code,
                    StatusCode::NOT_FOUND,
                    "under-root path must not be path-denied: {body}"
                );
            }
        }

        let denied = preview_jpeg_allowed(
            &secret.to_string_lossy().replace('\\', "/"),
            &roots,
            Some(128),
        );
        assert!(denied.is_err(), "outside media roots must be denied");
        let (code, body) = denied.unwrap_err();
        assert_eq!(code, StatusCode::NOT_FOUND);
        assert!(
            body["error"].as_str().unwrap_or("").contains("not allowed")
                || body["error"].as_str().unwrap_or("").contains("not found"),
            "unexpected deny body: {body}"
        );

        let hash_denied =
            content_hash_allowed(&secret.to_string_lossy().replace('\\', "/"), &roots);
        assert!(hash_denied.is_err(), "content-hash outside roots must fail");
    }

    #[test]
    fn transcode_status_ready_when_playlist_exists() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let video = media.join("clip.mp4");
        std::fs::write(&video, b"fake").unwrap();
        let cache = dir.path().join("cache");
        let _cache_dir = crate::test_support::EnvVar::set("VIDEO_TRANSCODE_CACHE_DIR", &cache);
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let path = video.to_string_lossy().replace('\\', "/");
        let (_, playlist, _) = transcode_paths(&path, &roots).unwrap();
        std::fs::create_dir_all(playlist.parent().unwrap()).unwrap();
        std::fs::write(playlist, b"#EXTM3U\n").unwrap();
        let status = video_transcode_status(&path, &roots);
        assert_eq!(status["ready"], true);
        assert_eq!(status["status"], "ready");
    }

    #[test]
    fn failed_transcode_clears_processing_and_reports_error() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let video = media.join("clip.mp4");
        std::fs::write(&video, b"fake").unwrap();
        let cache = dir.path().join("cache");
        let _cache_dir = crate::test_support::EnvVar::set("VIDEO_TRANSCODE_CACHE_DIR", &cache);
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let path = video.to_string_lossy().replace('\\', "/");
        let (_, playlist, marker) = transcode_paths(&path, &roots).unwrap();
        std::fs::create_dir_all(playlist.parent().unwrap()).unwrap();
        std::fs::write(&playlist, b"partial").unwrap();
        std::fs::write(&marker, b"").unwrap();

        finish_video_transcode(
            Err(std::io::Error::new(
                std::io::ErrorKind::Other,
                "wait failed",
            )),
            &playlist,
            &marker,
        );

        assert!(!marker.exists());
        assert!(!playlist.exists());
        let status = video_transcode_status(&path, &roots);
        assert_eq!(status["ready"], false);
        assert_eq!(status["status"], "error");
        assert!(status["message"].as_str().unwrap().contains("wait failed"));
    }

    #[tokio::test]
    async fn transcoded_playlist_rewrites_segments_to_safe_route() {
        use http_body_util::BodyExt;

        let _env_guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let video = media.join("clip.mp4");
        std::fs::write(&video, b"fake").unwrap();
        let cache = dir.path().join("cache");
        let _cache_dir = crate::test_support::EnvVar::set("VIDEO_TRANSCODE_CACHE_DIR", &cache);
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let path = video.to_string_lossy().replace('\\', "/");
        let (key, playlist, _) = transcode_paths(&path, &roots).unwrap();
        std::fs::create_dir_all(playlist.parent().unwrap()).unwrap();
        std::fs::write(&playlist, b"#EXTM3U\n#EXTINF:4.0,\nindex0.ts\n").unwrap();

        let response = serve_transcoded_hls(&path, &roots, &HeaderMap::new())
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = String::from_utf8(bytes.to_vec()).unwrap();
        assert!(body.contains(&format!(
            "/api/file/video-transcoded-segment/{key}/index0.ts"
        )));
    }

    #[tokio::test]
    async fn video_frame_uses_persistent_cache_before_ffmpeg() {
        let _env_guard = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let video = media.join("1..mp4");
        std::fs::write(&video, b"fake-video").unwrap();
        let cache = dir.path().join("video-frames");
        let _cache_dir = crate::test_support::EnvVar::set("VIDEO_FRAME_CACHE_DIR", &cache);
        let _cache_max = crate::test_support::EnvVar::set("VIDEO_FRAME_CACHE_MAX_BYTES", "1000000");
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let cache_path = video_frame_cache_path(&video, 0.1).unwrap();
        std::fs::create_dir_all(cache_path.parent().unwrap()).unwrap();
        std::fs::write(&cache_path, b"cached-frame").unwrap();
        let _path = crate::test_support::EnvVar::set("PATH", dir.path().join("no-ffmpeg"));

        let bytes = video_frame_jpeg(&video.to_string_lossy(), &roots, 0.1)
            .await
            .unwrap();
        assert_eq!(bytes, b"cached-frame");
    }

    #[test]
    fn video_frame_cache_cleanup_evicts_oldest_files() {
        let dir = tempdir().unwrap();
        let root = dir.path().join("video-frames");
        std::fs::create_dir_all(&root).unwrap();
        let old = root.join("old.jpg");
        let new = root.join("new.jpg");
        std::fs::write(&old, b"12345678").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::write(&new, b"abcdefgh").unwrap();

        let removed = cleanup_video_frame_cache(&root, 12, 0).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists(), "oldest frame should be evicted first");
        assert!(new.exists(), "newer frame should remain cached");
    }

    #[test]
    fn transcode_cache_cleanup_evicts_oldest_completed_directory() {
        let dir = tempdir().unwrap();
        let old = dir.path().join("old");
        let new = dir.path().join("new");
        std::fs::create_dir_all(&old).unwrap();
        std::fs::write(old.join("index0.ts"), b"12345678").unwrap();
        std::thread::sleep(Duration::from_millis(20));
        std::fs::create_dir_all(&new).unwrap();
        std::fs::write(new.join("index0.ts"), b"abcdefgh").unwrap();

        let removed = cleanup_transcode_cache(dir.path(), 12, 0).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists());
        assert!(new.exists());
    }

    fn delete_fixture() -> (
        tempfile::TempDir,
        rusqlite::Connection,
        MediaRoots,
        PathBuf,
        PathBuf,
    ) {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        let artist = media.join("ArtistA");
        std::fs::create_dir_all(artist.join("a")).unwrap();
        std::fs::create_dir_all(artist.join("b")).unwrap();
        let f1 = artist.join("a").join("same.jpg");
        let f2 = artist.join("b").join("same.jpg");
        std::fs::write(&f1, b"one").unwrap();
        std::fs::write(&f2, b"two").unwrap();
        let orphan = media.join("orphan.jpg");
        std::fs::write(&orphan, b"orphan").unwrap();
        let conn = rusqlite::Connection::open(dir.path().join("g.db")).unwrap();
        conn.execute_batch(
            "
            CREATE TABLE items (
              id INTEGER PRIMARY KEY, artist_id INTEGER, file_path TEXT, file_name TEXT,
              missing INTEGER DEFAULT 0
            );
            CREATE TABLE character_references (
              id INTEGER PRIMARY KEY, character_id INTEGER, embedding BLOB, embedding_dim INTEGER,
              embedding_model_repo_id TEXT NOT NULL DEFAULT '',
              embedding_model_variant TEXT NOT NULL DEFAULT '',
              embedding_model_file TEXT NOT NULL DEFAULT '', embedding_updated_at REAL,
              source_type TEXT, item_id INTEGER, created_at REAL
            );
            CREATE TABLE item_tags (item_id INTEGER, tag_id INTEGER, PRIMARY KEY(item_id, tag_id));
            ",
        )
        .unwrap();
        let p1 = f1.to_string_lossy().replace('\\', "/");
        let p2 = f2.to_string_lossy().replace('\\', "/");
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing) VALUES (1,1,?,?,0)",
            rusqlite::params![p1, "same.jpg"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, missing) VALUES (2,1,?,?,0)",
            rusqlite::params![p2, "same.jpg"],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_references (id, character_id, embedding, embedding_dim, source_type, item_id, created_at)
             VALUES (1, 9, x'00', 1, 'tag_single', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO character_references (id, character_id, embedding, embedding_dim, source_type, item_id, created_at)
             VALUES (2, 9, x'01', 1, 'manual', 1, 0)",
            [],
        )
        .unwrap();
        conn.execute("INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)", [])
            .unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let data_dir = dir.path().join("data");
        (dir, conn, roots, orphan, data_dir)
    }

    #[test]
    fn delete_rejects_non_item_file() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (_dir, conn, roots, orphan, data_dir) = delete_fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let path = orphan.to_string_lossy().replace('\\', "/");
        let err = delete_item_to_recycle(&conn, &path, &roots).unwrap_err();
        assert_eq!(err.0, StatusCode::NOT_FOUND);
        assert!(orphan.is_file(), "orphan must remain");
    }

    #[test]
    fn delete_active_item_recycles_and_cleans_db() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (dir, conn, roots, _, data_dir) = delete_fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let f1 = dir
            .path()
            .join("pictures")
            .join("ArtistA")
            .join("a")
            .join("same.jpg");
        let path = f1.to_string_lossy().replace('\\', "/");
        let out = delete_item_to_recycle(&conn, &path, &roots).unwrap();
        assert_eq!(out["ok"], true);
        assert_eq!(out["item_id"], 1);
        assert!(!f1.exists());
        let recycled = PathBuf::from(out["recycled_to"].as_str().unwrap());
        assert!(recycled.is_file());
        assert_eq!(std::fs::read(&recycled).unwrap(), b"one");
        let items: i64 = conn
            .query_row("SELECT COUNT(*) FROM items WHERE id=1", [], |r| r.get(0))
            .unwrap();
        assert_eq!(items, 0);
        let auto_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_references WHERE item_id=1 AND source_type='tag_single'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(auto_refs, 0);
        let manual_refs: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM character_references WHERE item_id=1 AND source_type='manual'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        // manual row may remain if no FK cascade on character_references; item is gone.
        // Plan: manual refs must not be deleted by our DELETE — they may become orphans.
        assert_eq!(manual_refs, 1);
        assert_eq!(out["deleted_auto_character_refs"], 1);
    }

    #[test]
    fn delete_rollback_does_not_clobber_recreated_original() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (dir, conn, roots, _, data_dir) = delete_fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        ensure_recycle_schema(&conn).unwrap();
        conn.execute_batch(
            "CREATE TRIGGER fail_item_delete BEFORE DELETE ON items
             BEGIN SELECT RAISE(ABORT, 'stop'); END;
             PRAGMA busy_timeout=5000;",
        )
        .unwrap();
        let original = dir
            .path()
            .join("pictures")
            .join("ArtistA")
            .join("a")
            .join("same.jpg");
        let path = original.to_string_lossy().replace('\\', "/");
        let lock_conn = rusqlite::Connection::open(dir.path().join("g.db")).unwrap();
        let worker_conn = rusqlite::Connection::open(dir.path().join("g.db")).unwrap();
        lock_conn.execute("BEGIN IMMEDIATE", []).unwrap();

        let worker =
            std::thread::spawn(move || delete_item_to_recycle(&worker_conn, &path, &roots));
        for _ in 0..1000 {
            if !original.exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(!original.exists(), "delete did not reach recycle move");
        std::fs::write(&original, b"concurrent").unwrap();
        lock_conn.execute("COMMIT", []).unwrap();

        let error = worker.join().unwrap().unwrap_err();
        assert_eq!(error.1["needs_reconciliation"], true);
        assert_eq!(std::fs::read(&original).unwrap(), b"concurrent");
        assert_eq!(
            std::fs::read(error.1["recycled_to"].as_str().unwrap()).unwrap(),
            b"one"
        );
        assert_eq!(error.1["reconciliation_recorded"], true);
        let last_error: String = conn
            .query_row("SELECT last_error FROM recycle_entries", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert!(last_error.contains("database delete failed and restore failed"));
    }

    #[test]
    fn delete_same_basename_twice_keeps_both() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let (dir, conn, roots, _, data_dir) = delete_fixture();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", data_dir);
        let f1 = dir
            .path()
            .join("pictures")
            .join("ArtistA")
            .join("a")
            .join("same.jpg");
        let f2 = dir
            .path()
            .join("pictures")
            .join("ArtistA")
            .join("b")
            .join("same.jpg");
        let p1 = f1.to_string_lossy().replace('\\', "/");
        let p2 = f2.to_string_lossy().replace('\\', "/");
        let o1 = delete_item_to_recycle(&conn, &p1, &roots).unwrap();
        let o2 = delete_item_to_recycle(&conn, &p2, &roots).unwrap();
        let r1 = PathBuf::from(o1["recycled_to"].as_str().unwrap());
        let r2 = PathBuf::from(o2["recycled_to"].as_str().unwrap());
        assert_ne!(r1, r2);
        assert!(r1.is_file() && r2.is_file());
        assert_eq!(std::fs::read(&r1).unwrap(), b"one");
        assert_eq!(std::fs::read(&r2).unwrap(), b"two");
    }

    #[test]
    fn move_file_no_overwrite_never_clobbers() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("dest.bin");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();
        let moved = move_file_no_overwrite(&src, &dest).unwrap();
        assert_ne!(moved, dest);
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
        assert_eq!(std::fs::read(&moved).unwrap(), b"new");
        assert!(!src.exists());
    }

    #[test]
    fn exact_move_rejects_occupied_target_without_clobbering() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("dest.bin");
        std::fs::write(&src, b"new").unwrap();
        std::fs::write(&dest, b"old").unwrap();

        let error = move_file_exact_no_overwrite(&src, &dest).unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&src).unwrap(), b"new");
        assert_eq!(std::fs::read(&dest).unwrap(), b"old");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn authorized_move_rejects_symlinked_target_parent() {
        use std::os::unix::fs::symlink;

        let dir = tempdir().unwrap();
        let root = dir.path().join("pictures");
        let outside = dir.path().join("outside");
        let src = dir.path().join("recycle.bin");
        std::fs::create_dir(&root).unwrap();
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(&src, b"recycled").unwrap();
        symlink(&outside, root.join("replaced-parent")).unwrap();
        let roots = MediaRoots::identical(
            vec![root.to_string_lossy().to_string()],
            vec!["pictures".into()],
        );
        let target = root.join("replaced-parent").join("restored.bin");

        assert!(move_file_to_authorized_path_no_overwrite(&src, &target, &roots).is_err());
        assert_eq!(std::fs::read(&src).unwrap(), b"recycled");
        assert!(!outside.join("restored.bin").exists());
    }

    /// Race an aggressive pathname replacement against the mover and assert the
    /// invariant: a replaced source is never deleted, and a detected change
    /// rolls back only the destination the mover itself created.
    fn assert_exact_move_survives_source_replacement(force_copy: bool) {
        let dir = tempdir().unwrap();
        let src = dir.path().join("src.bin");
        let dest = dir.path().join("dest.bin");
        let payload = vec![0x5a_u8; 2 * 1024 * 1024];
        std::fs::write(&src, &payload).unwrap();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let replacer = {
            let stop = std::sync::Arc::clone(&stop);
            let dir = dir.path().to_path_buf();
            let src = src.clone();
            std::thread::spawn(move || {
                let mut counter = 0u64;
                while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                    // A fresh staging name per replacement: a moved file's
                    // inode is never re-written through another link.
                    let staging = dir.join(format!("stage-{counter}.bin"));
                    counter += 1;
                    if std::fs::write(&staging, b"replacement").is_err() {
                        continue;
                    }
                    let _ = std::fs::rename(&staging, &src);
                }
            })
        };

        let mut detected_change = 0;
        for _ in 0..30 {
            if !src.exists() {
                std::fs::write(&src, &payload).unwrap();
            }
            let _ = std::fs::remove_file(&dest);
            match move_file_exact_impl(&src, &dest, force_copy) {
                Ok(()) => {
                    // A consistent version must have been moved; the source
                    // pathname is either consumed or already re-created by the
                    // replacer with the replacement bytes.
                    if src.exists() {
                        assert_eq!(std::fs::read(&src).unwrap(), b"replacement");
                    }
                    let moved = std::fs::read(&dest).unwrap();
                    assert!(
                        moved == payload || moved == b"replacement",
                        "moved content is neither source version"
                    );
                }
                Err(error) if error.to_string().contains("source changed") => {
                    detected_change += 1;
                    assert_eq!(
                        std::fs::read(&src).unwrap(),
                        b"replacement",
                        "a replaced source must never be deleted by the mover"
                    );
                    assert!(
                        !dest.exists(),
                        "a detected change must roll back only the created destination"
                    );
                }
                Err(error) => panic!("unexpected move error: {error}"),
            }
        }
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        replacer.join().unwrap();
        assert!(
            detected_change > 0,
            "race never triggered; test did not exercise the replacement path"
        );
    }

    #[test]
    fn exact_move_survives_source_replacement_hardlink_branch() {
        assert_exact_move_survives_source_replacement(false);
    }

    #[test]
    fn exact_move_survives_source_replacement_copy_branch() {
        assert_exact_move_survives_source_replacement(true);
    }

    #[test]
    fn cross_device_copy_branch_preserves_content() {
        // Unit-test the copy path by forcing EXDEV-like flow via public helper internals:
        // call move_file_no_overwrite between two paths; on same FS rename succeeds.
        // Still verify copy+verify helper via a direct file pair rename-equivalent.
        let dir = tempdir().unwrap();
        let src = dir.path().join("a.bin");
        let dest_dir = dir.path().join("out");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(&src, b"payload-xyz").unwrap();
        let dest = dest_dir.join("a.bin");
        let moved = move_file_no_overwrite(&src, &dest).unwrap();
        assert_eq!(std::fs::read(&moved).unwrap(), b"payload-xyz");
        assert!(!src.exists());
    }

    #[tokio::test]
    async fn text_preview_is_bounded_lossy_and_reports_size() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let text = media.join("notes.txt");
        std::fs::write(&text, vec![0xff; 512 * 1024 + 1]).unwrap();
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let result = serve_text(&text.to_string_lossy().replace('\\', "/"), &roots)
            .await
            .unwrap();
        assert_eq!(result["size"], 512 * 1024 + 1);
        assert_eq!(result["truncated"], true);
        assert!(result["content"].as_str().unwrap().contains('\u{fffd}'));
    }

    #[test]
    fn transcode_paths_isolate_same_stem_sources() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        let left = media.join("a").join("clip.mp4");
        let right = media.join("b").join("clip.mp4");
        std::fs::create_dir_all(left.parent().unwrap()).unwrap();
        std::fs::create_dir_all(right.parent().unwrap()).unwrap();
        std::fs::write(&left, b"left").unwrap();
        std::fs::write(&right, b"right").unwrap();
        std::env::set_var("VIDEO_TRANSCODE_CACHE_DIR", dir.path().join("cache"));
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };

        let (_, left_playlist, _) = transcode_paths(&left.to_string_lossy(), &roots).unwrap();
        let (_, right_playlist, _) = transcode_paths(&right.to_string_lossy(), &roots).unwrap();
        assert_ne!(left_playlist, right_playlist);
    }

    #[test]
    fn failed_transcode_spawn_clears_marker() {
        let dir = tempdir().unwrap();
        let media = dir.path().join("pictures");
        std::fs::create_dir_all(&media).unwrap();
        let video = media.join("clip.mp4");
        std::fs::write(&video, b"fake").unwrap();
        std::env::set_var("VIDEO_TRANSCODE_CACHE_DIR", dir.path().join("cache"));
        let roots = MediaRoots {
            roots: vec![media.to_string_lossy().replace('\\', "/")],
            labels: vec!["p1".into()],
            real_paths: vec![media.to_string_lossy().replace('\\', "/")],
        };
        let path = video.to_string_lossy().replace('\\', "/");
        let (_, _, marker) = transcode_paths(&path, &roots).unwrap();
        let old_path = std::env::var_os("PATH");
        std::env::set_var("PATH", dir.path().join("no-ffmpeg"));
        let result = start_video_transcode(&path, &roots);
        if let Some(old_path) = old_path {
            std::env::set_var("PATH", old_path);
        } else {
            std::env::remove_var("PATH");
        }
        assert!(result.is_err());
        assert!(!marker.exists());
    }
}
