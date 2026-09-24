//! Path canonicalization that survives a filesystem driver which cannot answer
//! the Windows final-path query.
//!
//! On Windows `std::fs::canonicalize` opens a handle and asks the volume for the
//! path behind it (`GetFinalPathNameByHandleW`). ImDisk's virtual disk driver,
//! which backs the `R:` RamDisk used as test scratch here, does not implement
//! that call and answers `ERROR_INVALID_FUNCTION` (1), regardless of whether the
//! path exists. Rust surfaces it as `io::Error { raw_os_error: Some(1) }` with
//! the OS text `函数不正确` ("incorrect function"). Measured on this machine:
//!
//! ```text
//! R:\gallery-test      exists=true  canonicalize=ERR Os { code: 1, .. } raw=Some(1)
//! D:\ASUS\Documents\3  exists=true  canonicalize=OK  \\?\D:\ASUS\Documents\3
//! ```
//!
//! That matters because every containment, identity and cache-key check in this
//! crate compares one canonicalized path against another. A driver that fails the
//! query does not just lose the canonical form for one caller: a root that
//! canonicalizes and a child that does not can never be compared, so the check
//! has to fall back for *both* sides through the same function.
//!
//! [`safe_canonicalize`] is that one function. It returns the standard result
//! whenever the OS can produce one, and otherwise reconstructs the same shape the
//! standard result has - an absolute, fully resolved path carrying the `\\?\`
//! verbatim prefix - from the logical path alone. Callers keep using
//! `canonicalize`-style comparisons; the two forms are interchangeable because
//! both carry the prefix.
//!
//! What the fallback deliberately does not do: resolve symlinks or junctions
//! (the driver that fails the query has no reparse points to resolve), and invent
//! a canonical form for a path that does not exist - existence is preserved as a
//! precondition, exactly as `canonicalize` requires it.

use std::io;
#[cfg(any(windows, test))]
use std::path::Component;
use std::path::{Path, PathBuf};

/// A canonical path for `path`, falling back to logical normalization while the
/// filesystem driver refuses the final-path query.
///
/// Errors are the standard ones: `NotFound` when the path does not exist, and the
/// original error for any failure the fallback does not claim to handle.
pub fn safe_canonicalize<P: AsRef<Path>>(path: P) -> io::Result<PathBuf> {
    let path = path.as_ref();
    match path.canonicalize() {
        Ok(p) => Ok(p),
        Err(err) => {
            #[cfg(windows)]
            if is_unsupported_final_path(&err) {
                return fallback_canonicalize(path);
            }
            Err(err)
        }
    }
}

/// `ERROR_INVALID_FUNCTION` (1) is ImDisk (and other virtual mounts that route
/// through no volume manager); `ERROR_NOT_SUPPORTED` (50) is the same refusal
/// phrased by network redirectors. Only these two are repaired: an access denial
/// or a broken path must stay an error, otherwise the fallback would hand out a
/// canonical form for a path the OS refused to look at.
#[cfg(windows)]
fn is_unsupported_final_path(err: &io::Error) -> bool {
    matches!(err.raw_os_error(), Some(1) | Some(50))
}

/// Logical normalization to the shape `canonicalize` returns: absolute, `.` and
/// `..` removed, `\\?\` verbatim prefix present.
#[cfg(any(windows, test))]
fn normalize_components(path: &Path) -> io::Result<PathBuf> {
    // `canonicalize` requires the target to exist; dropping that precondition
    // would let a containment check pass for a path nobody can open.
    let _ = std::fs::metadata(path)?;

    let abs = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()?.join(path)
    };

    let mut components = Vec::new();
    for c in abs.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if let Some(Component::Normal(_)) = components.last() {
                    components.pop();
                }
            }
            _ => components.push(c),
        }
    }

    let mut out = PathBuf::new();
    for c in components {
        out.push(c.as_os_str());
    }
    Ok(out)
}

#[cfg(windows)]
fn fallback_canonicalize(path: &Path) -> io::Result<PathBuf> {
    let out = normalize_components(path)?;
    // Match the standard Windows result: `\\?\C:\...`, and `\\?\UNC\server\share`
    // for a UNC path.
    //
    // Idempotence is not a nicety here: callers hand already-canonicalized paths
    // back into `safe_canonicalize` (a resolved path is checked against its root,
    // a root against its own canonical form), and on a volume that refuses the
    // final-path query those paths arrive carrying `\\?\`. Re-prefixing a verbatim
    // path as if it were a UNC share produces `\\?\UNC\?\C:\...`, which compares
    // unequal to everything and made every containment check on the R: RamDisk
    // fail.
    let s = out.to_string_lossy();
    if s.starts_with(r"\\?\") {
        return Ok(out);
    }
    if let Some(rest) = s.strip_prefix(r"\\") {
        return Ok(PathBuf::from(format!(r"\\?\UNC\{}", rest)));
    }
    Ok(PathBuf::from(format!(r"\\?\{}", s)))
}

#[cfg(all(not(windows), test))]
fn fallback_canonicalize(path: &Path) -> io::Result<PathBuf> {
    normalize_components(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_canonicalize_existing_path() {
        let cur = std::env::current_dir().unwrap();
        let res = safe_canonicalize(&cur).unwrap();
        assert!(res.exists());
    }

    #[test]
    fn test_safe_canonicalize_nonexistent_fails() {
        let non_existent = Path::new("this_file_really_should_not_exist_xyz12345.tmp");
        assert!(safe_canonicalize(non_existent).is_err());
    }

    /// The property the callers depend on: a root and a child canonicalized
    /// through *this* function can be compared with each other, whichever branch
    /// produced them. That is what broke on the ImDisk RamDisk, where business
    /// code canonicalized one side through std (failed) and the other through a
    /// fallback (succeeded).
    #[test]
    fn fallback_output_is_usable_and_self_consistent() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Artist");
        std::fs::create_dir(&sub).unwrap();
        std::fs::write(sub.join("a.jpg"), b"x").unwrap();

        let root = fallback_canonicalize(dir.path()).unwrap();
        assert!(root.is_dir(), "fallback result must be a usable path");
        assert_eq!(
            std::fs::read_dir(&root).unwrap().count(),
            1,
            "fallback result must be readable through the OS"
        );

        let child = fallback_canonicalize(&sub.join("a.jpg")).unwrap();
        assert!(
            child.starts_with(&root),
            "a child canonicalized by the fallback must sit under its fallback root: {} vs {}",
            child.display(),
            root.display()
        );
    }

    #[test]
    fn fallback_removes_dot_and_parent_segments() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("Artist");
        std::fs::create_dir(&sub).unwrap();

        let plain = fallback_canonicalize(dir.path()).unwrap();
        let round_trip = fallback_canonicalize(&sub.join("..")).unwrap();
        assert_eq!(plain, round_trip);
    }

    #[test]
    fn fallback_keeps_the_existence_precondition() {
        let missing = std::env::temp_dir().join("safe_canonicalize_missing_xyz12345");
        assert!(fallback_canonicalize(&missing).is_err());
    }

    /// Callers hand already-canonicalized paths back in - a resolved path is
    /// checked against its root, a root against its own canonical form - so the
    /// fallback must be a fixed point. Without that, the second pass turns the
    /// `\\?\` prefix into a UNC share and every containment check on a volume
    /// that refuses the final-path query answers false.
    #[test]
    fn fallback_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let once = fallback_canonicalize(dir.path()).unwrap();
        let twice = fallback_canonicalize(&once).unwrap();
        assert_eq!(
            once, twice,
            "canonicalizing an already canonical path must not change it"
        );

        let child = dir.path().join("Artist").join("a.jpg");
        std::fs::create_dir(dir.path().join("Artist")).unwrap();
        std::fs::write(&child, b"x").unwrap();
        let child_once = fallback_canonicalize(&child).unwrap();
        assert_eq!(child_once, fallback_canonicalize(&child_once).unwrap());
    }

    #[cfg(windows)]
    #[test]
    fn windows_fallback_matches_the_standard_verbatim_shape() {
        let dir = tempfile::tempdir().unwrap();
        let fallback = fallback_canonicalize(dir.path()).unwrap();

        assert!(
            fallback.to_string_lossy().starts_with(r"\\?\"),
            "fallback must carry the verbatim prefix so it compares equal to std output: {}",
            fallback.display()
        );
        // Where the volume does answer the final-path query the two forms must
        // agree. On the ImDisk RamDisk there is no std answer to compare with -
        // that refusal is the reason this module exists, and this test also runs
        // there, so the comparison is conditional rather than asserted.
        if let Ok(standard) = dir.path().canonicalize() {
            assert_eq!(
                standard.to_string_lossy().to_lowercase(),
                fallback.to_string_lossy().to_lowercase(),
                "on a volume that answers the final-path query the fallback must agree with std"
            );
        } else {
            eprintln!(
                "note: {} has no std canonical form (final-path query refused); \
                 compared the fallback against itself only",
                dir.path().display()
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_unsupported_final_path_is_narrow() {
        assert!(is_unsupported_final_path(&io::Error::from_raw_os_error(1)));
        assert!(is_unsupported_final_path(&io::Error::from_raw_os_error(50)));
        assert!(!is_unsupported_final_path(&io::Error::from_raw_os_error(5)));
        assert!(!is_unsupported_final_path(&io::Error::from_raw_os_error(2)));
    }
}
