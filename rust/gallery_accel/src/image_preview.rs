use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use image::DynamicImage;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

const PREVIEW_BG: [u8; 3] = [15, 23, 42];
const DEFAULT_QUALITY: u8 = 72;
pub const DEFAULT_MAX_EDGE: u32 = 512;
const MIN_EDGE: u32 = 64;
const MAX_EDGE_LIMIT: u32 = 2048;
const PREVIEW_CACHE_VERSION: u32 = 1;
const DEFAULT_MAX_SOURCE_PIXELS: u64 = 89_478_485;
const PREVIEW_CACHE_CLEANUP_INTERVAL: u64 = 300;
static PREVIEW_CACHE_LAST_CLEANUP: AtomicU64 = AtomicU64::new(0);

fn max_source_pixels() -> u64 {
    std::env::var("IMAGE_PREVIEW_MAX_SOURCE_PIXELS")
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_SOURCE_PIXELS)
}

fn quality() -> u8 {
    std::env::var("IMAGE_PREVIEW_QUALITY")
        .ok()
        .and_then(|value| value.trim().parse::<u8>().ok())
        .unwrap_or(DEFAULT_QUALITY)
        .clamp(1, 100)
}

fn preview_cache_root() -> Option<PathBuf> {
    let root = std::env::var("IMAGE_PREVIEW_CACHE_DIR").ok()?;
    let root = root.trim();
    if root.is_empty() {
        return None;
    }
    let path = PathBuf::from(root);
    let _ = std::fs::create_dir_all(&path);
    Some(path)
}

fn preview_cache_max_bytes() -> u64 {
    std::env::var("IMAGE_PREVIEW_CACHE_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(10_000_000_000)
}

fn file_mtime_ns_size(path: &Path) -> Option<(i64, u64)> {
    let meta = std::fs::metadata(path).ok()?;
    let size = meta.len();
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        // Match Python st_mtime_ns (seconds*1e9 + nsec).
        let ns = (meta.mtime() as i64)
            .saturating_mul(1_000_000_000)
            .saturating_add(meta.mtime_nsec() as i64);
        return Some((ns, size));
    }
    #[cfg(not(unix))]
    {
        use std::time::SystemTime;
        let mtime = meta.modified().ok()?;
        let ns = mtime
            .duration_since(SystemTime::UNIX_EPOCH)
            .ok()?
            .as_nanos() as i64;
        Some((ns, size))
    }
}

/// Match Python `app/api/files.py:_image_preview_cache_path` key + layout.
/// Cache is only enabled for the default max_edge (512) like Python.
pub fn preview_cache_path_for_source(full: &Path, max_edge: u32) -> Option<PathBuf> {
    if max_edge != DEFAULT_MAX_EDGE || preview_cache_max_bytes() == 0 {
        return None;
    }
    let root = preview_cache_root()?;
    let full = full.canonicalize().ok()?;
    let (mtime_ns, size) = file_mtime_ns_size(&full)?;
    let path_str = full.to_string_lossy().replace('\\', "/");
    // Python: json.dumps(key_data, sort_keys=True, ensure_ascii=False)
    // separators default → spaces after ':' and ','.
    let path_json = serde_json::to_string(&path_str).ok()?;
    let payload = format!(
        "{{\"format\": \"jpeg\", \"max_edge\": {}, \"mtime_ns\": {}, \"path\": {}, \"quality\": {}, \"size\": {}, \"version\": {}}}",
        max_edge,
        mtime_ns,
        path_json,
        quality(),
        size,
        PREVIEW_CACHE_VERSION
    );
    let mut hasher = Sha256::new();
    hasher.update(payload.as_bytes());
    let key = format!("{:x}", hasher.finalize());
    if key.len() < 4 {
        return None;
    }
    Some(
        root.join(&key[..2])
            .join(&key[2..4])
            .join(format!("{key}.jpg")),
    )
}

/// Return existing on-disk preview JPEG if the shared cache has one.
pub fn existing_preview_cache_file(full: &Path) -> Option<PathBuf> {
    let cache = preview_cache_path_for_source(full, DEFAULT_MAX_EDGE)?;
    if cache.is_file() {
        Some(cache)
    } else {
        None
    }
}

fn write_preview_cache(cache_path: &Path, body: &[u8]) {
    if let Some(root) = preview_cache_root() {
        maybe_cleanup_preview_cache(&root, body.len() as u64);
    }
    if let Some(parent) = cache_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let part = cache_path.with_extension("jpg.part");
    if std::fs::write(&part, body).is_ok() {
        let _ = std::fs::rename(&part, cache_path);
    } else {
        let _ = std::fs::remove_file(&part);
    }
}

/// Clamp the requested preview edge to the allowed range, mirroring
/// `app/api/files.py` (`max(64, min(parsed, IMAGE_PREVIEW_MAX_EDGE_LIMIT))`).
pub fn clamp_max_edge(requested: Option<u32>) -> u32 {
    let raw = requested.unwrap_or(DEFAULT_MAX_EDGE);
    raw.clamp(MIN_EDGE, MAX_EDGE_LIMIT)
}

fn exif_orientation(path: &Path) -> u8 {
    read_jpeg_exif_orientation(path).unwrap_or(1)
}

/// Minimal dependency-free reader for the EXIF `Orientation` tag (0x0112) of a
/// JPEG file. Returns 1 when no orientation is present or parsing fails, which
/// matches the "normal" orientation.
fn read_jpeg_exif_orientation(path: &Path) -> Option<u8> {
    let data = std::fs::read(path).ok()?;
    if data.len() < 4 || &data[0..2] != &[0xFF, 0xD8] {
        return None;
    }
    let mut pos = 2usize;
    while pos + 4 <= data.len() {
        if data[pos] != 0xFF {
            return None;
        }
        let marker = data[pos + 1];
        let seg_len = u16::from_be_bytes([data[pos + 2], data[pos + 3]]) as usize;
        if marker == 0xDA {
            return None;
        }
        if marker == 0xE1 {
            let exif_start = pos + 4;
            if data[exif_start..exif_start + 6]
                .iter()
                .eq(b"Exif\0\0".iter())
            {
                return parse_tiff_orientation(&data[exif_start + 6..]);
            }
            return None;
        }
        pos += 2 + seg_len;
    }
    None
}

fn parse_tiff_orientation(tiff: &[u8]) -> Option<u8> {
    if tiff.len() < 8 {
        return None;
    }
    let little = match &tiff[0..2] {
        b"II" => true,
        b"MM" => false,
        _ => return None,
    };
    let read_u16 = |offset: usize| -> Option<u16> {
        if offset + 2 > tiff.len() {
            return None;
        }
        Some(if little {
            u16::from_le_bytes([tiff[offset], tiff[offset + 1]])
        } else {
            u16::from_be_bytes([tiff[offset], tiff[offset + 1]])
        })
    };
    let read_u32 = |offset: usize| -> Option<u32> {
        if offset + 4 > tiff.len() {
            return None;
        }
        let bytes = [
            tiff[offset],
            tiff[offset + 1],
            tiff[offset + 2],
            tiff[offset + 3],
        ];
        Some(if little {
            u32::from_le_bytes(bytes)
        } else {
            u32::from_be_bytes(bytes)
        })
    };
    let _magic = read_u16(2)?;
    let ifd0 = read_u32(4)? as usize;
    let entry_count = read_u16(ifd0)? as usize;
    let entries_start = ifd0 + 2;
    for i in 0..entry_count {
        let entry = entries_start + i * 12;
        if entry + 12 > tiff.len() {
            return None;
        }
        let tag = read_u16(entry)?;
        if tag == 0x0112 {
            return Some((read_u16(entry + 8)? & 0xFF) as u8);
        }
    }
    None
}

fn apply_orientation(img: DynamicImage, orientation: u8) -> DynamicImage {
    match orientation {
        1 => img,
        2 => img.fliph(),
        3 => img.rotate180(),
        4 => img.flipv(),
        5 => img.rotate90().fliph(),
        6 => img.rotate90(),
        7 => img.rotate270().fliph(),
        8 => img.rotate270(),
        _ => img,
    }
}

/// Render a JPEG preview of `path` with the longest edge at most `max_edge`,
/// mirroring `app/api/files.py:_image_to_jpeg_preview`.
///
/// When `max_edge` is the default (512), reads/writes `IMAGE_PREVIEW_CACHE_DIR`
/// using the same key layout as Python so recognition can reuse thumbs.
pub fn image_preview_bytes(path: &str, max_edge: u32) -> Result<Vec<u8>> {
    let path = Path::new(path);
    if let Some(cache) = preview_cache_path_for_source(path, max_edge) {
        if cache.is_file() {
            if let Ok(bytes) = std::fs::read(&cache) {
                if !bytes.is_empty() {
                    return Ok(bytes);
                }
            }
        }
    }
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("open image for preview: {}", path.display()))?
        .with_guessed_format()
        .context("identify image format")?;
    let (width, height) = reader.into_dimensions().context("read image dimensions")?;
    if width == 0 || height == 0 {
        anyhow::bail!("invalid image dimensions");
    }
    let source_limit = max_source_pixels();
    if source_limit > 0 && (width as u64) * (height as u64) > source_limit {
        anyhow::bail!("image is too large for preview");
    }
    let img =
        image::open(path).with_context(|| format!("decode image for preview: {}", path.display()))?;

    let oriented = apply_orientation(img, exif_orientation(path));
    let thumb = oriented.thumbnail(max_edge, max_edge);

    let rgb = if matches!(
        thumb.color(),
        image::ColorType::Rgba8 | image::ColorType::La8
    ) || thumb.color().has_alpha()
    {
        let rgba = thumb.to_rgba8();
        let rgba_dyn = DynamicImage::ImageRgba8(rgba.clone());
        let mut base = DynamicImage::ImageRgb8(image::RgbImage::from_pixel(
            rgba.width(),
            rgba.height(),
            image::Rgb(PREVIEW_BG),
        ));
        image::imageops::overlay(&mut base, &rgba_dyn, 0, 0);
        base.to_rgb8()
    } else {
        thumb.to_rgb8()
    };

    let mut out = Cursor::new(Vec::new());
    {
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, quality());
        encoder
            .encode_image(&DynamicImage::ImageRgb8(rgb))
            .context("encode jpeg preview")?;
    }
    let bytes = out.into_inner();
    if let Some(cache) = preview_cache_path_for_source(path, max_edge) {
        write_preview_cache(&cache, &bytes);
    }
    Ok(bytes)
}

/// JSON envelope used by the HTTP route (callers that want raw bytes use
/// `image_preview_bytes` directly).
pub fn image_preview_response(path: &str, max_edge: u32) -> Result<Value> {
    let bytes = image_preview_bytes(path, max_edge)?;
    Ok(json!({
        "path": path,
        "max_edge": max_edge,
        "bytes": bytes,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::GenericImageView;

    #[test]
    fn preview_renders_valid_jpeg_within_bounds() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("in.png");
        let img = DynamicImage::new_rgba8(800, 600);
        img.save(&src).unwrap();

        let bytes = image_preview_bytes(src.to_str().unwrap(), 256).unwrap();
        assert!(bytes.len() > 0);
        let decoded = image::load_from_memory(&bytes).unwrap();
        let (w, h) = decoded.dimensions();
        assert!(w <= 256 && h <= 256);
        assert_eq!(w, 256);
    }

    #[test]
    fn clamp_max_edge_respects_limits() {
        assert_eq!(clamp_max_edge(None), 512);
        assert_eq!(clamp_max_edge(Some(10)), 64);
        assert_eq!(clamp_max_edge(Some(9999)), 2048);
        assert_eq!(clamp_max_edge(Some(200)), 200);
    }

    #[test]
    fn source_pixel_limit_defaults_to_pillow_warning_threshold() {
        std::env::remove_var("IMAGE_PREVIEW_MAX_SOURCE_PIXELS");
        assert_eq!(max_source_pixels(), 89_478_485);
    }

    #[test]
    fn preview_cache_key_matches_python_layout() {
        // Payload format must match Python json.dumps(sort_keys=True).
        let path_json = serde_json::to_string("/a/b.jpg").unwrap();
        let payload = format!(
            "{{\"format\": \"jpeg\", \"max_edge\": 512, \"mtime_ns\": 123, \"path\": {}, \"quality\": 72, \"size\": 456, \"version\": 1}}",
            path_json
        );
        let mut hasher = Sha256::new();
        hasher.update(payload.as_bytes());
        let key = format!("{:x}", hasher.finalize());
        assert_eq!(
            key,
            "84e369b64b5217f0186b24c1d0e7b6cf2a272b46efe68796f90f19658d27ed81"
        );
    }

    #[test]
    fn preview_cache_cleanup_evicts_oldest_files() {
        let dir = tempfile::tempdir().unwrap();
        let cache_dir = dir.path().join("aa").join("bb");
        std::fs::create_dir_all(&cache_dir).unwrap();
        let old = cache_dir.join("old.jpg");
        let new = cache_dir.join("new.jpg");
        std::fs::write(&old, b"12345678").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        std::fs::write(&new, b"abcdefgh").unwrap();

        let removed = cleanup_preview_cache(dir.path(), 12, 0).unwrap();
        assert_eq!(removed, 1);
        assert!(!old.exists());
        assert!(new.exists());
    }
}

fn maybe_cleanup_preview_cache(root: &Path, reserve_bytes: u64) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let last = PREVIEW_CACHE_LAST_CLEANUP.load(Ordering::Relaxed);
    if last > 0 && now.saturating_sub(last) < PREVIEW_CACHE_CLEANUP_INTERVAL {
        return;
    }
    if PREVIEW_CACHE_LAST_CLEANUP
        .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
        .is_err()
    {
        return;
    }
    let _ = cleanup_preview_cache(root, preview_cache_max_bytes(), reserve_bytes);
}

fn cleanup_preview_cache(
    root: &Path,
    max_bytes: u64,
    reserve_bytes: u64,
) -> std::io::Result<usize> {
    let mut total = 0u64;
    let mut entries = Vec::new();
    for entry in walkdir::WalkDir::new(root)
        .max_depth(3)
        .follow_links(false)
        .into_iter()
        .filter_map(|entry| entry.ok())
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
