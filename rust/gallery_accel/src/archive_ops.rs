//! 7-Zip archive operations: inspection, single-entry streaming, and safe extraction.
//!
//! Works across fnOS NAS (using bundled `7zz`) and Windows development environments (using `7z.exe`).

use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use crate::media_roots::{path_under_authorized_roots, MediaRoots};
use crate::media_type::media_type_for_file;

static CACHED_7Z_BIN: OnceLock<Option<PathBuf>> = OnceLock::new();

/// Locate 7-Zip executable on the system or bundled in package.
pub fn find_7z_binary() -> Result<PathBuf> {
    if let Some(cached) = CACHED_7Z_BIN.get() {
        return cached
            .clone()
            .ok_or_else(|| anyhow!("7-Zip executable (7zz / 7z) not found"));
    }

    let found = resolve_7z_binary_path();
    let _ = CACHED_7Z_BIN.set(found.clone());
    found.ok_or_else(|| anyhow!("7-Zip executable (7zz / 7z) not found"))
}

fn resolve_7z_binary_path() -> Option<PathBuf> {
    // 1. Explicit environment override
    if let Ok(env_path) = std::env::var("GALLERY_7Z_PATH") {
        let p = PathBuf::from(env_path);
        if p.is_file() {
            return Some(p);
        }
    }

    // 2. Check current executable directory (for fnOS packaged bin/7zz)
    if let Ok(current_exe) = std::env::current_exe() {
        if let Some(parent) = current_exe.parent() {
            #[cfg(unix)]
            {
                let cand = parent.join("7zz");
                if cand.is_file() {
                    return Some(cand);
                }
            }
            #[cfg(windows)]
            {
                let cand = parent.join("7z.exe");
                if cand.is_file() {
                    return Some(cand);
                }
            }
        }
    }

    // 3. Check repository / package relative paths from current_dir and parents
    let mut check_roots = Vec::new();
    if let Ok(cur) = std::env::current_dir() {
        check_roots.push(cur.clone());
        if let Some(p1) = cur.parent() {
            check_roots.push(p1.to_path_buf());
            if let Some(p2) = p1.parent() {
                check_roots.push(p2.to_path_buf());
            }
        }
    }

    #[cfg(unix)]
    {
        for base in &check_roots {
            for rel in &["app/bin/7zz", "output/rust/7zz", "bin/7zz"] {
                let p = base.join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    #[cfg(windows)]
    {
        for fixed in &[
            r"C:\Program Files\7-Zip\7z.exe",
            r"C:\Program Files (x86)\7-Zip\7z.exe",
            r"C:\Program Files\PeaZip\res\bin\7z\7z.exe",
            r"C:\Program Files (x86)\PeaZip\res\bin\7z\7z.exe",
        ] {
            let p = Path::new(fixed);
            if p.is_file() {
                return Some(p.to_path_buf());
            }
        }
        for base in &check_roots {
            for rel in &[
                ".test-cache/tools/7zip-win/7z.exe",
                "app/bin/7z.exe",
                "output/rust/7zip-win/7z.exe",
            ] {
                let p = base.join(rel);
                if p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    // 4. Search PATH
    #[cfg(unix)]
    let names = &["7zz", "7z", "7za"];
    #[cfg(windows)]
    let names = &["7z.exe", "7z", "7za.exe", "7zz.exe"];

    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            for name in names {
                let candidate = dir.join(name);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    None
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveInspectResponse {
    pub archive_name: String,
    pub archive_size: u64,
    pub is_encrypted: bool,
    pub header_encrypted: bool,
    pub stats: ArchiveStats,
    pub entries: Vec<ArchiveEntryItem>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ArchiveStats {
    pub total_files: usize,
    pub images: usize,
    pub videos: usize,
    pub sources: usize,
    pub texts: usize,
    pub others: usize,
    pub total_uncompressed_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchiveEntryItem {
    pub path: String,
    pub is_dir: bool,
    pub size: u64,
    pub packed_size: u64,
    pub modified: String,
    pub encrypted: bool,
    pub media_type: Option<String>,
}

/// Inspect archive contents using `7z l -slt -ba`.
pub fn inspect_archive(
    archive_path: &Path,
    password: Option<&str>,
) -> Result<ArchiveInspectResponse> {
    let bin = find_7z_binary()?;
    let file_meta = std::fs::metadata(archive_path)
        .with_context(|| format!("stat archive: {}", archive_path.display()))?;
    let archive_size = file_meta.len();
    let archive_name = archive_path
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "archive".to_string());

    let mut cmd = Command::new(&bin);
    cmd.arg("l").arg("-slt").arg("-ba").arg("-bsp0").arg("-bd");
    if let Some(pwd) = password.filter(|p| !p.is_empty()) {
        cmd.arg(format!("-p{pwd}"));
    } else {
        cmd.arg("-p");
    }
    cmd.arg("--");
    cmd.arg(archive_path);

    let output = cmd.output().context("execute 7z inspect command")?;

    let stdout_str = String::from_utf8_lossy(&output.stdout);
    let stderr_str = String::from_utf8_lossy(&output.stderr);

    // Detect header encryption or wrong password
    let password_error = stderr_str.contains("Wrong password")
        || stderr_str.contains("Cannot open encrypted archive")
        || stderr_str.contains("Enter password");

    if !output.status.success() {
        if password_error {
            return Ok(ArchiveInspectResponse {
                archive_name,
                archive_size,
                is_encrypted: true,
                header_encrypted: true,
                stats: ArchiveStats::default(),
                entries: Vec::new(),
            });
        }
        bail!("7-Zip inspection failed: {stderr_str}");
    }

    let mut entries = Vec::new();
    let mut stats = ArchiveStats::default();
    let mut any_encrypted = false;

    // Parse technical blocks separated by blank lines
    let normalized_archive_name = archive_name.to_ascii_lowercase();
    let normalized_stdout = stdout_str.replace("\r\n", "\n");

    for block in normalized_stdout.split("\n\n") {
        let block = block.trim();
        if block.is_empty() {
            continue;
        }

        let mut path = String::new();
        let mut is_dir = false;
        let mut size = 0u64;
        let mut packed_size = 0u64;
        let mut modified = String::new();
        let mut encrypted = false;

        for line in block.lines() {
            let line = line.trim();
            if let Some((k, v)) = line.split_once('=') {
                let key = k.trim();
                let val = v.trim();
                match key {
                    "Path" => path = val.replace('\\', "/"),
                    "Folder" => is_dir = val == "+",
                    "Size" => size = val.parse().unwrap_or(0),
                    "Packed Size" => packed_size = val.parse().unwrap_or(0),
                    "Modified" => modified = val.to_string(),
                    "Encrypted" => encrypted = val == "+",
                    "Attributes" if val.starts_with('D') => {
                        is_dir = true;
                    }
                    _ => {}
                }
            }
        }

        if path.is_empty() {
            continue;
        }

        // Skip archive self-header block if present
        if path
            .to_ascii_lowercase()
            .ends_with(&normalized_archive_name)
            && size == archive_size
        {
            continue;
        }

        // Filter macOS and Windows junk files
        if path.starts_with("__MACOSX/")
            || path == "__MACOSX"
            || path.ends_with(".DS_Store")
            || path.ends_with("Thumbs.db")
            || path.ends_with("desktop.ini")
        {
            continue;
        }

        if path.ends_with('/') {
            is_dir = true;
        }

        if encrypted {
            any_encrypted = true;
        }

        let fname = path.rsplit('/').next().unwrap_or(&path);
        let mtype = if is_dir {
            None
        } else {
            media_type_for_file(fname).map(|s| s.to_string())
        };

        if !is_dir {
            stats.total_files += 1;
            stats.total_uncompressed_bytes += size;
            match mtype.as_deref() {
                Some("image") => stats.images += 1,
                Some("video") => stats.videos += 1,
                Some("source") => stats.sources += 1,
                Some("text") => stats.texts += 1,
                _ => stats.others += 1,
            }
        }

        entries.push(ArchiveEntryItem {
            path,
            is_dir,
            size,
            packed_size,
            modified,
            encrypted,
            media_type: mtype,
        });
    }

    Ok(ArchiveInspectResponse {
        archive_name,
        archive_size,
        is_encrypted: any_encrypted || password_error,
        header_encrypted: false,
        stats,
        entries,
    })
}

/// Stream a single file from inside an archive using `7z e -so`.
pub fn stream_archive_entry(
    archive_path: &Path,
    entry_path: &str,
    password: Option<&str>,
) -> Result<Vec<u8>> {
    let clean_entry = entry_path.replace('\\', "/");
    let has_traversal = clean_entry.is_empty()
        || clean_entry.starts_with('/')
        || clean_entry.contains(':')
        || Path::new(&clean_entry).components().any(|c| {
            matches!(
                c,
                std::path::Component::ParentDir
                    | std::path::Component::RootDir
                    | std::path::Component::Prefix(_)
            )
        });
    if has_traversal {
        bail!("invalid entry path");
    }

    let bin = find_7z_binary()?;
    let mut cmd = Command::new(&bin);
    cmd.arg("e")
        .arg("-so")
        .arg("-bsp0")
        .arg("-bd")
        .arg("-mmt=on");
    if let Some(pwd) = password.filter(|p| !p.is_empty()) {
        cmd.arg(format!("-p{pwd}"));
    } else {
        cmd.arg("-p");
    }
    cmd.arg("--");
    cmd.arg(archive_path);
    cmd.arg(&clean_entry);

    let mut child = cmd
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .context("execute 7z single-entry extraction")?;

    let max_preview_bytes: u64 = 50 * 1024 * 1024;
    let mut stdout = child
        .stdout
        .take()
        .ok_or_else(|| anyhow!("missing child stdout pipe"))?;
    let mut buffer = Vec::new();
    let mut limited = std::io::Read::take(&mut stdout, max_preview_bytes + 1);
    std::io::Read::read_to_end(&mut limited, &mut buffer)?;

    if buffer.len() > max_preview_bytes as usize {
        let _ = child.kill();
        let _ = child.wait();
        bail!("entry exceeds 50MB in-memory preview limit");
    }

    let status = child.wait().context("wait for 7z extraction")?;
    if !status.success() && buffer.is_empty() {
        let mut stderr = String::new();
        if let Some(mut err_pipe) = child.stderr.take() {
            let _ = std::io::Read::read_to_string(&mut err_pipe, &mut stderr);
        }
        bail!("failed to stream entry: {stderr}");
    }

    Ok(buffer)
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractOptions {
    pub item_id: Option<i64>,
    pub file_path: Option<String>,
    pub password: Option<String>,
    pub target_mode: String, // "current_folder" or "new_folder"
    pub custom_folder_name: Option<String>,
    pub recycle_source: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExtractResponse {
    pub ok: bool,
    pub extracted_count: usize,
    pub target_dir: String,
    pub recycled_source: bool,
    pub message: Option<String>,
}

/// Safely extract an archive into target directory, clean junk, smart-flatten, and optionally recycle source.
pub fn extract_archive(
    conn: &rusqlite::Connection,
    roots: &MediaRoots,
    archive_path: &Path,
    password: Option<&str>,
    target_mode: &str,
    custom_folder_name: Option<&str>,
    recycle_source: bool,
) -> Result<ExtractResponse> {
    let bin = find_7z_binary()?;

    let parent_dir = archive_path
        .parent()
        .ok_or_else(|| anyhow!("archive has no parent directory"))?;

    let final_target_dir = if target_mode == "new_folder" {
        let stem = archive_path
            .file_stem()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| "unpacked".to_string());
        let folder_name = custom_folder_name
            .map(|s| s.trim())
            .filter(|s| !s.is_empty())
            .unwrap_or(&stem);
        // Sanitize folder name
        let safe_name: String = folder_name
            .chars()
            .map(|c| match c {
                '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
                other => other,
            })
            .collect();
        let trimmed = safe_name.trim().trim_matches('.');
        let final_name = if trimmed.is_empty() {
            "unpacked"
        } else {
            trimmed
        };
        parent_dir.join(final_name)
    } else {
        parent_dir.to_path_buf()
    };

    if !path_under_authorized_roots(&final_target_dir, roots) {
        bail!("target directory is outside authorized media roots");
    }

    // Create staging directory inside parent_dir to ensure same filesystem (fast atomic moves)
    let staging_name = format!(".extracting_{}", uuid::Uuid::new_v4().simple());
    let staging_dir = parent_dir.join(&staging_name);
    std::fs::create_dir_all(&staging_dir)
        .with_context(|| format!("create staging dir: {}", staging_dir.display()))?;

    // Execute 7z extraction
    let mut cmd = Command::new(&bin);
    cmd.arg("x")
        .arg("-y")
        .arg("-bso0")
        .arg("-bsp0")
        .arg("-bd")
        .arg("-mmt=on");
    if let Some(pwd) = password.filter(|p| !p.is_empty()) {
        cmd.arg(format!("-p{pwd}"));
    } else {
        cmd.arg("-p");
    }
    cmd.arg(format!("-o{}", staging_dir.display()));
    cmd.arg("--");
    cmd.arg(archive_path);

    let output = match cmd.output() {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(err).context("spawn 7z extract process");
        }
    };

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&staging_dir);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        if stderr_str.contains("Wrong password") || stderr_str.contains("Enter password") {
            bail!("解压密码错误");
        }
        bail!("解压执行失败: {stderr_str}");
    }

    // Clean macOS and Windows noise files from staging
    clean_staging_junk(&staging_dir);

    // Smart flatten: if staging_dir only contains 1 subfolder and no files, flatten it
    flatten_single_wrapper(&staging_dir);

    // Fast path: if final_target_dir does not exist yet (e.g. unpacking to a new folder),
    // atomically rename the staging directory directly to avoid iterating and moving thousands
    // of individual files and checking existence one-by-one.
    let extracted_count = if !final_target_dir.exists() {
        let count = match std::fs::read_dir(&staging_dir) {
            Ok(read) => read.flatten().count(),
            Err(_) => 0,
        };
        if std::fs::rename(&staging_dir, &final_target_dir).is_ok() {
            count
        } else {
            std::fs::create_dir_all(&final_target_dir).with_context(|| {
                format!("create final target dir: {}", final_target_dir.display())
            })?;
            let moved = move_staging_to_target(&staging_dir, &final_target_dir)?;
            let _ = std::fs::remove_dir_all(&staging_dir);
            moved
        }
    } else {
        std::fs::create_dir_all(&final_target_dir)
            .with_context(|| format!("create final target dir: {}", final_target_dir.display()))?;
        let moved = move_staging_to_target(&staging_dir, &final_target_dir)?;
        let _ = std::fs::remove_dir_all(&staging_dir);
        moved
    };

    // Recycle source archive if requested
    let mut recycled = false;
    if recycle_source {
        let archive_str = archive_path.to_string_lossy().to_string();
        if let Ok(_res) = crate::media_serve::delete_item_to_recycle(conn, &archive_str, roots) {
            recycled = true;
        } else {
            let recycle_target = crate::recycle::get_recycle_target_setting(conn);
            let prefer_system = recycle_target != "app";
            if let Ok((_recycled_path, _)) =
                crate::media_serve::move_into_recycle(archive_path, Some(roots), prefer_system)
            {
                recycled = true;
            }
        }
    }

    Ok(ExtractResponse {
        ok: true,
        extracted_count,
        target_dir: final_target_dir.to_string_lossy().to_string(),
        recycled_source: recycled,
        message: None,
    })
}

fn clean_staging_junk(dir: &Path) {
    let read = match std::fs::read_dir(dir) {
        Ok(r) => r,
        Err(_) => return,
    };
    for entry in read.flatten() {
        let p = entry.path();
        let is_symlink = entry.file_type().is_ok_and(|t| t.is_symlink());
        if is_symlink {
            let _ = std::fs::remove_file(&p);
            continue;
        }
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname == "__MACOSX"
            || fname == ".DS_Store"
            || fname == "Thumbs.db"
            || fname == "desktop.ini"
        {
            if p.is_dir() {
                let _ = std::fs::remove_dir_all(&p);
            } else {
                let _ = std::fs::remove_file(&p);
            }
            continue;
        }
        if p.is_dir() {
            clean_staging_junk(&p);
        }
    }
}

fn flatten_single_wrapper(staging_dir: &Path) {
    let entries: Vec<_> = match std::fs::read_dir(staging_dir) {
        Ok(r) => r.flatten().collect(),
        Err(_) => return,
    };
    let non_hidden: Vec<_> = entries
        .into_iter()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .collect();

    if non_hidden.len() == 1 {
        let entry = &non_hidden[0];
        let is_real_dir = entry
            .file_type()
            .is_ok_and(|t| t.is_dir() && !t.is_symlink());
        if is_real_dir {
            let single_dir = entry.path();
            let children: Vec<_> = match std::fs::read_dir(&single_dir) {
                Ok(r) => r.flatten().collect(),
                Err(_) => return,
            };
            for child in children {
                let dest = staging_dir.join(child.file_name());
                let _ = std::fs::rename(child.path(), dest);
            }
            let _ = std::fs::remove_dir(&single_dir);
        }
    }
}

fn move_staging_to_target(staging_dir: &Path, target_dir: &Path) -> Result<usize> {
    let mut count = 0;
    let entries: Vec<_> = std::fs::read_dir(staging_dir)
        .context("read staging directory")?
        .flatten()
        .collect();

    for entry in entries {
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        let src = entry.path();
        if file_type.is_symlink() {
            let _ = std::fs::remove_file(&src);
            continue;
        }
        let name = entry.file_name();
        let mut dest = target_dir.join(&name);

        // If target file/dir already exists, generate a safe unique name
        if dest.exists() {
            let (stem, ext) = if src.is_dir() {
                (name.to_string_lossy().to_string(), String::new())
            } else {
                let stem = src
                    .file_stem()
                    .map(|s| s.to_string_lossy().to_string())
                    .unwrap_or_default();
                let ext = src
                    .extension()
                    .map(|s| format!(".{}", s.to_string_lossy()))
                    .unwrap_or_default();
                (stem, ext)
            };
            let mut idx = 1;
            while dest.exists() && idx < 1000 {
                let new_name = format!("{stem} ({idx}){ext}");
                dest = target_dir.join(new_name);
                idx += 1;
            }
            if dest.exists() {
                let unique = uuid::Uuid::new_v4().simple();
                dest = target_dir.join(format!("{stem}_{unique}{ext}"));
            }
        }

        // Try atomic rename; if crossing volumes, copy and delete
        if std::fs::rename(&src, &dest).is_err() {
            if src.is_dir() {
                copy_dir_recursive(&src, &dest)?;
                let _ = std::fs::remove_dir_all(&src);
            } else {
                std::fs::copy(&src, &dest)?;
                let _ = std::fs::remove_file(&src);
            }
        }
        count += 1;
    }

    Ok(count)
}

fn copy_dir_recursive(src: &Path, dest: &Path) -> Result<()> {
    std::fs::create_dir_all(dest)?;
    for entry in std::fs::read_dir(src)?.flatten() {
        let sp = entry.path();
        let dp = dest.join(entry.file_name());
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            copy_dir_recursive(&sp, &dp)?;
        } else {
            std::fs::copy(&sp, &dp)?;
        }
    }
    Ok(())
}
