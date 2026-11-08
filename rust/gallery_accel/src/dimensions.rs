use anyhow::{Context, Result};
use rusqlite::{params, Connection};
use serde_json::{json, Value};
use std::fs::File;
use std::io::{BufReader, Cursor, Read, Seek};
use std::path::Path;
use std::process::{Command, Stdio};

use crate::media_roots::MediaRoots;
use crate::media_serve::resolve_allowed_path;

const MAX_BATCH_SIZE: i64 = 64;
const IMAGE_DIMENSION_READ_LIMIT: u64 = 1024 * 1024;

struct DimensionRow {
    id: i64,
    file_path: String,
    media_type: String,
}

/// Fill missing intrinsic dimensions without touching media files.
///
/// `after_id` makes a batch resumable without storing job state in SQLite;
/// failed rows remain eligible for a later pass.
pub fn backfill_item_dimensions(
    conn: &Connection,
    roots: &MediaRoots,
    after_id: i64,
    requested_limit: i64,
) -> Result<Value> {
    let limit = requested_limit.clamp(1, MAX_BATCH_SIZE);
    let after_id = after_id.max(0);
    let rows = {
        let mut stmt = conn.prepare(
            "SELECT id, file_path, media_type
             FROM items
             WHERE id > ?
               AND missing=0
               AND media_type IN ('image', 'video')
               AND (width <= 0 OR height <= 0)
             ORDER BY id
             LIMIT ?",
        )?;
        let mapped = stmt.query_map(params![after_id, limit], |row| {
            Ok(DimensionRow {
                id: row.get("id")?,
                file_path: row.get("file_path")?,
                media_type: row.get("media_type")?,
            })
        })?;
        mapped.collect::<rusqlite::Result<Vec<_>>>()?
    };

    let mut updates = Vec::new();
    let mut failed = 0;
    for row in &rows {
        let path = match resolve_allowed_path(&row.file_path, roots) {
            Ok(path) => path,
            Err(_) => {
                failed += 1;
                continue;
            }
        };
        match media_dimensions(&path, &row.media_type) {
            Ok((width, height)) if width > 0 && height > 0 => {
                updates.push((row.id, width as i64, height as i64));
            }
            Ok(_) | Err(_) => failed += 1,
        }
    }

    let transaction = conn.unchecked_transaction()?;
    let mut updated = 0;
    for (id, width, height) in updates {
        updated += transaction.execute(
            "UPDATE items
             SET width=?, height=?
             WHERE id=? AND missing=0 AND (width <= 0 OR height <= 0)",
            params![width, height, id],
        )?;
    }
    transaction.commit()?;

    // Exact on every batch: the route publishes it as progress and only the
    // final batch may conclude `complete`. (Skipping it on intermediate batches
    // would be cheaper — there is no index over media_type/width/height — but
    // the reported number is part of the response contract.)
    let remaining: i64 = conn.query_row(
        "SELECT COUNT(*) FROM items
         WHERE missing=0
           AND media_type IN ('image', 'video')
           AND (width <= 0 OR height <= 0)",
        [],
        |row| row.get(0),
    )?;
    let next_after_id = rows.last().map(|row| row.id);
    Ok(json!({
        "ok": true,
        "processed": rows.len(),
        "updated": updated,
        "failed": failed,
        "remaining": remaining,
        "next_after_id": next_after_id,
        "cursor_done": rows.len() < limit as usize,
        "complete": remaining == 0,
    }))
}

pub(crate) fn media_dimensions(path: &Path, media_type: &str) -> Result<(u32, u32)> {
    match media_type {
        "image" => image_dimensions(path),
        "video" => video_dimensions(path),
        _ => anyhow::bail!("unsupported media type"),
    }
}

fn fast_header_dimensions(prefix: &[u8]) -> Option<(u32, u32)> {
    if prefix.len() >= 10 && (&prefix[..6] == b"GIF87a" || &prefix[..6] == b"GIF89a") {
        let width = u32::from(u16::from_le_bytes([prefix[6], prefix[7]]));
        let height = u32::from(u16::from_le_bytes([prefix[8], prefix[9]]));
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }
    if prefix.len() >= 24 && &prefix[..8] == b"\x89PNG\r\n\x1a\n" && &prefix[12..16] == b"IHDR" {
        let width = u32::from_be_bytes([prefix[16], prefix[17], prefix[18], prefix[19]]);
        let height = u32::from_be_bytes([prefix[20], prefix[21], prefix[22], prefix[23]]);
        if width > 0 && height > 0 {
            return Some((width, height));
        }
    }
    if prefix.len() >= 30 && &prefix[..4] == b"RIFF" && &prefix[8..12] == b"WEBP" {
        match &prefix[12..16] {
            b"VP8 " if prefix[23] == 0x9D && prefix[24] == 0x01 && prefix[25] == 0x2A => {
                let width = u32::from(u16::from_le_bytes([prefix[26], prefix[27]]) & 0x3FFF);
                let height = u32::from(u16::from_le_bytes([prefix[28], prefix[29]]) & 0x3FFF);
                if width > 0 && height > 0 {
                    return Some((width, height));
                }
            }
            b"VP8L" if prefix[20] == 0x2F => {
                let bits = u32::from_le_bytes([prefix[21], prefix[22], prefix[23], prefix[24]]);
                let width = (bits & 0x3FFF) + 1;
                let height = ((bits >> 14) & 0x3FFF) + 1;
                if width > 0 && height > 0 {
                    return Some((width, height));
                }
            }
            b"VP8X" => {
                let width = u32::from_le_bytes([prefix[24], prefix[25], prefix[26], 0]) + 1;
                let height = u32::from_le_bytes([prefix[27], prefix[28], prefix[29], 0]) + 1;
                if width > 0 && height > 0 {
                    return Some((width, height));
                }
            }
            _ => {}
        }
    }
    None
}

fn fast_jpeg_sof_dimensions(file: &mut File) -> Option<(u32, u32)> {
    let mut reader = BufReader::with_capacity(8192, file);
    let mut soi = [0u8; 2];
    if reader.read_exact(&mut soi).is_err() || soi != [0xFF, 0xD8] {
        return None;
    }
    let mut scanned = 2u64;
    let mut byte = [0u8; 1];
    while scanned < IMAGE_DIMENSION_READ_LIMIT {
        if reader.read_exact(&mut byte).is_err() {
            return None;
        }
        scanned += 1;
        if byte[0] != 0xFF {
            continue;
        }
        let marker = loop {
            if reader.read_exact(&mut byte).is_err() {
                return None;
            }
            scanned += 1;
            if byte[0] != 0xFF {
                break byte[0];
            }
        };
        if marker == 0x00 || (0xD0..=0xD8).contains(&marker) || marker == 0x01 {
            continue;
        }
        if marker == 0xD9 || marker == 0xDA {
            return None;
        }
        let mut len_bytes = [0u8; 2];
        if reader.read_exact(&mut len_bytes).is_err() {
            return None;
        }
        scanned += 2;
        let seg_len = u16::from_be_bytes(len_bytes) as u64;
        if seg_len < 2 {
            return None;
        }
        if matches!(
            marker,
            0xC0..=0xC3 | 0xC5..=0xC7 | 0xC9..=0xCB | 0xCD..=0xCF
        ) {
            if seg_len < 7 {
                return None;
            }
            let mut sof = [0u8; 5];
            if reader.read_exact(&mut sof).is_err() {
                return None;
            }
            let height = u32::from(u16::from_be_bytes([sof[1], sof[2]]));
            let width = u32::from(u16::from_be_bytes([sof[3], sof[4]]));
            if width > 0 && height > 0 {
                return Some((width, height));
            }
            return None;
        }
        let skip = seg_len - 2;
        if reader
            .seek(std::io::SeekFrom::Current(skip as i64))
            .is_err()
        {
            return None;
        }
        scanned = scanned.saturating_add(skip);
    }
    None
}

fn image_dimensions(path: &Path) -> Result<(u32, u32)> {
    let mut file =
        File::open(path).with_context(|| format!("open image dimensions: {}", path.display()))?;
    let mut header = [0u8; 30];
    let header_len = file
        .read(&mut header)
        .with_context(|| format!("read image header: {}", path.display()))?;
    if let Some(dimensions) = fast_header_dimensions(&header[..header_len]) {
        return Ok(dimensions);
    }
    file.seek(std::io::SeekFrom::Start(0))
        .with_context(|| format!("rewind image dimensions: {}", path.display()))?;

    // `image` 0.25 buffers JPEG input to EOF before returning dimensions.
    // Scan JPEG SOF markers first without allocating 1 MiB, then fall back to a
    // bounded prefix and finally the full reader.
    let is_jpeg_magic = header_len >= 2 && header[0] == 0xFF && header[1] == 0xD8;
    if is_jpeg_magic || is_jpeg_path(path) {
        if is_jpeg_magic {
            if let Some(dimensions) = fast_jpeg_sof_dimensions(&mut file) {
                return Ok(dimensions);
            }
            file.seek(std::io::SeekFrom::Start(0))
                .with_context(|| format!("rewind image dimensions: {}", path.display()))?;
        }
        let prefix = read_prefix(file, IMAGE_DIMENSION_READ_LIMIT)
            .with_context(|| format!("read image dimensions: {}", path.display()))?;
        if let Ok(dimensions) = image_dimensions_from_reader(Cursor::new(prefix)) {
            return Ok(dimensions);
        }
        let file = File::open(path)
            .with_context(|| format!("open image dimensions: {}", path.display()))?;
        let reader = image::ImageReader::new(BufReader::new(file))
            .with_guessed_format()
            .context("identify image format")?;
        return reader.into_dimensions().context("read image dimensions");
    }
    let reader = image::ImageReader::new(BufReader::new(file))
        .with_guessed_format()
        .context("identify image format")?;
    reader.into_dimensions().context("read image dimensions")
}

fn is_jpeg_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(extension) if extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
            || extension.eq_ignore_ascii_case("jpe")
            || extension.eq_ignore_ascii_case("jfif")
    )
}

fn image_dimensions_from_reader<R>(reader: R) -> Result<(u32, u32)>
where
    R: Read + Seek,
{
    let reader = image::ImageReader::new(BufReader::new(reader))
        .with_guessed_format()
        .context("identify image format")?;
    reader.into_dimensions().context("read image dimensions")
}

fn read_prefix<R>(reader: R, limit: u64) -> std::io::Result<Vec<u8>>
where
    R: Read,
{
    let mut prefix = Vec::new();
    reader.take(limit).read_to_end(&mut prefix)?;
    Ok(prefix)
}

fn video_dimensions(path: &Path) -> Result<(u32, u32)> {
    // A hung probe (flapping mount, pathological file) must not pin its
    // backfill thread and a pooled connection forever: bound the whole probe
    // and kill the child past the deadline. Pipes are drained on threads so a
    // chatty probe can never fill them and deadlock the wait.
    const FFPROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
    let mut child = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-nostdin",
            "-select_streams",
            "v:0",
            "-analyzeduration",
            "1000000",
            "-probesize",
            "2000000",
            "-show_entries",
            "stream=width,height",
            "-of",
            "csv=p=0:s=x",
        ])
        .arg(path)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .context("spawn ffprobe")?;
    let mut stdout_pipe = child.stdout.take().context("ffprobe stdout")?;
    let mut stderr_pipe = child.stderr.take().context("ffprobe stderr")?;
    let stdout_drain = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        stdout_pipe.read_to_end(&mut buffer)?;
        Ok(buffer)
    });
    let stderr_drain = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut buffer = Vec::new();
        stderr_pipe.read_to_end(&mut buffer)?;
        Ok(buffer)
    });
    let deadline = std::time::Instant::now() + FFPROBE_TIMEOUT;
    let status = loop {
        match child.try_wait()? {
            Some(status) => break Some(status),
            None if std::time::Instant::now() >= deadline => break None,
            None => std::thread::sleep(std::time::Duration::from_millis(20)),
        }
    };
    let Some(status) = status else {
        let _ = child.kill();
        let _ = child.wait();
        anyhow::bail!("ffprobe timed out");
    };
    let stdout = stdout_drain
        .join()
        .map_err(|_| anyhow::anyhow!("ffprobe stdout reader failed"))?
        .context("read ffprobe stdout")?;
    let _stderr = stderr_drain
        .join()
        .map_err(|_| anyhow::anyhow!("ffprobe stderr reader failed"))?
        .context("read ffprobe stderr")?;
    if !status.success() {
        anyhow::bail!("ffprobe failed");
    }
    let dimensions = String::from_utf8_lossy(&stdout);
    let first_line = dimensions
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    let (width, height) = first_line
        .split_once('x')
        .context("ffprobe returned no video dimensions")?;
    Ok((
        width.parse().context("parse video width")?,
        height.parse().context("parse video height")?,
    ))
}
