use std::path::Path;

use crate::media_roots::MediaRoots;

pub(crate) fn default_label(root: &str) -> String {
    let cleaned = root.trim_end_matches(['/', '\\']);
    Path::new(cleaned)
        .file_name()
        .and_then(|name| name.to_str())
        .map(|name| name.to_string())
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| cleaned.trim_matches(['/', '\\']).to_string())
}

pub(crate) fn display_path(path: &str, roots: &MediaRoots) -> String {
    let normalized = path.replace('\\', "/");
    // Match longest among virtual roots and real authorized roots.
    let mut candidates: Vec<(usize, String)> = Vec::new();
    for (index, root) in roots.roots.iter().enumerate() {
        let root_n = root.replace('\\', "/").trim_end_matches('/').to_string();
        if !root_n.is_empty() {
            candidates.push((index, root_n));
        }
        if let Some(real) = roots.real_root_at(index) {
            let real_n = real.replace('\\', "/").trim_end_matches('/').to_string();
            if !real_n.is_empty() {
                candidates.push((index, real_n));
            }
        }
    }
    candidates.sort_by(|a, b| b.1.len().cmp(&a.1.len()));
    for (index, root) in candidates {
        if normalized == root || normalized.starts_with(&(root.clone() + "/")) {
            let rel = normalized[root.len()..].trim_start_matches('/');
            let label = roots
                .labels
                .get(index)
                .cloned()
                .unwrap_or_else(|| default_label(&root));
            if rel.is_empty() {
                return label;
            }
            return format!("{}/{}", label, rel);
        }
    }
    path.to_string()
}
