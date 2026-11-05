//! Discover/commit service for the autonomous character organization pipeline
//! (RFC-2026-AUTONOMOUS-ORGANIZE v2, milestone 2).
//!
//! `discover_character_clusters` runs the chapter-2 pipeline over one artist's
//! unorganized items: stage-1 pre-filter against the verified reference
//! library, then stage-2/3 clustering via `character_clusters`, and returns
//! representative samples with filename/txt-derived `text_hints` for the
//! external VLM agent.
//!
//! `commit_cluster_decisions` re-runs the same deterministic discovery
//! (stateless — cluster ids must still resolve), enforces the anti-poisoning
//! gates, tags items through the shared same-hash group propagation, and
//! enrolls medoid/diverse prototypes as `source_type='cluster_verified'`
//! references (idempotent: an already-enrolled item/character pair is never
//! inserted twice; resubmitting a fully applied decision reports
//! `rejected/cluster_not_found` because the tagged items graduated out of the
//! unorganized set, and writes nothing).

use anyhow::{anyhow, bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::io::Read;

use crate::character_ccip::{
    embed_candidate_with_roots, load_index_vectors, rank_characters, CCIP_EMBEDDING_DIM,
};
use crate::character_clusters::{cluster_unorganized, prefilter_hit, DEFAULT_MIN_SIMILARITY};
use crate::media_roots::MediaRoots;

/// Longest-edge spec for representative preview images (RFC chapter 5.1).
pub const REPRESENTATIVE_PREVIEW_MAX_EDGE: u32 = 768;

fn env_bool(key: &str) -> bool {
    std::env::var(key)
        .map(|v| {
            matches!(
                v.trim().to_ascii_lowercase().as_str(),
                "1" | "true" | "yes" | "on"
            )
        })
        .unwrap_or(false)
}

/// Stage-1 silent tagging switch. Deliberately default-off until the
/// real-library distribution scan (RFC chapter 6.5) validates the 0.7174
/// pre-filter floor against production reference data.
fn silent_prefilter_enabled() -> bool {
    env_bool("CHARACTER_CLUSTER_SILENT_PREFILTER")
}

/// Upper bound on unorganized items embedded per discover run so one bloated
/// artist cannot hold the blocking worker indefinitely.
fn discover_max_items() -> i64 {
    std::env::var("CHARACTER_CLUSTER_MAX_ITEMS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .filter(|v| *v > 0)
        .unwrap_or(400)
}

const HINT_STEM_CAP: usize = 12;
const HINT_TOKEN_CAP: usize = 16;
const HINT_TOTAL_CAP: usize = 24;
const HINT_TXT_DIRS_CAP: usize = 3;
const HINT_TXT_PER_DIR_CAP: usize = 5;
const HINT_TXT_CONTENT_CAP_CHARS: usize = 200;
const HINT_TXT_CONTENT_FILES_CAP: usize = 6;

#[derive(Clone)]
struct OrganizeItem {
    id: i64,
    file_path: String,
    file_name: String,
    folder_name: String,
    media_type: String,
}

struct DiscoveredCluster {
    cluster_id: i64,
    /// Item ids (ascending).
    members: Vec<i64>,
    medoid: i64,
    diverse: Option<i64>,
    mean_similarity: f32,
    is_all_duplicates: bool,
    text_hints: Vec<String>,
}

struct Discovery {
    artist_name: String,
    /// Items that were selected, embedded, and not pre-filtered away.
    items: Vec<OrganizeItem>,
    /// 768-dim embeddings aligned by index with `items` (commit reuses them
    /// for prototype enrollment without re-running inference).
    vectors: Vec<Vec<f32>>,
    clusters: Vec<DiscoveredCluster>,
    clusters_omitted: usize,
    /// (item_id, character_name, score) — stage-1 matches regardless of the
    /// silent-tagging switch.
    prefilter_candidates: Vec<(i64, String, f32)>,
    /// Stage-1 matches actually tagged and excluded (switch on).
    pre_filtered_count: usize,
    embed_failed_count: usize,
    truncated: bool,
}


fn embed_for_organize(roots: &MediaRoots, item: &OrganizeItem) -> Result<Vec<f32>> {

    embed_candidate_with_roots(roots, item.id, &item.file_path, &item.media_type)
}

/// Items of the artist that carry no tag sharing a name with a library
/// character (the established tag↔character name mapping, cf. the stale
/// tag_single sweep in `product_ui`). Images and videos only.
fn select_unorganized_items(
    conn: &Connection,
    artist_id: i64,
) -> Result<(Vec<OrganizeItem>, bool)> {
    let limit = discover_max_items();
    let mut stmt = conn.prepare(
        "SELECT i.id, i.file_path, i.file_name, COALESCE(i.folder_name, ''), i.media_type
         FROM items i
         WHERE i.artist_id = ?1
           AND COALESCE(i.missing, 0) = 0
           AND COALESCE(i.is_archive, 0) = 0
           AND (i.media_type IN ('image', 'video') OR i.media_type = '')
           AND NOT EXISTS (
             SELECT 1 FROM item_tags it
             JOIN tags t ON t.id = it.tag_id
             JOIN characters c ON c.name = t.name
             WHERE it.item_id = i.id
           )
         ORDER BY i.id
         LIMIT ?2 + 1",
    )?;
    let rows = stmt
        .query_map(params![artist_id, limit], |r| {
            Ok(OrganizeItem {
                id: r.get(0)?,
                file_path: r.get(1)?,
                file_name: r.get(2)?,
                folder_name: r.get(3)?,
                media_type: r.get(4)?,
            })
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let truncated = rows.len() as i64 > limit;
    let mut rows = rows;
    if truncated {
        rows.truncate(limit as usize);
    }
    Ok((rows, truncated))
}

fn parent_directory(file_path: &str) -> &str {
    match file_path.rfind('/') {
        Some(pos) => &file_path[..pos],
        None => "",
    }
}

fn file_stem(file_name: &str) -> &str {
    match file_name.rfind('.') {
        Some(pos) if pos > 0 => &file_name[..pos],
        _ => file_name,
    }
}

/// Filename/txt-derived cross-check material for the VLM (RFC chapter 2,
/// stage 3): folder names, file-name stems and their separator tokens, plus
/// same-directory `.txt` item names and (bounded, lossy) contents. Everything
/// is deduplicated and hard-capped; media paths never enter the output.
fn text_hints_for_members(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    members: &[&OrganizeItem],
) -> Result<Vec<String>> {
    let mut hints: Vec<String> = Vec::new();
    let mut seen: BTreeSet<String> = BTreeSet::new();
    fn push(hints: &mut Vec<String>, seen: &mut BTreeSet<String>, value: String) {
        let value = value.trim().to_string();
        if value.is_empty() || seen.contains(&value) || hints.len() >= HINT_TOTAL_CAP {
            return;
        }
        seen.insert(value.clone());
        hints.push(value);
    }

    let mut dirs: Vec<&str> = members
        .iter()
        .map(|item| parent_directory(&item.file_path))
        .filter(|d| !d.is_empty())
        .collect();
    dirs.sort();
    dirs.dedup();

    for item in members {
        push(&mut hints, &mut seen, item.folder_name.clone());
    }
    for item in members.iter().take(HINT_STEM_CAP) {
        push(
            &mut hints,
            &mut seen,
            file_stem(&item.file_name).to_string(),
        );
    }
    // Separator tokens from the stems: "雷电将军_2024summer" yields
    // "雷电将军" and "2024summer" as individually matchable hints.
    let mut token_count = 0;
    'outer: for item in members {
        let stem = file_stem(&item.file_name);
        for token in stem.split(|c: char| c == '_' || c == '-' || c == ' ' || c == '.') {
            let token = token.trim();
            if token.len() >= 2 && token.len() < stem.len() {
                push(&mut hints, &mut seen, token.to_string());
                token_count += 1;
                if token_count >= HINT_TOKEN_CAP || hints.len() >= HINT_TOTAL_CAP {
                    break 'outer;
                }
            }
        }
    }

    // Sibling .txt items: names first, then bounded lossy contents.
    let mut txt_files_read = 0;
    // `%`/`_` in a stored path are literal characters, not wildcards; without
    // the escape a `2024_05` directory also matched `2024X05`. The artist
    // scope keeps an overlapping prefix from pulling another artist's notes in.
    fn escape_like(value: &str) -> String {
        value
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_")
    }
    'dirs: for dir in dirs.iter().take(HINT_TXT_DIRS_CAP) {
        let pattern = format!("{}%", escape_like(dir));
        let rows: Vec<(String, String)> = {
            let mut stmt = conn.prepare(
                "SELECT file_name, file_path FROM items
                 WHERE artist_id = ?3
                   AND file_path LIKE ?1 ESCAPE '\\'
                   AND lower(file_name) LIKE '%.txt'
                 ORDER BY id
                 LIMIT ?2",
            )?;
            // Bound the MappedRows temporary to an inner statement: a
            // block-tail `?` would extend its borrow past `stmt`'s scope.
            let collected: std::result::Result<Vec<(String, String)>, rusqlite::Error> = stmt
                .query_map(params![pattern, HINT_TXT_PER_DIR_CAP, artist_id], |r| {
                    Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
                })?
                .collect();
            collected?
        };
        for (txt_name, txt_path) in rows {
            push(&mut hints, &mut seen, file_stem(&txt_name).to_string());
            if txt_files_read >= HINT_TXT_CONTENT_FILES_CAP {
                continue;
            }
            if let Ok(path) = crate::media_roots::authorized_media_path(roots, &txt_path) {
                if let Some(content) = read_text_head(&path, 1024) {
                    push(&mut hints, &mut seen, content);
                    txt_files_read += 1;
                }
            }
            if hints.len() >= HINT_TOTAL_CAP {
                break 'dirs;
            }
        }
    }
    Ok(hints)
}

fn read_text_head(path: &std::path::Path, max_bytes: usize) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let mut buf = vec![0u8; max_bytes];
    let mut filled = 0;
    while filled < max_bytes {
        let n = file.read(&mut buf[filled..]).ok()?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    buf.truncate(filled);
    let text = String::from_utf8_lossy(&buf);
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    Some(trimmed.chars().take(HINT_TXT_CONTENT_CAP_CHARS).collect())
}

fn cluster_json(cluster: &DiscoveredCluster) -> Value {
    let mut samples = vec![json!({
        "item_id": cluster.medoid,
        "role": "medoid",
        "preview_url": format!(
            "/api/file/preview?item_id={}&max_edge={REPRESENTATIVE_PREVIEW_MAX_EDGE}",
            cluster.medoid
        ),
    })];
    if let Some(diverse) = cluster.diverse {
        samples.push(json!({
            "item_id": diverse,
            "role": "diverse",
            "preview_url": format!(
                "/api/file/preview?item_id={}&max_edge={REPRESENTATIVE_PREVIEW_MAX_EDGE}",
                diverse
            ),
        }));
    }
    json!({
        "cluster_id": cluster.cluster_id,
        "size": cluster.members.len(),
        "mean_similarity": (cluster.mean_similarity * 1000.0).round() / 1000.0,
        "is_all_duplicates": cluster.is_all_duplicates,
        "text_hints": cluster.text_hints,
        "representative_samples": samples,
        "item_ids": cluster.members,
    })
}

fn run_discovery(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    min_similarity: f32,
    max_clusters: Option<usize>,
    apply_silent_tags: bool,
) -> Result<Discovery> {
    let artist_name: String = conn
        .query_row(
            "SELECT name FROM artists WHERE id = ?",
            params![artist_id],
            |r| r.get(0),
        )
        .optional()?
        .ok_or_else(|| anyhow!("artist not found"))?;

    let (selected, truncated) = select_unorganized_items(conn, artist_id)?;
    let mut embedded: Vec<(OrganizeItem, Vec<f32>)> = Vec::with_capacity(selected.len());
    let mut embed_failed_count = 0usize;
    for item in selected {
        match embed_for_organize(roots, &item) {
            Ok(vector) if vector.len() == CCIP_EMBEDDING_DIM => {
                embedded.push((item, vector));
            }
            _ => {
                embed_failed_count += 1;
            }
        }
    }

    // Stage 1: pre-filter against the verified library (RFC chapter 2). The
    // silent-tagging switch only decides whether matches are tagged and
    // excluded here — candidates are always reported.
    let refs = load_index_vectors(conn)?;
    let mut prefilter_candidates: Vec<(i64, String, f32)> = Vec::new();
    if !refs.is_empty() {
        for (item, vector) in embedded.iter() {
            let ranked = rank_characters(vector, &refs, 2);
            if ranked.is_empty() {
                continue;
            }
            let top = ranked[0]["score"].as_f64().unwrap_or(0.0) as f32;
            let second = ranked
                .get(1)
                .and_then(|v| v["score"].as_f64())
                .map(|v| v as f32);
            if prefilter_hit(top, second) {
                prefilter_candidates.push((
                    item.id,
                    ranked[0]["character_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    top,
                ));
            }
        }
    }
    let mut pre_filtered_count = 0usize;
    if apply_silent_tags && !prefilter_candidates.is_empty() {
        let tx = conn.unchecked_transaction()?;
        // Every candidate matched exactly one character (ranked[0]); tag each
        // item with its own match, never with the union of the batch's
        // matches — one batch regularly spans several characters.
        let mut ids_by_tag: BTreeMap<i64, Vec<i64>> = BTreeMap::new();
        for (item_id, character_name, _) in &prefilter_candidates {
            let tag_id = get_or_create_tag(&tx, artist_id, character_name)?;
            ids_by_tag.entry(tag_id).or_default().push(*item_id);
        }
        for (tag_id, ids) in &ids_by_tag {
            crate::tags_write::update_item_tags_in_tx(&tx, ids, &[*tag_id], "add")?;
        }
        tx.commit()?;
        pre_filtered_count = prefilter_candidates.len();
        // Tagged items leave the clustering input; the next discover run will
        // not select them again (their tag now matches a character name).
        let excluded: BTreeSet<i64> = prefilter_candidates.iter().map(|(id, _, _)| *id).collect();
        embedded.retain(|(item, _)| !excluded.contains(&item.id));
    }

    // Stage 2/3 over the remaining unknowns.
    let vectors: Vec<Vec<f32>> = embedded.iter().map(|(_, v)| v.clone()).collect();
    let result = cluster_unorganized(&vectors, min_similarity)?;
    let mut clusters = Vec::new();
    for (idx, cluster) in result.clusters.iter().enumerate() {
        let member_items: Vec<&OrganizeItem> =
            cluster.members.iter().map(|i| &embedded[*i].0).collect();
        let text_hints = text_hints_for_members(conn, roots, artist_id, &member_items)?;
        clusters.push(DiscoveredCluster {
            cluster_id: (idx + 1) as i64,
            members: cluster.members.iter().map(|i| embedded[*i].0.id).collect(),
            medoid: embedded[cluster.medoid].0.id,
            diverse: cluster.diverse.map(|i| embedded[i].0.id),
            mean_similarity: cluster.mean_similarity,
            is_all_duplicates: cluster.is_all_duplicates,
            text_hints,
        });
    }
    let clusters_omitted = match max_clusters {
        Some(cap) if clusters.len() > cap => clusters.split_off(cap),
        _ => Vec::new(),
    }
    .len();

    let items: Vec<OrganizeItem> = embedded.iter().map(|(item, _)| item.clone()).collect();
    Ok(Discovery {
        artist_name,
        items,
        vectors: embedded.into_iter().map(|(_, v)| v).collect(),
        clusters,
        clusters_omitted,
        prefilter_candidates,
        pre_filtered_count,
        embed_failed_count,
        truncated,
    })
}

pub fn discover_character_clusters(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    min_similarity: Option<f32>,
    max_clusters: Option<usize>,
) -> Result<Value> {
    let min_similarity = min_similarity.unwrap_or(DEFAULT_MIN_SIMILARITY);
    if !(0.0..=1.0).contains(&min_similarity) {
        bail!("min_similarity must be within 0.0..=1.0");
    }
    let silent = silent_prefilter_enabled();
    let discovery = run_discovery(conn, roots, artist_id, min_similarity, max_clusters, silent)?;
    let clustered: usize = discovery.clusters.iter().map(|c| c.members.len()).sum();
    Ok(json!({
        "artist_id": artist_id,
        "artist_name": discovery.artist_name,
        "min_similarity": min_similarity,
        "silent_tagging_enabled": silent,
        "pre_filter_candidates_count": discovery.prefilter_candidates.len(),
        "pre_filtered_count": discovery.pre_filtered_count,
        "total_unorganized_items": discovery.items.len(),
        "embed_failed_count": discovery.embed_failed_count,
        "unclustered_noise_count": discovery.items.len() - clustered,
        "truncated": discovery.truncated,
        "clusters_omitted": discovery.clusters_omitted,
        "clusters": discovery.clusters.iter().map(cluster_json).collect::<Vec<_>>(),
    }))
}

fn get_or_create_tag(conn: &Connection, artist_id: i64, name: &str) -> Result<i64> {
    if let Some(id) = conn
        .query_row(
            "SELECT id FROM tags WHERE artist_id = ? AND name = ?",
            params![artist_id, name],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
    {
        return Ok(id);
    }
    conn.execute(
        "INSERT INTO tags (artist_id, name) VALUES (?, ?)",
        params![artist_id, name],
    )?;
    Ok(conn.last_insert_rowid())
}

fn get_or_create_character(conn: &Connection, name: &str, series: Option<&str>) -> Result<i64> {
    let existing: Option<(i64, Option<String>)> = conn
        .query_row(
            "SELECT id, series FROM characters WHERE name = ?",
            params![name],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    match existing {
        Some((id, stored_series)) => {
            if let Some(series) = series {
                if !series.is_empty() && stored_series.map(|s| s.is_empty()).unwrap_or(true) {
                    conn.execute(
                        "UPDATE characters SET series = ? WHERE id = ?",
                        params![series, id],
                    )?;
                }
            }
            Ok(id)
        }
        None => {
            conn.execute(
                "INSERT INTO characters (name, series) VALUES (?, ?)",
                params![name, series],
            )?;
            Ok(conn.last_insert_rowid())
        }
    }
}

#[derive(serde::Deserialize)]
struct DecisionCharacter {
    name: String,
    #[serde(default)]
    series: Option<String>,
}

#[derive(serde::Deserialize)]
struct ClusterDecision {
    cluster_id: i64,
    #[serde(default)]
    item_ids: Vec<i64>,
    verdict: String,
    #[serde(default)]
    matched_text_hint: Option<String>,
    #[serde(default)]
    is_multi_character_shot: bool,
    #[serde(default)]
    characters: Vec<DecisionCharacter>,
    #[serde(default)]
    enroll_prototypes: Vec<i64>,
    #[serde(default)]
    min_similarity: Option<f32>,
}

#[derive(serde::Serialize)]
struct DecisionOutcome {
    cluster_id: i64,
    status: &'static str,
    reason: String,
    tagged_item_ids: Vec<i64>,
    enrolled_reference_ids: Vec<i64>,
    enroll_verified: Option<bool>,
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// RFC chapter 5.2 server-side gates. Runs inside the caller's transaction:
/// tagging goes through `update_item_tags_in_tx` (same-hash group union +
/// folder-plan invalidation), enrollment writes `cluster_verified` references
/// whose `verified` flag follows the text-hint gate.
#[allow(clippy::too_many_arguments)]
fn process_decision(
    tx: &rusqlite::Transaction<'_>,
    discovery: &Discovery,
    artist_id: i64,
    decision: &ClusterDecision,
) -> Result<DecisionOutcome> {
    let mut outcome = DecisionOutcome {
        cluster_id: decision.cluster_id,
        status: "applied",
        reason: String::new(),
        tagged_item_ids: Vec::new(),
        enrolled_reference_ids: Vec::new(),
        enroll_verified: None,
    };
    let Some(cluster) = discovery
        .clusters
        .iter()
        .find(|c| c.cluster_id == decision.cluster_id)
    else {
        outcome.status = "rejected";
        outcome.reason = "cluster_not_found".into();
        return Ok(outcome);
    };
    if decision.item_ids.is_empty() {
        outcome.status = "rejected";
        outcome.reason = "empty_item_ids".into();
        return Ok(outcome);
    }
    let member_set: BTreeSet<i64> = cluster.members.iter().copied().collect();
    if let Some(offending) = decision.item_ids.iter().find(|id| !member_set.contains(id)) {
        outcome.status = "rejected";
        outcome.reason = format!("item_set_changed:{offending}");
        return Ok(outcome);
    }
    // Enrollment may only use the two cluster representatives (RFC chapter
    // 5.2 assert 3). Validated before any write so a bad decision stays
    // atomic.
    let mut enroll_items: Vec<(i64, &'static str)> = Vec::new();
    if decision.verdict == "CONFIRMED_CANON" && !decision.is_multi_character_shot {
        let allowed: Vec<(i64, &'static str)> = std::iter::once((cluster.medoid, "medoid"))
            .chain(cluster.diverse.iter().map(|d| (*d, "diverse")))
            .collect();
        let requested = if decision.enroll_prototypes.is_empty() {
            allowed.iter().map(|(id, _)| *id).collect()
        } else {
            decision.enroll_prototypes.clone()
        };
        for item_id in requested {
            match allowed.iter().find(|(id, _)| *id == item_id) {
                Some((_, role)) => {
                    if !enroll_items.iter().any(|(id, _)| *id == item_id) {
                        enroll_items.push((item_id, role));
                    }
                }
                None => {
                    outcome.status = "rejected";
                    outcome.reason = format!("invalid_enroll_prototype:{item_id}");
                    return Ok(outcome);
                }
            }
        }
    }

    match decision.verdict.as_str() {
        "UNKNOWN_CLUSTER" => {
            outcome.status = "skipped";
            return Ok(outcome);
        }
        "ORIGINAL_CHARACTER" | "CONFIRMED_CANON" => {}
        other => {
            outcome.status = "rejected";
            outcome.reason = format!("invalid_verdict:{other}");
            return Ok(outcome);
        }
    }

    let names: Vec<(String, Option<String>)> = {
        let mut seen = BTreeSet::new();
        for character in &decision.characters {
            let name = character.name.trim().to_string();
            if !name.is_empty() && seen.len() < 8 {
                seen.insert((
                    name.clone(),
                    character.series.as_deref().unwrap_or("").to_string(),
                ));
            }
        }
        seen.into_iter()
            .map(|(name, series)| {
                (
                    name,
                    if series.is_empty() {
                        None
                    } else {
                        Some(series)
                    },
                )
            })
            .collect()
    };
    if names.is_empty() {
        outcome.status = "rejected";
        outcome.reason = "missing_characters".into();
        return Ok(outcome);
    }

    // Tags: character names become artist tags through the shared write path
    // (same-hash group union, folder-plan invalidation included).
    let mut tag_ids = Vec::new();
    for (name, series) in &names {
        get_or_create_character(tx, name, series.as_deref())?;
        tag_ids.push(get_or_create_tag(tx, artist_id, name)?);
    }
    let tagged =
        crate::tags_write::update_item_tags_in_tx(tx, &decision.item_ids, &tag_ids, "add")?;
    outcome.tagged_item_ids = tagged["changed_item_ids"]
        .as_array()
        .map(|a| a.iter().filter_map(|v| v.as_i64()).collect())
        .unwrap_or_default();

    if decision.verdict == "ORIGINAL_CHARACTER" {
        // RFC chapter 2 stage 5: OC is tagged but never enters the one-person
        // reference library.
        outcome.enroll_verified = Some(false);
        return Ok(outcome);
    }

    // CONFIRMED_CANON: the text-hint gate decides verified vs staging.
    let hint_ok = decision
        .matched_text_hint
        .as_deref()
        .map(|hint| cluster.text_hints.iter().any(|h| h == hint))
        .unwrap_or(false);
    if decision.is_multi_character_shot {
        // RFC chapter 3.3 hard rule: multi-character shots are tagged but
        // never enroll any vector, not even into the staging pool.
        outcome.enroll_verified = Some(false);
        outcome.reason = "multi_character_shot".into();
        return Ok(outcome);
    }
    let verified: i64 = if hint_ok {
        outcome.enroll_verified = Some(true);
        1
    } else {
        outcome.status = "needs_review";
        outcome.reason = "hint_mismatch".into();
        outcome.enroll_verified = Some(false);
        0
    };

    // Resolve the character id once; every decision names exactly one
    // character for enrollment purposes (multi-name decisions are tagged for
    // all, but prototypes belong to the primary/first character).
    let (primary_name, primary_series) = &names[0];
    let character_id = get_or_create_character(tx, primary_name, primary_series.as_deref())?;
    let vector_by_item: HashMap<i64, &Vec<f32>> = discovery
        .items
        .iter()
        .enumerate()
        .map(|(idx, item)| (item.id, &discovery.vectors[idx]))
        .collect();
    for (item_id, role) in enroll_items {
        let already: Option<i64> = tx
            .query_row(
                "SELECT id FROM character_references
                 WHERE character_id = ? AND item_id = ? AND source_type = 'cluster_verified'",
                params![character_id, item_id],
                |r| r.get(0),
            )
            .optional()?;
        if already.is_some() {
            continue;
        }
        let is_video: bool = tx
            .query_row(
                "SELECT COALESCE(media_type, 'image') = 'video' FROM items WHERE id = ?",
                params![item_id],
                |r| r.get(0),
            )
            .unwrap_or(false);
        if is_video {
            continue;
        }
        let Some(vector) = vector_by_item.get(&item_id) else {
            continue;
        };
        let blob = crate::character_ccip::pack_embedding_blob(vector)?;
        let (repo, variant, file) = crate::character_ccip::embedding_model_meta();
        tx.execute(
            "INSERT INTO character_references
             (character_id, embedding, embedding_dim, source_type, item_id, created_at,
              embedding_model_repo_id, embedding_model_variant, embedding_model_file,
              embedding_updated_at, prototype_role, verified)
             VALUES (?, ?, ?, 'cluster_verified', ?, ?, ?, ?, ?, ?, ?, ?)",
            params![
                character_id,
                blob,
                CCIP_EMBEDDING_DIM as i64,
                item_id,
                now_epoch(),
                repo,
                variant,
                file,
                now_epoch(),
                role,
                verified,
            ],
        )?;
        outcome.enrolled_reference_ids.push(tx.last_insert_rowid());
    }
    Ok(outcome)
}

pub fn commit_cluster_decisions(
    conn: &Connection,
    roots: &MediaRoots,
    artist_id: i64,
    decisions: &[Value],
) -> Result<Value> {
    if decisions.is_empty() {
        bail!("decisions must not be empty");
    }
    let parsed: Vec<ClusterDecision> = decisions
        .iter()
        .map(|d| {
            serde_json::from_value::<ClusterDecision>(d.clone())
                .map_err(|e| anyhow!("invalid decision: {e}"))
        })
        .collect::<Result<Vec<_>>>()?;
    let min_similarity = parsed
        .first()
        .and_then(|d| d.min_similarity)
        .unwrap_or(DEFAULT_MIN_SIMILARITY);
    if !(0.0..=1.0).contains(&min_similarity) {
        bail!("min_similarity must be within 0.0..=1.0");
    }
    // Stateless re-discovery: cluster ids must resolve against a fresh,
    // deterministic run of the same pipeline. Silent tagging is suppressed so
    // commit never mutates state outside its own transaction.
    let discovery = run_discovery(conn, roots, artist_id, min_similarity, None, false)?;

    let tx = conn.unchecked_transaction()?;
    let mut outcomes = Vec::new();
    for decision in &parsed {
        outcomes.push(serde_json::to_value(process_decision(
            &tx, &discovery, artist_id, decision,
        )?)?);
    }
    tx.commit()?;

    let status_of = |value: &Value, key: &str| {
        value["results"]
            .as_array()
            .map(|results| results.iter().filter(|r| r["status"] == key).count())
            .unwrap_or(0)
    };
    let mut response = json!({
        "artist_id": artist_id,
        "results": outcomes,
    });
    response["applied_count"] = json!(status_of(&response, "applied"));
    response["needs_review_count"] = json!(status_of(&response, "needs_review"));
    response["skipped_count"] = json!(status_of(&response, "skipped"));
    response["rejected_count"] = json!(status_of(&response, "rejected"));
    Ok(response)
}
