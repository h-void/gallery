use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

use crate::fs_util::safe_canonicalize;
use crate::path_display::default_label;

/// Virtual media-root aliases (`/picturesN`), display labels, and parallel real authorized roots.
#[derive(Clone, Debug)]
pub struct MediaRoots {
    pub roots: Vec<String>,
    pub labels: Vec<String>,
    /// Parallel to `roots`: fnOS authorized host paths used for scan/file I/O and DB storage.
    pub real_paths: Vec<String>,
}

impl MediaRoots {
    /// Roots where virtual aliases and real paths are identical (tests / direct mounts).
    pub fn identical(roots: Vec<String>, labels: Vec<String>) -> Self {
        let real_paths = roots.clone();
        Self {
            roots,
            labels,
            real_paths,
        }
    }

    pub fn real_root_at(&self, index: usize) -> Option<&str> {
        self.real_paths
            .get(index)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
            .or_else(|| self.roots.get(index).map(|s| s.as_str()))
    }

    /// Authorized roots used for filesystem allow-checks (virtual + real).
    ///
    /// An install with nothing configured authorizes nothing. The old fallback
    /// to `/pictures` was the development-mode read behaviour: it turned "the
    /// user has not said which directories are media" into "this one hard-coded
    /// path is", which is a write destination nobody chose. Fail closed and let
    /// the caller report that no root is configured.
    pub fn allowed_roots(&self) -> Vec<String> {
        let mut allowed = Vec::new();
        for (i, root) in self.roots.iter().enumerate() {
            let r = root.trim_end_matches(['/', '\\']).to_string();
            if !r.is_empty() {
                allowed.push(r);
            }
            if let Some(real) = self.real_root_at(i) {
                let real = real.trim_end_matches(['/', '\\']).to_string();
                if !real.is_empty() && !allowed.iter().any(|a| a == &real) {
                    allowed.push(real);
                }
            }
        }
        allowed
    }

    /// Map a virtual or already-real media path to the host path under authorized roots.
    /// Rejects `..` segments. Does not require the target to exist.
    pub fn map_to_real(&self, path: &str) -> Result<PathBuf> {
        let cleaned = normalize_slashes(path.trim());
        if cleaned.is_empty() || cleaned.split('/').any(|part| part == "..") {
            return Err(anyhow!("path not allowed"));
        }
        let cleaned_trim = cleaned.trim_end_matches('/').to_string();

        // Prefer longest matching virtual root.
        let mut best: Option<(usize, String)> = None;
        for (i, root) in self.roots.iter().enumerate() {
            let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
            if root_n.is_empty() {
                continue;
            }
            if (cleaned_trim == root_n
                || cleaned_trim
                    .strip_prefix(&root_n)
                    .is_some_and(|r| r.starts_with('/')))
                && best.as_ref().map(|(_, r)| r.len()).unwrap_or(0) < root_n.len()
            {
                best = Some((i, root_n));
            }
        }
        if let Some((i, root_n)) = best {
            if let Some(real) = self.real_root_at(i) {
                let real = real.trim_end_matches(['/', '\\']);
                let suffix = cleaned_trim.strip_prefix(&root_n).unwrap_or("");
                let candidate = format!("{real}{suffix}");
                if path_under_root(&candidate, real) {
                    return Ok(PathBuf::from(candidate));
                }
                return Err(anyhow!("path escapes authorized media root"));
            }
        }

        // Already a real authorized path (or under one).
        for i in 0..self.roots.len() {
            if let Some(real) = self.real_root_at(i) {
                let real_n = real.trim_end_matches(['/', '\\']);
                if cleaned_trim == real_n
                    || cleaned_trim
                        .strip_prefix(real_n)
                        .is_some_and(|r| r.starts_with('/'))
                {
                    if path_under_root(&cleaned_trim, real_n) {
                        return Ok(PathBuf::from(&cleaned_trim));
                    }
                    return Err(anyhow!("path escapes authorized media root"));
                }
            }
        }

        // No mapping configured: return the cleaned path as-is (legacy
        // single-root / test layouts). Accepted risk: with empty roots a host
        // path can pass through here, but authorized_media_path only allows
        // it while roots are empty, and resolve_allowed_path still
        // canonicalizes and re-checks against authorized roots before any
        // filesystem access.
        Ok(PathBuf::from(cleaned_trim))
    }

    /// Index of the authorized root whose real path contains `path`, if any.
    /// Virtual aliases and already-real paths both resolve through `map_to_real`.
    pub fn root_index_for_path(&self, path: &str) -> Option<usize> {
        let mapped = self.map_to_real(path).ok()?;
        let mapped = normalize_slashes(&mapped.to_string_lossy())
            .trim_end_matches('/')
            .to_string();
        if mapped.is_empty() {
            return None;
        }
        for i in 0..self.roots.len() {
            if let Some(real) = self.real_root_at(i) {
                let real_n = normalize_slashes(real)
                    .trim_end_matches(['/', '\\'])
                    .to_string();
                if !real_n.is_empty()
                    && (mapped == real_n || mapped.starts_with(&(real_n.clone() + "/")))
                {
                    return Some(i);
                }
            }
        }
        None
    }

    /// Normalize a DB path: rewrite virtual roots to real roots; leave other paths unchanged.
    pub fn normalize_db_path(&self, path: &str) -> String {
        match self.map_to_real(path) {
            Ok(p) => normalize_slashes(&p.to_string_lossy()),
            Err(_) => normalize_slashes(path),
        }
    }

    /// The real root that `root` names, or `root` itself when it is already real.
    ///
    /// The inverse of the root lookup in `map_to_real`: callers that rewrite
    /// stored paths after a filesystem move use it to learn which real root (and
    /// therefore which virtual aliases) a stored path belongs to.
    pub fn real_root_for(&self, root: &str) -> String {
        let wanted = normalize_slashes(root).trim_end_matches('/').to_string();
        for index in 0..self.roots.len() {
            let real = self
                .real_root_at(index)
                .map(|real| normalize_slashes(real).trim_end_matches('/').to_string())
                .unwrap_or_else(|| {
                    normalize_slashes(&self.roots[index])
                        .trim_end_matches('/')
                        .to_string()
                });
            if real == wanted {
                return real;
            }
        }
        wanted
    }
}

/// Resolve a stored media path and reject anything that is not inside an
/// authorized, canonical root before touching the filesystem.
pub(crate) fn authorized_media_path(roots: &MediaRoots, path: &str) -> Result<PathBuf> {
    let mapped = roots.map_to_real(path)?;
    if roots.roots.is_empty() {
        return Ok(mapped);
    }
    if !path_under_authorized_roots(&mapped, roots) {
        return Err(anyhow!("path is outside configured media roots"));
    }
    Ok(mapped)
}

pub fn env_media_roots() -> MediaRoots {
    let roots = split_csv(
        &env::var("PICTURES_ROOT").unwrap_or_else(|_| "/pictures".to_string()),
        true,
    );
    let roots = if roots.is_empty() {
        vec!["/pictures".to_string()]
    } else {
        roots
    };
    let explicit_labels = split_csv(&env::var("PICTURES_ROOT_LABELS").unwrap_or_default(), false);
    let explicit_real = split_csv(
        &env::var("PICTURES_ROOT_REAL_PATHS").unwrap_or_default(),
        true,
    );
    let labels = roots
        .iter()
        .enumerate()
        .map(|(index, root)| {
            explicit_labels
                .get(index)
                .cloned()
                .unwrap_or_else(|| default_label(root))
        })
        .collect();
    let real_paths = roots
        .iter()
        .enumerate()
        .map(|(index, root)| {
            explicit_real
                .get(index)
                .cloned()
                .unwrap_or_else(|| root.clone())
        })
        .collect();
    MediaRoots {
        roots,
        labels,
        real_paths,
    }
}

pub(crate) fn split_csv(value: &str, strip_trailing_slashes: bool) -> Vec<String> {
    value
        .split(',')
        .filter_map(|part| {
            let mut cleaned = part.trim().to_string();
            if strip_trailing_slashes {
                cleaned = cleaned.trim_end_matches(['/', '\\']).to_string();
            }
            if cleaned.is_empty() {
                None
            } else {
                Some(cleaned)
            }
        })
        .collect()
}

pub(crate) fn normalize_slashes(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .strip_prefix("//?/")
        .unwrap_or(&normalized)
        .to_string()
}

fn path_under_root(candidate: &str, root: &str) -> bool {
    let cand = normalize_slashes(candidate);
    let root = normalize_slashes(root).trim_end_matches('/').to_string();
    if root.is_empty() {
        return false;
    }
    // Reject path components that escape after naive join (already filtered `..`).
    if cand.split('/').any(|p| p == "..") {
        return false;
    }
    cand == root || cand.starts_with(&(root + "/"))
}

/// The install's own persistent directory: the database, its backups, the logs,
/// the recognition models and the transcode cache all live under it.
///
/// `DATA_DIR` is the single root of all of them, so excluding it excludes each
/// one without a second list to keep in sync.
fn persistent_data_root() -> String {
    normalize_slashes(
        std::env::var("DATA_DIR")
            .unwrap_or_else(|_| "data".to_string())
            .trim_end_matches(['/', '\\']),
    )
}

/// Whether `path` names something inside the install's persistent directory.
///
/// Living under an authorized media root is not enough. On a normal fnOS
/// install the data directory sits on the same volume as the pictures, so a
/// download, an ingest or an archive that targeted it would write over the
/// database, a backup or the log files. `PLAN_KEMONO_SUBSCRIPTION_2026-09-14.md`
/// §7 excludes those directories explicitly, aliases included — which is why the
/// comparison is repeated on the canonical paths rather than only on the
/// strings.
pub fn is_persistent_data_path(path: &Path) -> bool {
    let root = persistent_data_root();
    if root.is_empty() {
        return false;
    }
    let logical = normalize_slashes(&path.to_string_lossy());
    if path_under_root(&logical, &root) {
        return true;
    }

    // An alias reaches the same directory under a name the strings do not
    // share, so the deepest existing ancestor is compared for real.
    let mut existing = path.to_path_buf();
    while !existing.exists() {
        if !existing.pop() {
            return false;
        }
    }
    let Ok(canonical) = safe_canonicalize(&existing) else {
        return false;
    };
    let Ok(root) = safe_canonicalize(&root) else {
        return false;
    };
    path_under_root(
        &normalize_slashes(&canonical.to_string_lossy()),
        &normalize_slashes(&root.to_string_lossy()),
    )
}

/// True when `path` (logical or canonical) is under any authorized root.
pub fn path_under_authorized_roots(path: &Path, roots: &MediaRoots) -> bool {
    // The install's own data is never a media destination, whatever the roots
    // say. Checked first so a misconfigured root cannot authorize it.
    if is_persistent_data_path(path) {
        return false;
    }
    let allowed = roots.allowed_roots();
    let logical = normalize_slashes(&path.to_string_lossy());
    if !allowed.iter().any(|root| path_under_root(&logical, root)) {
        return false;
    }

    let mut existing = path.to_path_buf();
    while !existing.exists() {
        if !existing.pop() {
            return false;
        }
    }
    let Ok(canonical) = safe_canonicalize(&existing) else {
        return false;
    };
    let canonical = normalize_slashes(&canonical.to_string_lossy());
    allowed.iter().any(|root| {
        safe_canonicalize(root)
            .ok()
            .is_some_and(|root| path_under_root(&canonical, &root.to_string_lossy()))
    })
}
