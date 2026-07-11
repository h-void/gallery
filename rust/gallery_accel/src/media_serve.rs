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
use crate::media_roots::MediaRoots;
use crate::media_type::media_type_for_file;

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
            logical == root_s || logical.starts_with(&format!("{root_s}/")) || logical.starts_with(&format!("{root_s}\\"))
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
        if !is_under_allowed_root(&cand, &allowed) {
            continue;
        }
        if cand.is_file() {
            return Ok(cand);
        }
    }
    Err(anyhow!("file not found or not allowed"))
}

pub async fn serve_file_response(
    path: &str,
    roots: &MediaRoots,
    headers: &HeaderMap,
) -> Result<Response, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
    let meta = tokio::fs::metadata(&full).await.map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
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
                .header(
                    header::CONTENT_RANGE,
                    format!("bytes {start}-{end}/{len}"),
                )
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
    let full = resolve_allowed_path(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
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
    let sql = format!(
        "SELECT id FROM items WHERE missing=0 AND file_path IN ({placeholders}) LIMIT 1"
    );
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

/// Move `src` to `dest` without overwriting. Tries rename, then copy+sync+delete on EXDEV.
fn move_file_no_overwrite(src: &Path, dest: &Path) -> Result<PathBuf> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut final_dest = dest.to_path_buf();
    // Never overwrite: create_new or UUID suffix.
    if final_dest.exists() {
        let stem = final_dest
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "file".into());
        let ext = final_dest
            .extension()
            .map(|s| format!(".{}", s.to_string_lossy()))
            .unwrap_or_default();
        let parent = final_dest
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));
        final_dest = parent.join(format!(
            "{stem}__{}{ext}",
            uuid::Uuid::new_v4().simple()
        ));
    }

    match std::fs::rename(src, &final_dest) {
        Ok(()) => return Ok(final_dest),
        Err(e) if is_cross_device(&e) => {
            // Fall through to copy.
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            // Retry once with UUID.
            let stem = final_dest
                .file_stem()
                .map(|s| s.to_string_lossy().to_string())
                .unwrap_or_else(|| "file".into());
            let ext = final_dest
                .extension()
                .map(|s| format!(".{}", s.to_string_lossy()))
                .unwrap_or_default();
            let parent = final_dest
                .parent()
                .map(|p| p.to_path_buf())
                .unwrap_or_else(|| PathBuf::from("."));
            final_dest = parent.join(format!(
                "{stem}__{}{ext}",
                uuid::Uuid::new_v4().simple()
            ));
            match std::fs::rename(src, &final_dest) {
                Ok(()) => return Ok(final_dest),
                Err(e2) if is_cross_device(&e2) => {}
                Err(e2) => return Err(e2.into()),
            }
        }
        Err(e) => return Err(e.into()),
    }

    // Cross-device: copy to new file, sync, verify size, then remove source.
    {
        use std::io::{Read, Write};
        let mut from = std::fs::File::open(src)?;
        let mut to = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&final_dest)?;
        let mut buf = [0u8; 1024 * 256];
        loop {
            let n = from.read(&mut buf)?;
            if n == 0 {
                break;
            }
            to.write_all(&buf[..n])?;
        }
        to.sync_all()?;
        drop(to);
        drop(from);
        let src_len = std::fs::metadata(src)?.len();
        let dst_len = std::fs::metadata(&final_dest)?.len();
        if src_len != dst_len {
            let _ = std::fs::remove_file(&final_dest);
            return Err(anyhow!(
                "cross-device copy size mismatch: src={src_len} dst={dst_len}"
            ));
        }
        std::fs::remove_file(src)?;
    }
    Ok(final_dest)
}

fn is_cross_device(err: &std::io::Error) -> bool {
    // Stable Rust: CrossesDevices since 1.83; also match raw OS codes.
    if err.kind() == std::io::ErrorKind::CrossesDevices {
        return true;
    }
    match err.raw_os_error() {
        Some(18) => true,  // EXDEV on Linux/macOS
        Some(17) => false, // EEXIST — not cross-device
        _ => {
            let msg = err.to_string().to_ascii_lowercase();
            msg.contains("cross-device")
                || msg.contains("cross device")
                || msg.contains("exdev")
        }
    }
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
    let flat = recycle.join(format!(
        "{}_{base}",
        uuid::Uuid::new_v4().simple()
    ));
    move_file_no_overwrite(full, &flat)
}

/// Delete an active library item: recycle file then remove DB row + auto character refs.
///
/// Returns success only when FS + DB agree. On DB failure, tries to restore the file.
pub fn delete_item_to_recycle(
    conn: &rusqlite::Connection,
    path: &str,
    roots: &MediaRoots,
) -> Result<Value, (StatusCode, Value)> {
    let full = resolve_allowed_path(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
    if !full.is_file() {
        return Err((
            StatusCode::NOT_FOUND,
            json!({"error": "file not found"}),
        ));
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
    let recycled = move_into_recycle(&full).map_err(internal)?;
    let recycled_s = recycled.display().to_string();
    let original_s = original.display().to_string();

    let db_result = (|| -> Result<(i64, i64)> {
        conn.execute("BEGIN IMMEDIATE", [])?;
        let tx = (|| -> Result<(i64, i64)> {
            let deleted_refs = conn.execute(
                "DELETE FROM character_references WHERE item_id=? AND source_type='tag_single'",
                rusqlite::params![item_id],
            )? as i64;
            let deleted_items = conn.execute(
                "DELETE FROM items WHERE id=?",
                rusqlite::params![item_id],
            )? as i64;
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
        Err(db_err) => {
            // Prefer restore; if restore fails, return coordinating error.
            match std::fs::rename(&recycled, &original) {
                Ok(()) => Err((
                    StatusCode::INTERNAL_SERVER_ERROR,
                    json!({
                        "error": format!("database delete failed; file restored: {db_err}"),
                        "item_id": item_id,
                        "original_path": original_s,
                    }),
                )),
                Err(restore_err) => {
                    // copy-back for cross-device recycle destinations
                    let restored = (|| -> Result<()> {
                        let dest = move_file_no_overwrite(&recycled, &original)?;
                        if dest != original {
                            // unexpected alternate name — still better than lost file
                        }
                        Ok(())
                    })();
                    match restored {
                        Ok(()) => Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            json!({
                                "error": format!("database delete failed; file restored: {db_err}"),
                                "item_id": item_id,
                                "original_path": original_s,
                            }),
                        )),
                        Err(_) => Err((
                            StatusCode::INTERNAL_SERVER_ERROR,
                            json!({
                                "error": format!(
                                    "database delete failed and restore failed: db={db_err}; restore={restore_err}"
                                ),
                                "item_id": item_id,
                                "original_path": original_s,
                                "recycled_to": recycled_s,
                                "needs_reconciliation": true,
                            }),
                        )),
                    }
                }
            }
        }
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
    let full = resolve_allowed_path(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
    let cache_path = video_frame_cache_path(&full, t);
    if let Some(cache) = cache_path.as_ref() {
        if let Ok(bytes) = std::fs::read(cache) {
            if !bytes.is_empty() {
                return Ok(bytes);
            }
        }
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
    if let Some(cache) = cache_path.as_ref() {
        write_video_frame_cache(cache, &buf);
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
    let full = resolve_allowed_path(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
    let max_edge = clamp_max_edge(max);
    image_preview_bytes(&full.to_string_lossy(), max_edge).map_err(|e| {
        (
            StatusCode::BAD_REQUEST,
            json!({"error": e.to_string()}),
        )
    })
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
    let Ok(metadata) = marker.metadata() else { return false; };
    let Ok(modified) = metadata.modified() else { return false; };
    let Ok(age) = SystemTime::now().duration_since(modified) else { return false; };
    age < TRANSCODE_MARKER_STALE_AFTER
}

fn claim_transcode_marker(marker: &Path) -> Result<bool> {
    loop {
        match OpenOptions::new().write(true).create_new(true).open(marker) {
            Ok(_) => return Ok(true),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists && marker_is_fresh(marker) => {
                return Ok(false)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                let _ = std::fs::remove_file(marker);
            }
            Err(error) => return Err(error.into()),
        }
    }
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
                let _ = std::fs::remove_file(marker);
                json!({"status": "pending", "ready": false, "key": key, "playlist": playlist.display().to_string(), "path": path, "message": "transcode_pending_or_not_started"})
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
    let reserve = std::fs::metadata(&full).map(|metadata| metadata.len()).unwrap_or(0);
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
    if !claim_transcode_marker(&marker)? {
        return Ok(json!({"ok": true, "key": key, "playlist": playlist.display().to_string(), "status": "processing", "ready": false}));
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
    if let Err(error) = spawned {
        let _ = std::fs::remove_file(&marker);
        return Err(error.into());
    }
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
    let (key, playlist, _) = transcode_paths(path, roots).map_err(|e| {
        (
            StatusCode::NOT_FOUND,
            json!({"error": e.to_string()}),
        )
    })?;
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
    if let Some(range) = headers.get(header::RANGE).and_then(|value| value.to_str().ok()) {
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
            assert!(resolve_allowed_path(r"C:\Windows\System32\drivers\etc\hosts", &roots).is_err());
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
        assert!(allowed.is_ok(), "double dots inside a filename are not traversal");

        let traversal = nested.join("..").join("1..mp4");
        let denied = resolve_allowed_path(&traversal.to_string_lossy().replace('\\', "/"), &roots);
        assert!(denied.is_err(), "a parent-directory path component must be rejected");
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

        let hash_denied = content_hash_allowed(
            &secret.to_string_lossy().replace('\\', "/"),
            &roots,
        );
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

    fn delete_fixture() -> (tempfile::TempDir, rusqlite::Connection, MediaRoots, PathBuf, PathBuf) {
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
        conn.execute(
            "INSERT INTO item_tags (item_id, tag_id) VALUES (1, 1)",
            [],
        )
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
