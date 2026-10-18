// Video frame extraction and HLS transcoding for media_serve.
//
// Split out of media_serve.rs to keep each review surface bounded. Every
// name here stays reachable as `crate::media_serve::*` through the glob
// re-exports in media_serve.rs, and `use super::*` keeps the parent helpers,
// constants and imports in scope exactly as before the split.
use super::*;
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

pub(crate) fn video_frame_cache_path(full: &Path, t: f64) -> Option<PathBuf> {
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

pub(crate) fn cleanup_video_frame_cache(
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
        // Crash leftovers are named `<name>.part-<pid>-<seq>`, so
        // `extension()` never yields `part` and the old test was dead: the
        // leftovers were neither reclaimed nor counted against the budget.
        let part_name = entry.file_name().to_string_lossy().into_owned();
        if part_name.ends_with(".part") || part_name.contains(".part-") {
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
        if entry.path().extension().and_then(|ext| ext.to_str()) != Some("jpg") {
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

pub(crate) fn cleanup_transcode_cache(
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

pub(crate) fn transcode_paths(
    path: &str,
    roots: &MediaRoots,
) -> Result<(String, PathBuf, PathBuf)> {
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

pub(crate) fn finish_video_transcode(
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
    // A failed acquire has to release the marker again: leaving it behind made
    // every retry short-circuit as "processing" until it went stale an hour
    // later, with no ffmpeg ever spawned.
    let ffmpeg_slot = match FfmpegSlotGuard::acquire_blocking() {
        Ok(slot) => slot,
        Err(error) => {
            let _ = std::fs::remove_file(&marker);
            return Err(error);
        }
    };
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
