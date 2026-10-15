//! Low-level atomic directory rename and descriptor-pinned filesystem operations.

#[cfg(target_os = "linux")]
use std::path::Component;
use std::path::{Path, PathBuf};

#[cfg(target_os = "linux")]
use crate::fs_util::safe_canonicalize;
#[cfg(not(target_os = "linux"))]
use crate::media_roots::path_under_authorized_roots;
use crate::media_roots::MediaRoots;

#[cfg(target_os = "linux")]
pub(crate) fn rename_dir_no_overwrite(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::ffi::CString;
    use std::os::raw::c_int;
    use std::os::unix::ffi::OsStrExt;

    const AT_FDCWD: c_int = -100;
    let source = CString::new(source.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target = CString::new(target.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    renameat2_no_replace(AT_FDCWD, &source, AT_FDCWD, &target)
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn rename_dir_no_overwrite(source: &Path, target: &Path) -> std::io::Result<()> {
    if target.exists() {
        return Err(std::io::Error::from(std::io::ErrorKind::AlreadyExists));
    }
    std::fs::rename(source, target)
}

/// Rename a directory only through no-follow handles rooted in configured
/// media paths, with an optional source identity captured during preview.
#[cfg(target_os = "linux")]
pub(crate) fn rename_directory_under_authorized_roots_no_overwrite_expected(
    source: &Path,
    target: &Path,
    roots: &MediaRoots,
    expected_identity: Option<(u64, u64)>,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    use std::os::unix::fs::MetadataExt;

    let (source_root, source_relative) = authorized_root_relative(source, roots)?;
    let (target_root, target_relative) = authorized_root_relative(target, roots)?;
    let source_name = source_relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = target_relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_root_fd = open_absolute_dir(&source_root)?;
    let target_root_fd = open_absolute_dir(&target_root)?;
    let source_parent = open_relative_dir(
        &source_root_fd,
        source_relative.parent().unwrap_or_else(|| Path::new("")),
        false,
    )?;
    let target_parent = open_relative_dir(
        &target_root_fd,
        target_relative.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let source_name = std::ffi::CString::new(source_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = std::ffi::CString::new(target_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_dir = open_dir_at(source_parent.as_raw_fd(), &source_name)?;
    let source_metadata = std::fs::File::from(source_dir.try_clone()?).metadata()?;
    if expected_identity
        .is_some_and(|(dev, ino)| source_metadata.dev() != dev || source_metadata.ino() != ino)
    {
        return Err(std::io::Error::other(
            "source directory identity changed after preview",
        ));
    }

    renameat2_no_replace(
        source_parent.as_raw_fd(),
        &source_name,
        target_parent.as_raw_fd(),
        &target_name,
    )?;
    let actual =
        std::fs::File::from(open_dir_at(target_parent.as_raw_fd(), &target_name)?).metadata()?;
    if source_metadata.dev() == actual.dev() && source_metadata.ino() == actual.ino() {
        return Ok(());
    }
    match renameat2_no_replace(
        target_parent.as_raw_fd(),
        &target_name,
        source_parent.as_raw_fd(),
        &source_name,
    ) {
        Ok(()) => Err(std::io::Error::other(
            "renamed directory identity changed during operation",
        )),
        Err(rollback_error) => Err(std::io::Error::other(format!(
            "renamed directory identity changed during operation; rollback failed: {rollback_error}"
        ))),
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn rename_directory_under_authorized_roots_no_overwrite_expected(
    source: &Path,
    target: &Path,
    roots: &MediaRoots,
    _expected_identity: Option<(u64, u64)>,
) -> std::io::Result<()> {
    if !path_under_authorized_roots(source, roots) || !path_under_authorized_roots(target, roots) {
        return Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied));
    }
    rename_dir_no_overwrite(source, target)
}

#[cfg(target_os = "linux")]
fn authorized_root_relative(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<(PathBuf, PathBuf)> {
    let (root, relative) = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|root| {
            let root = safe_canonicalize(root).ok()?;
            let relative = path.strip_prefix(&root).ok()?.to_path_buf();
            Some((root, relative))
        })
        .filter(|(_, relative)| {
            !relative.as_os_str().is_empty()
                && relative
                    .components()
                    .all(|component| matches!(component, Component::Normal(_)))
        })
        .max_by_key(|(root, _)| root.components().count())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    Ok((root, relative))
}

#[cfg(target_os = "linux")]
pub(crate) struct AuthorizedDirectory {
    fd: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl AuthorizedDirectory {
    pub(crate) fn as_raw_fd(&self) -> std::os::raw::c_int {
        use std::os::fd::AsRawFd;

        self.fd.as_raw_fd()
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_authorized_directory_no_follow(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<AuthorizedDirectory> {
    let (root, relative) = authorized_root_relative(path, roots)?;
    let root_fd = open_absolute_dir(&root)?;
    let fd = open_relative_dir(&root_fd, &relative, false)?;
    Ok(AuthorizedDirectory { fd })
}

#[cfg(target_os = "linux")]
pub(crate) fn create_authorized_directory_no_follow(
    path: &Path,
    roots: &MediaRoots,
) -> std::io::Result<AuthorizedDirectory> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    let (root, relative) = authorized_root_relative(path, roots)?;
    let name = relative
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let root_fd = open_absolute_dir(&root)?;
    let parent = open_relative_dir(
        &root_fd,
        relative.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let name = std::ffi::CString::new(name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    if unsafe { mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    let fd = open_dir_at(parent.as_raw_fd(), &name)?;
    Ok(AuthorizedDirectory { fd })
}

#[cfg(target_os = "linux")]
fn renameat2_no_replace(
    source_parent: std::os::raw::c_int,
    source: &std::ffi::CStr,
    target_parent: std::os::raw::c_int,
    target: &std::ffi::CStr,
) -> std::io::Result<()> {
    use std::os::raw::{c_char, c_int, c_uint};

    unsafe extern "C" {
        fn renameat2(
            olddirfd: c_int,
            oldpath: *const c_char,
            newdirfd: c_int,
            newpath: *const c_char,
            flags: c_uint,
        ) -> c_int;
    }

    const RENAME_NOREPLACE: c_uint = 1;
    if unsafe {
        renameat2(
            source_parent,
            source.as_ptr(),
            target_parent,
            target.as_ptr(),
            RENAME_NOREPLACE,
        )
    } == 0
    {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) struct PreparedArtistRename {
    artist: PathBuf,
    target: PathBuf,
    source_parent: std::os::fd::OwnedFd,
    target_parent: std::os::fd::OwnedFd,
    source_name: std::ffi::CString,
    target_name: std::ffi::CString,
    source_dir: std::os::fd::OwnedFd,
}

#[cfg(target_os = "linux")]
impl PreparedArtistRename {
    pub(super) fn execute(&self) -> std::io::Result<()> {
        use std::os::fd::AsRawFd;

        renameat2_no_replace(
            self.source_parent.as_raw_fd(),
            &self.source_name,
            self.target_parent.as_raw_fd(),
            &self.target_name,
        )?;
        if let Err(error) = self.verify_target() {
            return match renameat2_no_replace(
                self.target_parent.as_raw_fd(),
                &self.target_name,
                self.source_parent.as_raw_fd(),
                &self.source_name,
            ) {
                Ok(()) => Err(error),
                Err(rollback_error) => Err(std::io::Error::other(format!(
                    "renamed directory verification failed: {error}; rollback failed: {rollback_error}"
                ))),
            };
        }
        Ok(())
    }

    fn verify_target(&self) -> std::io::Result<()> {
        use std::os::unix::fs::MetadataExt;

        let expected = std::fs::File::from(self.source_dir.try_clone()?).metadata()?;
        let artist = open_absolute_dir(&self.artist)?;
        let actual = open_relative_dir(&artist, &self.target, false)?;
        let actual = std::fs::File::from(actual).metadata()?;
        if expected.dev() != actual.dev() || expected.ino() != actual.ino() {
            return Err(std::io::Error::other(
                "renamed directory identity changed during operation",
            ));
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
fn open_dir_at_with_access(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
    access_flags: std::os::raw::c_int,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::FromRawFd;
    use std::os::raw::{c_char, c_int};

    unsafe extern "C" {
        fn openat(dirfd: c_int, pathname: *const c_char, flags: c_int, ...) -> c_int;
    }

    const O_CLOEXEC: c_int = 0o2000000;
    const O_DIRECTORY: c_int = 0o200000;
    const O_NOFOLLOW: c_int = 0o400000;
    let fd = unsafe {
        openat(
            parent,
            name.as_ptr(),
            access_flags | O_CLOEXEC | O_DIRECTORY | O_NOFOLLOW,
        )
    };
    if fd < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(unsafe { std::os::fd::OwnedFd::from_raw_fd(fd) })
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn open_dir_at(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    open_dir_at_with_access(parent, name, 0)
}

#[cfg(target_os = "linux")]
fn open_path_dir_at(
    parent: std::os::raw::c_int,
    name: &std::ffi::CStr,
) -> std::io::Result<std::os::fd::OwnedFd> {
    const O_PATH: std::os::raw::c_int = 0o10000000;
    open_dir_at_with_access(parent, name, O_PATH)
}

#[cfg(target_os = "linux")]
pub(super) fn open_absolute_dir(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    const AT_FDCWD: std::os::raw::c_int = -100;
    if !path.is_absolute() {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    let slash = std::ffi::CString::new("/").unwrap();
    let root = open_path_dir_at(AT_FDCWD, &slash)?;
    open_relative_dir(&root, path.strip_prefix("/").unwrap_or(path), false)
}

#[cfg(target_os = "linux")]
fn open_relative_dir(
    root: &std::os::fd::OwnedFd,
    relative: &Path,
    create: bool,
) -> std::io::Result<std::os::fd::OwnedFd> {
    use std::os::fd::AsRawFd;
    use std::os::raw::{c_char, c_int, c_uint};
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn mkdirat(dirfd: c_int, pathname: *const c_char, mode: c_uint) -> c_int;
    }

    let dot = std::ffi::CString::new(".").unwrap();
    let mut current = open_path_dir_at(root.as_raw_fd(), &dot)?;
    for component in relative.components() {
        let Component::Normal(component) = component else {
            return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
        };
        let name = std::ffi::CString::new(component.as_bytes())
            .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
        let next = match open_path_dir_at(current.as_raw_fd(), &name) {
            Ok(fd) => fd,
            Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {
                let created = unsafe { mkdirat(current.as_raw_fd(), name.as_ptr(), 0o755) };
                if created != 0 {
                    let mkdir_error = std::io::Error::last_os_error();
                    if mkdir_error.kind() != std::io::ErrorKind::AlreadyExists {
                        return Err(mkdir_error);
                    }
                }
                open_path_dir_at(current.as_raw_fd(), &name)?
            }
            Err(error) => return Err(error),
        };
        current = next;
    }
    Ok(current)
}


#[cfg(target_os = "linux")]
pub(super) fn rename_permission_denied_parent(source: &Path, target: &Path) -> Option<PathBuf> {


    let source_parent = source.parent()?;
    if directory_write_access_error(source_parent)
        .is_some_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
    {
        return Some(source_parent.to_path_buf());
    }
    let mut target_parent = target.parent()?;
    while !target_parent.exists() {
        target_parent = target_parent.parent()?;
    }
    directory_write_access_error(target_parent)
        .filter(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
        .map(|_| target_parent.to_path_buf())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn rename_permission_denied_parent(_source: &Path, _target: &Path) -> Option<PathBuf> {

    None
}

#[cfg(target_os = "linux")]
fn directory_write_access_error(path: &Path) -> Option<std::io::Error> {
    use std::os::raw::c_int;
    use std::os::unix::ffi::OsStrExt;

    unsafe extern "C" {
        fn faccessat(dirfd: c_int, pathname: *const c_char, mode: c_int, flags: c_int) -> c_int;
    }

    use std::os::raw::c_char;
    const AT_FDCWD: c_int = -100;
    const AT_EACCESS: c_int = 0x200;
    const W_OK: c_int = 2;
    const X_OK: c_int = 1;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).ok()?;
    if unsafe { faccessat(AT_FDCWD, path.as_ptr(), W_OK | X_OK, AT_EACCESS) } == 0 {
        None
    } else {
        Some(std::io::Error::last_os_error())
    }
}

#[cfg(target_os = "linux")]
pub(super) fn prepare_artist_dir_rename(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<PreparedArtistRename> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;

    let artist = safe_canonicalize(artist)?;
    let authorized_root = roots
        .allowed_roots()
        .into_iter()
        .filter_map(|root| safe_canonicalize(root).ok())
        .filter(|root| artist.starts_with(root))
        .max_by_key(|root| root.components().count())
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let authorized_fd = open_absolute_dir(&authorized_root)?;
    let artist_relative = artist
        .strip_prefix(&authorized_root)
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::PermissionDenied))?;
    let artist_fd = open_relative_dir(&authorized_fd, artist_relative, false)?;

    let source = Path::new(source);
    let target = Path::new(target);
    let source_name = source
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = target
        .file_name()
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_parent = open_relative_dir(
        &artist_fd,
        source.parent().unwrap_or_else(|| Path::new("")),
        false,
    )?;
    let target_parent = open_relative_dir(
        &artist_fd,
        target.parent().unwrap_or_else(|| Path::new("")),
        true,
    )?;
    let source_name = std::ffi::CString::new(source_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let target_name = std::ffi::CString::new(target_name.as_bytes())
        .map_err(|_| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    let source_dir = open_dir_at(source_parent.as_raw_fd(), &source_name)?;

    Ok(PreparedArtistRename {
        artist,
        target: target.to_path_buf(),
        source_parent,
        target_parent,
        source_name,
        target_name,
        source_dir,
    })
}

#[cfg(target_os = "linux")]
pub(super) fn rename_artist_dir_no_overwrite(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {
    prepare_artist_dir_rename(artist, roots, source, target)?.execute()?;
    // The DB commits the rename as executed; make the directory entries
    // themselves durable so a power cut cannot resurrect the old name.
    crate::media_serve::fsync_parent_dir_best_effort(&artist.join(source));
    crate::media_serve::fsync_parent_dir_best_effort(&artist.join(target));
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(super) fn rename_artist_dir_no_overwrite(
    artist: &Path,
    _roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {
    let source = artist.join(source);
    let target = artist.join(target);
    if let Some(parent) = target.parent() {
        super::ensure_target_parent_under_artist(parent, artist).map_err(std::io::Error::other)?;
    }
    rename_dir_no_overwrite(&source, &target)?;
    crate::media_serve::fsync_parent_dir_best_effort(&source);
    crate::media_serve::fsync_parent_dir_best_effort(&target);
    Ok(())
}

pub(super) fn rollback_undo_folder_rename(
    artist: &Path,
    roots: &MediaRoots,
    source: &str,
    target: &str,
) -> std::io::Result<()> {

    rename_artist_dir_no_overwrite(artist, roots, source, target)
}

