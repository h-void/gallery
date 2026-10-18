use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use crate::route_params::{
    ArtistReferenceScoreRequest, CandidateQuery, CharacterSummaryQuery, CharactersQuery,
    FoldersQuery, GroupQuery, HistoryQuery, ItemsQuery, OperationHistoryQuery, ReferenceQuery,
    ScanCandidateQuery, TagSearchQuery, TagsQuery,
};
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{ConnectInfo, DefaultBodyLimit, Path, Query, Request, State};
use axum::http::{header, HeaderMap, HeaderValue, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use gallery_accel::product_ui::{read_log_tail as read_bounded_log_tail, recent_log_errors};
use gallery_accel::{
    apply_folder_rename_template, apply_hash_unique_scan_candidate_response_with_roots,
    apply_move_candidate_response_with_roots, apply_scan_candidate_move_response_with_roots,
    artist_detail_response, artist_links_response, artist_profile_links_response,
    artist_recognition_status, artist_reference_scores_response, artist_references_response,
    artist_stats_response, artists_response, auto_resolve_move_candidates_with_roots,
    backfill_item_dimensions, cancel_character_import_job, character_model_signature,
    character_recognition_status, character_references_response, character_response,
    character_summary_response, characters_response, clear_recycle_entries,
    cluster_scores_response, confirm_all_artist_plans, confirm_artist_suggestion,
    content_hash_allowed, create_artist_profile_link, create_db_backup,
    create_new_item_response_with_roots, create_tag, delete_artist_profile_link,
    delete_character_reference, delete_folder_to_recycle, delete_tag, delete_to_recycle,
    duplicate_artists_response, ensure_netdisk_staging_directory, env_media_roots,
    execute_artist_folder_move, execute_folder_renames, folder_archive_failed_plans_count,
    folder_error_artists, folder_paths_response, folder_rename_auto_response,
    folder_rename_auto_run, folder_rename_format_settings, folders_response,
    get_character_import_job, get_recycle_target_setting, get_scan_state, hash_status_response,
    health_summary, ignore_move_candidate_response, item_detail_response,
    items_page_cursor_query_response_ext, items_page_query_response_ext, list_folder_renames,
    list_media_root_directories, log_error, log_warn, mark_move_candidate_new_response,
    merge_move_candidate_group_with_roots, move_candidate_groups_response,
    move_candidates_response, move_history_response, open_writable_db, operation_history_response,
    operation_log_response, preview_artist_folder_move, preview_folder_rename_template,
    preview_jpeg_allowed, propagate_hash_tags_response, purge_recycle_entry,
    rebuild_character_index, recheck_plan, recognize_character_native_topk_with_roots,
    reconfirm_plan, recycle_entries_response, recycle_entry_preview, reindex_artist_links,
    resolve_existing_scan_candidate_response_with_roots, resolve_scan_scope, restore_recycle_entry,
    run_folder_rename_all_now, run_full_library_scan_claimed, run_hash_batch_with_roots,
    run_scan_claimed, scan_candidates_response, serve_file_response, serve_text,
    serve_transcoded_hls, serve_transcoded_hls_segment, serve_video_compatible, serve_video_hls,
    set_folder_rename_auto, set_folder_rename_format_settings, set_item_favorite_response,
    set_recycle_target_setting, spawn_character_import_job_with_roots, start_video_transcode,
    suggest_artists_native, tag_search_response, tags_response, unconfirm_all_artist_plans,
    unconfirm_plan, undo_folder_rename_plan, update_folder_tags_by_name_response,
    update_folder_tags_response, update_item_dates_response, update_item_tags_by_name_response,
    update_item_tags_response, update_tag, video_frame_jpeg, video_transcode_status, DbConfig,
    DbPool, MediaRoots, ScanControl, StatsRefreshGate, WorkerStatus, DEFAULT_BATCH_ITEM_LIMIT,
    DEFAULT_ITEM_PAGE_LIMIT, MAX_CLUSTER_SCORE_VECTORS, MAX_ITEM_PAGE_LIMIT,
};
use rusqlite::OptionalExtension;
use serde::Deserialize;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower_http::services::{ServeDir, ServeFile};

#[path = "routes_downloads.rs"]
mod downloads;
use downloads::{
    api_add_pawchive_subscription, api_check_pawchive_subscription, api_content_group_detail,
    api_content_groups, api_delete_pawchive_subscription, api_get_pawchive_settings,
    api_list_pawchive_subscriptions, api_mark_group_location, api_netdisk_bridge_exchange,
    api_netdisk_connect, api_netdisk_connection, api_netdisk_create_job, api_netdisk_disconnect,
    api_netdisk_get_script, api_netdisk_get_settings, api_netdisk_job_control,
    api_netdisk_list_jobs, api_netdisk_path_check, api_netdisk_rotate_token,
    api_netdisk_save_settings, api_netdisk_test, api_pawchive_accept_legacy_scope,
    api_pawchive_audit_evidence, api_pawchive_cancel_attempt, api_pawchive_check,
    api_pawchive_create_attempt, api_pawchive_create_selection, api_pawchive_events,
    api_pawchive_external_receipt, api_pawchive_file_retry, api_pawchive_index_groups,
    api_pawchive_list_post_files, api_pawchive_naming_apply, api_pawchive_naming_preview,
    api_pawchive_pairing_preview, api_pawchive_post_acquisition, api_pawchive_post_attempts,
    api_pawchive_post_candidates, api_pawchive_post_decision, api_pawchive_post_import,
    api_pawchive_post_list, api_pawchive_reconcile, api_pawchive_selection_preview,
    api_pawchive_status, api_pawchive_subscription_posts, api_pawchive_subscription_summary,
    api_pawchive_sync, api_pawchive_verify_post, api_reconcile_pawchive_subscription,
    api_save_pawchive_settings, api_set_pawchive_subscription_mode,
    api_toggle_pawchive_subscription,
};

#[derive(Clone, Copy)]
pub struct Capabilities {
    pub read_only: bool,
    pub writes: bool,
    pub media: bool,
    pub ml: bool,
}

impl Capabilities {
    fn allows_writes(self) -> bool {
        self.writes && !self.read_only
    }
    fn allows_media(self) -> bool {
        self.media
    }
    fn allows_ml(self) -> bool {
        self.ml
    }
}

#[derive(Clone)]
pub struct AppState {
    pool: Arc<DbPool>,
    roots: MediaRoots,
    capabilities: Capabilities,
    db_path: PathBuf,
    primary: bool,
    scan: Arc<ScanControl>,
    /// Shared with the background worker loops, so a manual run and a loop tick
    /// spend the same bounded statistics-refresh budget instead of each keeping
    /// its own interval.
    stats_gate: Arc<StatsRefreshGate>,
    dimension_backfill: Arc<Mutex<DimensionBackfillStatus>>,
    workers: WorkerStatus,
    data_dir: PathBuf,
    ui_log_max_bytes: u64,
    ui_log_backups: usize,
    /// The operator's management token, read once at construction.
    ///
    /// Held on the state rather than read from the environment per request so
    /// the gate is a property of the configured instance. That also keeps tests
    /// from having to mutate a process-wide environment variable, which under
    /// `cargo test`'s parallel threads made every other mutating management
    /// request in the process answer `403` for the duration of that one test.
    management_token: Option<String>,
}

#[derive(Clone, Default)]
struct DimensionBackfillStatus {
    running: bool,
    processed: u64,
    updated: u64,
    failed: u64,
    remaining: u64,
    next_after_id: i64,
    cursor_done: bool,
    complete: bool,
    error: Option<String>,
}

impl AppState {


    pub fn with_options(
        db_path: PathBuf,
        config: DbConfig,
        capabilities: Capabilities,
        primary: bool,
    ) -> anyhow::Result<Self> {
        let data_dir = PathBuf::from(std::env::var("DATA_DIR").unwrap_or_else(|_| "data".into()));
        let roots = env_media_roots();
        if primary {
            let _ = ensure_netdisk_staging_directory(&roots);
        }
        Ok(Self {
            pool: Arc::new(DbPool::with_config(db_path.clone(), config)?),
            roots,
            capabilities,
            db_path,
            primary,
            scan: Arc::new(ScanControl::new()),
            stats_gate: Arc::new(StatsRefreshGate::from_env()),
            dimension_backfill: Arc::new(Mutex::new(DimensionBackfillStatus::default())),
            workers: WorkerStatus::default(),
            data_dir,
            ui_log_max_bytes: env_positive_u64("UI_LOG_MAX_BYTES", UI_LOG_MAX_BYTES),
            ui_log_backups: env_positive_u64("UI_LOG_BACKUP_COUNT", UI_LOG_BACKUP_COUNT as u64)
                .min(10) as usize,
            management_token: configured_management_token(),
        })
    }


    pub fn worker_inputs(
        &self,
    ) -> (
        Arc<DbPool>,
        MediaRoots,
        Arc<ScanControl>,
        WorkerStatus,
        Arc<StatsRefreshGate>,
    ) {
        (
            Arc::clone(&self.pool),
            self.roots.clone(),
            Arc::clone(&self.scan),
            self.workers.clone(),
            Arc::clone(&self.stats_gate),
        )
    }
}

pub fn router(state: AppState) -> Router {
    let app = Router::new()
        .route("/favicon.ico", get(api_favicon))
        .route("/api/health", get(api_health))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/content-hash", get(api_content_hash))
        .route("/api/image-preview", get(api_image_preview))
        .route("/api/file/preview", get(api_file_preview))
        .route("/api/archives/inspect", post(api_archive_inspect))
        .route("/api/archives/entry", get(api_archive_entry))
        .route("/api/archives/extract", post(api_archive_extract))
        .route("/api/hash/status", get(api_hash_status))
        .route("/api/move-candidates", get(api_move_candidates))
        .route("/api/scan-candidates", get(api_scan_candidates))
        .route(
            "/api/move-candidates/groups",
            get(api_move_candidate_groups),
        )
        .route("/api/move-history", get(api_move_history))
        .route("/api/operation-log", get(api_operation_log))
        .route("/api/operation-log/history", get(api_operation_history))
        .route("/api/folder-renames/auto", get(api_folder_rename_auto))
        .route(
            "/api/folder-renames/auto/run",
            post(api_folder_rename_auto_run),
        )
        .route("/api/artists/duplicates", get(api_duplicate_artists))
        .route("/api/media-roots", get(api_media_roots))
        .route(
            "/api/media-roots/directories",
            get(api_media_root_directories),
        )
        .route("/api/artists", get(api_artists))
        .route("/api/artists/{artist_id}", get(api_artist_detail))
        .route(
            "/api/artists/{artist_id}/folder-move/preview",
            post(api_artist_folder_move_preview),
        )
        .route(
            "/api/artists/{artist_id}/folder-move/execute",
            post(api_artist_folder_move_execute),
        )
        .route(
            "/api/artists/{artist_id}/profile-links",
            get(api_artist_profile_links).post(api_create_artist_profile_link),
        )
        .route(
            "/api/artists/{artist_id}/profile-links/{link_id}",
            delete(api_delete_artist_profile_link),
        )
        .route("/api/artists/{artist_id}/links", get(api_artist_links))
        .route(
            "/api/artists/{artist_id}/links/reindex",
            post(api_artist_links_reindex),
        )
        .route(
            "/api/artists/{artist_id}/references",
            get(api_artist_references),
        )
        .route(
            "/api/artists/{artist_id}/folder-paths",
            get(api_folder_paths),
        )
        .route("/api/folders", get(api_folders))
        .route("/api/folders/tags", put(api_update_folder_tags))
        .route(
            "/api/folders/tags-by-name",
            put(api_update_folder_tags_by_name),
        )
        .route("/api/folders/annotate", post(api_annotate_folder))
        .route(
            "/api/folders/delete",
            post(api_delete_folder).delete(api_delete_folder),
        )
        .route("/api/artists/{artist_id}/stats", get(api_artist_stats))
        .route("/api/items", get(api_items_page))
        .route(
            "/api/items/dimensions/backfill",
            post(api_backfill_item_dimensions),
        )
        .route(
            "/api/items/dimensions/backfill/status",
            get(api_backfill_item_dimensions_status),
        )
        .route("/api/items/tags", put(api_update_item_tags))
        .route("/api/items/tags-by-name", put(api_update_item_tags_by_name))
        .route("/api/items/bundle", post(api_bundle_items))
        .route("/api/items/date", put(api_update_item_dates))
        .route("/api/items/{item_id}/favorite", put(api_set_item_favorite))
        .route("/api/items/{item_id}", get(api_item_detail))
        .route("/api/tags/search", get(api_tag_search))
        .route("/api/tags", get(api_tags))
        .route("/api/tags", post(api_create_tag))
        .route("/api/tags/propagate-hash", post(api_propagate_hash_tags))
        .route(
            "/api/scan-candidates/resolve-existing",
            post(api_resolve_existing_scan_candidate),
        )
        .route(
            "/api/scan-candidates/apply-hash-unique",
            post(api_apply_hash_unique_scan_candidate),
        )
        .route(
            "/api/scan-candidates/apply-move",
            post(api_apply_scan_candidate_move),
        )
        .route(
            "/api/scan-candidates/create-new-item",
            post(api_create_new_item_scan_candidate),
        )
        .route("/api/move-candidates/apply", post(api_apply_move_candidate))
        .route(
            "/api/move-candidates/ignore",
            post(api_ignore_move_candidate),
        )
        .route(
            "/api/move-candidates/mark-new",
            post(api_mark_move_candidate_new),
        )
        // Public UI paths (Python historical names) — same handlers as sidecar write routes.
        .route(
            "/api/move-candidates/{candidate_id}/confirm",
            post(api_confirm_move_candidate_public),
        )
        .route(
            "/api/move-candidates/{candidate_id}/ignore",
            post(api_ignore_move_candidate_public),
        )
        .route(
            "/api/move-candidates/{candidate_id}/new",
            post(api_mark_move_candidate_new_public),
        )
        .route(
            "/api/move-candidates/auto-resolve",
            post(api_move_auto_resolve),
        )
        .route(
            "/api/move-candidates/groups/{old_artist_id}/{new_artist_id}/merge",
            post(api_move_group_merge),
        )
        .route("/ws/scan", get(api_ws_scan))
        .route(
            "/api/tags/{tag_id}",
            put(api_update_tag).delete(api_delete_tag),
        )
        .route("/api/characters", get(api_characters))
        .route("/api/characters/summary", get(api_character_summary))
        .route("/api/characters/{character_id}", get(api_character))
        .route(
            "/api/characters/{character_id}/references",
            get(api_character_references),
        )
        .route(
            "/api/artist-reference-scores",
            post(api_artist_reference_scores),
        )
        .route("/api/cluster-scores", post(api_cluster_scores))
        // Pure-Rust product routes (no residual required)
        .route("/api/scan", post(api_scan_start))
        .route("/api/scan/folder", post(api_scan_folder))
        .route("/api/scan/stop", post(api_scan_stop))
        .route("/api/scan/state", get(api_scan_state))
        .route("/api/hash/run", post(api_hash_run))
        .route("/api/file", get(api_serve_file))
        .route("/api/file/stream", get(api_serve_file))
        .route("/api/file/text", get(api_file_text))
        .route("/api/file/delete", delete(api_file_delete))
        .route(
            "/api/recycle",
            get(api_recycle_entries).delete(api_recycle_clear),
        )
        .route("/api/recycle/clear", post(api_recycle_clear))
        .route(
            "/api/recycle/settings",
            get(api_recycle_settings_get)
                .put(api_recycle_settings_put)
                .post(api_recycle_settings_put),
        )
        .route("/api/recycle/{entry_id}", delete(api_recycle_purge))
        .route("/api/recycle/{entry_id}/delete", post(api_recycle_purge))
        .route("/api/recycle/{entry_id}/restore", post(api_recycle_restore))
        .route("/api/recycle/{entry_id}/preview", get(api_recycle_preview))
        .route("/api/file/video-frame", get(api_video_frame))
        .route("/api/file/video-compatible", get(api_video_compatible))
        .route("/api/file/video-hls", get(api_video_hls))
        .route("/api/file/video-transcoded", get(api_video_transcoded))
        .route(
            "/api/file/video-transcoded-segment/{key}/{segment}",
            get(api_video_transcoded_segment),
        )
        .route("/api/file/video-transcode", post(api_video_transcode))
        .route(
            "/api/file/video-transcode-status",
            get(api_video_transcode_status),
        )
        .route("/api/folder-renames", get(api_folder_renames_list))
        .route(
            "/api/folder-renames/refresh",
            post(api_folder_renames_refresh),
        )
        .route(
            "/api/folder-renames/error-artists",
            get(api_folder_rename_error_artists),
        )
        .route(
            "/api/folder-renames/settings",
            get(api_folder_renames_settings).put(api_folder_renames_settings_put),
        )
        .route(
            "/api/folder-renames/preview",
            post(api_folder_renames_preview),
        )
        .route(
            "/api/folder-renames/apply-template",
            post(api_folder_renames_apply_template),
        )
        .route(
            "/api/folder-renames/execute",
            post(api_folder_renames_execute),
        )
        .route(
            "/api/folder-renames/execute-all",
            post(api_folder_renames_execute_all),
        )
        .route("/api/folder-renames/auto", put(api_folder_renames_auto_put))
        .route(
            "/api/folder-renames/plans/{plan_id}/recheck",
            post(api_folder_plan_recheck),
        )
        .route(
            "/api/folder-renames/plans/{plan_id}/reconfirm",
            post(api_folder_plan_reconfirm),
        )
        .route(
            "/api/folder-renames/plans/{plan_id}/unconfirm",
            post(api_folder_plan_unconfirm),
        )
        .route(
            "/api/folder-renames/confirm-all",
            post(api_folder_plans_confirm_all),
        )
        .route(
            "/api/folder-renames/unconfirm-all",
            post(api_folder_plans_unconfirm_all),
        )
        .route(
            "/api/folder-renames/plans/{plan_id}/undo",
            post(api_folder_plan_undo),
        )
        .route("/api/backup", post(api_backup))
        .route("/api/ui-log", post(api_ui_log))
        .route("/api/logs/tail", get(api_logs_tail))
        .route(
            "/api/character-recognition/status",
            get(api_character_status),
        )
        .route(
            "/api/character-recognition/model-signature",
            get(api_character_signature),
        )
        .route("/api/artist-recognition/status", get(api_artist_status))
        .route("/api/ml-runtime/status", get(api_ml_runtime_status))
        .route("/api/ml-runtime/settings", get(api_ml_runtime_settings_get))
        .route("/api/ml-runtime/settings", put(api_ml_runtime_settings_put))
        .route("/api/ml-runtime/retry", post(api_ml_runtime_retry))
        .route(
            "/api/items/{item_id}/artist-suggestions",
            post(api_artist_suggestions),
        )
        .route(
            "/api/items/{item_id}/character-recognition",
            post(api_character_recognize),
        )
        .route(
            "/api/items/{item_id}/artist-suggestions/{artist_id}/confirm",
            post(api_confirm_artist_suggestion),
        )
        .route("/api/characters", post(api_create_character))
        .route(
            "/api/characters/{character_id}",
            delete(api_delete_character),
        )
        .route(
            "/api/characters/{character_id}/references/{reference_id}",
            delete(api_delete_character_reference),
        )
        // Manual reference photo. The body is the raw image and the upload cap
        // is set per-route: axum's default body limit is 2 MB, far below a
        // normal photo.
        .route(
            "/api/characters/{character_id}/references/upload",
            post(api_upload_character_reference).layer(DefaultBodyLimit::max(
                gallery_accel::product_ui::REFERENCE_IMAGE_MAX_BYTES,
            )),
        )
        .route(
            "/api/characters/{character_id}/references/{reference_id}/image",
            get(api_character_reference_image),
        )
        .route(
            "/api/characters/import-from-tags/jobs/current",
            get(api_character_import_job_current),
        )
        .route(
            "/api/characters/import-from-tags/jobs",
            post(api_character_import_job_start),
        )
        .route(
            "/api/characters/import-from-tags/jobs/{job_id}/cancel",
            post(api_character_import_job_cancel),
        )
        .route(
            "/api/admin/rebuild-character-index",
            post(api_rebuild_character_index),
        )
        .route(
            "/api/pawchive/settings",
            get(api_get_pawchive_settings).put(api_save_pawchive_settings),
        )
        .route(
            "/api/pawchive/subscriptions",
            get(api_list_pawchive_subscriptions).post(api_add_pawchive_subscription),
        )
        .route(
            "/api/pawchive/subscriptions/{id}",
            delete(api_delete_pawchive_subscription),
        )
        .route(
            "/api/pawchive/subscriptions/{id}/toggle",
            post(api_toggle_pawchive_subscription),
        )
        .route(
            "/api/pawchive/subscriptions/{id}/mode",
            post(api_set_pawchive_subscription_mode),
        )
        .route("/api/pawchive/status", get(api_pawchive_status))
        .route("/api/pawchive/events", get(api_pawchive_events))
        .route("/api/pawchive/sync", post(api_pawchive_sync))
        .route("/api/pawchive/check", post(api_pawchive_check))
        // Observation only: re-derives every post from the library without
        // claiming or downloading anything, so the panel can show what is
        // actually there before any download is authorised.
        .route("/api/pawchive/reconcile", post(api_pawchive_reconcile))
        // Per-artist halves of the two buttons above, for the subscription row.
        // Same single round slot, so the panel's status line still describes
        // whichever round is actually running.
        .route(
            "/api/pawchive/subscriptions/{id}/check",
            post(api_check_pawchive_subscription),
        )
        .route(
            "/api/pawchive/subscriptions/{id}/reconcile",
            post(api_reconcile_pawchive_subscription),
        )
        .route(
            "/api/pawchive/audit-evidence",
            get(api_pawchive_audit_evidence),
        )
        .route(
            "/api/pawchive/subscriptions/{id}/summary",
            get(api_pawchive_subscription_summary),
        )
        .route(
            "/api/pawchive/subscriptions/{id}/posts",
            get(api_pawchive_subscription_posts),
        )
        .route(
            "/api/pawchive/posts/{id}/decisions",
            post(api_pawchive_post_decision),
        )
        .route(
            "/api/pawchive/posts/{id}/accept-legacy-scope",
            post(api_pawchive_accept_legacy_scope),
        )
        .route(
            "/api/pawchive/posts/{id}/acquisition",
            get(api_pawchive_post_acquisition),
        )
        .route(
            "/api/pawchive/posts/{id}/candidates",
            get(api_pawchive_post_candidates),
        )
        .route(
            "/api/pawchive/posts/{id}/attempts",
            get(api_pawchive_post_attempts),
        )
        .route(
            "/api/pawchive/selections/preview",
            post(api_pawchive_selection_preview),
        )
        .route(
            "/api/pawchive/selections",
            post(api_pawchive_create_selection),
        )
        .route("/api/pawchive/attempts", post(api_pawchive_create_attempt))
        .route(
            "/api/pawchive/attempts/{attempt_id}/cancel",
            post(api_pawchive_cancel_attempt),
        )
        .route(
            "/api/pawchive/posts/{id}/verify",
            post(api_pawchive_verify_post),
        )
        // 单文件重试 and 手动入库. Both are explicit instructions about one
        // resource or one work, so they are not gated on 启用订阅 or the
        // subscription's mode; they are still gated on read-only and on the
        // publish protocol that keeps an existing file from being replaced.
        .route(
            "/api/pawchive/posts/{id}/files",
            get(api_pawchive_list_post_files),
        )
        .route(
            "/api/pawchive/posts/{id}/files/{file_id}/retry",
            post(api_pawchive_file_retry),
        )
        .route(
            "/api/pawchive/posts/{id}/import",
            post(api_pawchive_post_import),
        )
        // Reconciliation reads: the stable work list, the content groups the
        // local index produced, the naming preview, and the same-day candidate
        // graph. None of them decides anything; the only writer here is the
        // manual-location mark, which records that the user moved a group.
        .route("/api/pawchive/posts", get(api_pawchive_post_list))
        .route("/api/pawchive/groups", get(api_content_groups))
        .route(
            "/api/pawchive/groups/{group_id}",
            get(api_content_group_detail),
        )
        .route(
            "/api/pawchive/groups/{group_id}/locations",
            post(api_mark_group_location),
        )
        .route(
            "/api/pawchive/pairing/preview",
            get(api_pawchive_pairing_preview),
        )
        .route(
            "/api/pawchive/naming/preview",
            get(api_pawchive_naming_preview),
        )
        .route(
            "/api/pawchive/naming/apply",
            post(api_pawchive_naming_apply),
        )
        .route(
            "/api/pawchive/groups/index",
            post(api_pawchive_index_groups),
        )
        .route(
            "/api/pawchive/receipts",
            post(api_pawchive_external_receipt),
        )
        // /api/v1 compatibility surface (B6 gate)
        .route(
            "/api/v1/settings",
            get(api_get_pawchive_settings).put(api_save_pawchive_settings),
        )
        .route(
            "/api/v1/subscriptions",
            get(api_list_pawchive_subscriptions).post(api_add_pawchive_subscription),
        )
        .route(
            "/api/v1/subscriptions/{id}",
            delete(api_delete_pawchive_subscription),
        )
        .route(
            "/api/v1/subscriptions/{id}/posts",
            get(api_pawchive_subscription_posts),
        )
        .route("/api/v1/status", get(api_pawchive_status))
        .route("/api/v1/posts", get(api_pawchive_post_list))
        .route(
            "/api/v1/posts/{id}/acquisition",
            get(api_pawchive_post_acquisition),
        )
        .route("/api/v1/attempts", post(api_pawchive_create_attempt))
        .route(
            "/api/v1/attempts/{attempt_id}/cancel",
            post(api_pawchive_cancel_attempt),
        )
        .route("/api/v1/receipts", post(api_pawchive_external_receipt))
        // Netdisk & JDownloader Local Bridge routes
        .route(
            "/api/netdisk/settings",
            get(api_netdisk_get_settings).put(api_netdisk_save_settings),
        )
        .route("/api/netdisk/token/rotate", post(api_netdisk_rotate_token))
        .route("/api/netdisk/script", post(api_netdisk_get_script))
        .route("/api/netdisk/test", post(api_netdisk_test))
        .route("/api/netdisk/connection", get(api_netdisk_connection))
        .route("/api/netdisk/connect", post(api_netdisk_connect))
        .route("/api/netdisk/disconnect", post(api_netdisk_disconnect))
        .route("/api/netdisk/path-check", post(api_netdisk_path_check))
        .route(
            "/api/netdisk/jobs",
            get(api_netdisk_list_jobs).post(api_netdisk_create_job),
        )
        .route(
            "/api/netdisk/jobs/{task_id}/{action}",
            post(api_netdisk_job_control),
        )
        .route(
            "/api/netdisk/bridge/exchange",
            post(api_netdisk_bridge_exchange),
        );

    app.layer(middleware::from_fn_with_state(
        state.clone(),
        capability_gate,
    ))
    .layer(middleware::from_fn_with_state(
        state.clone(),
        management_gate,
    ))
    .layer(middleware::from_fn(security_headers))
    .with_state(state)
}

/// Global hardening header: browsers must not sniff content types. Media
/// responses already carry a strict inline whitelist; this covers every other
/// response (including errors and static assets).
pub async fn security_headers(request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mut response = next.run(request).await;
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    // Static assets carry a release ?v= string; no-cache plus 304 revalidation
    // keeps an unbumped version from serving stale JS/CSS after an upgrade.
    //
    // The document routes need it more than the assets do, and they are not
    // under `/static/`: `/` and `/{artist_path}` serve `index.html`, and a
    // browser may reuse that HTML from heuristic freshness (a fraction of its
    // age since `Last-Modified`) without ever asking. The new JS is then loaded
    // by the old document, which is the one stale-bundle case the `?v=` bump
    // cannot fix, because the old HTML never requests the new URLs. No
    // extension in the path is what identifies those two routes: an artist path
    // is a single bare segment, and every asset has one.
    let cacheable_document = path == "/" || !path.contains('.');
    if (path.starts_with("/static/") || cacheable_document)
        && !response.headers().contains_key(header::CACHE_CONTROL)
    {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
    }
    response
}

fn is_media_path(path: &str) -> bool {
    path == "/api/content-hash"
        || path == "/api/image-preview"
        || path == "/api/file"
        || path.starts_with("/api/file/")
        // The recycle preview renders library images too, so a process started
        // without --enable-media must refuse it like /api/file does instead of
        // serving the same bytes through another path.
        || (path.starts_with("/api/recycle/") && path.ends_with("/preview"))
        // Archive members are library bytes too. Without this, a process
        // started without --enable-media answered 403 on /api/file and then
        // handed out the same full-resolution member through this GET route.
        || path == "/api/archives/entry"
}

fn is_ml_path(path: &str) -> bool {
    path == "/api/cluster-scores"
        || path == "/api/artist-reference-scores"
        || path == "/api/admin/rebuild-character-index"
        || path == "/api/ml-runtime/status"
        || path == "/api/ml-runtime/settings"
        || path == "/api/ml-runtime/retry"
        || path.starts_with("/api/characters/import-from-tags")
        // Uploading a reference photo embeds it, so it needs the ML stack too.
        || (path.starts_with("/api/characters/") && path.ends_with("/references/upload"))
        || path.contains("/artist-suggestions")
        || path.contains("/character-recognition")
}

fn is_nonmutating_post(path: &str) -> bool {
    path == "/api/cluster-scores"
        || path == "/api/artist-reference-scores"
        || path.ends_with("/artist-suggestions")
        || path.contains("/character-recognition")
        || path == "/api/folder-renames/preview"
        || path == "/api/pawchive/selection-preview"
        || path == "/api/netdisk/path-check"
        || path == "/api/netdisk/test"
}

async fn capability_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path();
    if is_media_path(path) && !state.capabilities.allows_media() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "media capability is disabled"})),
        )
            .into_response();
    }
    if is_ml_path(path) && !state.capabilities.allows_ml() {
        return (
            StatusCode::FORBIDDEN,
            Json(json!({"error": "ml capability is disabled"})),
        )
            .into_response();
    }
    if !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    ) && !is_nonmutating_post(path)
        && !state.capabilities.allows_writes()
    {
        return (
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        )
            .into_response();
    }
    next.run(request).await
}

/// The state-changing routes the plan calls the download-management surface.
///
/// This install has no accounts: it is a trusted-LAN application that serves
/// its own UI. Anything that starts work, changes where work writes, or records
/// a decision therefore has to be a request the operator made from that UI —
/// not one a page somewhere else in the browser talked the operator into. A GET
/// never mutates, so the method check in `management_gate` is what separates
/// reads from writes.
///
/// The answer is default-deny per namespace rather than a hand-written list of
/// handlers. Enumerating them failed twice: `/api/pawchive/selections` and
/// `/api/pawchive/attempts` were added with a download behind them and no entry
/// here, so a cross-site page could start a fetch with a simple POST. Under the
/// prefix rule a new mutating route is gated by construction, and the only way
/// to lose the gate is to add it to `EXEMPT` on purpose.
fn is_management_path(path: &str) -> bool {
    /// The one write route that is not the operator's: the receipt endpoint an
    /// external downloader posts to. It carries its own contract (a version and
    /// task id that must match, a loopback peer, a completed file) and is not a
    /// browser panel call.
    const EXEMPT: [&str; 1] = ["/api/pawchive/receipts"];
    if EXEMPT.contains(&path) {
        return false;
    }
    path.starts_with("/api/pawchive/") || path == "/api/pawchive" || path.starts_with("/api/admin/")
}

/// Whether the request's `Origin` matches the authority it was sent to.
///
/// A browser attaches `Origin` to every cross-site request and never lets the
/// page forge it, so a mismatch is a page that is not this UI. Requests with no
/// `Origin` at all are not browser cross-site requests — curl, the FNpack
/// tooling and the local test suites all come through this way — and are left
/// to the capability check.
fn request_origin_is_same_site(headers: &HeaderMap) -> bool {
    let Some(origin) = headers
        .get(header::ORIGIN)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return true;
    };
    let Some(host) = headers
        .get(header::HOST)
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .filter(|value| !value.is_empty())
    else {
        return false;
    };
    // "null" is what a sandboxed or file:// page sends; it names no authority,
    // so it can never match the one this request reached.
    origin.eq_ignore_ascii_case(&format!("http://{host}"))
        || origin.eq_ignore_ascii_case(&format!("https://{host}"))
}

/// The management token, when the operator set one.
///
/// Unset is the default: the plan's boundary is the local UI, and demanding a
/// token from it would break installs that never had one. Setting
/// `GALLERY_MANAGEMENT_TOKEN` adds the authorization half for deployments that
/// expose the app beyond the machine that runs it.
fn configured_management_token() -> Option<String> {
    std::env::var("GALLERY_MANAGEMENT_TOKEN")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn management_token_matches(headers: &HeaderMap, expected: &str) -> bool {
    headers
        .get("x-gallery-management-token")
        .and_then(|value| value.to_str().ok())
        .map(str::trim)
        .is_some_and(|presented| presented == expected)
}

/// Gate the download-management routes on the operator's own origin, and on a
/// management token when one is configured.
async fn management_gate(State(state): State<AppState>, request: Request, next: Next) -> Response {
    let path = request.uri().path().to_owned();
    let mutating = !matches!(
        *request.method(),
        Method::GET | Method::HEAD | Method::OPTIONS
    );
    if mutating && is_management_path(&path) {
        if let Some(expected) = state.management_token.as_deref() {
            if !management_token_matches(request.headers(), expected) {
                return (
                    StatusCode::FORBIDDEN,
                    Json(json!({"error": "management token is required for this route"})),
                )
                    .into_response();
            }
        }
        if !request_origin_is_same_site(request.headers()) {
            return (
                StatusCode::FORBIDDEN,
                Json(json!({"error": "cross-site management request is refused"})),
            )
                .into_response();
        }
    }
    next.run(request).await
}

/// Attach classic static UI (`index.html` + assets) for primary mode.
///
/// UI loads `/static/style.css` and `/static/js/*.js` from the same directory
/// that holds `index.html` (mirrors FastAPI StaticFiles mount).
pub fn with_static_ui(router: Router, static_dir: PathBuf) -> Router {
    let index = static_dir.join("index.html");
    router
        .route_service("/", ServeFile::new(index.clone()))
        .nest_service("/static", ServeDir::new(static_dir))
        .route_service("/{artist_path}", ServeFile::new(index))
        .layer(middleware::from_fn(security_headers))
}

/// Whether a stored path names `root` itself or something inside it, judged on
/// path segments rather than on the raw string prefix.
///
/// A bare `str::starts_with` made `/pictures` claim `/pictures2/Alice` and
/// `/pictures_old/Alice`, inflating the health panel's per-root artist counts
/// for any install whose roots share a prefix. Separators are normalized first:
/// `artists.path` keeps the platform separator, while the configured roots may
/// spell it either way.
fn stored_path_is_under_root(path: &str, root: &str) -> bool {
    let path = path.replace('\\', "/");
    let root = root.replace('\\', "/");
    let root = root.trim_end_matches('/');
    if root.is_empty() {
        return false;
    }
    path == root
        || path
            .strip_prefix(root)
            .is_some_and(|rest| rest.starts_with('/'))
}

/// Blocking portion of `/api/health`: SQLite reads, backup-dir listing,
/// per-root `is_dir` probes, and bounded log tails. Runs on the blocking
/// pool so a slow NAS mount cannot stall async workers.
fn health_body(
    pool: &std::sync::Arc<DbPool>,
    db_path: &std::path::Path,
    data_dir: &std::path::Path,
    roots: &MediaRoots,
    workers: &Value,
) -> Value {
    let conn = pool.get().ok();
    let mut body = health_summary(Some(db_path), conn.as_deref());
    body["workers"] = workers.clone();
    for name in ["scan", "hash", "backup"] {
        if workers[name]["last"]["error"].as_str().is_some() {
            mark_degraded(&mut body, &format!("worker_{name}"));
        }
    }
    let scan = match conn.as_deref() {
        Some(conn) => match get_scan_state(conn) {
            Ok(scan) => scan,
            Err(error) => {
                let message = error.to_string();
                body["scan_error"] = json!(message);
                mark_degraded(&mut body, "database_error");
                json!({"status": "error", "phase": message})
            }
        },
        None => {
            mark_degraded(&mut body, "database_connection_unavailable");
            json!({"status": "unknown", "phase": "database connection unavailable"})
        }
    };
    body["scan"] = scan;
    body["scan_schedule"] = worker_schedule(
        workers,
        "scan",
        env_positive_interval("SCAN_INTERVAL"),
        false,
        "next_auto_scan_at",
    );
    body["backups"] = backup_summary(&data_dir.join("db-backups"));
    if body["backups"]["error"].as_str().is_some() {
        mark_degraded(&mut body, "backup_error");
    }
    // Count of authorized real media roots only — never dump untrusted host paths.
    let media_root_count = roots.real_paths.len().max(roots.roots.len());
    let accessible = (0..roots.roots.len())
        .filter(|&i| {
            roots
                .real_root_at(i)
                .map(|p| std::path::Path::new(p).is_dir())
                .unwrap_or(false)
        })
        .count();
    let per_root_artists: Vec<Value> = if let Some(conn) = conn.as_deref() {
        let artist_paths: Vec<String> = conn
            .prepare("SELECT path FROM artists WHERE missing = 0")
            .and_then(|mut stmt| {
                stmt.query_map([], |row| row.get::<_, String>(0))?
                    .collect::<rusqlite::Result<Vec<_>>>()
            })
            .unwrap_or_default();
        (0..roots.roots.len())
            .map(|i| {
                let virtual_root = &roots.roots[i];
                let real_root = roots.real_root_at(i).unwrap_or(virtual_root);
                let label = roots.labels.get(i).cloned().unwrap_or_default();
                let count = artist_paths
                    .iter()
                    .filter(|path| {
                        stored_path_is_under_root(path, virtual_root)
                            || stored_path_is_under_root(path, real_root)
                    })
                    .count();
                json!({
                    "root_index": i,
                    "label": label,
                    "artist_count": count as i64,
                })
            })
            .collect()
    } else {
        Vec::new()
    };
    body["media_roots"] = json!({
        "count": media_root_count as i64,
        "accessible": accessible as i64,
        "per_root_artists": per_root_artists,
    });
    body["backups"]["last_backup_time"] = body["backups"]["latest"]["updated_at"].clone();
    let hk_summary = backup_summary(&data_dir.join("db-backups").join("housekeeping"));
    body["backups"]["last_housekeeping_backup_time"] = hk_summary["latest"]["updated_at"].clone();
    body["logs"] = health_logs_summary(data_dir);
    body["recent_errors"] = json!(recent_log_errors(&data_dir.join("logs"), 8));
    body["backup_schedule"] = worker_schedule(
        workers,
        "backup",
        env_positive_interval("DB_BACKUP_INTERVAL"),
        env_flag("DB_BACKUP_ON_START"),
        "next_run_at",
    );
    if let Some(conn) = conn.as_deref() {
        let recycle_moving: Option<i64> = conn
            .query_row(
                "SELECT COUNT(*) FROM recycle_entries WHERE status = 'moving'",
                [],
                |row| row.get(0),
            )
            .ok();
        body["recycle"] = json!({
            "awaiting_reconciliation": recycle_moving,
        });
        match folder_archive_failed_plans_count(conn) {
            Ok(count) => {
                body["folder_archive"] = json!({"failed_plans": count});
                if count > 0 {
                    mark_degraded(&mut body, "folder_archive_failed_plans");
                }
            }
            Err(error) => {
                body["folder_archive"] = json!({
                    "failed_plans": Value::Null,
                    "error": error.to_string()
                });
                mark_degraded(&mut body, "database_error");
            }
        }
    } else {
        body["recycle"] = json!({"awaiting_reconciliation": Value::Null});
        body["folder_archive"] = json!({"failed_plans": Value::Null});
    }
    body
}

async fn api_health(State(state): State<AppState>) -> Json<Value> {
    let workers = state.workers.snapshot();
    let mut body = {
        let pool = std::sync::Arc::clone(&state.pool);
        let db_path = state.db_path.clone();
        let data_dir = state.data_dir.clone();
        let roots = state.roots.clone();
        tokio::task::spawn_blocking(move || {
            health_body(&pool, &db_path, &data_dir, &roots, &workers)
        })
        .await
        .unwrap_or_else(|error| {
            let mut body = json!({"error": error.to_string()});
            mark_degraded(&mut body, "health_worker_failed");
            body
        })
    };
    // A poisoned worker-status mutex means a background loop panicked; the
    // loop recovered, but health must stay degraded until restart.
    if state.workers.recovered_from_poison() {
        mark_degraded(&mut body, "worker_status_mutex_poisoned");
    }
    Json(body)
}

async fn api_favicon() -> StatusCode {
    StatusCode::NO_CONTENT
}

fn mark_degraded(body: &mut Value, reason: &str) {
    let reasons = body["degraded_reasons"].as_array_mut();
    if let Some(reasons) = reasons {
        if !reasons.iter().any(|value| value.as_str() == Some(reason)) {
            reasons.push(json!(reason));
        }
    } else {
        body["degraded_reasons"] = json!([reason]);
    }
    body["degraded"] = json!(true);
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0)
}

fn env_positive_interval(name: &str) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse::<u64>().ok())
        .filter(|value| *value > 0)
        .unwrap_or(0)
}

fn env_flag(name: &str) -> bool {
    matches!(
        std::env::var(name)
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes" | "on"
    )
}

fn worker_schedule(
    workers: &Value,
    name: &str,
    interval: u64,
    on_start: bool,
    next_key: &str,
) -> Value {
    let worker = workers.get(name).cloned().unwrap_or_else(|| json!({}));
    let next_at = worker.get("next_at").and_then(Value::as_f64).unwrap_or(0.0);
    let enabled = interval > 0 || on_start || worker["running"].as_bool().unwrap_or(false);
    let now = now_seconds();
    let overdue = enabled && next_at > 0.0 && next_at <= now;
    let seconds_until_next = if enabled && next_at > 0.0 {
        Some(if overdue {
            0.0
        } else {
            (next_at - now).max(0.0)
        })
    } else {
        None
    };
    let mut schedule = json!({
        "enabled": enabled,
        "interval": interval,
        "on_start": on_start,
        next_key: next_at,
        "seconds_until_next": seconds_until_next,
        "overdue": overdue,
        "deferred_by_manual": false,
    });
    if let Some(error) = worker["last"]["error"].as_str() {
        schedule["last_error"] = json!(error);
    }
    schedule
}

fn metadata_updated_at(metadata: &std::fs::Metadata) -> Option<f64> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(UNIX_EPOCH).ok())
        .map(|duration| duration.as_secs_f64())
}

fn health_file_summary(path: &std::path::Path) -> Value {
    match std::fs::metadata(path) {
        Ok(metadata) if metadata.is_file() => json!({
            "path": path.display().to_string(),
            "exists": true,
            "size_bytes": metadata.len(),
            "updated_at": metadata_updated_at(&metadata),
        }),
        Ok(_) => json!({
            "path": path.display().to_string(),
            "exists": false,
            "size_bytes": 0,
            "updated_at": Value::Null,
        }),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => json!({
            "path": path.display().to_string(),
            "exists": false,
            "size_bytes": 0,
            "updated_at": Value::Null,
        }),
        Err(error) => json!({
            "path": path.display().to_string(),
            "exists": false,
            "size_bytes": 0,
            "updated_at": Value::Null,
            "error": error.to_string(),
        }),
    }
}

fn health_logs_summary(data_dir: &std::path::Path) -> Value {
    let root = data_dir.join("logs");
    json!({
        "root": root.display().to_string(),
        "gallery_log": health_file_summary(&root.join("gallery.log")),
        "ui_actions_log": health_file_summary(&root.join("ui-actions.log")),
    })
}

fn backup_timestamp(path: &std::path::Path, db_path: &std::path::Path) -> Option<f64> {
    if let Ok(raw) = std::fs::read_to_string(path.join("metadata.json")) {
        if let Some(value) = serde_json::from_str::<Value>(&raw)
            .ok()
            .and_then(|metadata| metadata.get("created_at").and_then(Value::as_f64))
        {
            return Some(value);
        }
    }
    // The directory label is written by `db_backup::create_db_backup_in` as
    // `chrono::Local::now().format("%Y%m%d-%H%M%S")`, i.e. LOCAL wall-clock
    // time without an offset. Reading it back as UTC (the previous
    // `and_utc()`) shifted every snapshot without a `metadata.json` by the
    // host offset — on a +08:00 NAS the health panel dated backups eight hours
    // into the future. Interpret the label in the same timezone that wrote it.
    use chrono::TimeZone;
    if let Some(value) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| chrono::NaiveDateTime::parse_from_str(name, "%Y%m%d-%H%M%S").ok())
        .and_then(|value| {
            chrono::Local
                .from_local_datetime(&value)
                .earliest()
                .map(|local| local.timestamp() as f64)
        })
    {
        return Some(value);
    }
    std::fs::metadata(db_path)
        .or_else(|_| std::fs::metadata(path))
        .ok()
        .and_then(|metadata| metadata_updated_at(&metadata))
}

fn backup_summary(root: &std::path::Path) -> Value {
    let mut backups = Vec::new();
    let entries = match std::fs::read_dir(root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return json!({
                "root": root.display().to_string(),
                "count": 0,
                "retained_count": 0,
                "total_size_bytes": 0,
                "latest": Value::Null,
                "recent": [],
            });
        }
        Err(error) => {
            return json!({
                "root": root.display().to_string(),
                "count": Value::Null,
                "retained_count": Value::Null,
                "total_size_bytes": Value::Null,
                "latest": Value::Null,
                "recent": [],
                "error": error.to_string(),
            });
        }
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path
            .file_name()
            .and_then(|name| name.to_str())
            .map(|name| name.starts_with('.'))
            .unwrap_or(true)
        {
            continue;
        }
        let Ok(file_type) = entry.file_type() else {
            continue;
        };
        if !file_type.is_dir() {
            continue;
        }
        let db_path = path.join("gallery.db");
        // A snapshot is a directory holding the copied database. The
        // `db-backups/housekeeping/` subtree is a *container* for lifecycle
        // snapshots, not one itself; listed as a backup it showed up as a
        // 0-byte entry named "housekeeping", and because taking a lifecycle
        // backup bumps its directory mtime it could even become "latest".
        // Directories without a snapshot file are not backup entries at all.
        if !db_path.is_file() && !path.join("metadata.json").is_file() {
            continue;
        }
        let size_bytes = std::fs::metadata(&db_path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len())
            .unwrap_or(0);
        let Some(updated_at) = backup_timestamp(&path, &db_path) else {
            continue;
        };
        backups.push(json!({
            "name": path.file_name().and_then(|name| name.to_str()).unwrap_or_default(),
            "path": path.display().to_string(),
            "size_bytes": size_bytes,
            "updated_at": updated_at,
        }));
    }
    backups.sort_by(|left, right| {
        right["updated_at"]
            .as_f64()
            .partial_cmp(&left["updated_at"].as_f64())
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let count = backups.len();
    let total_size_bytes = backups
        .iter()
        .filter_map(|backup| backup["size_bytes"].as_u64())
        .sum::<u64>();
    let recent = backups.into_iter().take(5).collect::<Vec<_>>();
    json!({
        "root": root.display().to_string(),
        "count": count,
        "retained_count": count,
        "total_size_bytes": total_size_bytes,
        "latest": recent.first().cloned().unwrap_or(Value::Null),
        "recent": recent,
    })
}

async fn api_capabilities(State(state): State<AppState>) -> Json<Value> {
    let caps = state.capabilities;
    Json(json!({
        "read_only": caps.read_only,
        "writes": caps.writes,
        "media": caps.media,
        "ml": caps.ml,
        "db_mode": if caps.read_only { "read-only" } else { "read-write" },
    }))
}

#[derive(serde::Deserialize)]
struct ContentHashQuery {
    path: String,
}

async fn api_content_hash(
    State(state): State<AppState>,
    Query(query): Query<ContentHashQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Always allowlist client paths (same choke-point as media serve).
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || content_hash_allowed(&query.path, &roots))
        .await
        .map_err(blocking_http_error)?
        .map(Json)
        .map_err(to_media_path_http_error)
}

#[derive(serde::Deserialize)]
struct ImagePreviewQuery {
    path: String,
    #[serde(default)]
    max_edge: Option<u32>,
    #[serde(default)]
    v: Option<String>,
}

async fn api_image_preview(
    State(state): State<AppState>,
    Query(query): Query<ImagePreviewQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let roots = state.roots.clone();
    let versioned = query.v.is_some();
    let result = tokio::task::spawn_blocking(move || {
        preview_jpeg_allowed(&query.path, &roots, query.max_edge)
    })
    .await
    .map_err(blocking_http_error)?;
    match result {
        Ok(bytes) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, preview_cache_control(versioned))
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())),
        Err((code, body)) => Err((code, Json(body))),
    }
}

/// Public product path used by the static UI (`API.previewUrl` → `/api/file/preview`).
#[derive(serde::Deserialize)]
struct FilePreviewQuery {
    path: String,
    #[serde(default)]
    max: Option<u32>,
    #[serde(default)]
    max_edge: Option<u32>,
    #[serde(default)]
    v: Option<String>,
}

async fn api_file_preview(
    State(state): State<AppState>,
    Query(query): Query<FilePreviewQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    if state.capabilities.media || state.primary {
        let roots = state.roots.clone();
        let versioned = query.v.is_some();
        let result = tokio::task::spawn_blocking(move || {
            preview_jpeg_allowed(&query.path, &roots, query.max.or(query.max_edge))
        })
        .await
        .map_err(blocking_http_error)?;
        match result {
            Ok(bytes) => {
                return Ok(Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "image/jpeg")
                    .header(header::CACHE_CONTROL, preview_cache_control(versioned))
                    .header(header::CONTENT_LENGTH, bytes.len())
                    .body(Body::from(bytes))
                    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
            }
            Err((code, body)) => {
                return Err((code, Json(body)));
            }
        }
    }
    Err((
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "media mode not enabled"})),
    ))
}

async fn api_hash_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Reports a per-status COUNT over every non-missing item, which walks the
    // whole `idx_items_hash_queue` index (measured 51ms on a 755k-item
    // library). Keep it on the blocking pool like the other aggregate
    // endpoints: on an async worker it stalls unrelated requests while it runs.
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        hash_status_response(&conn).map(Json).map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_move_candidates(
    State(state): State<AppState>,
    Query(query): Query<CandidateQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        move_candidates_response(
            &conn,
            &roots,
            query.status.as_deref().unwrap_or("pending"),
            query.hide_grouped.unwrap_or(false),
            query.limit,
            query.offset,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_scan_candidates(
    State(state): State<AppState>,
    Query(query): Query<ScanCandidateQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let status = query.status.as_deref().unwrap_or("pending").to_string();
    if !matches!(
        status.as_str(),
        "pending" | "candidate" | "new" | "resolved" | "ignored"
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid scan candidate status"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        scan_candidates_response(&conn, &status, query.limit, query.offset)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_move_candidate_groups(
    State(state): State<AppState>,
    Query(query): Query<GroupQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        move_candidate_groups_response(
            &conn,
            &roots,
            query.status.as_deref().unwrap_or("pending"),
            query.sample_limit,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_move_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        move_history_response(
            &conn,
            &roots,
            query.status.as_deref(),
            query.limit,
            query.offset,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_operation_history(
    State(state): State<AppState>,
    Query(query): Query<OperationHistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        operation_history_response(&conn, &roots, query.limit)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_operation_log(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let limit = q.get("limit").and_then(|v| v.parse().ok());
    let error_limit = q.get("error_limit").and_then(|v| v.parse().ok());
    // Bounded log reads (several hundred KB) are blocking file I/O.
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = state.pool.get().map_err(to_http_error)?;
        operation_log_response(&conn, &roots, limit, error_limit)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_folder_rename_auto(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    folder_rename_auto_response(&conn)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_rename_auto_run(
    State(state): State<AppState>,
    Query(q): Query<ArtistIdQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let artist_id = q.artist_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "artist_id required"})),
        )
    })?;
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        folder_rename_auto_run(&conn, &roots, artist_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_artists(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        artists_response(&conn).map(Json).map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_duplicate_artists(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        duplicate_artists_response(&conn, &roots)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_artist_stats(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artist_stats_response(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_artist_detail(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artist_detail_response(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_media_roots(State(state): State<AppState>) -> Json<Value> {
    Json(json!({
        "roots": state.roots.roots.iter().enumerate().map(|(index, path)| json!({
            "index": index,
            "path": path,
            "label": state.roots.labels.get(index).unwrap_or(path),
        })).collect::<Vec<_>>()
    }))
}

#[derive(serde::Deserialize)]
struct MediaRootDirectoriesQuery {
    root_index: usize,
    path: Option<String>,
}

async fn api_media_root_directories(
    State(state): State<AppState>,
    Query(query): Query<MediaRootDirectoriesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let roots = state.roots.clone();
    let path = query.path.unwrap_or_default();
    tokio::task::spawn_blocking(move || {
        list_media_root_directories(&roots, query.root_index, &path)
    })
    .await
    .map_err(blocking_http_error)?
    .map(Json)
    .map_err(to_artist_folder_move_http_error)
}

#[derive(serde::Deserialize)]
struct ArtistFolderMoveBody {
    root_index: usize,
    destination: String,
}

async fn api_artist_folder_move_preview(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
    Json(body): Json<ArtistFolderMoveBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        preview_artist_folder_move(&conn, &roots, artist_id, body.root_index, &body.destination)
            .map(Json)
            .map_err(to_artist_folder_move_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_artist_folder_move_execute(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
    Json(body): Json<ArtistFolderMoveBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "a scan or file operation is already running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let conn = pool.get().map_err(to_http_error)?;
        execute_artist_folder_move(&conn, &roots, artist_id, body.root_index, &body.destination)
            .map(Json)
            .map_err(to_artist_folder_move_http_error)
    })
    .await;
    result.map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
    })?
}

#[derive(serde::Deserialize)]
struct ArtistProfileLinkBody {
    kind: String,
    platform: Option<String>,
    url: String,
}

async fn api_artist_profile_links(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artist_profile_links_response(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_create_artist_profile_link(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
    Json(body): Json<ArtistProfileLinkBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    create_artist_profile_link(
        &conn,
        artist_id,
        &body.kind,
        body.platform.as_deref().unwrap_or_default(),
        &body.url,
    )
    .map(Json)
    .map_err(to_artist_profile_link_http_error)
}

async fn api_delete_artist_profile_link(
    State(state): State<AppState>,
    Path((artist_id, link_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    delete_artist_profile_link(&conn, artist_id, link_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_artist_links(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artist_links_response(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_artist_links_reindex(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    if !state.capabilities.allows_media() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "media capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        reindex_artist_links(&conn, &roots, artist_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
    })?
}

async fn api_artist_references(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
    Query(query): Query<ReferenceQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artist_references_response(&conn, artist_id, query.limit)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_paths(
    State(state): State<AppState>,
    Path(artist_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    folder_paths_response(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folders(
    State(state): State<AppState>,
    Query(query): Query<FoldersQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        folders_response(&conn, query.artist_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_item_detail(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    item_detail_response(&conn, item_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_items_page(
    State(state): State<AppState>,
    Query(query): Query<ItemsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let global_search = query.artist_id.is_none()
        && query
            .search
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .is_some();
    if query.cursor.is_some() && (!global_search || query.offset.is_some()) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "cursor requires global search without offset"})),
        ));
    }
    let cursor_requested = query.cursor.is_some();
    // Ordering an unscoped page sorts the whole library before it can return
    // the first 50 rows, and `pool.get()` can wait for a free connection.
    // Both would otherwise run on an async worker and stall unrelated routes.
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        let (limit, offset) = gallery_accel::normalize_pagination(query.limit, query.offset);
        let media_type = if query.archive_only.unwrap_or(false) {
            Some("archive".to_string())
        } else {
            query.media_type.clone()
        };
        let use_cursor = global_search && (query.cursor.is_some() || query.offset.is_none());
        let result = if use_cursor {
            items_page_cursor_query_response_ext(
                &conn,
                query.artist_id,
                Some(limit),
                Some(offset),
                query.sort.as_deref(),
                media_type.as_deref(),
                query.folder.as_deref(),
                query.date_from.as_deref(),
                query.date_to.as_deref(),
                query.image_only,
                query.untagged,
                query.tag_id,
                query.duplicates_only,
                query.tags.as_deref(),
                query.search.as_deref(),
                query.search_tags_only.unwrap_or(false),
                query.favorite_only,
                query.inbox,
                query.cursor.as_deref(),
            )
        } else {
            items_page_query_response_ext(
                &conn,
                query.artist_id,
                Some(limit),
                Some(offset),
                query.sort.as_deref(),
                media_type.as_deref(),
                query.folder.as_deref(),
                query.date_from.as_deref(),
                query.date_to.as_deref(),
                query.image_only,
                query.untagged,
                query.tag_id,
                query.duplicates_only,
                query.tags.as_deref(),
                query.search.as_deref(),
                query.search_tags_only.unwrap_or(false),
                query.favorite_only,
                query.inbox,
            )
        };
        result.map_err(|error| {
            if cursor_requested && error.to_string().starts_with("invalid cursor") {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": error.to_string()})),
                )
            } else {
                to_http_error(error)
            }
        })
    })
    .await
    .map_err(blocking_http_error)?
    .map(Json)
}

async fn api_backfill_item_dimensions(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let status = Arc::clone(&state.dimension_backfill);
    {
        let mut current = status.lock().map_err(|error| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(json!({"error": format!("dimension backfill state unavailable: {error}")})),
            )
        })?;
        if current.running {
            return Ok(Json(dimension_backfill_value(&current)));
        }
        *current = DimensionBackfillStatus {
            running: true,
            ..DimensionBackfillStatus::default()
        };
    }

    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let control = Arc::clone(&state.scan);
    let task_status = Arc::clone(&status);
    tokio::spawn(async move {
        let mut after_id = 0_i64;
        loop {
            let slot = loop {
                if control.is_shutting_down() {
                    return;
                }
                if let Some(guard) = control.try_claim() {
                    break guard;
                }
                tokio::time::sleep(Duration::from_millis(250)).await;
            };
            let batch_pool = Arc::clone(&pool);
            let batch_roots = roots.clone();
            let batch = tokio::task::spawn_blocking(move || {
                let _slot = slot;
                let conn = batch_pool.get()?;
                backfill_item_dimensions(&conn, &batch_roots, after_id, 32)
            })
            .await;

            let value = match batch {
                Ok(Ok(value)) => value,
                Ok(Err(error)) => {
                    if let Ok(mut current) = task_status.lock() {
                        current.running = false;
                        current.complete = false;
                        current.error = Some(error.to_string());
                    }
                    break;
                }
                Err(error) => {
                    if let Ok(mut current) = task_status.lock() {
                        current.running = false;
                        current.complete = false;
                        current.error = Some(error.to_string());
                    }
                    break;
                }
            };

            let processed = value.get("processed").and_then(Value::as_u64).unwrap_or(0);
            let updated = value.get("updated").and_then(Value::as_u64).unwrap_or(0);
            let failed = value.get("failed").and_then(Value::as_u64).unwrap_or(0);
            // `-1` only covers a response that carries no `remaining` (an
            // error payload): `backfill_item_dimensions` measures the count on
            // every batch, so a number that does arrive is always authoritative
            // and writing it back can never regress the reported progress.
            let remaining = value.get("remaining").and_then(Value::as_i64).unwrap_or(-1);
            let next_after_id = value
                .get("next_after_id")
                .and_then(Value::as_i64)
                .unwrap_or(after_id);
            let cursor_done = value
                .get("cursor_done")
                .and_then(Value::as_bool)
                .unwrap_or(false);

            if let Ok(mut current) = task_status.lock() {
                current.processed += processed;
                current.updated += updated;
                current.failed += failed;
                if remaining >= 0 {
                    current.remaining = remaining as u64;
                }
                current.next_after_id = next_after_id;
                current.cursor_done = cursor_done;
            }
            if cursor_done {
                if let Ok(mut current) = task_status.lock() {
                    current.running = false;
                    current.complete = remaining == 0;
                }
                break;
            }
            if next_after_id <= after_id {
                if let Ok(mut current) = task_status.lock() {
                    current.running = false;
                    current.complete = false;
                    current.error = Some("dimension backfill cursor did not advance".to_string());
                }
                break;
            }
            after_id = next_after_id;
        }
    });

    let current = status.lock().map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("dimension backfill state unavailable: {error}")})),
        )
    })?;
    Ok(Json(dimension_backfill_value(&current)))
}

async fn api_backfill_item_dimensions_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let current = state.dimension_backfill.lock().map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": format!("dimension backfill state unavailable: {error}")})),
        )
    })?;
    Ok(Json(dimension_backfill_value(&current)))
}

fn dimension_backfill_value(status: &DimensionBackfillStatus) -> Value {
    json!({
        "running": status.running,
        "processed": status.processed,
        "updated": status.updated,
        "failed": status.failed,
        "remaining": status.remaining,
        "next_after_id": status.next_after_id,
        "cursor_done": status.cursor_done,
        "complete": status.complete,
        "error": status.error,
    })
}

async fn api_tags(
    State(state): State<AppState>,
    Query(query): Query<TagsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    tags_response(&conn, query.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct TagCreateInput {
    artist_id: Option<i64>,
    name: Option<String>,
}

#[derive(serde::Deserialize)]
struct ItemTagsPayload {
    artist_id: i64,
    item_ids: Vec<i64>,
    tag_ids: Vec<i64>,
    mode: String,
}

#[derive(serde::Deserialize)]
struct FavoritePayload {
    favorite: bool,
}

async fn api_set_item_favorite(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Json(payload): Json<FavoritePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let result =
        set_item_favorite_response(&conn, item_id, payload.favorite).map_err(to_http_error)?;
    if result.is_null() {
        Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "item not found"})),
        ))
    } else {
        Ok(Json(result))
    }
}

#[derive(serde::Deserialize)]
struct HashTagPropagationPayload {
    item_ids: Vec<i64>,
}

#[derive(serde::Deserialize)]
struct ItemDateUpdatePayload {
    artist_id: i64,
    item_ids: Vec<i64>,
    #[serde(default)]
    manual_date: Option<String>,
}

async fn api_update_item_dates(
    State(state): State<AppState>,
    Json(payload): Json<ItemDateUpdatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        update_item_dates_response(
            &conn,
            payload.artist_id,
            &payload.item_ids,
            payload.manual_date.as_deref(),
        )
        .map(Json)
        .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct ScanCandidatePayload {
    candidate_id: i64,
}

#[derive(serde::Deserialize)]
struct ScanCandidateApplyMovePayload {
    candidate_id: i64,
    item_id: i64,
    reason: String,
}

#[derive(serde::Deserialize)]
struct MoveCandidatePayload {
    move_candidate_id: i64,
}

async fn api_create_tag(
    State(state): State<AppState>,
    Query(query): Query<TagCreateInput>,
    payload: Result<Json<TagCreateInput>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let (artist_id, name) = match (query.artist_id, query.name) {
        (Some(artist_id), Some(name)) => (artist_id, name),
        _ => match payload {
            Ok(Json(TagCreateInput {
                artist_id: Some(artist_id),
                name: Some(name),
            })) => (artist_id, name),
            _ => {
                return Err((
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": "artist_id and name are required"})),
                ));
            }
        },
    };
    if name.trim().is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "tag name must not be empty"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    create_tag(&conn, artist_id, &name)
        .map(Json)
        .map_err(to_tag_write_http_error)
}

async fn api_propagate_hash_tags(
    State(state): State<AppState>,
    Json(payload): Json<HashTagPropagationPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    propagate_hash_tags_response(&conn, &payload.item_ids)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_resolve_existing_scan_candidate(
    State(state): State<AppState>,
    Json(payload): Json<ScanCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        resolve_existing_scan_candidate_response_with_roots(&conn, &roots, payload.candidate_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_apply_hash_unique_scan_candidate(
    State(state): State<AppState>,
    Json(payload): Json<ScanCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        apply_hash_unique_scan_candidate_response_with_roots(&conn, &roots, payload.candidate_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_apply_scan_candidate_move(
    State(state): State<AppState>,
    Json(payload): Json<ScanCandidateApplyMovePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        apply_scan_candidate_move_response_with_roots(
            &conn,
            &roots,
            payload.candidate_id,
            payload.item_id,
            &payload.reason,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_create_new_item_scan_candidate(
    State(state): State<AppState>,
    Json(payload): Json<ScanCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        create_new_item_response_with_roots(&conn, &roots, payload.candidate_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_apply_move_candidate(
    State(state): State<AppState>,
    Json(payload): Json<MoveCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        apply_move_candidate_response_with_roots(&conn, &roots, payload.move_candidate_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_ignore_move_candidate(
    State(state): State<AppState>,
    Json(payload): Json<MoveCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    ignore_move_candidate_response(&conn, payload.move_candidate_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_mark_move_candidate_new(
    State(state): State<AppState>,
    Json(payload): Json<MoveCandidatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    mark_move_candidate_new_response(&conn, payload.move_candidate_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_update_item_tags(
    State(state): State<AppState>,
    Json(payload): Json<ItemTagsPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        update_item_tags_response(
            &conn,
            payload.artist_id,
            &payload.item_ids,
            &payload.tag_ids,
            &payload.mode,
        )
        .map(Json)
        .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct ItemTagsByNamePayload {
    item_ids: Vec<i64>,
    tag_names: Vec<String>,
    #[serde(default = "default_tag_mode")]
    mode: String,
}

fn default_tag_mode() -> String {
    "add".to_string()
}

async fn api_update_item_tags_by_name(
    State(state): State<AppState>,
    Json(payload): Json<ItemTagsByNamePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        update_item_tags_by_name_response(
            &conn,
            &payload.item_ids,
            &payload.tag_names,
            &payload.mode,
        )
        .map(Json)
        .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct UpdateTagPayload {
    artist_id: i64,
    name: Option<String>,
    sort_order: Option<i64>,
}

async fn api_update_tag(
    State(state): State<AppState>,
    Path(tag_id): Path<i64>,
    Json(payload): Json<UpdateTagPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    match update_tag(
        &conn,
        payload.artist_id,
        tag_id,
        payload.name.as_deref(),
        payload.sort_order,
    )
    .map_err(to_tag_write_http_error)?
    {
        Some(result) => Ok(Json(result)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "tag not found"})),
        )),
    }
}

#[derive(serde::Deserialize)]
struct DeleteTagQuery {
    artist_id: i64,
}

async fn api_delete_tag(
    State(state): State<AppState>,
    Path(tag_id): Path<i64>,
    Query(query): Query<DeleteTagQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let tag_result = delete_tag(&conn, query.artist_id, tag_id).map_err(to_http_error)?;
    match tag_result {
        Some(result) => Ok(Json(result)),
        None => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "tag not found"})),
        )),
    }
}

async fn api_tag_search(
    State(state): State<AppState>,
    Query(query): Query<TagSearchQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // The item_count rollup joins item_tags x items for every matched tag. One
    // call against the whole library blocked an async worker for seconds and
    // stalled unrelated requests, so keep it on the blocking pool like the
    // other aggregate endpoints below.
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        tag_search_response(&conn, query.artist_id, query.search.as_deref(), query.limit)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_characters(
    State(state): State<AppState>,
    Query(query): Query<CharactersQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        characters_response(&conn, query.search.as_deref())
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_character(
    State(state): State<AppState>,
    Path(character_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    character_response(&conn, character_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_character_summary(
    State(state): State<AppState>,
    Query(query): Query<CharacterSummaryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // The tag rollup amplifies through item_tags x items; keep it off the
    // async workers.
    let pool = Arc::clone(&state.pool);
    let result = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        character_summary_response(
            &conn,
            query.artist_id,
            query.model_repo_id.as_deref().unwrap_or(""),
            query.model_variant.as_deref().unwrap_or(""),
            query.model_file.as_deref().unwrap_or(""),
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?;
    result
}

async fn api_character_references(
    State(state): State<AppState>,
    Path(character_id): Path<i64>,
    Query(query): Query<ReferenceQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    character_references_response(&conn, character_id, query.limit)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_artist_reference_scores(
    State(state): State<AppState>,
    Json(payload): Json<ArtistReferenceScoreRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Full-table embedding scan + dot products: run on the blocking pool.
    let pool = Arc::clone(&state.pool);
    let result = tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        artist_reference_scores_response(
            &conn,
            &payload.dino_embedding,
            &payload.wd14_embedding,
            payload.dino_weight.unwrap_or(0.65),
            payload.wd14_weight.unwrap_or(0.35),
            payload.limit,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?;
    result
}

#[derive(serde::Deserialize)]
struct ClusterScoresRequest {
    vectors: Vec<Vec<f32>>,
}

async fn api_cluster_scores(
    Json(payload): Json<ClusterScoresRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if payload.vectors.len() > MAX_CLUSTER_SCORE_VECTORS {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": format!("too many vectors (max {MAX_CLUSTER_SCORE_VECTORS})")})),
        ));
    }
    // The O(n^2 x dim) matrix must not occupy a Tokio worker.
    tokio::task::spawn_blocking(move || cluster_scores_response(&payload.vectors))
        .await
        .map_err(|error| to_similarity_http_error(anyhow::Error::new(error)))?
        .map(Json)
        .map_err(to_similarity_http_error)
}

async fn api_scan_start(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let guard = match state.scan.try_claim() {
        Some(g) => g,
        None => {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({"error": "a scan or file operation is already running"})),
            ));
        }
    };
    let db_path = state.db_path.clone();
    let roots = state.roots.clone();
    let control = Arc::clone(&state.scan);
    let stats_gate = Arc::clone(&state.stats_gate);
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        // Dedicated connection avoids borrowing the request pool during long walks.
        let conn = match open_writable_db(&db_path) {
            Ok(c) => c,
            Err(err) => {
                log_error!("scan open db failed: {err}");
                return;
            }
        };
        let outcome = run_full_library_scan_claimed(&conn, &roots, &control);
        // A scan is the main way a library grows, which is exactly why the scan
        // loop refreshes statistics after one. The manual path does the same work,
        // so it must not leave the planner on the cardinalities the startup
        // bootstrap collected until the next restart.
        stats_gate.refresh_after_work(&conn);
        match outcome {
            Ok(outcome) => {
                let phase = outcome.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                if phase == "partial" || phase == "failed" {
                    log_warn!(
                        "scan finished with {phase}: {}",
                        outcome.get("errors").unwrap_or(&json!([]))
                    );
                }
            }
            Err(err) => {
                log_error!("scan failed: {err}");
            }
        }
    });
    Ok(Json(json!({"ok": true})))
}

#[derive(serde::Deserialize)]
struct ScanFolderQuery {
    artist_id: i64,
    folder: Option<String>,
}

async fn api_scan_folder(
    State(state): State<AppState>,
    Query(q): Query<ScanFolderQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Synchronous precheck: reject traversal/escape before claiming the scan slot.
    {
        let conn = state.pool.get().map_err(to_http_error)?;
        let artist_path: String = conn
            .query_row(
                "SELECT path FROM artists WHERE id=? AND COALESCE(missing,0)=0",
                rusqlite::params![q.artist_id],
                |r| r.get(0),
            )
            .map_err(|e| match e {
                rusqlite::Error::QueryReturnedNoRows => (
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "artist not found"})),
                ),
                other => to_http_error(other.into()),
            })?;
        resolve_scan_scope(&artist_path, q.folder.as_deref(), &state.roots).map_err(|e| {
            let msg = e.to_string();
            let code = if msg.contains("outside") {
                StatusCode::FORBIDDEN
            } else {
                StatusCode::BAD_REQUEST
            };
            (code, Json(json!({"error": msg, "ok": false})))
        })?;
    }
    let guard = match state.scan.try_claim() {
        Some(g) => g,
        None => {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({"error": "a scan or file operation is already running"})),
            ));
        }
    };
    let db_path = state.db_path.clone();
    let roots = state.roots.clone();
    let control = Arc::clone(&state.scan);
    let folder = q.folder.clone();
    let artist_id = q.artist_id;
    tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let conn = match open_writable_db(&db_path) {
            Ok(c) => c,
            Err(err) => {
                log_error!("scan open db failed: {err}");
                return;
            }
        };
        match run_scan_claimed(&conn, &roots, &control, Some(artist_id), folder.as_deref()) {
            Ok(outcome) => {
                let phase = outcome.get("phase").and_then(|v| v.as_str()).unwrap_or("");
                if phase == "partial" || phase == "failed" {
                    log_warn!(
                        "scan folder finished with {phase}: {}",
                        outcome.get("errors").unwrap_or(&json!([]))
                    );
                }
            }
            Err(err) => {
                log_error!("scan failed: {err}");
            }
        }
    });
    Ok(Json(json!({"ok": true})))
}

async fn api_scan_stop(State(state): State<AppState>) -> Json<Value> {
    if !state.scan.is_running() {
        return Json(json!({"ok": false, "message": "Not scanning"}));
    }
    state.scan.request_stop();
    Json(json!({"ok": true}))
}

async fn api_scan_state(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    get_scan_state(&conn).map(Json).map_err(to_http_error)
}

async fn api_hash_run(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // Hashing reads the same media files that scans, moves, and archive
    // executions rewrite; share their operation slot instead of racing them.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let stats_gate = Arc::clone(&state.stats_gate);
    let result = tokio::task::spawn_blocking(move || {
        // The operation lock lives inside the blocking task: a client
        // disconnect drops only the response, never the mutex early.
        let _slot = guard;
        let conn = pool.get()?;
        let result = run_hash_batch_with_roots(&conn, &roots, 32);
        // The hash loop is the half of the ingestion path that promotes scan
        // candidates into items, and it refreshes statistics for exactly that
        // reason. The manual run must close the same gap.
        stats_gate.refresh_after_work(&conn);
        result
    })
    .await
    .map_err(blocking_http_error)?
    .map(Json)
    .map_err(to_http_error);
    result
}

async fn api_serve_file(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    match serve_file_response(&path, &state.roots, &headers).await {
        Ok(r) => r,
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

async fn api_file_text(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path = q.get("path").cloned().unwrap_or_default();
    serve_text(&path, &state.roots)
        .await
        .map(Json)
        .map_err(|(c, v)| (c, Json(v)))
}

async fn api_file_delete(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let path = q.get("path").cloned().unwrap_or_default();
    // A recycle delete moves real files; share the operation slot with scans,
    // moves, and archive executions instead of racing them.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let roots = state.roots.clone();
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        delete_to_recycle(&path, &roots, &conn)
            .map(Json)
            .map_err(|(c, v)| (c, Json(v)))
    })
    .await
    .map_err(|error| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": error.to_string()})),
        )
    })?
}

#[derive(serde::Deserialize)]
struct RecycleQuery {
    status: Option<String>,
    limit: Option<i64>,
    offset: Option<i64>,
}

async fn api_recycle_entries(
    State(state): State<AppState>,
    Query(query): Query<RecycleQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !matches!(
        query.status.as_deref().unwrap_or("recycled"),
        "recycled" | "restored"
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid recycle status"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        recycle_entries_response(
            &conn,
            &roots,
            query.status.as_deref(),
            query.limit,
            query.offset,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_recycle_restore(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    // A restore writes real files back into the library; share the operation
    // slot with scans, moves, and archive executions.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        restore_recycle_entry(&conn, &roots, entry_id)
            .map(Json)
            .map_err(|(status, body)| (status, Json(body)))
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(Debug, Deserialize, Default)]
struct RecyclePurgeQuery {
    // `?record_only=1` is what the frontend sends; a bare `Option<bool>` is
    // parsed with `str::parse::<bool>()`, which only accepts `true`/`false`, so
    // the record-only exit for fnOS system-trash entries answered 400.
    #[serde(
        default,
        deserialize_with = "crate::route_params::deserialize_bool_lenient"
    )]
    record_only: Option<bool>,
}

async fn api_recycle_purge(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    Query(query): Query<RecyclePurgeQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    // Purging deletes a real file (or drops the only pointer to one), so it
    // shares the operation slot with scans, moves, restores and archive
    // executions: a purge racing a restore of the same entry would remove the
    // file the restore is moving back into the library.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let record_only = query.record_only.unwrap_or(false);
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        purge_recycle_entry(&conn, &roots, entry_id, record_only)
            .map(Json)
            .map_err(|(status, body)| (status, Json(body)))
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(Debug, Deserialize)]
struct RecycleClearQuery {
    status: Option<String>,
}

async fn api_recycle_clear(
    State(state): State<AppState>,
    Query(query): Query<RecycleClearQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    // Emptying the bin deletes real files, so it takes the same operation slot
    // as the per-entry purge and restore.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let status = query.status;
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        clear_recycle_entries(&conn, &roots, status.as_deref())
            .map(Json)
            .map_err(|(status, body)| (status, Json(body)))
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(Debug, Deserialize, Default)]
struct RecyclePreviewQuery {
    max: Option<u32>,
    max_edge: Option<u32>,
}

async fn api_recycle_preview(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
    Query(query): Query<RecyclePreviewQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let max = query.max.or(query.max_edge);
    let result = tokio::task::spawn_blocking(move || -> Result<Vec<u8>, (StatusCode, Value)> {
        let conn = pool.get().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        })?;
        recycle_entry_preview(&conn, &roots, entry_id, max)
    })
    .await
    .map_err(blocking_http_error)?;

    match result {
        Ok(bytes) => Ok(Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            // Private: this is deletable user content, not a shareable asset.
            .header(header::CACHE_CONTROL, "private, max-age=3600")
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())),
        Err((code, body)) => Err((code, Json(body))),
    }
}

#[derive(Debug, Deserialize)]
struct RecycleSettingsPayload {
    target: String,
}

async fn api_recycle_settings_get(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let target = tokio::task::spawn_blocking(move || -> Result<String, (StatusCode, Value)> {
        let conn = pool.get().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        })?;
        Ok(get_recycle_target_setting(&conn))
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(|(code, body)| (code, Json(body)))?;

    let system_supported = cfg!(target_os = "linux");
    Ok(Json(json!({
        "ok": true,
        "target": target,
        "system_supported": system_supported,
    })))
}

async fn api_recycle_settings_put(
    State(state): State<AppState>,
    Json(payload): Json<RecycleSettingsPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let target = payload.target.trim().to_lowercase();
    if target != "system" && target != "app" {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid recycle target; expected 'system' or 'app'"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let saved = tokio::task::spawn_blocking(move || -> Result<String, (StatusCode, Value)> {
        let conn = pool.get().map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        })?;
        set_recycle_target_setting(&conn, &target).map_err(|e| {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                json!({"error": e.to_string()}),
            )
        })
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(|(code, body)| (code, Json(body)))?;

    let system_supported = cfg!(target_os = "linux");
    Ok(Json(json!({
        "ok": true,
        "target": saved,
        "system_supported": system_supported,
    })))
}

async fn api_video_frame(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    let t = q.get("t").and_then(|v| v.parse().ok()).unwrap_or(0.1);
    let cache_control = if q.contains_key("v") {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    };
    match video_frame_jpeg(&path, &state.roots, t).await {
        Ok(bytes) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "image/jpeg")
            .header(header::CACHE_CONTROL, cache_control)
            .header(header::CONTENT_LENGTH, bytes.len())
            .body(Body::from(bytes))
            .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()),
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

async fn api_video_transcode(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path = q.get("path").cloned().unwrap_or_default();
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || start_video_transcode(&path, &roots))
        .await
        .map_err(blocking_http_error)?
        .map(Json)
        .map_err(to_media_path_http_error)
}

async fn api_video_transcode_status(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let path = q.get("path").cloned().unwrap_or_default();
    let status = video_transcode_status(&path, &state.roots);
    if status.get("status") == Some(&json!("error")) {
        let message = status
            .get("message")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            message,
            "path not allowed" | "file not found or not allowed"
        ) {
            return Err((StatusCode::NOT_FOUND, Json(status)));
        }
    }
    Ok(Json(status))
}

async fn api_video_compatible(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    match serve_video_compatible(&path, &state.roots, &headers).await {
        Ok(r) => r,
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

async fn api_video_hls(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    match serve_video_hls(&path, &state.roots, &headers).await {
        Ok(r) => r,
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

async fn api_video_transcoded(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
    headers: axum::http::HeaderMap,
) -> Response {
    let path = q.get("path").cloned().unwrap_or_default();
    match serve_transcoded_hls(&path, &state.roots, &headers).await {
        Ok(r) => r,
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

async fn api_video_transcoded_segment(
    Path((key, segment)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
) -> Response {
    match serve_transcoded_hls_segment(&key, &segment, &headers).await {
        Ok(response) => response,
        Err((code, body)) => (code, Json(body)).into_response(),
    }
}

#[derive(serde::Deserialize)]
struct ArtistIdQuery {
    artist_id: Option<i64>,
}

async fn api_folder_renames_list(
    State(state): State<AppState>,
    Query(q): Query<ArtistIdQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        list_folder_renames(&conn, Some(&roots), q.artist_id, false)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

/// Explicit refresh: auto-discover plans and recompute targets for one artist,
/// then return the list. Mutating work lives behind POST and the write gate so
/// the GET list endpoint stays read-only.
async fn api_folder_renames_refresh(
    State(state): State<AppState>,
    Query(q): Query<ArtistIdQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        list_folder_renames(&conn, Some(&roots), q.artist_id, true)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct FolderErrorArtistsQuery {
    q: Option<String>,
    sort: Option<String>,
    offset: Option<i64>,
    limit: Option<i64>,
}

async fn api_folder_rename_error_artists(
    State(state): State<AppState>,
    Query(q): Query<FolderErrorArtistsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let sort = if q.sort.as_deref() == Some("count") {
        "count"
    } else {
        "recent"
    };
    let offset = q.offset.unwrap_or(0).max(0);
    let limit = q
        .limit
        .unwrap_or(DEFAULT_ITEM_PAGE_LIMIT)
        .clamp(1, MAX_ITEM_PAGE_LIMIT);
    folder_error_artists(&conn, q.q.as_deref(), Some(sort), offset, limit)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct FolderRenameSettingsBody {
    settings: Value,
    artist_id: Option<i64>,
}

async fn api_folder_renames_settings(
    State(state): State<AppState>,
    Query(q): Query<ArtistIdQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    folder_rename_format_settings(&conn, q.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_renames_settings_put(
    State(state): State<AppState>,
    Json(body): Json<FolderRenameSettingsBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    set_folder_rename_format_settings(&conn, &body.settings, body.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct FolderRenameTemplateBody {
    artist_id: i64,
    plan_ids: Option<Vec<i64>>,
    profile_id: Option<String>,
    template: Option<String>,
    index_start: Option<usize>,
}

async fn api_folder_renames_preview(
    State(state): State<AppState>,
    Json(body): Json<FolderRenameTemplateBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        preview_folder_rename_template(
            &conn,
            &roots,
            body.artist_id,
            body.plan_ids.as_deref(),
            body.profile_id.as_deref(),
            body.template.as_deref(),
            body.index_start,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_folder_renames_apply_template(
    State(state): State<AppState>,
    Json(body): Json<FolderRenameTemplateBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        apply_folder_rename_template(
            &conn,
            &roots,
            body.artist_id,
            body.plan_ids.as_deref(),
            body.profile_id.as_deref(),
            body.template.as_deref(),
            body.index_start,
        )
        .map(Json)
        .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct ExecuteBody {
    artist_id: i64,
    #[serde(default)]
    dry_run: bool,
}

async fn api_folder_renames_execute(
    State(state): State<AppState>,
    Json(body): Json<ExecuteBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "a scan or file operation is already running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let conn = pool.get().map_err(to_http_error)?;
        execute_folder_renames(&conn, &roots, body.artist_id, body.dry_run)
            .map(Json)
            .map_err(to_http_error)
    })
    .await;
    result.map_err(blocking_http_error)?
}

async fn api_folder_renames_execute_all(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "a scan or file operation is already running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let conn = pool.get().map_err(to_http_error)?;
        run_folder_rename_all_now(&conn, &roots)
            .map(Json)
            .map_err(to_http_error)
    })
    .await;
    result.map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct AutoBody {
    enabled: bool,
}

async fn api_folder_renames_auto_put(
    State(state): State<AppState>,
    Json(body): Json<AutoBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    set_folder_rename_auto(&conn, body.enabled)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_plan_recheck(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        recheck_plan(&conn, &roots, plan_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_folder_plan_reconfirm(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        reconfirm_plan(&conn, &roots, plan_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_folder_plan_unconfirm(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    unconfirm_plan(&conn, plan_id)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct FolderBatchConfirmBody {
    artist_id: i64,
}

async fn api_folder_plans_confirm_all(
    State(state): State<AppState>,
    Json(body): Json<FolderBatchConfirmBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        confirm_all_artist_plans(&conn, &roots, body.artist_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_folder_plans_unconfirm_all(
    State(state): State<AppState>,
    Json(body): Json<FolderBatchConfirmBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    unconfirm_all_artist_plans(&conn, body.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_plan_undo(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error":"write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "a scan or file operation is already running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let result = tokio::task::spawn_blocking(move || {
        let _guard = guard;
        let conn = pool.get().map_err(to_http_error)?;
        undo_folder_rename_plan(&conn, &roots, plan_id)
            .map(Json)
            .map_err(to_folder_rename_undo_http_error)
    })
    .await;
    result.map_err(blocking_http_error)?
}

async fn api_backup(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        create_db_backup(&conn).map(|path| json!({"ok": true, "path": path}))
    })
    .await
    .map_err(blocking_http_error)?
    .map(Json)
    .map_err(to_http_error)
}

const UI_LOG_MAX_BYTES: u64 = 2 * 1024 * 1024;
const UI_LOG_BACKUP_COUNT: usize = 3;
const UI_LOG_LINE_MAX_BYTES: usize = 8 * 1024;
const LOG_TAIL_DEFAULT_BYTES: u64 = 256 * 1024;
const LOG_TAIL_MAX_BYTES: u64 = 256 * 1024;
const LOG_TAIL_DEFAULT_LINES: usize = 200;
const LOG_TAIL_MAX_LINES: usize = 2_000;

fn log_path(data_dir: &std::path::Path, name: &str) -> PathBuf {
    data_dir.join("logs").join(name)
}

fn env_positive_u64(name: &str, default: u64) -> u64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.trim().parse().ok())
        .filter(|value| *value > 0)
        .unwrap_or(default)
}

fn rotated_log_path(path: &std::path::Path, number: usize) -> PathBuf {
    PathBuf::from(format!("{}.{}", path.display(), number))
}

fn rotate_log(path: &std::path::Path, backups: usize) -> std::io::Result<()> {
    for number in (1..=backups).rev() {
        let source = if number == 1 {
            path.to_path_buf()
        } else {
            rotated_log_path(path, number - 1)
        };
        if !source.is_file() {
            continue;
        }
        let target = rotated_log_path(path, number);
        if target.exists() {
            std::fs::remove_file(&target)?;
        }
        std::fs::rename(source, target)?;
    }
    Ok(())
}

fn append_ui_log(
    path: &std::path::Path,
    line: &str,
    max_bytes: u64,
    backups: usize,
) -> std::io::Result<()> {
    use std::io::Write;

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if path
        .metadata()
        .map(|metadata| metadata.len().saturating_add(line.len() as u64) > max_bytes)
        .unwrap_or(false)
    {
        rotate_log(path, backups)?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?
        .write_all(line.as_bytes())
}

async fn api_ui_log(State(state): State<AppState>, Json(body): Json<Value>) -> Json<Value> {
    let payload = body.to_string();
    let payload = if payload.len() > UI_LOG_LINE_MAX_BYTES {
        json!({"truncated": true, "payload_bytes": payload.len()}).to_string()
    } else {
        payload
    };
    let line = format!("{} {}\n", gallery_accel::logging::log_timestamp(), payload);
    Json(json!({
        "ok": append_ui_log(
            &log_path(&state.data_dir, "ui-actions.log"),
            &line,
            state.ui_log_max_bytes,
            state.ui_log_backups,
        )
        .is_ok()
    }))
}

async fn api_logs_tail(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let source = q.get("source").cloned().unwrap_or_else(|| "ui".into());
    let name = match source.as_str() {
        "gallery" => "gallery.log",
        "startup" => "startup.log",
        _ => "ui-actions.log",
    };
    let line_limit = q
        .get("lines")
        .and_then(|value| value.parse::<usize>().ok())
        .unwrap_or(LOG_TAIL_DEFAULT_LINES)
        .clamp(1, LOG_TAIL_MAX_LINES);
    let max_bytes = q
        .get("max_bytes")
        .and_then(|value| value.parse::<u64>().ok())
        .unwrap_or(LOG_TAIL_DEFAULT_BYTES)
        .clamp(1, LOG_TAIL_MAX_BYTES);
    let path = log_path(&state.data_dir, name);
    let exists = path.is_file();
    let updated_at = path
        .metadata()
        .and_then(|metadata| metadata.modified())
        .ok()
        .and_then(|value| value.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|value| value.as_millis().min(i64::MAX as u128) as i64)
        .unwrap_or(0);
    let (lines, truncated) =
        read_bounded_log_tail(&path, line_limit, max_bytes).unwrap_or_default();
    Json(json!({
        "source": source,
        "exists": exists,
        "updated_at": updated_at,
        "lines": lines,
        "truncated": truncated,
        "max_bytes": max_bytes
    }))
}

async fn api_character_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    character_recognition_status(&conn)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_character_signature() -> Json<Value> {
    Json(character_model_signature())
}

async fn api_artist_status() -> Json<Value> {
    Json(artist_recognition_status())
}

async fn api_ml_runtime_status(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    // GPU detection scans /dev and /sys; keep it off the async workers.
    let status = tokio::task::spawn_blocking(move || {
        let conn = state.pool.get().map_err(to_http_error)?;
        let mut status = gallery_accel::runtime_prepare::ml_runtime_status(&conn);
        if state.capabilities.ml || state.primary {
            let recognition = gallery_accel::character_ccip::session_status();
            status["actual_provider"] = recognition
                .get("actual_provider")
                .cloned()
                .unwrap_or_else(|| json!("not_initialized"));
            status["provider_error"] = recognition
                .get("fallback_reason")
                .cloned()
                .or_else(|| recognition.get("error").cloned())
                .unwrap_or(Value::Null);
            status["character_recognition"] = recognition;
        }
        Ok(status)
    })
    .await
    .map_err(|error| to_http_error(anyhow::anyhow!(error.to_string())))?;
    status.map(Json)
}

async fn api_ml_runtime_settings_get(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    Ok(Json(gallery_accel::runtime_prepare::runtime_settings(
        &conn,
    )))
}

async fn api_ml_runtime_settings_put(
    State(state): State<AppState>,
    Json(body): Json<Value>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    gallery_accel::runtime_prepare::update_runtime_settings(&conn, &body)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_ml_runtime_retry(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    Ok(Json(
        gallery_accel::runtime_prepare::retry_missing_runtimes(&conn),
    ))
}

async fn api_artist_suggestions(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    suggest_artists_native(&conn, item_id, 3)
        .map(Json)
        .map_err(to_artist_suggestion_http_error)
}

async fn api_character_recognize(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let top_k = q.get("top_k").and_then(|v| v.parse().ok()).unwrap_or(3);
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        recognize_character_native_topk_with_roots(&conn, &roots, item_id, top_k)
            .map(Json)
            .map_err(to_character_recognition_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct CreateCharacterBody {
    name: String,
}

async fn api_create_character(
    State(state): State<AppState>,
    Json(body): Json<CreateCharacterBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name required"})),
        ));
    }
    match conn.execute(
        "INSERT INTO characters (name) VALUES (?)",
        rusqlite::params![name],
    ) {
        Ok(_) => {
            let id = conn.last_insert_rowid();
            Ok(Json(json!({"id": id, "name": name})))
        }
        Err(rusqlite::Error::SqliteFailure(inner, _))
            if inner.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
        {
            Err((
                StatusCode::CONFLICT,
                Json(json!({"error": "character already exists"})),
            ))
        }
        Err(e) => Err(to_http_error(e.into())),
    }
}

async fn api_delete_character(
    State(state): State<AppState>,
    Path(character_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    // Uploaded reference photos live on disk, so dropping the rows is not
    // enough. Best effort: a cleanup failure must not block the delete.
    if let Err(error) =
        gallery_accel::product_ui::remove_character_reference_images(&conn, character_id)
    {
        log_warn!("character delete: reference image cleanup failed: {error:#}");
    }
    let n = (|| -> anyhow::Result<usize> {
        conn.execute("BEGIN IMMEDIATE", [])?;
        let res = (|| -> anyhow::Result<usize> {
            conn.execute(
                "DELETE FROM character_references WHERE character_id=?",
                rusqlite::params![character_id],
            )?;
            let n = conn.execute(
                "DELETE FROM characters WHERE id=?",
                rusqlite::params![character_id],
            )?;
            Ok(n)
        })();
        match res {
            Ok(count) => {
                conn.execute("COMMIT", [])?;
                Ok(count)
            }
            Err(e) => {
                let _ = conn.execute("ROLLBACK", []);
                Err(e)
            }
        }
    })()
    .map_err(to_http_error)?;
    Ok(Json(json!({"ok": n > 0, "id": character_id})))
}

async fn api_delete_character_reference(
    State(state): State<AppState>,
    Path((character_id, reference_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    delete_character_reference(&conn, character_id, reference_id)
        .map(Json)
        .map_err(to_http_error)
}

/// Manual reference photo upload (角色库 → 参考图 → 添加照片). The body is the raw
/// image. The type is sniffed from the bytes rather than trusted from the file
/// name or `Content-Type`, and the stored name is generated server-side.
async fn api_upload_character_reference(
    State(state): State<AppState>,
    Path(character_id): Path<i64>,
    body: Bytes,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    if body.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "empty image body"})),
        ));
    }
    if body.len() > gallery_accel::product_ui::REFERENCE_IMAGE_MAX_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "image exceeds the upload limit"})),
        ));
    }
    let Some(extension) = gallery_accel::product_ui::reference_image_extension_for_bytes(&body)
    else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unsupported image type"})),
        ));
    };
    // Refuse before storing anything when the recognizer could never embed it.
    // A merely idle (unloaded) session is deliberately not a refusal.
    if let Some(blocker) = gallery_accel::product_ui::manual_reference_embedding_blocker() {
        return Err((
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"error": blocker})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let outcome =
        tokio::task::spawn_blocking(move || -> Result<Value, (StatusCode, Json<Value>)> {
            let conn = pool.get().map_err(to_http_error)?;
            let exists: Option<i64> = conn
                .query_row(
                    "SELECT id FROM characters WHERE id=?",
                    rusqlite::params![character_id],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|error| to_http_error(error.into()))?;
            if exists.is_none() {
                return Err((
                    StatusCode::NOT_FOUND,
                    Json(json!({"error": "character not found"})),
                ));
            }
            let stored = gallery_accel::product_ui::store_manual_reference_image(
                character_id,
                extension,
                &body,
            )
            .map_err(to_http_error)?;
            let embedding = match gallery_accel::product_ui::embed_manual_reference_image(&stored) {
                Ok(embedding) => embedding,
                Err(error) => {
                    // A failed embed must not leave an orphaned upload behind.
                    gallery_accel::product_ui::remove_reference_image_file(
                        &stored.to_string_lossy(),
                    );
                    log_error!("character reference upload embed failed: {error:#}");
                    return Err((
                        StatusCode::SERVICE_UNAVAILABLE,
                        Json(json!({"error": "识别失败，请稍后重试"})),
                    ));
                }
            };
            let reference_id = gallery_accel::product_ui::insert_manual_reference(
                &conn,
                character_id,
                &stored,
                &embedding,
            )
            .map_err(to_http_error)?;
            Ok(json!({
                "ok": true,
                "character_id": character_id,
                "reference_id": reference_id,
            }))
        })
        .await
        .map_err(blocking_http_error)?;
    outcome.map(Json)
}

/// Serve one manually uploaded reference photo. The database row is the
/// authorization: only the stored `image_path` of that character's reference is
/// ever read, so no client-supplied path reaches the filesystem.
async fn api_character_reference_image(
    State(state): State<AppState>,
    Path((character_id, reference_id)): Path<(i64, i64)>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let image_path: Option<String> = {
        let conn = state.pool.get().map_err(to_http_error)?;
        conn.query_row(
            "SELECT image_path FROM character_references
             WHERE id=? AND character_id=? AND image_path IS NOT NULL",
            rusqlite::params![reference_id, character_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| to_http_error(error.into()))?
    };
    let Some(image_path) = image_path else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "reference image not found"})),
        ));
    };
    if !gallery_accel::product_ui::is_safe_character_reference_path(std::path::Path::new(
        &image_path,
    )) {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "access denied"})),
        ));
    }
    let bytes = tokio::fs::read(&image_path).await.map_err(|error| {
        log_error!("character reference image read failed: {image_path}: {error}");
        (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "reference image is missing"})),
        )
    })?;
    let content_type = mime_guess::from_path(&image_path)
        .first_or_octet_stream()
        .to_string();
    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=86400"),
        )
        .header(header::CONTENT_LENGTH, bytes.len())
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

async fn api_character_import_job_current() -> Json<Value> {
    Json(get_character_import_job())
}

async fn api_character_import_job_start(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    // A malformed body must not silently fall back to `{}`: that maps to the
    // most expensive scope ("all" — full-library import).
    let payload = body.map(|j| j.0).map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid JSON body: {err}")})),
        )
    })?;
    let db_path = state.db_path.clone();
    let roots = state.roots.clone();
    spawn_character_import_job_with_roots(db_path, roots, payload)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_character_import_job_cancel(Path(job_id): Path<String>) -> Json<Value> {
    Json(cancel_character_import_job(&job_id))
}

async fn api_rebuild_character_index(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    rebuild_character_index(&conn)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_confirm_artist_suggestion(
    State(state): State<AppState>,
    Path((item_id, artist_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    confirm_artist_suggestion(&conn, item_id, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_move_auto_resolve(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let limit = q
        .get("limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_BATCH_ITEM_LIMIT);
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        auto_resolve_move_candidates_with_roots(&conn, &roots, limit)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_move_group_merge(
    State(state): State<AppState>,
    Path((old_artist_id, new_artist_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        merge_move_candidate_group_with_roots(&conn, &roots, old_artist_id, new_artist_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_update_folder_tags(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let artist_id = q
        .get("artist_id")
        .and_then(|v| v.parse().ok())
        .ok_or_else(|| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "artist_id required"})),
            )
        })?;
    let folder = q.get("folder").cloned().unwrap_or_default();
    let mode = q.get("mode").cloned().unwrap_or_else(|| "add".into());
    let tag_ids: Vec<i64> = q
        .get("tag_ids")
        .map(|s| s.split(',').filter_map(|p| p.trim().parse().ok()).collect())
        .unwrap_or_default();
    let conn_pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = conn_pool.get().map_err(to_http_error)?;
        update_folder_tags_response(&conn, artist_id, &folder, &tag_ids, &mode)
            .map(Json)
            .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct FolderTagsByNameBody {
    artist_id: i64,
    #[serde(default)]
    folder: String,
    #[serde(default)]
    tag_names: Vec<String>,
    #[serde(default = "default_mode_add")]
    mode: String,
}

fn default_mode_add() -> String {
    "add".into()
}

async fn api_update_folder_tags_by_name(
    State(state): State<AppState>,
    Json(body): Json<FolderTagsByNameBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        update_folder_tags_by_name_response(
            &conn,
            body.artist_id,
            &body.folder,
            &body.tag_names,
            &body.mode,
        )
        .map(Json)
        .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_annotate_folder(
    State(state): State<AppState>,
    Json(body): Json<gallery_accel::product_ui::FolderAnnotatePayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        gallery_accel::product_ui::annotate_folder_response(&conn, Some(&roots), body)
            .map(Json)
            .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

#[derive(serde::Deserialize)]
struct DeleteFolderBody {
    artist_id: i64,
    #[serde(default)]
    folder: String,
}

async fn api_delete_folder(
    State(state): State<AppState>,
    Json(body): Json<DeleteFolderBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        delete_folder_to_recycle(&conn, &roots, body.artist_id, &body.folder)
            .map(Json)
            .map_err(|(c, v)| (c, Json(v)))
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_bundle_items(
    State(state): State<AppState>,
    Json(body): Json<gallery_accel::product_ui::BundleItemsPayload>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "another file operation is running"})),
        ));
    };
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let _slot = guard;
        let conn = pool.get().map_err(to_http_error)?;
        gallery_accel::product_ui::bundle_items_response(&conn, Some(&roots), body)
            .map(Json)
            .map_err(to_tag_write_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

fn preview_cache_control(versioned: bool) -> &'static str {
    if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

fn blocking_http_error(error: tokio::task::JoinError) -> (StatusCode, Json<Value>) {
    to_http_error(anyhow::anyhow!(error.to_string()))
}

fn to_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    // Details (including server paths from the anyhow chain) stay in the
    // process log; the response body is generic so paths never leak to clients.
    log_error!("request failed: {error:#}");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": "internal server error"})),
    )
}

fn to_tag_write_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    // User-input errors answer 400 with the message intact: a caller that sends
    // an empty folder or an unparseable date must see what was wrong, not a
    // generic 500 that reads like a server fault.
    let status = if message == "artist not found" {
        StatusCode::NOT_FOUND
    } else if matches!(
        message.as_str(),
        "tag name must not be empty"
            | "Bad mode"
            | "artist_id must be positive"
            | "folder must not be empty"
            | "folder has no items or does not exist"
            | "invalid target_folder path"
            | "target folder must be within artist directory"
            | "item_ids must not be empty"
            | "item_ids must be positive"
            | "item_ids must not contain duplicates"
            | "too many item_ids"
    ) || message.starts_with("invalid date: ")
        || message.starts_with("target name exhausted")
        || message.ends_with("item(s) do not exist or belong to this artist")
    {
        StatusCode::BAD_REQUEST
    } else {
        return to_http_error(error);
    };
    (status, Json(json!({"error": message})))
}

fn to_artist_suggestion_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    if message == "item not found" {
        return (StatusCode::NOT_FOUND, Json(json!({"error": message})));
    }
    to_http_error(error)
}

/// Character recognition answers 404 for a missing item and 400 for an item
/// whose media cannot be recognized. Both used to become a generic 500, so the
/// editor showed "识别不可用" and the log said "request failed" instead of the
/// actual reason (deleted/moved row, unsupported media type, file outside the
/// authorized roots).
fn to_character_recognition_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    if message == "item not found" {
        return (StatusCode::NOT_FOUND, Json(json!({"error": message})));
    }
    if message == "Item media type is not supported for character recognition"
        || message == "item media path is not allowed"
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message})));
    }
    to_http_error(error)
}

fn to_similarity_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    if message == "vectors must be non-empty"
        || message == "all vectors must have the same dimension"
        || message == "vectors must contain only finite values"
    {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": message})));
    }
    to_http_error(error)
}

fn to_media_path_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error.to_string();
    if message == "path not allowed"
        || message == "file not found or not allowed"
        || message.starts_with("No such file or directory")
    {
        return (StatusCode::NOT_FOUND, Json(json!({"error": message})));
    }
    to_http_error(error)
}

fn to_folder_rename_undo_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let reason = [
        "plan_not_found",
        "plan_not_executed",
        "artist_missing",
        "target_missing",
        "target_not_directory",
        "source_exists",
        "stale_state",
        "outside_artist",
    ]
    .into_iter()
    .find(|reason| message.contains(reason));
    let status = match reason {
        Some("plan_not_found") => StatusCode::NOT_FOUND,
        Some(_) => StatusCode::CONFLICT,
        None => StatusCode::INTERNAL_SERVER_ERROR,
    };
    (
        status,
        Json(json!({
            "ok": false,
            "reason": reason.unwrap_or("undo_failed"),
            "message": message,
        })),
    )
}

fn to_artist_profile_link_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let status = if message.contains("artist not found") {
        StatusCode::NOT_FOUND
    } else if message.contains("URL")
        || message.contains("link kind")
        || message.contains("platform")
    {
        StatusCode::BAD_REQUEST
    } else {
        StatusCode::INTERNAL_SERVER_ERROR
    };
    (status, Json(json!({"error": message})))
}

fn to_artist_folder_move_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    let status = if message.contains("not found") {
        StatusCode::NOT_FOUND
    } else if message.contains("conflict")
        || message.contains("target")
        || message.contains("outside")
        || message.contains("source directory")
    {
        StatusCode::CONFLICT
    } else {
        StatusCode::BAD_REQUEST
    };
    (status, Json(json!({"error": message})))
}

async fn api_confirm_move_candidate_public(
    State(state): State<AppState>,
    Path(candidate_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        apply_move_candidate_response_with_roots(&conn, &roots, candidate_id)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
}

async fn api_ignore_move_candidate_public(
    State(state): State<AppState>,
    Path(candidate_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    ignore_move_candidate_response(&conn, candidate_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_mark_move_candidate_new_public(
    State(state): State<AppState>,
    Path(candidate_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    mark_move_candidate_new_response(&conn, candidate_id)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(Debug, Deserialize)]
struct ArchiveInspectRequest {
    item_id: Option<i64>,
    file_path: Option<String>,
    password: Option<String>,
}

async fn api_archive_inspect(
    State(state): State<AppState>,
    Json(payload): Json<ArchiveInspectRequest>,
) -> Result<Json<gallery_accel::archive_ops::ArchiveInspectResponse>, (StatusCode, Json<Value>)> {
    let roots = state.roots.clone();
    let conn = state.pool.get().map_err(to_http_error)?;

    let path_str = if let Some(id) = payload.item_id {
        conn.query_row("SELECT file_path FROM items WHERE id = ?", [id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "item not found"})),
            )
        })?
    } else if let Some(p) = payload.file_path {
        p
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "item_id or file_path is required"})),
        ));
    };

    let full_path = gallery_accel::media_serve::resolve_allowed_path(&path_str, &roots)
        .map_err(|e| (StatusCode::NOT_FOUND, Json(json!({"error": e.to_string()}))))?;
    drop(conn);

    let inspection = tokio::task::spawn_blocking(move || {
        gallery_accel::archive_ops::inspect_archive(&full_path, payload.password.as_deref())
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(to_archive_http_error)?;

    Ok(Json(inspection))
}

#[derive(Debug, Deserialize)]
struct ArchiveEntryQuery {
    item_id: Option<i64>,
    file_path: Option<String>,
    entry: String,
    password: Option<String>,
}

async fn api_archive_entry(
    State(state): State<AppState>,
    Query(query): Query<ArchiveEntryQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let roots = state.roots.clone();
    let conn = state.pool.get().map_err(to_http_error)?;

    let path_str = if let Some(id) = query.item_id {
        conn.query_row("SELECT file_path FROM items WHERE id = ?", [id], |row| {
            row.get::<_, String>(0)
        })
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "item not found"})),
            )
        })?
    } else if let Some(p) = query.file_path {
        p
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "item_id or file_path is required"})),
        ));
    };

    let full_path = gallery_accel::media_serve::resolve_allowed_path(&path_str, &roots)
        .map_err(|e| (StatusCode::NOT_FOUND, Json(json!({"error": e.to_string()}))))?;
    drop(conn);

    let entry_name = query.entry.clone();
    let pwd = query.password.clone();
    let bytes = tokio::task::spawn_blocking(move || {
        gallery_accel::archive_ops::stream_archive_entry(&full_path, &entry_name, pwd.as_deref())
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(to_archive_http_error)?;

    // Same inline policy as /api/file: an archive downloaded from the net is
    // untrusted input, so an .html/.svg member must not render on this origin.
    // Only media types that the browser cannot execute as script stay inline;
    // everything else is an attachment.
    let entry_path = std::path::Path::new(&query.entry);
    let (content_type, attachment) = gallery_accel::media_serve::inline_media_headers(entry_path);
    let mut builder = Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::CACHE_CONTROL, "private, max-age=3600");
    if let Some(name) = attachment {
        builder = builder.header(
            header::CONTENT_DISPOSITION,
            gallery_accel::media_serve::attachment_disposition(&name),
        );
    }
    Ok(builder
        .body(Body::from(bytes))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response()))
}

#[derive(Debug, Deserialize)]
struct ArchiveExtractRequest {
    item_id: Option<i64>,
    file_path: Option<String>,
    password: Option<String>,
    target_mode: Option<String>, // "current_folder" or "new_folder"
    custom_folder_name: Option<String>,
    recycle_source: Option<bool>,
    #[serde(default)]
    skip_scan: Option<bool>,
}

async fn api_archive_extract(
    State(state): State<AppState>,
    Json(payload): Json<ArchiveExtractRequest>,
) -> Result<Json<gallery_accel::archive_ops::ExtractResponse>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write capability is disabled"})),
        ));
    }
    // Extract writes into the media tree (staging, unpack, moves, optional
    // source recycle), so it takes the same exclusive file-operation slot as
    // every other filesystem-mutating route: without it an extract could run
    // while a folder rename or artist move relocates the very directory it is
    // unpacking into.
    let Some(guard) = state.scan.try_claim() else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "a scan or file operation is already running"})),
        ));
    };
    let roots = state.roots.clone();
    let conn = state.pool.get().map_err(to_http_error)?;

    let (path_str, artist_id) = if let Some(id) = payload.item_id {
        conn.query_row(
            "SELECT file_path, artist_id FROM items WHERE id = ?",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|_| {
            (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "item not found"})),
            )
        })?
    } else if let Some(p) = payload.file_path {
        let aid = conn
            .query_row(
                "SELECT artist_id FROM items WHERE file_path = ?",
                [&p],
                |row| row.get::<_, Option<i64>>(0),
            )
            .ok()
            .flatten();
        (p, aid)
    } else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "item_id or file_path is required"})),
        ));
    };

    let full_path = gallery_accel::media_serve::resolve_allowed_path(&path_str, &roots)
        .map_err(|e| (StatusCode::NOT_FOUND, Json(json!({"error": e.to_string()}))))?;
    drop(conn);

    let pool = state.pool.clone();
    let roots_clone = roots.clone();
    let scan_control = state.scan.clone();
    let target_mode = payload
        .target_mode
        .unwrap_or_else(|| "current_folder".to_string());
    let custom_name = payload.custom_folder_name;
    let recycle_source = payload.recycle_source.unwrap_or(false);
    let skip_scan = payload.skip_scan.unwrap_or(false);
    let pwd = payload.password;

    let res = tokio::task::spawn_blocking(
        move || -> anyhow::Result<gallery_accel::archive_ops::ExtractResponse> {
            let _guard = guard;
            let conn = pool.get()?;
            let res = gallery_accel::archive_ops::extract_archive(
                &conn,
                &roots_clone,
                &full_path,
                pwd.as_deref(),
                &target_mode,
                custom_name.as_deref(),
                recycle_source,
            )?;
            // Run the scoped artist scan synchronously under the already-claimed
            // operation slot (`_guard`) unless the caller is in the middle of a
            // multi-archive batch (`skip_scan = true`): that way extracted files
            // are indexed before the response returns AND a detached scan never
            // re-claims `state.scan` behind the caller's back (which caused 409
            // conflicts on subsequent archives in a batch).
            let target_str = res.target_dir.replace('\\', "/");
            let scan_artist_id = artist_id.or_else(|| {
                // Byte-exact prefix match, never LIKE: SQLite's LIKE is ASCII
                // case-insensitive and treats `_`/`%` in a legal artist path as
                // wildcards, and `path || '%'` has no '/' boundary, so an artist
                // `Art` also matched a target under `Artist2`. Both sides are
                // slash-normalized because `artists.path` keeps the platform
                // separator while the extract target is always normalized.
                conn.query_row(
                    "SELECT id FROM artists
                 WHERE COALESCE(missing, 0) = 0
                   AND (
                     replace(path, '\\', '/') = ?1
                     OR substr(?1, 1, length(replace(path, '\\', '/')) + 1)
                        = replace(path, '\\', '/') || '/'
                   )
                 ORDER BY length(path) DESC
                 LIMIT 1",
                    [&target_str],
                    |r| r.get(0),
                )
                .ok()
            });
            if let Some(aid) = scan_artist_id {
                // Demote stale folder rename plans whose source folder is the
                // extract target directory. `source_folder_for_item` derives the
                // *parent* of an item file, so handing it the target directory
                // demoted nothing for a top-level folder (relative depth 1 returns
                // "") and demoted the parent for a nested one;
                // `source_folder_for_dir` keys the directory on itself instead.
                if let Ok(artist_path) =
                    conn.query_row("SELECT path FROM artists WHERE id = ?", [aid], |r| {
                        r.get::<_, String>(0)
                    })
                {
                    if let Some(folder) = gallery_accel::folder_archive::source_folder_for_dir(
                        &artist_path,
                        &target_str,
                    ) {
                        let _ =
                            gallery_accel::folder_archive::invalidate_plans_after_item_date_change(
                                &conn,
                                aid,
                                &[folder],
                            );
                    }
                }

                if !skip_scan {
                    let _ = gallery_accel::scan::run_scan_claimed(
                        &conn,
                        &roots_clone,
                        &scan_control,
                        Some(aid),
                        None,
                    );
                }
            }
            Ok(res)
        },
    )
    .await
    .map_err(blocking_http_error)?
    .map_err(to_archive_http_error)?;

    Ok(Json(res))
}

fn to_archive_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let msg = error.to_string();
    if msg.contains("密码错误") || msg.contains("Wrong password") {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "解压密码错误", "wrong_password": true})),
        );
    }
    if msg.contains("not found") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": msg})));
    }
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": msg})),
    )
}

async fn api_ws_scan(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    // Prefer native scan_state polling from the local DB (pure Rust product).
    let pool = Arc::clone(&state.pool);
    ws.on_upgrade(move |mut socket| async move {
        let mut last = String::new();
        loop {
            // pool.get() + scan-state query are blocking SQLite work: keep
            // the 400ms poll off the async workers.
            let state_json = {
                let pool = Arc::clone(&pool);
                match tokio::task::spawn_blocking(move || {
                    pool.get()
                        .map(|conn| {
                            get_scan_state(&conn).unwrap_or_else(|_| json!({"status": "idle"}))
                        })
                        .map_err(|_| json!({"status": "idle"}))
                })
                .await
                {
                    Ok(Ok(scan)) => scan.to_string(),
                    _ => json!({"status": "idle"}).to_string(),
                }
            };
            if state_json != last {
                if socket
                    .send(axum::extract::ws::Message::Text(state_json.clone().into()))
                    .await
                    .is_err()
                {
                    break;
                }
                last = state_json;
            }
            tokio::select! {
                msg = socket.recv() => {
                    match msg {
                        Some(Ok(axum::extract::ws::Message::Close(_))) | None => break,
                        Some(Ok(axum::extract::ws::Message::Ping(p))) => {
                            let _ = socket.send(axum::extract::ws::Message::Pong(p)).await;
                        }
                        _ => {}
                    }
                }
                _ = tokio::time::sleep(std::time::Duration::from_millis(400)) => {}
            }
        }
    })
}

