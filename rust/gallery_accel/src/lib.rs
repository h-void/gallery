use serde_json::{json, Value};

mod artist_reference_scores;
mod artist_references;
mod artist_stats;
mod artists;
pub mod character_ccip;
pub mod character_cleanup;
mod character_references;
mod character_summary;
mod character_summary_tags;
mod characters;
mod content_hash;
mod db;
mod duplicate_artists;
pub mod folder_archive;
mod folder_paths;
mod folder_tree;
mod folders;
pub mod hash_run;
mod hash_status;
mod image_preview;
mod item_detail;
mod item_detail_tags;
mod items;
mod maintenance;
mod media_roots;
pub mod media_serve;
pub mod media_type;
mod move_context;
mod move_filters;
mod move_group_logic;
mod move_groups;
mod move_history;
mod move_rows;
mod moves;
mod natural_sort;
mod operation_folder_renames;
mod operation_helpers;
mod operations;
mod path_display;
pub mod pinyin_search;
pub mod product_ui;
pub mod recognition_status;
pub mod scan;
mod scan_candidates_write;
mod similarity;
mod tag_search;
mod tags;
mod tags_write;
pub mod upstream;
mod workers;

#[cfg(test)]
pub(crate) mod test_support;

pub use artist_reference_scores::artist_reference_scores_response;
pub use artist_references::artist_references_response;
pub use artist_stats::artist_stats_response;
pub use artists::{artist_detail_response, artists_response};
pub use character_cleanup::cleanup_character_references;
pub use character_references::character_references_response;
pub use character_summary::character_summary_response;
pub use characters::{character_response, characters_response};
pub use content_hash::content_hash_response;
pub use db::{env_db_path, DbConfig, DbPool, PooledConn};
pub use duplicate_artists::duplicate_artists_response;
pub use folder_archive::{
    create_db_backup, execute_folder_renames, folder_rename_auto_enabled, list_folder_renames,
    recheck_plan, run_folder_rename_auto_after_full_scan, set_folder_rename_auto,
    upsert_folder_rename_plans,
};
pub use folder_paths::folder_paths_response;
pub use folders::folders_response;
pub use hash_run::run_hash_batch;
pub use hash_status::hash_status_response;
pub use image_preview::{
    clamp_max_edge, existing_preview_cache_file, image_preview_bytes, image_preview_response,
    DEFAULT_MAX_EDGE as IMAGE_PREVIEW_DEFAULT_MAX_EDGE,
};
pub use item_detail::item_detail_response;
pub use items::{items_page_query_response, items_page_response};
pub use maintenance::folder_rename_auto_response;
pub use media_roots::{env_media_roots, MediaRoots};
pub use media_serve::{
    content_hash_allowed, delete_item_to_recycle, delete_to_recycle, preview_jpeg_allowed,
    preview_or_fallback, resolve_allowed_path, serve_file_response, serve_text,
    serve_transcoded_hls, serve_transcoded_hls_segment, serve_video_compatible, serve_video_hls,
    start_video_transcode, video_frame_jpeg, video_transcode_status,
};
pub use move_groups::move_candidate_groups_response;
pub use move_history::move_history_response;
pub use moves::move_candidates_response;
pub use operations::operation_history_response;
pub use pinyin_search::{search_text_for_values, text_matches_search};
pub use product_ui::{
    auto_resolve_move_candidates, cancel_character_import_job, cleanup_stale_tag_single_references,
    confirm_artist_suggestion, delete_character_reference, folder_rename_auto_run,
    get_character_import_job, merge_move_candidate_group, operation_log_response,
    purge_pseudo_tag_single_references, rebuild_character_index, reconfirm_plan,
    run_idle_character_import_once, spawn_character_import_idle_worker, start_character_import_job,
    unconfirm_plan, update_folder_tags_by_name_response, update_folder_tags_response,
};
pub use recognition_status::{
    artist_recognition_status, character_model_signature, character_recognition_status,
    recognize_character_native, recognize_character_native_topk, suggest_artists_native,
};
pub use scan::{get_scan_state, resolve_scan_scope, run_full_library_scan, run_scan, ScanControl};
pub use scan_candidates_write::{
    apply_hash_unique_scan_candidate_response, apply_move_candidate_response,
    apply_scan_candidate_move_response, create_new_item_response, ignore_move_candidate_response,
    mark_move_candidate_new_response, resolve_existing_scan_candidate_response,
};
pub use similarity::{cluster_scores_response, MAX_CLUSTER_SCORE_VECTORS};
pub use tag_search::tag_search_response;
pub use tags::tags_response;
pub use tags_write::{
    create_tag, delete_tag, propagate_hash_tags_response, update_item_tags_by_name_response,
    update_item_tags_response, update_tag,
};
pub use workers::{spawn_configured_workers, WorkerStatus};

const DEFAULT_LIMIT: i64 = 100;
const MAX_LIMIT: i64 = 500;

pub fn normalize_pagination(limit: Option<i64>, offset: Option<i64>) -> (i64, i64) {
    let normalized_limit = match limit {
        Some(value) if value > 0 => value.min(MAX_LIMIT),
        _ => DEFAULT_LIMIT,
    };
    let normalized_offset = offset.unwrap_or(0).max(0);
    (normalized_limit, normalized_offset)
}

pub fn health() -> Value {
    health_summary(None, None)
}

/// Product-facing health shape used when Rust is the primary process on :8899.
///
/// The route layer adds the remaining read-only scan, backup, and log summaries;
/// this function owns the database and hash portion of that product contract.
pub fn health_summary(
    db_path: Option<&std::path::Path>,
    conn: Option<&rusqlite::Connection>,
) -> Value {
    let mut ok = true;
    let mut body = json!({
        "ok": true,
        "process": {"pid": std::process::id()},
        "runtime": "rust-primary",
    });
    if let Some(path) = db_path {
        let exists = path.exists();
        let meta = std::fs::metadata(path).ok();
        body["database"] = json!({
            "path": path.display().to_string(),
            "exists": exists,
            "size_bytes": meta.as_ref().map(|m| m.len()).unwrap_or(0),
            "updated_at": meta.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok()).map(|d| d.as_secs_f64()),
        });
        if !exists {
            ok = false;
        } else if meta.is_none() {
            ok = false;
            body["database_error"] = json!("database metadata unavailable");
        }
    }
    if let Some(conn) = conn {
        match hash_status_response(conn) {
            Ok(hash) => {
                body["hash"] = json!({
                    "blake3_available": true,
                    "items": hash.get("items").cloned().unwrap_or(json!({})),
                    "scan_candidates": hash.get("scan_candidates").cloned().unwrap_or(json!({})),
                });
            }
            Err(err) => {
                ok = false;
                body["database_error"] = json!(err.to_string());
            }
        }
        // Require core tables to exist for a healthy product process.
        if conn
            .query_row(
                "SELECT 1 FROM sqlite_master WHERE type='table' AND name='artists'",
                [],
                |r| r.get::<_, i64>(0),
            )
            .is_err()
        {
            ok = false;
            body["schema_error"] = json!("missing artists table");
        }
    } else if db_path.is_some() {
        // Caller expected a DB connection but could not open one.
        ok = false;
        body["database_error"] = json!("database connection unavailable");
    }
    body["ok"] = json!(ok);
    body
}

#[cfg(test)]
mod tests;
