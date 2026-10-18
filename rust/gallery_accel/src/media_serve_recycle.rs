// Recycle-bin targeting and safe file moves for media_serve.
//
// Split out of media_serve.rs to keep each review surface bounded. Every
// name here stays reachable as `crate::media_serve::*` through the glob
// re-exports in media_serve.rs, and `use super::*` keeps the parent helpers,
// constants and imports in scope exactly as before the split.
use super::*;
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
pub(crate) fn move_file_exact_impl(
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

    // Both steps below return before anything touches `dest`: `create_new` is
    // the only writer, so a destination that already existed is someone else's
    // file and must survive the failure. The previous unconditional
    // `remove_created_file(dest, None)` deleted it — the same clobber the
    // hard-link branch avoids at its `AlreadyExists` arm.
    let mut from = source_file.try_clone()?;
    let mut to = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dest)?;
    // From here on `dest` holds a file this call created, so a failure may
    // remove it again.
    let copy_result = std::io::copy(&mut from, &mut to)
        .and_then(|_| to.sync_all())
        .and_then(|_| {
            if from.metadata()?.len() != to.metadata()?.len() {
                return Err(std::io::Error::other("copied file size mismatch"));
            }
            Ok(())
        });
    if let Err(error) = copy_result {
        // Close the handle first: Windows refuses to unlink an open file, and
        // the previous code removed `dest` from inside the closure, after `to`
        // had already been dropped.
        drop(to);
        let _ = remove_created_file(dest, None);
        return Err(error);
    }
    // Same reason as above: every rollback below unlinks `dest`, which needs
    // the write handle closed on Windows.
    drop(to);
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
    Ok(merge_move_warnings(copy_reason, cleanup_warning))
}

/// Fold the reason a move had to copy into the warning it returns, so a
/// degraded (cross-volume) move is reported instead of looking like a clean
/// link-based one. Either input alone is still a warning worth surfacing.
pub(crate) fn merge_move_warnings(
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
                        // The magic-dfd call consumed the source (`unlinkat`
                        // returned 0) but could not name the destination. Every
                        // fallback below would then fail with ENOENT and bury
                        // this marker, so the caller would delete the recycle
                        // record while the item row stays active — the file is
                        // gone and nothing can restore it. Surface it instead.
                        if err.to_string().contains(FNOS_TRASH_DESTINATION_UNKNOWN) {
                            return Err(err.into());
                        }
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
    let variants = path_variants(path, &full, roots);
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
    let (artist_id, file_path_db, folder_name_db, artist_path_db): (
        i64,
        String,
        String,
        Option<String>,
    ) = conn
        .query_row(
            "SELECT i.artist_id, i.file_path, i.folder_name, a.path
             FROM items i LEFT JOIN artists a ON a.id = i.artist_id
             WHERE i.id = ?",
            [item_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
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
            if let Some(artist_path) = artist_path_db.as_deref() {
                if let Some(folder) = crate::folder_archive::source_folder_for_item(
                    artist_path,
                    &file_path_db,
                    &folder_name_db,
                ) {
                    if !folder.is_empty() {
                        let _ = crate::folder_archive::invalidate_plans_after_item_date_change(
                            conn,
                            artist_id,
                            &[folder],
                        );
                    }
                }
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
        let _ = crate::folder_archive::invalidate_plans_after_item_date_change(
            conn,
            artist_id,
            &[norm_folder],
        );
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
