//! Character library vector cleanup — port of Python `cleanup_character_references`.
//!
//! Policy (aligned with docs/OPERATIONS.md):
//! - Only delete `source_type=tag_single` (auto imports). Never delete protected/manual.
//! - `CLEANUP_MIN_REFERENCES` is a **floor** (keep at least N), not a max library size.
//! - Near-duplicate auto refs: cosine ≥ DUPLICATE (0.95) vs core → delete.
//! - Outlier auto refs: support_count < 2 (or max sim < MIN 0.35) → delete.
//! - Skip refs without valid non-degenerate embeddings (no mis-delete on placeholders).

use anyhow::Result;
use rusqlite::{params, Connection};
use serde_json::{json, Value};

fn env_bool(key: &str, default: bool) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(default)
}

fn env_f32(key: &str, default: f32) -> f32 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(default)
}

fn env_i64(key: &str, default: i64) -> i64 {
    std::env::var(key)
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v >= 0)
        .unwrap_or(default)
}

fn auto_cleanup_enabled() -> bool {
    env_bool("CHARACTER_IMPORT_AUTO_CLEANUP_ENABLED", true)
}

fn min_similarity() -> f32 {
    env_f32("CHARACTER_IMPORT_MIN_SIMILARITY", 0.35)
}

fn duplicate_similarity() -> f32 {
    env_f32("CHARACTER_IMPORT_DUPLICATE_SIMILARITY", 0.95)
}

/// Floor: do not clean characters with this many or fewer total refs.
fn cleanup_min_references() -> usize {
    env_i64("CHARACTER_IMPORT_CLEANUP_MIN_REFERENCES_PER_CHARACTER", 3).max(1) as usize
}

/// Core size target when building stable core (not a library size cap).
pub(crate) fn seed_core_size() -> usize {
    env_i64("CHARACTER_IMPORT_SEED_REFERENCES_PER_CHARACTER", 3).max(1) as usize
}

fn sim_at_least(score: f32, threshold: f32) -> bool {
    score + 1e-6 >= threshold
}

#[derive(Clone, Debug)]
pub(crate) struct RefRec {
    id: i64,
    source_type: String,
    pub(crate) vector: Vec<f32>,
    artist_id: Option<i64>,
    folder: String,
}

impl RefRec {
    pub(crate) fn candidate(
        item_id: i64,
        vector: Vec<f32>,
        artist_id: Option<i64>,
        file_path: &str,
    ) -> Self {
        Self {
            id: -item_id,
            source_type: "tag_single".into(),
            vector,
            artist_id,
            folder: folder_key(file_path),
        }
    }
}

fn parse_embedding(blob: &[u8], dim: i64) -> Option<Vec<f32>> {
    if dim <= 0 {
        return None;
    }
    let dim = dim as usize;
    let need = dim.checked_mul(4)?;
    if blob.len() != need {
        return None;
    }
    let mut out = Vec::with_capacity(dim);
    let mut sum_sq = 0.0f32;
    for i in 0..dim {
        let start = i * 4;
        let bytes: [u8; 4] = blob[start..start + 4].try_into().ok()?;
        let v = f32::from_le_bytes(bytes);
        if !v.is_finite() {
            return None;
        }
        sum_sq += v * v;
        out.push(v);
    }
    // Placeholder / degenerate (all zeros or near-zero) — skip for cleanup votes.
    if sum_sq < 1e-12 {
        return None;
    }
    Some(out)
}

fn folder_key(file_path: &str) -> String {
    let p = file_path.replace('\\', "/");
    match p.rsplit_once('/') {
        Some((dir, _)) => dir.to_string(),
        None => String::new(),
    }
}

pub(crate) fn load_character_refs(conn: &Connection, character_id: i64) -> Result<Vec<RefRec>> {
    let (repo, variant, file) = crate::character_ccip::embedding_model_meta();
    let mut stmt = conn.prepare(
        "SELECT cr.id, cr.source_type, cr.embedding, cr.embedding_dim,
                i.artist_id, i.file_path
         FROM character_references cr
         LEFT JOIN items i ON i.id = cr.item_id
         WHERE cr.character_id = ?
           AND cr.embedding_dim = ?
           AND cr.embedding_model_repo_id = ?
           AND cr.embedding_model_variant = ?
           AND cr.embedding_model_file = ?
         ORDER BY cr.id DESC",
    )?;
    let rows = stmt.query_map(
        params![
            character_id,
            crate::character_ccip::CCIP_EMBEDDING_DIM as i64,
            repo,
            variant,
            file
        ],
        |r| {
            Ok((
                r.get::<_, i64>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, Vec<u8>>(2)?,
                r.get::<_, i64>(3)?,
                r.get::<_, Option<i64>>(4)?,
                r.get::<_, Option<String>>(5)?,
            ))
        },
    )?;
    let mut out = Vec::new();
    for row in rows {
        let (id, source_type, emb, dim, artist_id, path) = row?;
        let Some(vector) = parse_embedding(&emb, dim) else {
            continue;
        };
        let folder = path.as_deref().map(folder_key).unwrap_or_default();
        out.push(RefRec {
            id,
            source_type,
            vector,
            artist_id,
            folder,
        });
    }
    Ok(out)
}

fn density_order(refs: &[RefRec]) -> Vec<usize> {
    if refs.is_empty() {
        return Vec::new();
    }
    if refs.len() == 1 {
        return vec![0];
    }
    let dim = refs[0].vector.len();
    if dim == 0 || refs.iter().any(|reference| reference.vector.len() != dim) {
        return (0..refs.len()).collect();
    }
    let mut vector_sum = vec![0.0f32; dim];
    for reference in refs {
        for (sum, value) in vector_sum.iter_mut().zip(&reference.vector) {
            *sum += value;
        }
    }
    let mut ranked: Vec<(f32, usize)> = (0..refs.len())
        .map(|i| {
            let vector = &refs[i].vector;
            let density =
                (dot(vector, &vector_sum) - dot(vector, vector)) / (refs.len() - 1) as f32;
            (density, i)
        })
        .collect();
    ranked.sort_by(|a, b| {
        b.0.partial_cmp(&a.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.cmp(&b.1))
    });
    ranked.into_iter().map(|(_, i)| i).collect()
}

/// Prefer artist/folder spread then density (Python `_select_diverse_references` simplified).
pub(crate) fn select_diverse_indices(refs: &[RefRec], desired: usize) -> Vec<usize> {
    if desired == 0 || refs.is_empty() {
        return Vec::new();
    }
    let order = density_order(refs);
    let mut selected: Vec<usize> = Vec::new();
    let mut used_artists = std::collections::HashSet::new();
    let mut used_folders = std::collections::HashSet::new();
    let multi_artist = refs
        .iter()
        .filter_map(|r| r.artist_id)
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;
    let multi_folder = refs
        .iter()
        .map(|r| r.folder.as_str())
        .filter(|f| !f.is_empty())
        .collect::<std::collections::HashSet<_>>()
        .len()
        > 1;

    let push = |selected: &mut Vec<usize>,
                used_artists: &mut std::collections::HashSet<i64>,
                used_folders: &mut std::collections::HashSet<String>,
                refs: &[RefRec],
                idx: usize| {
        if selected.contains(&idx) {
            return;
        }
        selected.push(idx);
        if let Some(a) = refs[idx].artist_id {
            used_artists.insert(a);
        }
        if !refs[idx].folder.is_empty() {
            used_folders.insert(refs[idx].folder.clone());
        }
    };

    if multi_artist {
        for &idx in &order {
            if selected.len() >= desired {
                break;
            }
            if let Some(a) = refs[idx].artist_id {
                if !used_artists.contains(&a) {
                    push(
                        &mut selected,
                        &mut used_artists,
                        &mut used_folders,
                        refs,
                        idx,
                    );
                }
            }
        }
    }
    if multi_folder {
        for &idx in &order {
            if selected.len() >= desired {
                break;
            }
            let f = refs[idx].folder.clone();
            if !f.is_empty() && !used_folders.contains(&f) {
                push(
                    &mut selected,
                    &mut used_artists,
                    &mut used_folders,
                    refs,
                    idx,
                );
            }
        }
    }
    for &idx in &order {
        if selected.len() >= desired {
            break;
        }
        push(
            &mut selected,
            &mut used_artists,
            &mut used_folders,
            refs,
            idx,
        );
    }
    selected
}

fn stable_core(refs: &[RefRec]) -> Vec<usize> {
    let desired = seed_core_size();
    let mut core_indices = Vec::new();
    // Protected first
    for (i, r) in refs.iter().enumerate() {
        if r.source_type != "tag_single" {
            core_indices.push(i);
            if core_indices.len() >= desired {
                return core_indices;
            }
        }
    }
    let auto_indices: Vec<usize> = refs
        .iter()
        .enumerate()
        .filter(|(_, r)| r.source_type == "tag_single")
        .map(|(i, _)| i)
        .collect();
    if core_indices.is_empty() {
        // Pure auto: diversify among auto only
        let auto_refs: Vec<RefRec> = auto_indices.iter().map(|&i| refs[i].clone()).collect();
        let local = select_diverse_indices(&auto_refs, desired);
        return local.into_iter().map(|li| auto_indices[li]).collect();
    }
    // Fill from auto that sit in mid-band vs protected (not dup, not outlier)
    let protected_vecs: Vec<&[f32]> = core_indices
        .iter()
        .map(|&i| refs[i].vector.as_slice())
        .collect();
    let min_s = min_similarity();
    let dup_s = duplicate_similarity();
    let mut candidates = Vec::new();
    for &ai in &auto_indices {
        let v = &refs[ai].vector;
        let max_sim = protected_vecs
            .iter()
            .map(|p| dot(v, p))
            .fold(0.0f32, f32::max);
        if sim_at_least(max_sim, min_s) && !sim_at_least(max_sim, dup_s) {
            candidates.push(ai);
        }
    }
    if candidates.is_empty() {
        candidates = auto_indices.clone();
    }
    let cand_refs: Vec<RefRec> = candidates.iter().map(|&i| refs[i].clone()).collect();
    let need = desired.saturating_sub(core_indices.len());
    let local = select_diverse_indices(&cand_refs, need);
    for li in local {
        core_indices.push(candidates[li]);
    }
    if core_indices.len() < desired {
        let fallback: Vec<usize> = auto_indices
            .into_iter()
            .filter(|index| !core_indices.contains(index))
            .collect();
        let fallback_refs: Vec<RefRec> = fallback.iter().map(|&i| refs[i].clone()).collect();
        let need = desired.saturating_sub(core_indices.len());
        for local_index in select_diverse_indices(&fallback_refs, need) {
            core_indices.push(fallback[local_index]);
        }
    }
    core_indices
}

fn dot(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b.iter()).map(|(x, y)| x * y).sum()
}

pub(crate) struct CoreVote {
    pub(crate) duplicate: bool,
    pub(crate) supported: bool,
}

fn core_vote(vector: &[f32], core: &[&RefRec]) -> CoreVote {
    if core.is_empty() {
        return CoreVote {
            duplicate: false,
            supported: false,
        };
    }
    let min_s = min_similarity();
    let dup_s = duplicate_similarity();
    let mut max_similarity = 0.0f32;
    let mut support_refs: Vec<&RefRec> = Vec::new();
    for r in core {
        let s = dot(vector, &r.vector);
        if s > max_similarity {
            max_similarity = s;
        }
        if sim_at_least(s, min_s) {
            support_refs.push(r);
        }
    }
    let core_artists: std::collections::HashSet<i64> =
        core.iter().filter_map(|r| r.artist_id).collect();
    let support_artists: std::collections::HashSet<i64> =
        support_refs.iter().filter_map(|r| r.artist_id).collect();
    let artist_supported = core_artists.len() < 2 || support_artists.len() >= 2;
    let support_count = support_refs.len();
    CoreVote {
        duplicate: sim_at_least(max_similarity, dup_s),
        supported: support_count >= 2 && artist_supported,
    }
}

pub(crate) fn stable_core_records(refs: &[RefRec]) -> Vec<RefRec> {
    let core_indices = stable_core(refs);
    core_indices
        .into_iter()
        .map(|index| refs[index].clone())
        .collect()
}

pub(crate) fn core_vote_records(vector: &[f32], core: &[RefRec]) -> CoreVote {
    let core: Vec<&RefRec> = core.iter().collect();
    core_vote(vector, &core)
}

/// Vector quality cleanup for the whole library (or no-op when disabled / no vectors).
pub fn cleanup_character_references(conn: &Connection) -> Result<Value> {
    if !auto_cleanup_enabled() {
        return Ok(json!({
            "status": "skipped",
            "reason": "auto_cleanup_disabled",
            "auto_deleted_low_similarity": 0,
            "auto_deleted_duplicate": 0,
            "cleanup_deleted_reference_ids": [],
        }));
    }

    let min_keep = cleanup_min_references();
    let character_ids: Vec<i64> = conn
        .prepare("SELECT id FROM characters ORDER BY id")?
        .query_map([], |r| r.get(0))?
        .collect::<rusqlite::Result<_>>()?;

    let mut low_similarity_ids = Vec::new();
    let mut duplicate_ids = Vec::new();
    let mut checked_auto = 0i64;
    let mut protected_count = 0i64;
    let mut characters_with_vectors = 0i64;
    let mut skipped_no_vectors = 0i64;

    for cid in character_ids {
        let refs = load_character_refs(conn, cid)?;
        if refs.is_empty() {
            skipped_no_vectors += 1;
            continue;
        }
        characters_with_vectors += 1;
        let auto_refs: Vec<&RefRec> = refs
            .iter()
            .filter(|r| r.source_type == "tag_single")
            .collect();
        let protected: Vec<&RefRec> = refs
            .iter()
            .filter(|r| r.source_type != "tag_single")
            .collect();
        checked_auto += auto_refs.len() as i64;
        protected_count += protected.len() as i64;
        if auto_refs.is_empty() {
            continue;
        }
        // Floor: total refs ≤ min keep → skip whole character
        if refs.len() <= min_keep {
            continue;
        }

        let core_idx = stable_core(&refs);
        let core: Vec<&RefRec> = core_idx.iter().map(|&i| &refs[i]).collect();
        let keep_ids: std::collections::HashSet<i64> = core.iter().map(|r| r.id).collect();

        let mut remaining = refs.len();
        for r in &auto_refs {
            if keep_ids.contains(&r.id) {
                continue;
            }
            if remaining <= min_keep {
                break;
            }
            let vote = core_vote(&r.vector, &core);
            if vote.duplicate {
                duplicate_ids.push(r.id);
                remaining -= 1;
                continue;
            }
            if !vote.supported {
                low_similarity_ids.push(r.id);
                remaining -= 1;
            }
        }
    }

    let mut delete_ids = low_similarity_ids.clone();
    delete_ids.extend(duplicate_ids.iter().copied());
    delete_ids.sort_unstable();
    delete_ids.dedup();

    if !delete_ids.is_empty() {
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let result = (|| -> Result<()> {
            for id in &delete_ids {
                conn.execute(
                    "DELETE FROM character_references WHERE id=? AND source_type='tag_single'",
                    params![id],
                )?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => conn.execute_batch("COMMIT")?,
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }

    Ok(json!({
        "status": if delete_ids.is_empty() { "ok" } else { "cleaned" },
        "auto_deleted_low_similarity": low_similarity_ids.len(),
        "auto_deleted_duplicate": duplicate_ids.len(),
        "cleanup_checked_references": checked_auto,
        "cleanup_protected_references": protected_count,
        "cleanup_deleted_reference_ids": delete_ids,
        "characters_with_vectors": characters_with_vectors,
        "characters_skipped_no_vectors": skipped_no_vectors,
        "min_references_floor": min_keep,
        "min_similarity": min_similarity(),
        "duplicate_similarity": duplicate_similarity(),
        "max_references_per_character": 0,
        "note": "min_references is a keep-floor; no default max library size",
    }))
}
