use serde_json::{json, Value};

mod archive_format;
mod archive_profiles;
mod artist_folder_move;
mod artist_profile_links;
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
mod db_housekeeping;
mod duplicate_artists;
pub mod folder_archive;
mod folder_paths;
mod folder_tree;
mod folders;
pub mod hash_run;
mod hash_status;
mod image_preview;
mod item_dates;
mod item_detail;
mod item_detail_tags;
mod items;
mod link_index;
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
mod recycle;
pub mod runtime_prepare;
pub mod scan;
mod scan_candidates_write;
mod similarity;
mod tag_search;
mod tags;
mod tags_write;
pub mod upstream;
mod workers;


pub use archive_profiles::{
    apply_folder_rename_template, folder_rename_format_settings, preview_folder_rename_template,
    set_folder_rename_format_settings,
};
pub use artist_folder_move::{
    execute_artist_folder_move, list_media_root_directories, preview_artist_folder_move,
    reconcile_pending_artist_move,
};
pub use artist_profile_links::{
    artist_profile_links_response, create_artist_profile_link, delete_artist_profile_link,
};
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
    create_db_backup, execute_folder_renames, folder_archive_failed_plans_count,
    folder_error_artists, folder_rename_auto_enabled, list_folder_renames, recheck_plan,
    run_folder_rename_all_now, run_folder_rename_auto_after_full_scan, set_folder_rename_auto,
    undo_folder_rename_plan, upsert_folder_rename_plans,
};
pub use folder_paths::folder_paths_response;
pub use folders::folders_response;
pub use hash_run::{run_hash_batch, run_hash_batch_with_roots};
pub use hash_status::hash_status_response;
pub use image_preview::{
    clamp_max_edge, existing_preview_cache_file, image_preview_bytes, image_preview_response,
    DEFAULT_MAX_EDGE as IMAGE_PREVIEW_DEFAULT_MAX_EDGE,
};
pub use item_dates::update_item_dates_response;
pub use item_detail::item_detail_response;
pub use items::items_page_cursor_query_response;
pub use items::set_item_favorite_response;
pub use items::{items_page_query_response, items_page_response};
pub use link_index::{artist_links_response, reindex_artist_links, reindex_scanned_artist_links};
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
    auto_resolve_move_candidates, auto_resolve_move_candidates_with_roots,
    cancel_character_import_job, cleanup_stale_tag_single_references, confirm_all_artist_plans,
    confirm_artist_suggestion, delete_character_reference, folder_rename_auto_run,
    get_character_import_job, merge_move_candidate_group, merge_move_candidate_group_with_roots,
    operation_log_response, purge_pseudo_tag_single_references, rebuild_character_index,
    reconfirm_plan, run_idle_character_import_once, spawn_character_import_idle_worker,
    start_character_import_job, start_character_import_job_with_roots, unconfirm_all_artist_plans,
    unconfirm_plan, update_folder_tags_by_name_response, update_folder_tags_response,
};
pub use recognition_status::{
    artist_recognition_status, character_model_signature, character_recognition_status,
    recognize_character_native, recognize_character_native_topk,
    recognize_character_native_topk_with_roots, suggest_artists_native,
};
pub use recycle::{
    capture_item_snapshot, ensure_recycle_schema, reconcile_moving_recycle_entries,
    recycle_entries_response, restore_recycle_entry,
};
pub use scan::{
    get_scan_state, resolve_scan_scope, run_full_library_scan, run_scan, update_scan_state,
    ScanControl,
};
pub use scan_candidates_write::{
    apply_hash_unique_scan_candidate_response,
    apply_hash_unique_scan_candidate_response_with_roots, apply_move_candidate_response,
    apply_move_candidate_response_with_roots, apply_scan_candidate_move_response,
    apply_scan_candidate_move_response_with_roots, create_new_item_response,
    create_new_item_response_with_roots, ignore_move_candidate_response,
    mark_move_candidate_new_response, resolve_existing_scan_candidate_response,
    resolve_existing_scan_candidate_response_with_roots, resolve_scan_candidate_response,
    scan_candidates_response,
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
    let mut degraded_reasons = Vec::new();
    let mut body = json!({
        "ok": true,
        "degraded": false,
        "degraded_reasons": [],
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
        for (key, value) in database_storage_summary(path, conn) {
            body["database"][key.as_str()] = value;
        }
        if !exists {
            degraded_reasons.push("database_missing");
        } else if meta.is_none() {
            degraded_reasons.push("database_metadata_unavailable");
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
                degraded_reasons.push("database_error");
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
            degraded_reasons.push("schema_error");
            body["schema_error"] = json!("missing artists table");
        }
    } else if db_path.is_some() {
        // Caller expected a DB connection but could not open one.
        degraded_reasons.push("database_connection_unavailable");
        body["database_error"] = json!("database connection unavailable");
    }
    let degraded = !degraded_reasons.is_empty();
    body["degraded"] = json!(degraded);
    body["degraded_reasons"] = json!(degraded_reasons);
    body
}

fn database_storage_summary(
    db_path: &std::path::Path,
    conn: Option<&rusqlite::Connection>,
) -> serde_json::Map<String, Value> {
    let mut summary = serde_json::Map::from_iter([
        ("page_size_bytes".to_string(), Value::Null),
        ("page_count".to_string(), Value::Null),
        ("free_pages".to_string(), Value::Null),
        ("reclaimable_bytes".to_string(), Value::Null),
        ("wal_size_bytes".to_string(), Value::Null),
    ]);
    let mut errors = Vec::new();
    let mut page_size = None;
    let mut free_pages = None;
    if let Some(conn) = conn {
        for (key, pragma) in [
            ("page_size_bytes", "page_size"),
            ("page_count", "page_count"),
            ("free_pages", "freelist_count"),
        ] {
            match conn.query_row(&format!("PRAGMA {pragma}"), [], |row| row.get::<_, i64>(0)) {
                Ok(value) => {
                    if key == "page_size_bytes" {
                        page_size = Some(value);
                    } else if key == "free_pages" {
                        free_pages = Some(value);
                    }
                    summary.insert(key.to_string(), json!(value));
                }
                Err(error) => errors.push(format!("{pragma}: {error}")),
            }
        }
    } else {
        errors.push("sqlite connection unavailable".to_string());
    }
    if let (Some(page_size), Some(free_pages)) = (page_size, free_pages) {
        summary.insert(
            "reclaimable_bytes".to_string(),
            json!(page_size.saturating_mul(free_pages)),
        );
    }
    let wal_path = std::path::PathBuf::from(format!("{}-wal", db_path.display()));
    match std::fs::metadata(wal_path) {
        Ok(metadata) if metadata.is_file() => {
            summary.insert("wal_size_bytes".to_string(), json!(metadata.len()));
        }
        Ok(_) => {
            summary.insert("wal_size_bytes".to_string(), json!(0));
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            summary.insert("wal_size_bytes".to_string(), json!(0));
        }
        Err(error) => errors.push(format!("wal: {error}")),
    };
    if !errors.is_empty() {
        summary.insert("storage_error".to_string(), json!(errors.join("; ")));
    }
    summary
}

