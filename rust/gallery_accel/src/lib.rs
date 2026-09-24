use serde_json::{json, Value};

#[macro_use]
pub mod logging;

mod archive_format;
pub mod archive_ops;
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
mod db_identity;
mod dimensions;
mod duplicate_artists;
pub mod evidence_audit;
pub mod folder_archive;
mod folder_paths;
mod folder_tree;
mod folders;
pub mod fs_util;
pub mod hash_run;
mod hash_status;
mod image_preview;
pub mod ingest_publish;
mod item_dates;
mod item_detail;
mod item_detail_tags;
mod items;
mod link_index;
mod maintenance;
mod media_roots;
pub mod media_serve;
pub mod media_type;
pub mod model_config;
mod move_context;
mod move_filters;
mod move_group_logic;
mod move_groups;
mod move_history;
mod move_rows;
mod moves;
mod natural_sort;
pub mod netdisk;
pub mod netdisk_import;
mod operation_folder_renames;
mod operation_helpers;
mod operations;
mod path_display;
pub mod pawchive;
pub mod pawchive_groups;
pub mod pawchive_import;
pub mod pawchive_pairing;
pub mod pawchive_pairing_write;
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
pub mod work_naming;
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
pub use db::{
    env_db_path, open_writable_db, DbConfig, DbPool, PooledConn, StatsRefreshGate,
    DEFAULT_SQLITE_BUSY_TIMEOUT_MS,
};
pub use dimensions::backfill_item_dimensions;
pub use duplicate_artists::duplicate_artists_response;
pub use folder_archive::{
    create_db_backup, execute_folder_renames, folder_archive_failed_plans_count,
    folder_error_artists, folder_rename_auto_enabled, list_folder_renames, recheck_plan,
    run_folder_rename_all_now, run_folder_rename_auto_after_full_scan, set_folder_rename_auto,
    undo_folder_rename_plan, upsert_folder_rename_plans,
};
pub use folder_paths::folder_paths_response;
pub use folders::folders_response;
#[cfg(test)]
pub use hash_run::run_hash_batch;
pub use hash_run::{run_hash_batch_with_budget, run_hash_batch_with_roots, HashCommitBudget};
pub use hash_status::hash_status_response;
pub use image_preview::{
    clamp_max_edge, existing_preview_cache_file, image_preview_bytes,
    DEFAULT_MAX_EDGE as IMAGE_PREVIEW_DEFAULT_MAX_EDGE,
};
pub use item_dates::update_item_dates_response;
pub use item_detail::item_detail_response;
pub use items::items_page_cursor_query_response;
pub use items::items_page_query_response;
#[cfg(test)]
pub use items::items_page_response;
pub use items::set_item_favorite_response;
pub use link_index::{
    artist_links_response, reindex_artist_links, reindex_scanned_artist_links,
    reindex_scanned_items_links,
};
pub use maintenance::folder_rename_auto_response;
pub use media_roots::{env_media_roots, path_under_authorized_roots, MediaRoots};
pub use media_serve::{
    content_hash_allowed, delete_item_to_recycle, delete_to_recycle, preview_jpeg_allowed,
    resolve_allowed_path, serve_file_response, serve_text, serve_transcoded_hls,
    serve_transcoded_hls_segment, serve_video_compatible, serve_video_hls, start_video_transcode,
    video_frame_jpeg, video_transcode_status,
};
pub use move_groups::move_candidate_groups_response;
pub use move_history::move_history_response;
pub use moves::move_candidates_response;
pub use netdisk::{
    bridge_move_is_isolated, bridge_task_view, create_bridge_task, default_bridge_identity,
    ensure_netdisk_bridge_schema, ensure_netdisk_staging_directory, generate_event_scripter_script,
    latest_bridge_session, list_bridge_tasks, load_bridge_task, load_netdisk_settings,
    netdisk_is_disconnected, post_evidence_state, process_bridge_exchange, queue_bridge_command,
    remember_bridge_token, resolve_netdisk_staging_directory, rotate_bridge_token,
    save_netdisk_settings, saved_bridge_token, set_netdisk_disconnected, submit_bridge_task,
    verify_bridge_token, BridgeCapabilities, BridgeCommand, BridgeCommandResult, BridgeConflict,
    BridgeExchangePayload, BridgeExchangeResponse, BridgeInvalid, BridgeLinkStatus,
    BridgeSnapshotPage, BridgeTask, NetdiskSettings, BRIDGE_TASK_REGISTERED, BRIDGE_TASK_SETTLED,
    BRIDGE_TASK_SUBMITTED, NETDISK_BRIDGE_PAYLOAD_MAX_BYTES, NETDISK_PROTOCOL_VERSION,
    NETDISK_SCRIPT_VERSION,
};
pub use operations::operation_history_response;
pub use pawchive::{
    accept_legacy_scope, add_subscription_from_url, assess_post, assess_work_from_ledger,
    cancel_attempt, create_attempt, delete_subscription, demand_set, demand_set_for_work,
    finish_pawchive_sync, get_pawchive_settings, get_subscription,
    link_evidence_to_items, list_artist_posts_page, list_filtered_post_ids, list_pawchive_events,
    list_post_attempts, list_post_candidates, list_subscription_posts, list_subscriptions,
    pawchive_http_client, pawchive_redirect_policy, pawchive_round_due, pawchive_sync_status,
    plan_manual_post, record_external_receipt, record_post_decision, record_selection,
    record_selection_for_filter, required_resources, run_manual_attempt, run_pawchive_reconcile,
    run_pawchive_sync, save_pawchive_settings, schedule_round_order, set_subscription_mode,
    subscription_summary, toggle_subscription, try_begin_pawchive_sync, verify_post_files,
    AttemptError, AttemptOutcome, CancelOutcome, DecisionOutcome, DemandSet, ExternalReceipt,
    FilterSelectionOutcome, LegacyScopeOutcome, ManualPostPlan, PawchiveSettings,
    PawchiveSyncStatus, PostAssessment, PostDecisionAction, PostListFilter, PostState,
    ReceiptOutcome, RoundQueue, ScheduledPost, SelectionError, SelectionOutcome, StablePostPage,
    StablePostRow, SubscriptionMode, SyncTrigger, LEDGER_REASON_UNKNOWN_WORK,
    PAWCHIVE_FILTER_SELECTION_MAX_POSTS,
};
pub use pawchive_groups::{
    apply_grouping, begin_group_move_intent, claim_publish_reservation, content_group_locations,
    content_group_members, content_groups_for_day, ensure_content_group_schema,
    finish_group_move_intent, group_index, group_index_entries, list_content_groups,
    mark_group_location_manual, owning_group_for_path, relative_under_root,
    release_publish_reservation, relocate_groups_in_tx, verified_group_location,
    verified_location_for_post, ContentGroup, DatePrecision, GroupingApplyReport, GroupingResult,
    OwnedGroup, StoredContentGroup, StoredGroupLocation, StoredGroupMember, VerifiedLocation,
    GROUP_MOVE_INTENT_APPLIED, GROUP_MOVE_INTENT_FAILED, GROUP_MOVE_INTENT_PENDING,
    PUBLISH_RESERVATION_TTL_SECS,
};
pub use pawchive_pairing::{
    normalize_title_for_pairing, pair_day, preview_day_pairing, CandidateEdge, EdgeState,
    GroupEvidence, MatchBasis, PairingResult, WorkCandidate,
};
pub use pawchive_pairing_write::{
    group_is_excluded, group_location_paths, list_work_group_links, record_group_pairing,
    revoke_group_pairing, set_group_baseline_exclusion, BaselineOutcome, PairingOutcome,
};
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
    capture_item_snapshot, clear_recycle_entries, ensure_recycle_schema,
    purge_recycle_entry, reconcile_moving_recycle_entries,
    recycle_entries_response, restore_recycle_entry,
};
pub use scan::{
    get_scan_state, reconcile_interrupted_scan, resolve_scan_scope, run_full_library_scan,
    run_full_library_scan_claimed, run_scan, run_scan_claimed, update_scan_state, ScanControl,
    ScanSlotGuard,
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
pub use work_naming::{
    apply_naming_migration, check_archive_coordination, naming_revision, naming_rule_sources,
    plan_naming_migration, read_organize_rule, render, switch_naming, NamingApplyError,
    NamingApplyOutcome, NamingApplyRequest, NamingMigrationConflict, NamingMigrationConflictReason,
    NamingMigrationPreview, NamingMigrationPreviewRow, NamingTemplateSet, RenderedNaming,
    RenderedToken, SemanticVersion, WorkNamingContext,
};
pub use workers::{spawn_configured_workers, WorkerStatus};

pub const MAX_PAGINATION_LIMIT: i64 = 500;
pub const DEFAULT_PAGINATION_LIMIT: i64 = 100;
pub const MAX_OPERATION_LOG_LIMIT: i64 = 300;
pub const MAX_ITEM_PAGE_LIMIT: i64 = 200;
pub const DEFAULT_ITEM_PAGE_LIMIT: i64 = 50;
pub const MAX_PREVIEW_RECYCLE_LIMIT: i64 = 100;
pub const DEFAULT_PREVIEW_RECYCLE_LIMIT: i64 = 80;
pub const MAX_BATCH_ITEM_LIMIT: i64 = 5000;
pub const DEFAULT_BATCH_ITEM_LIMIT: i64 = 1000;
pub const MAX_RECENT_ERRORS_LIMIT: i64 = 120;

const DEFAULT_LIMIT: i64 = DEFAULT_PAGINATION_LIMIT;
const MAX_LIMIT: i64 = MAX_PAGINATION_LIMIT;

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

