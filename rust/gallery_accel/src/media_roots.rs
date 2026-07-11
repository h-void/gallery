use std::env;
use std::path::{Path, PathBuf};

use anyhow::{anyhow, Result};

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
        if allowed.is_empty() {
            allowed.push("/pictures".into());
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
            if cleaned_trim == root_n || cleaned_trim.starts_with(&(root_n.clone() + "/")) {
                if best.as_ref().map(|(_, r)| r.len()).unwrap_or(0) < root_n.len() {
                    best = Some((i, root_n));
                }
            }
        }
        if let Some((i, root_n)) = best {
            if let Some(real) = self.real_root_at(i) {
                let real = real.trim_end_matches(['/', '\\']);
                let suffix = &cleaned_trim[root_n.len()..];
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
                if cleaned_trim == real_n || cleaned_trim.starts_with(&(real_n.to_string() + "/")) {
                    if path_under_root(&cleaned_trim, real_n) {
                        return Ok(PathBuf::from(&cleaned_trim));
                    }
                    return Err(anyhow!("path escapes authorized media root"));
                }
            }
        }

        // No mapping configured: return cleaned path as-is (legacy single-root / test layouts).
        Ok(PathBuf::from(cleaned_trim))
    }

    /// Normalize a DB path: rewrite virtual roots to real roots; leave other paths unchanged.
    pub fn normalize_db_path(&self, path: &str) -> String {
        match self.map_to_real(path) {
            Ok(p) => normalize_slashes(&p.to_string_lossy()),
            Err(_) => normalize_slashes(path),
        }
    }
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
    path.replace('\\', "/")
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

/// True when `path` (logical or canonical) is under any authorized root.
pub fn path_under_authorized_roots(path: &Path, roots: &MediaRoots) -> bool {
    let allowed = roots.allowed_roots();
    let logical = normalize_slashes(&path.to_string_lossy());
    if allowed.iter().any(|root| {
        let root = root.trim_end_matches(['/', '\\']);
        logical == root || logical.starts_with(&format!("{root}/"))
    }) {
        return true;
    }
    if let Ok(canon) = path.canonicalize() {
        let logical = normalize_slashes(&canon.to_string_lossy());
        return allowed.iter().any(|root| {
            let root_path = PathBuf::from(root);
            if let Ok(root_canon) = root_path.canonicalize() {
                let root_s = normalize_slashes(&root_canon.to_string_lossy());
                logical == root_s || logical.starts_with(&format!("{root_s}/"))
            } else {
                let root_s = root.trim_end_matches(['/', '\\']);
                logical == root_s || logical.starts_with(&format!("{root_s}/"))
            }
        });
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_virtual_root_to_real() {
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["/vol1/authorized".into()],
            real_paths: vec!["/vol1/authorized".into()],
        };
        let mapped = roots.map_to_real("/pictures1/ArtistA/a.jpg").unwrap();
        assert_eq!(
            normalize_slashes(&mapped.to_string_lossy()),
            "/vol1/authorized/ArtistA/a.jpg"
        );
    }

    #[test]
    fn rejects_parent_escape() {
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["/vol1/authorized".into()],
            real_paths: vec!["/vol1/authorized".into()],
        };
        assert!(roots.map_to_real("/pictures1/../etc/passwd").is_err());
    }

    #[test]
    fn normalize_db_path_rewrites_virtual() {
        let roots = MediaRoots {
            roots: vec!["/pictures1".into()],
            labels: vec!["real".into()],
            real_paths: vec!["/nas/real".into()],
        };
        assert_eq!(
            roots.normalize_db_path("/pictures1/foo"),
            "/nas/real/foo"
        );
    }
}
