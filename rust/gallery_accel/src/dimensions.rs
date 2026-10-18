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

fn image_dimensions(path: &Path) -> Result<(u32, u32)> {
    // `image` 0.25 buffers JPEG input to EOF before returning dimensions.
    // Probe a bounded prefix first; unusual headers fall back to the full reader.
    if is_jpeg_path(path) {
        let prefix = read_prefix(
            File::open(path)
                .with_context(|| format!("open image dimensions: {}", path.display()))?,
            IMAGE_DIMENSION_READ_LIMIT,
        )
        .with_context(|| format!("read image dimensions: {}", path.display()))?;
        if let Ok(dimensions) = image_dimensions_from_reader(Cursor::new(prefix)) {
            return Ok(dimensions);
        }
    }
    let reader = image::ImageReader::open(path)
        .with_context(|| format!("open image dimensions: {}", path.display()))?
        .with_guessed_format()
        .context("identify image format")?;
    reader.into_dimensions().context("read image dimensions")
}

fn is_jpeg_path(path: &Path) -> bool {
    matches!(
        path.extension().and_then(|extension| extension.to_str()),
        Some(extension) if extension.eq_ignore_ascii_case("jpg")
            || extension.eq_ignore_ascii_case("jpeg")
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
    let output = Command::new("ffprobe")
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
        .output()
        .context("read video dimensions")?;
    if !output.status.success() {
        anyhow::bail!("ffprobe failed");
    }
    let dimensions = String::from_utf8_lossy(&output.stdout);
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
