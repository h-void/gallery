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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_password: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub candidate_passwords: Vec<String>,
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
                matched_password: None,
                candidate_passwords: Vec::new(),
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
        if size == archive_size
            && path.len() >= normalized_archive_name.len()
            && path.as_bytes()[path.len() - normalized_archive_name.len()..]
                .eq_ignore_ascii_case(normalized_archive_name.as_bytes())
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
        matched_password: None,
        candidate_passwords: Vec::new(),
    })
}

pub fn ensure_archive_password_schema(conn: &rusqlite::Connection) -> Result<()> {
    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS artist_archive_passwords (
            artist_id INTEGER NOT NULL,
            password TEXT NOT NULL,
            hit_count INTEGER NOT NULL DEFAULT 1,
            last_used_at REAL NOT NULL,
            PRIMARY KEY (artist_id, password)
        );
        CREATE INDEX IF NOT EXISTS idx_artist_archive_passwords_rank
            ON artist_archive_passwords(artist_id, hit_count DESC, last_used_at DESC);",
    )?;
    Ok(())
}

pub fn record_artist_archive_password(conn: &rusqlite::Connection, artist_id: i64, password: &str) {
    let pwd = password.trim();
    if artist_id <= 0 || pwd.is_empty() || pwd.len() > 128 {
        return;
    }
    let _ = ensure_archive_password_schema(conn);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0);
    let _ = conn.execute(
        "INSERT INTO artist_archive_passwords (artist_id, password, hit_count, last_used_at)
         VALUES (?1, ?2, 1, ?3)
         ON CONFLICT(artist_id, password) DO UPDATE SET
            hit_count = hit_count + 1,
            last_used_at = excluded.last_used_at",
        rusqlite::params![artist_id, pwd, now],
    );
}

pub fn lookup_artist_id_for_archive(
    conn: &rusqlite::Connection,
    archive_path: &Path,
) -> Option<i64> {
    let raw = archive_path.to_string_lossy().to_string();
    let norm = raw.replace('\\', "/");
    if let Ok(Some(aid)) = conn.query_row(
        "SELECT artist_id FROM items WHERE file_path = ?1 OR replace(file_path, '\\', '/') = ?2 LIMIT 1",
        rusqlite::params![raw, norm],
        |r| r.get::<_, Option<i64>>(0),
    ) {
        return Some(aid);
    }
    conn.query_row(
        "SELECT id FROM artists
         WHERE COALESCE(missing, 0) = 0
           AND (
             replace(path, '\\', '/') = ?1
             OR substr(?1, 1, length(replace(path, '\\', '/')) + 1)
                = replace(path, '\\', '/') || '/'
           )
         ORDER BY length(path) DESC
         LIMIT 1",
        [&norm],
        |r| r.get::<_, i64>(0),
    )
    .ok()
}

pub fn collect_candidate_passwords(
    conn: Option<&rusqlite::Connection>,
    artist_id: Option<i64>,
    archive_path: &Path,
) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut push_unique = |cand: &str| {
        let trimmed = cand
            .trim()
            .trim_matches(|c| matches!(c, '"' | '\'' | '`' | '“' | '”'));
        if trimmed.is_empty() || trimmed.len() > 64 || out.len() >= 16 {
            return;
        }
        if !out.iter().any(|existing| existing == trimmed) {
            out.push(trimmed.to_string());
        }
    };

    static LABELED_PWD_RE: OnceLock<regex::Regex> = OnceLock::new();
    let labeled_re = LABELED_PWD_RE.get_or_init(|| {
        regex::Regex::new(
            r"(?i)(?:解压密码|解压码|密码|提取码|password|passcode|pass|pwd|pw)\s*(?:[:：=\-]|is|为)?\s*([^\s\r\n,，。；;！!]{1,64})",
        )
        .unwrap()
    });

    if let Some(parent_dir) = archive_path.parent() {
        if let Ok(read_dir) = std::fs::read_dir(parent_dir) {
            let mut txt_files: Vec<PathBuf> = read_dir
                .flatten()
                .map(|e| e.path())
                .filter(|p| {
                    p.is_file()
                        && p.extension()
                            .and_then(|s| s.to_str())
                            .is_some_and(|ext| ext.eq_ignore_ascii_case("txt"))
                })
                .take(16)
                .collect();
            txt_files.sort();
            for txt_path in txt_files {
                use std::io::Read;
                let Ok(mut f) = std::fs::File::open(&txt_path) else {
                    continue;
                };
                let mut buf = Vec::with_capacity(4096);
                if std::io::Read::take(&mut f, 16_384)
                    .read_to_end(&mut buf)
                    .is_err()
                {
                    continue;
                }
                let content = String::from_utf8_lossy(&buf);
                let mut matched_in_file = false;
                for caps in labeled_re.captures_iter(&content) {
                    if let Some(m) = caps.get(1) {
                        push_unique(m.as_str());
                        matched_in_file = true;
                    }
                }
                let non_empty_lines: Vec<&str> = content
                    .lines()
                    .map(str::trim)
                    .filter(|l| !l.is_empty())
                    .collect();
                let fname_lower = txt_path
                    .file_name()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_ascii_lowercase();
                let is_pwd_named = fname_lower.contains("密码")
                    || fname_lower.contains("pass")
                    || fname_lower.contains("pwd")
                    || fname_lower.contains("pw");
                if non_empty_lines.len() == 1 {
                    let single = non_empty_lines[0].trim_start_matches('\u{feff}').trim();
                    if !single.is_empty()
                        && single.len() <= 64
                        && !single.contains(char::is_whitespace)
                        && !single.starts_with("http://")
                        && !single.starts_with("https://")
                    {
                        push_unique(single);
                    }
                } else if !matched_in_file && is_pwd_named {
                    for line in non_empty_lines.into_iter().take(4) {
                        let clean = line.trim_start_matches('\u{feff}').trim();
                        if !clean.is_empty()
                            && clean.len() <= 64
                            && !clean.contains(char::is_whitespace)
                            && !clean.starts_with("http://")
                            && !clean.starts_with("https://")
                        {
                            push_unique(clean);
                        }
                    }
                }
            }
        }
    }

    if let (Some(c), Some(aid)) = (conn, artist_id) {
        if let Ok(mut stmt) = c.prepare(
            "SELECT password FROM artist_archive_passwords
             WHERE artist_id = ?1
             ORDER BY hit_count DESC, last_used_at DESC
             LIMIT 12",
        ) {
            if let Ok(rows) = stmt.query_map([aid], |r| r.get::<_, String>(0)) {
                for pwd in rows.flatten() {
                    push_unique(&pwd);
                }
            }
        }
        if let Ok(mut stmt) = c.prepare(
            "SELECT passcode FROM artist_link_occurrences
             WHERE artist_id = ?1 AND passcode != ''
             ORDER BY id DESC
             LIMIT 8",
        ) {
            if let Ok(rows) = stmt.query_map([aid], |r| r.get::<_, String>(0)) {
                for code in rows.flatten() {
                    push_unique(&code);
                }
            }
        }
    }

    out
}

fn verify_archive_password_entry(
    archive_path: &Path,
    entries: &[ArchiveEntryItem],
    password: &str,
) -> bool {
    if password.is_empty() {
        return false;
    }
    let Ok(bin) = find_7z_binary() else {
        return false;
    };
    let target_entry = entries
        .iter()
        .filter(|e| !e.is_dir && e.encrypted && e.size <= 64 * 1024 * 1024)
        .min_by_key(|e| e.packed_size.max(e.size))
        .or_else(|| {
            entries
                .iter()
                .filter(|e| !e.is_dir && e.size <= 64 * 1024 * 1024)
                .min_by_key(|e| e.packed_size.max(e.size))
        });

    let mut cmd = Command::new(&bin);
    cmd.arg("t")
        .arg("-bsp0")
        .arg("-bd")
        .arg(format!("-p{password}"))
        .arg("--")
        .arg(archive_path);
    if let Some(entry) = target_entry {
        if !entry.path.chars().any(|c| matches!(c, '*' | '?')) {
            cmd.arg(&entry.path);
        }
    }
    cmd.output().is_ok_and(|out| out.status.success())
}

pub fn inspect_archive_with_candidates(
    archive_path: &Path,
    password: Option<&str>,
    candidates: &[String],
) -> Result<ArchiveInspectResponse> {
    let explicit_pwd = password.map(str::trim).filter(|p| !p.is_empty());
    let mut res = inspect_archive(archive_path, explicit_pwd)?;
    res.candidate_passwords = candidates.to_vec();

    if let Some(pwd) = explicit_pwd {
        if !res.header_encrypted
            && (!res.is_encrypted || verify_archive_password_entry(archive_path, &res.entries, pwd))
        {
            res.matched_password = Some(pwd.to_string());
        }
        return Ok(res);
    }

    if !res.is_encrypted || candidates.is_empty() {
        return Ok(res);
    }

    if res.header_encrypted {
        for cand in candidates {
            if let Ok(mut trial) = inspect_archive(archive_path, Some(cand)) {
                if !trial.header_encrypted {
                    let data_ok = !trial.entries.iter().any(|e| e.encrypted)
                        || verify_archive_password_entry(archive_path, &trial.entries, cand);
                    if data_ok {
                        trial.matched_password = Some(cand.clone());
                        trial.candidate_passwords = candidates.to_vec();
                        return Ok(trial);
                    }
                }
            }
        }
    } else {
        for cand in candidates {
            if verify_archive_password_entry(archive_path, &res.entries, cand) {
                res.matched_password = Some(cand.clone());
                break;
            }
        }
    }

    Ok(res)
}

/// Stream a single file from inside an archive using `7z e -so`.
pub fn stream_archive_entry(
    archive_path: &Path,
    entry_path: &str,
    password: Option<&str>,
) -> Result<Vec<u8>> {
    let clean_entry = entry_path.replace('\\', "/");
    // `*` and `?` are wildcards to the 7z include pattern, so an entry named
    // with them (legal inside a zip) would stream every matching entry
    // concatenated instead of the one requested. Bracket classes are rarer and
    // cannot be disabled without breaking legitimate names, so they stay.
    let has_glob = clean_entry.chars().any(|c| matches!(c, '*' | '?'));
    let has_traversal = has_glob
        || clean_entry.is_empty()
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

    // Drain stderr on a thread from the start: reading it only after the
    // stdout loop deadlocked whenever a chatty 7z filled the pipe while we
    // were still waiting for stdout to reach EOF.
    let stderr_pipe = child
        .stderr
        .take()
        .ok_or_else(|| anyhow!("missing child stderr pipe"))?;
    let stderr_drain = std::thread::spawn(move || -> std::io::Result<Vec<u8>> {
        let mut pipe = stderr_pipe;
        let mut buffer = Vec::new();
        std::io::Read::read_to_end(&mut pipe, &mut buffer)?;
        Ok(buffer)
    });

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
        let _ = stderr_drain.join();
        bail!("entry exceeds 50MB in-memory preview limit");
    }

    let status = child.wait().context("wait for 7z extraction")?;
    let exit_code = status.code();
    // 7-Zip documents its exit codes: 0 success, 1 warning (the stream is
    // still complete, e.g. "There are data after the end of archive"), 2
    // fatal error, 7 command line, 8 out of memory, 255 user stopped. A
    // warning is served; every other non-zero code fails the request, and an
    // empty body after a non-zero one is never a real file (an empty entry
    // exits 0). Deciding from stderr wording instead served a partial body —
    // under a one-hour cache header — whenever a fatal message used text this
    // code did not list.
    if !matches!(exit_code, Some(0) | Some(1)) || (buffer.is_empty() && exit_code != Some(0)) {
        let stderr = stderr_drain
            .join()
            .ok()
            .and_then(|read| read.ok())
            .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
            .unwrap_or_default();
        let code = exit_code
            .map(|value| value.to_string())
            .unwrap_or_else(|| "none".to_string());
        bail!("failed to stream entry (7z exit {code}): {stderr}");
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_password: Option<String>,
}

/// Discover all volumes belonging to `archive_path` and verify multi-volume completeness
/// before extraction starts.
pub fn collect_archive_volumes(archive_path: &Path) -> Result<Vec<PathBuf>> {
    let parent_dir = archive_path
        .parent()
        .ok_or_else(|| anyhow!("archive has no parent directory"))?;
    let file_name = archive_path
        .file_name()
        .and_then(|s| s.to_str())
        .ok_or_else(|| anyhow!("invalid archive filename"))?;

    static PART_RE: OnceLock<regex::Regex> = OnceLock::new();
    static SPLIT_RE: OnceLock<regex::Regex> = OnceLock::new();

    let part_re = PART_RE
        .get_or_init(|| regex::Regex::new(r"(?i)^(.+?)\.part0*(\d+)\.(rar|zip|7z)$").unwrap());
    if let Some(caps) = part_re.captures(file_name) {
        let prefix = caps[1].to_ascii_lowercase();
        let vol_num: usize = caps[2].parse().unwrap_or(0);
        let ext = caps[3].to_ascii_lowercase();
        if vol_num != 1 {
            bail!("请从第 1 卷开始解压: {file_name}");
        }
        let mut numbered: std::collections::BTreeMap<usize, PathBuf> =
            std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(parent_dir)?.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ename) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(ecaps) = part_re.captures(ename) {
                if ecaps[1].to_ascii_lowercase() == prefix && ecaps[3].to_ascii_lowercase() == ext {
                    if let Ok(n) = ecaps[2].parse::<usize>() {
                        if n >= 1 {
                            numbered.insert(n, p);
                        }
                    }
                }
            }
        }
        let max_vol = numbered.keys().copied().max().unwrap_or(1);
        let mut vols = Vec::with_capacity(max_vol);
        for idx in 1..=max_vol {
            let Some(p) = numbered.remove(&idx) else {
                bail!("分卷压缩包缺失第 {idx} 卷");
            };
            vols.push(p);
        }
        verify_volume_sizes_and_headers(&vols, &ext, true)?;
        return Ok(vols);
    }

    let split_re = SPLIT_RE.get_or_init(|| {
        regex::Regex::new(r"(?i)^(.+?\.(7z|zip|rar|tar|gz|bz2|xz))\.(\d{3,4})$").unwrap()
    });
    if let Some(caps) = split_re.captures(file_name) {
        let prefix = caps[1].to_ascii_lowercase();
        let inner_ext = caps[2].to_ascii_lowercase();
        let digits_str = &caps[3];
        let vol_num: usize = digits_str.parse().unwrap_or(0);
        if vol_num != 1 {
            bail!("请从第 1 卷开始解压: {file_name}");
        }
        let mut numbered: std::collections::BTreeMap<usize, PathBuf> =
            std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(parent_dir)?.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ename) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            if let Some(ecaps) = split_re.captures(ename) {
                if ecaps[1].to_ascii_lowercase() == prefix {
                    if let Ok(n) = ecaps[3].parse::<usize>() {
                        if n >= 1 {
                            numbered.insert(n, p);
                        }
                    }
                }
            }
        }
        let max_vol = numbered.keys().copied().max().unwrap_or(1);
        let mut vols = Vec::with_capacity(max_vol);
        for idx in 1..=max_vol {
            let Some(p) = numbered.remove(&idx) else {
                bail!("分卷压缩包缺失第 {idx} 卷");
            };
            vols.push(p);
        }
        verify_volume_sizes_and_headers(&vols, &inner_ext, true)?;
        return Ok(vols);
    }

    let lower_name = file_name.to_ascii_lowercase();
    if lower_name.ends_with(".zip") {
        let stem = &lower_name[..lower_name.len() - 4];
        let mut numbered: std::collections::BTreeMap<usize, PathBuf> =
            std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(parent_dir)?.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ename) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let elower = ename.to_ascii_lowercase();
            if let Some(rest) = elower.strip_prefix(stem).and_then(|s| s.strip_prefix(".z")) {
                if rest.len() >= 2 && rest.chars().all(|c| c.is_ascii_digit()) {
                    if let Ok(n) = rest.parse::<usize>() {
                        if n >= 1 {
                            numbered.insert(n, p);
                        }
                    }
                }
            }
        }
        if !numbered.is_empty() {
            let max_vol = numbered.keys().copied().max().unwrap_or(1);
            let mut vols = Vec::with_capacity(max_vol + 1);
            for idx in 1..=max_vol {
                let Some(p) = numbered.remove(&idx) else {
                    bail!("ZIP 分卷压缩包缺失 .z{idx:02} 分卷");
                };
                vols.push(p);
            }
            vols.push(archive_path.to_path_buf());
            verify_multivol_zip_eocd(archive_path, max_vol)?;
            return Ok(vols);
        }
        verify_multivol_zip_eocd(archive_path, 0)?;
    } else if lower_name.ends_with(".rar") {
        let stem = &lower_name[..lower_name.len() - 4];
        let mut numbered: std::collections::BTreeMap<usize, PathBuf> =
            std::collections::BTreeMap::new();
        for entry in std::fs::read_dir(parent_dir)?.flatten() {
            let p = entry.path();
            if !p.is_file() {
                continue;
            }
            let Some(ename) = p.file_name().and_then(|s| s.to_str()) else {
                continue;
            };
            let elower = ename.to_ascii_lowercase();
            if let Some(rest) = elower.strip_prefix(stem).and_then(|s| s.strip_prefix('.')) {
                let bytes = rest.as_bytes();
                if bytes.len() == 3
                    && matches!(bytes[0], b'r' | b's' | b't')
                    && bytes[1].is_ascii_digit()
                    && bytes[2].is_ascii_digit()
                {
                    let num: usize = std::str::from_utf8(&bytes[1..])
                        .unwrap_or("0")
                        .parse()
                        .unwrap_or(0);
                    let seq = ((bytes[0] - b'r') as usize) * 100 + num + 1;
                    numbered.insert(seq, p);
                }
            }
        }
        if !numbered.is_empty() {
            let max_vol = numbered.keys().copied().max().unwrap_or(1);
            let mut vols = Vec::with_capacity(max_vol + 1);
            vols.push(archive_path.to_path_buf());
            for idx in 1..=max_vol {
                let Some(p) = numbered.remove(&idx) else {
                    bail!("RAR 分卷压缩包缺失分卷 #{idx}");
                };
                vols.push(p);
            }
            return Ok(vols);
        }
    }

    Ok(vec![archive_path.to_path_buf()])
}

fn verify_volume_sizes_and_headers(
    vols: &[PathBuf],
    ext: &str,
    check_7z_header: bool,
) -> Result<()> {
    let mut sizes = Vec::with_capacity(vols.len());
    for v in vols {
        let len = std::fs::metadata(v)?.len();
        if len == 0 {
            bail!("分卷文件为空 (0 字节): {}", v.display());
        }
        sizes.push(len);
    }
    if sizes.len() >= 3 {
        let first = sizes[0];
        for (i, &sz) in sizes[..sizes.len() - 1].iter().enumerate() {
            if sz != first {
                bail!(
                    "中间分卷 #{} 大小异常 ({sz} != {first} 字节): {}",
                    i + 1,
                    vols[i].display()
                );
            }
        }
        if let Some(&last) = sizes.last() {
            if last > first {
                bail!(
                    "末尾分卷大小异常 ({last} > 首分卷 {first} 字节): {}",
                    vols.last().unwrap().display()
                );
            }
        }
    } else if sizes.len() == 2 && sizes[0] < sizes[1] {
        bail!(
            "首分卷大小异常 ({} < 末尾分卷 {} 字节): {}",
            sizes[0],
            sizes[1],
            vols[0].display()
        );
    }
    if check_7z_header && ext == "7z" {
        use std::io::Read;
        let mut f = std::fs::File::open(&vols[0])?;
        let mut hdr = [0u8; 32];
        if f.read(&mut hdr)? == 32 && &hdr[0..6] == b"7z\xbc\xaf\x27\x1c" {
            let next_offset = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
            let next_size = u64::from_le_bytes(hdr[20..28].try_into().unwrap());
            let expected_total = 32u64.saturating_add(next_offset).saturating_add(next_size);
            let actual_total: u64 = sizes.iter().sum();
            if expected_total > actual_total {
                bail!("7z 分卷包不完整，缺少末尾分卷 (预期 {expected_total} 字节，实际 {actual_total} 字节)");
            }
        }
    }
    Ok(())
}

fn verify_multivol_zip_eocd(zip_path: &Path, z_count: usize) -> Result<()> {
    use std::io::{Read, Seek, SeekFrom};
    let mut f = std::fs::File::open(zip_path)?;
    let fsize = f.metadata()?.len();
    let read_len = fsize.min(65558) as usize;
    f.seek(SeekFrom::Start(fsize - read_len as u64))?;
    let mut tail = vec![0u8; read_len];
    f.read_exact(&mut tail)?;
    if let Some(pos) = tail.windows(4).rposition(|w| w == b"PK\x05\x06") {
        if pos + 22 <= tail.len() {
            let disk_num = u16::from_le_bytes([tail[pos + 4], tail[pos + 5]]) as usize;
            if disk_num != 0xFFFF && disk_num != z_count {
                bail!("ZIP 分卷包缺失分卷: 预期 {disk_num} 个 .zXX 分卷，实际只有 {z_count} 个");
            }
        }
    }
    Ok(())
}

const SEVENZ_AES_METHOD_ID: [u8; 4] = [0x06, 0xF1, 0x07, 0x01];

static SEVENZ_ENCRYPTED_CACHE: OnceLock<
    std::sync::Mutex<std::collections::HashMap<(String, i64, u64), bool>>,
> = OnceLock::new();

/// Returns `true` if the archive item should be included in batch extraction
/// (`解压全部压缩包` / `解压所选`):
/// - Secondary multi-volume parts (`.part2.rar`, `.7z.002`, `.z01`, etc.) are excluded.
/// - Non-`.7z` primary archives (`.zip`, `.rar`, `.tar`, `.gz`, `.bz2`, `.xz`, etc.) are included.
/// - Primary `.7z` archives (`.7z`, `.7z.001`, `.part1.7z`) are included ONLY when password-protected
///   (encrypted); unencrypted `.7z` cold-storage archives (such as compressed `.psd` sources) are excluded.
pub fn is_extractable_archive(
    file_path: &str,
    file_name: &str,
    file_size: i64,
    file_mtime: f64,
) -> bool {
    let effective_name = if file_name.trim().is_empty() {
        file_path.rsplit(['/', '\\']).next().unwrap_or(file_path)
    } else {
        file_name
    };
    if crate::media_type::is_secondary_archive_volume(effective_name) {
        return false;
    }
    if !crate::media_type::is_7z_archive_filename(effective_name) {
        return true;
    }
    is_7z_archive_encrypted_cached(file_path, file_size, file_mtime)
}

fn is_7z_archive_encrypted_cached(file_path: &str, file_size: i64, file_mtime: f64) -> bool {
    let key = (file_path.to_string(), file_size, file_mtime.to_bits());
    let cache = SEVENZ_ENCRYPTED_CACHE
        .get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::with_capacity(1024)));
    if let Ok(guard) = cache.lock() {
        if let Some(&cached) = guard.get(&key) {
            return cached;
        }
    }

    let path = Path::new(file_path);
    let resolved = if path.is_file() {
        Some(path.to_path_buf())
    } else {
        let roots = crate::media_roots::env_media_roots();
        crate::media_serve::resolve_allowed_path(file_path, &roots).ok()
    };
    let Some(target) = resolved else {
        return false;
    };
    let Some(encrypted) = is_7z_archive_encrypted_inner(&target) else {
        return false;
    };
    if let Ok(mut guard) = cache.lock() {
        if guard.len() >= 32_768 {
            guard.clear();
        }
        guard.insert(key, encrypted);
    }
    encrypted
}

/// Fast binary header check for whether a `.7z` archive (single or multi-volume)
/// is password-protected (`7zAES` coder `[0x06, 0xF1, 0x07, 0x01]`).
pub fn is_7z_archive_encrypted(archive_path: &Path) -> bool {
    is_7z_archive_encrypted_inner(archive_path).unwrap_or(false)
}

fn is_7z_archive_encrypted_inner(archive_path: &Path) -> Option<bool> {
    use std::io::{Read, Seek, SeekFrom};

    let mut f = std::fs::File::open(archive_path).ok()?;
    let mut hdr = [0u8; 32];
    if f.read(&mut hdr).ok() != Some(32) || &hdr[0..6] != b"7z\xbc\xaf\x27\x1c" {
        return Some(false);
    }
    let next_offset = u64::from_le_bytes(hdr[12..20].try_into().unwrap());
    let next_size = u64::from_le_bytes(hdr[20..28].try_into().unwrap());
    if next_size == 0 || next_size > 16 * 1024 * 1024 {
        return Some(false);
    }

    let file_len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let nh_abs_start = 32u64.saturating_add(next_offset);

    if nh_abs_start.saturating_add(next_size) <= file_len {
        let mut next_header = vec![0u8; next_size as usize];
        if f.seek(SeekFrom::Start(nh_abs_start)).is_err() || f.read_exact(&mut next_header).is_err()
        {
            return None;
        }
        return Some(check_7z_next_header_encrypted(
            &mut f,
            nh_abs_start,
            next_offset,
            &next_header,
            archive_path,
        ));
    }

    // Multi-volume `.7z`: NextHeader lives at the end of the last volume.
    // Return `None` when companion volumes are not yet complete so the cache
    // does not poison `.7z.001` with `false` while `.7z.002..` are still arriving.
    let vols = collect_archive_volumes(archive_path).ok()?;
    let last_vol = vols.last()?;
    let mut lf = std::fs::File::open(last_vol).ok()?;
    let last_len = lf.metadata().map(|m| m.len()).unwrap_or(0);
    if last_len >= next_size {
        let nh_local_start = last_len - next_size;
        let mut next_header = vec![0u8; next_size as usize];
        if lf.seek(SeekFrom::Start(nh_local_start)).is_ok()
            && lf.read_exact(&mut next_header).is_ok()
        {
            return Some(check_7z_next_header_encrypted(
                &mut lf,
                nh_local_start,
                next_offset,
                &next_header,
                archive_path,
            ));
        }
    }

    // NextHeader straddles volume boundary: fall back to 7z inspection.
    inspect_archive(archive_path, None)
        .map(|res| res.is_encrypted)
        .ok()
}

fn check_7z_next_header_encrypted(
    file: &mut std::fs::File,
    nh_file_offset: u64,
    next_offset: u64,
    next_header: &[u8],
    archive_path: &Path,
) -> bool {
    use std::io::{Read, Seek, SeekFrom};

    // 1. Direct match for 7zAES coder ID (06 F1 07 01):
    //    Covers all `-mhe=on` encrypted 7z archives and uncompressed-header `-mhe=off` archives.
    if next_header
        .windows(SEVENZ_AES_METHOD_ID.len())
        .any(|w| w == SEVENZ_AES_METHOD_ID)
    {
        return true;
    }

    // 2. Uncompressed header (`kHeader = 0x01`) without `7zAES`: guaranteed unencrypted.
    if next_header.first() == Some(&0x01) {
        return false;
    }

    // 3. Compressed header (`kEncodedHeader = 0x17`) without `7zAES` in outer stream:
    //    Decompress up to 8192 bytes of the inner `Header` stream using LZMA1 to check
    //    whether the data stream uses `7zAES` (`-mhe=off` on multi-file archives).
    if next_header.len() >= 12 && next_header[0] == 0x17 && next_header[1] == 0x06 {
        if let Some((pack_pos, _)) = read_7z_varint(&next_header[2..]) {
            if let Some(lzma_idx) = next_header
                .windows(4)
                .position(|w| w == [0x03, 0x01, 0x01, 0x05])
            {
                if lzma_idx + 9 <= next_header.len() && next_offset >= pack_pos {
                    let props = &next_header[lzma_idx + 4..lzma_idx + 9];
                    let delta_before_nh = next_offset - pack_pos;
                    if nh_file_offset >= delta_before_nh {
                        let packed_file_offset = nh_file_offset - delta_before_nh;
                        let read_len = delta_before_nh.min(32_768) as usize;
                        let mut packed = vec![0u8; read_len];
                        if file.seek(SeekFrom::Start(packed_file_offset)).is_ok()
                            && file.read_exact(&mut packed).is_ok()
                        {
                            if let Some(decoded) = lzma1_decompress_prefix(props, &packed, 8192) {
                                if decoded.first() == Some(&0x01) {
                                    return decoded
                                        .windows(SEVENZ_AES_METHOD_ID.len())
                                        .any(|w| w == SEVENZ_AES_METHOD_ID);
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    // Fallback for exotic header codecs
    inspect_archive(archive_path, None)
        .map(|res| res.is_encrypted)
        .unwrap_or(false)
}

fn read_7z_varint(buf: &[u8]) -> Option<(u64, usize)> {
    let first = *buf.first()?;
    let mask_bits = first.leading_ones() as usize;
    if mask_bits == 0 {
        return Some((first as u64, 1));
    }
    if buf.len() < 1 + mask_bits {
        return None;
    }
    if mask_bits == 8 {
        let val = u64::from_le_bytes(buf[1..9].try_into().ok()?);
        return Some((val, 9));
    }
    let high = (first & ((1u8 << (8 - mask_bits)) - 1)) as u64;
    let mut low = 0u64;
    for i in 0..mask_bits {
        low |= (buf[1 + i] as u64) << (8 * i);
    }
    Some((low | (high << (8 * mask_bits)), 1 + mask_bits))
}

struct LzmaRangeDecoder<'a> {
    src: &'a [u8],
    pos: usize,
    rng: u32,
    code: u32,
    probs: Vec<u16>,
}

impl<'a> LzmaRangeDecoder<'a> {
    fn bit(&mut self, idx: usize) -> usize {
        if self.rng < 0x0100_0000 {
            self.rng <<= 8;
            self.code = (self.code << 8) | (*self.src.get(self.pos).unwrap_or(&0) as u32);
            self.pos += 1;
        }
        let ttt = self.probs[idx] as u32;
        let bound = (self.rng >> 11) * ttt;
        if self.code < bound {
            self.rng = bound;
            self.probs[idx] = (ttt + ((2048 - ttt) >> 5)) as u16;
            0
        } else {
            self.rng -= bound;
            self.code -= bound;
            self.probs[idx] = (ttt - (ttt >> 5)) as u16;
            1
        }
    }

    fn tree(&mut self, base: usize, bits: usize) -> usize {
        let mut m = 1usize;
        for _ in 0..bits {
            let b = self.bit(base + m);
            m = (m << 1) | b;
        }
        m - (1usize << bits)
    }

    fn rev_tree(&mut self, base: usize, bits: usize) -> usize {
        let mut m = 1usize;
        let mut res = 0usize;
        for i in 0..bits {
            let b = self.bit(base + m);
            m = (m << 1) | b;
            res |= b << i;
        }
        res
    }

    fn decode_len(&mut self, base: usize, pos_state: usize) -> usize {
        if self.bit(base) == 0 {
            self.tree(base + 2 + (pos_state << 3), 3)
        } else if self.bit(base + 1) == 0 {
            8 + self.tree(base + 130 + (pos_state << 3), 3)
        } else {
            16 + self.tree(base + 258, 8)
        }
    }

    fn direct_bits(&mut self, count: usize) -> usize {
        let mut res = 0usize;
        for _ in 0..count {
            if self.rng < 0x0100_0000 {
                self.rng <<= 8;
                self.code = (self.code << 8) | (*self.src.get(self.pos).unwrap_or(&0) as u32);
                self.pos += 1;
            }
            self.rng >>= 1;
            if self.code >= self.rng {
                self.code -= self.rng;
                res = (res << 1) | 1;
            } else {
                res <<= 1;
            }
        }
        res
    }
}

fn lzma1_decompress_prefix(props: &[u8], src: &[u8], out_limit: usize) -> Option<Vec<u8>> {
    if props.len() < 5 || src.len() < 5 {
        return None;
    }
    let mut d = props[0] as usize;
    if d >= 9 * 5 * 5 {
        return None;
    }
    let lc = d % 9;
    d /= 9;
    let lp = d % 5;
    let pb = d / 5;

    let mut dec = LzmaRangeDecoder {
        src,
        pos: 5,
        rng: 0xFFFF_FFFF,
        code: ((src[1] as u32) << 24)
            | ((src[2] as u32) << 16)
            | ((src[3] as u32) << 8)
            | (src[4] as u32),
        probs: vec![1024u16; 1846 + (768 << (lc + lp))],
    };

    let mut out = Vec::with_capacity(out_limit);
    let mut state = 0usize;
    let mut rep0 = 1usize;
    let mut rep1 = 1usize;
    let mut rep2 = 1usize;
    let mut rep3 = 1usize;
    let pos_mask = (1usize << pb) - 1;
    let lp_mask = (1usize << lp) - 1;

    while out.len() < out_limit && dec.pos <= src.len() + 5 {
        let pos_state = out.len() & pos_mask;
        if dec.bit((state << 4) + pos_state) == 0 {
            let prev_byte = *out.last().unwrap_or(&0) as usize;
            let lit_state = ((out.len() & lp_mask) << lc) + (prev_byte >> (8 - lc));
            let lit_base = 1846 + 768 * lit_state;
            let mut sym = 1usize;
            if state >= 7 {
                let mut match_byte = if out.len() >= rep0 {
                    out[out.len() - rep0] as usize
                } else {
                    0
                };
                while sym < 0x100 {
                    let match_bit = (match_byte >> 7) & 1;
                    match_byte = (match_byte << 1) & 0xFF;
                    let b = dec.bit(lit_base + ((1 + match_bit) << 8) + sym);
                    sym = (sym << 1) | b;
                    if match_bit != b {
                        break;
                    }
                }
            }
            while sym < 0x100 {
                sym = (sym << 1) | dec.bit(lit_base + sym);
            }
            out.push((sym & 0xFF) as u8);
            state = if state < 4 {
                0
            } else if state < 10 {
                state - 3
            } else {
                state - 6
            };
        } else {
            let length: usize;
            if dec.bit(192 + state) == 1 {
                if dec.bit(204 + state) == 0 {
                    if dec.bit(240 + (state << 4) + pos_state) == 0 {
                        state = if state < 7 { 9 } else { 11 };
                        let Some(&b) = out.get(out.len().wrapping_sub(rep0)) else {
                            break;
                        };
                        out.push(b);
                        continue;
                    }
                } else {
                    let dist = if dec.bit(216 + state) == 0 {
                        rep1
                    } else {
                        let d = if dec.bit(228 + state) == 0 {
                            rep2
                        } else {
                            let d3 = rep3;
                            rep3 = rep2;
                            d3
                        };
                        rep2 = rep1;
                        d
                    };
                    rep1 = rep0;
                    rep0 = dist;
                }
                length = dec.decode_len(1332, pos_state) + 2;
                state = if state < 7 { 8 } else { 11 };
            } else {
                rep3 = rep2;
                rep2 = rep1;
                rep1 = rep0;
                length = dec.decode_len(818, pos_state) + 2;
                state = if state < 7 { 7 } else { 10 };
                let len_state = if length - 2 < 4 { length - 2 } else { 3 };
                let pos_slot = dec.tree(432 + (len_state << 6), 6);
                let dist = if pos_slot >= 4 {
                    let num_direct = (pos_slot >> 1) - 1;
                    let mut d = (2 | (pos_slot & 1)) << num_direct;
                    if pos_slot < 14 {
                        d += dec.rev_tree(688 + d - pos_slot - 1, num_direct);
                    } else {
                        d += dec.direct_bits(num_direct - 4) << 4;
                        d += dec.rev_tree(802, 4);
                    }
                    d
                } else {
                    pos_slot
                };
                if dist == 0xFFFF_FFFF {
                    break;
                }
                rep0 = dist.saturating_add(1);
            }
            for _ in 0..length {
                if out.len() >= out_limit {
                    break;
                }
                let Some(&b) = out.get(out.len().wrapping_sub(rep0)) else {
                    return if out.is_empty() { None } else { Some(out) };
                };
                out.push(b);
            }
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
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
    let all_volumes = collect_archive_volumes(archive_path)?;

    let parent_dir = archive_path
        .parent()
        .ok_or_else(|| anyhow!("archive has no parent directory"))?;

    let final_target_dir = if target_mode == "new_folder" {
        let stem = archive_path
            .file_name()
            .map(|s| {
                crate::media_type::strip_archive_volume_suffix(&s.to_string_lossy()).to_string()
            })
            .filter(|s| !s.is_empty())
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

    let explicit_pwd = password.map(str::trim).filter(|p| !p.is_empty());
    let artist_id = lookup_artist_id_for_archive(conn, archive_path);
    let mut matched_password: Option<String> = explicit_pwd.map(str::to_string);

    let run_extract_attempt = |pwd_opt: Option<&str>| -> Result<std::process::Output> {
        let mut cmd = Command::new(&bin);
        cmd.arg("x")
            .arg("-y")
            .arg("-bso0")
            .arg("-bsp0")
            .arg("-bd")
            .arg("-mmt=on");
        if let Some(pwd) = pwd_opt.filter(|p| !p.is_empty()) {
            cmd.arg(format!("-p{pwd}"));
        } else {
            cmd.arg("-p");
        }
        cmd.arg(format!("-o{}", staging_dir.display()));
        cmd.arg("--");
        cmd.arg(archive_path);
        cmd.output().context("spawn 7z extract process")
    };

    let is_pwd_err = |stderr_str: &str| -> bool {
        stderr_str.contains("Wrong password")
            || stderr_str.contains("Enter password")
            || stderr_str.contains("Cannot open encrypted archive")
            || stderr_str.contains("Data Error in encrypted file")
    };

    let mut output = match run_extract_attempt(explicit_pwd) {
        Ok(out) => out,
        Err(err) => {
            let _ = std::fs::remove_dir_all(&staging_dir);
            return Err(err);
        }
    };

    if !output.status.success() {
        let first_stderr = String::from_utf8_lossy(&output.stderr).to_string();
        if explicit_pwd.is_none() && is_pwd_err(&first_stderr) {
            let candidates = collect_candidate_passwords(Some(conn), artist_id, archive_path);
            for cand in candidates {
                let _ = std::fs::remove_dir_all(&staging_dir);
                if std::fs::create_dir_all(&staging_dir).is_err() {
                    break;
                }
                if let Ok(cand_out) = run_extract_attempt(Some(&cand)) {
                    if cand_out.status.success() {
                        output = cand_out;
                        matched_password = Some(cand);
                        break;
                    }
                }
            }
        }
    }

    if !output.status.success() {
        let _ = std::fs::remove_dir_all(&staging_dir);
        let stderr_str = String::from_utf8_lossy(&output.stderr);
        if is_pwd_err(&stderr_str) {
            bail!("解压密码错误");
        }
        let lower_err = stderr_str.to_ascii_lowercase();
        if lower_err.contains("missing volume")
            || lower_err.contains("unavailable data")
            || lower_err.contains("unexpected end of archive")
        {
            bail!("分卷压缩包不完整或缺失分卷: {stderr_str}");
        }
        bail!("解压执行失败: {stderr_str}");
    }

    if let (Some(aid), Some(ref pwd)) = (artist_id, &matched_password) {
        record_artist_archive_password(conn, aid, pwd);
    }

    // Clean macOS and Windows noise files from staging
    clean_staging_junk(&staging_dir);

    // Smart flatten: if staging_dir only contains 1 subfolder and no files, flatten it
    flatten_single_wrapper(&staging_dir);

    // Fast path: if final_target_dir does not exist yet (e.g. unpacking to a new folder),
    // atomically rename the staging directory directly to avoid iterating and moving thousands
    // of individual files and checking existence one-by-one.
    // Files, not top-level entries: an archive holding two folders with 500
    // files each reported `extracted_count: 2`, and the UI shows this number.
    let extracted_files = count_staged_files(&staging_dir);
    let published = (|| -> Result<()> {
        if !final_target_dir.exists() && std::fs::rename(&staging_dir, &final_target_dir).is_ok() {
            return Ok(());
        }
        std::fs::create_dir_all(&final_target_dir)
            .with_context(|| format!("create final target dir: {}", final_target_dir.display()))?;
        move_staging_to_target(&staging_dir, &final_target_dir)?;
        Ok(())
    })();
    if let Err(error) = published {
        // A failure here used to return with `.extracting_<uuid>` left in the
        // media folder: the dot prefix hides it from the scanner, so every
        // failed extraction silently kept its staging bytes forever.
        let _ = std::fs::remove_dir_all(&staging_dir);
        return Err(error);
    }
    let _ = std::fs::remove_dir_all(&staging_dir);
    let extracted_count = extracted_files;

    // Recycle source archive (and all companion volumes) if requested
    let mut recycled = false;
    if recycle_source {
        let recycle_target = crate::recycle::get_recycle_target_setting(conn);
        let prefer_system = recycle_target != "app";
        for vol_path in &all_volumes {
            if !vol_path.exists() {
                continue;
            }
            let vol_str = vol_path.to_string_lossy().to_string();
            match crate::media_serve::delete_item_to_recycle(conn, &vol_str, roots) {
                Ok(_) => {
                    recycled = true;
                }
                Err((axum::http::StatusCode::NOT_FOUND, _)) => {
                    if crate::media_serve::move_into_recycle(vol_path, Some(roots), prefer_system)
                        .is_ok()
                    {
                        recycled = true;
                    }
                }
                Err(_) => {}
            }
        }
    }

    Ok(ExtractResponse {
        ok: true,
        extracted_count,
        target_dir: final_target_dir.to_string_lossy().to_string(),
        recycled_source: recycled,
        message: None,
        matched_password,
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

/// Regular files under `root`, recursively.
fn count_staged_files(root: &Path) -> usize {
    let mut count = 0;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => stack.push(entry.path()),
                Ok(kind) if kind.is_file() => count += 1,
                _ => {}
            }
        }
    }
    count
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
