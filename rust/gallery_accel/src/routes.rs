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
use axum::routing::{any, delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use serde::Deserialize;
use gallery_accel::post_evidence_state;
use gallery_accel::product_ui::{read_log_tail as read_bounded_log_tail, recent_log_errors};
use gallery_accel::upstream::{proxy_error, Upstream};
use gallery_accel::BridgeCapabilities;
use gallery_accel::{
    accept_legacy_scope, add_subscription_from_url, apply_folder_rename_template, apply_grouping,
    apply_hash_unique_scan_candidate_response_with_roots, apply_move_candidate_response_with_roots,
    apply_naming_migration, apply_scan_candidate_move_response_with_roots, artist_detail_response,
    artist_links_response, artist_profile_links_response, artist_recognition_status,
    artist_reference_scores_response, artist_references_response, artist_stats_response,
    artists_response, assess_post, assess_work_from_ledger,
    auto_resolve_move_candidates_with_roots, backfill_item_dimensions, bridge_move_is_isolated,
    bridge_task_view, cancel_attempt, cancel_character_import_job, character_model_signature,
    character_recognition_status, character_references_response, character_response,
    character_summary_response, characters_response, cluster_scores_response,
    confirm_all_artist_plans, confirm_artist_suggestion, content_group_locations,
    content_group_members, content_groups_for_day, content_hash_allowed,
    create_artist_profile_link, create_attempt, create_bridge_task, create_db_backup,
    create_new_item_response_with_roots, create_tag, default_bridge_identity,
    delete_artist_profile_link, delete_character_reference, delete_subscription, delete_tag,
    delete_to_recycle, demand_set, duplicate_artists_response, ensure_content_group_schema,
    ensure_netdisk_staging_directory, env_media_roots,
    execute_artist_folder_move, execute_folder_renames, finish_pawchive_sync,
    folder_archive_failed_plans_count, folder_error_artists, folder_paths_response,
    folder_rename_auto_response, folder_rename_auto_run, folder_rename_format_settings,
    folders_response, generate_event_scripter_script, get_character_import_job,
    get_pawchive_settings, get_scan_state, get_subscription, group_index_entries,
    hash_status_response, health_summary, ignore_move_candidate_response, item_detail_response,
    items_page_cursor_query_response, items_page_query_response, latest_bridge_session,
    list_artist_posts_page, list_bridge_tasks, list_content_groups, list_filtered_post_ids,
    list_folder_renames, list_media_root_directories, list_pawchive_events, list_post_attempts,
    list_post_candidates, list_subscription_posts, list_subscriptions, list_work_group_links,
    load_bridge_task, load_netdisk_settings, log_error, log_warn, mark_group_location_manual,
    mark_move_candidate_new_response, merge_move_candidate_group_with_roots,
    move_candidate_groups_response, move_candidates_response, move_history_response,
    netdisk_is_disconnected, open_writable_db, operation_history_response, operation_log_response,
    path_under_authorized_roots, pawchive_http_client, pawchive_sync_status, plan_manual_post,
    plan_naming_migration, preview_artist_folder_move, preview_day_pairing,
    preview_folder_rename_template, preview_jpeg_allowed, process_bridge_exchange,
    propagate_hash_tags_response, queue_bridge_command, rebuild_character_index, recheck_plan,
    recognize_character_native_topk_with_roots, reconfirm_plan, record_external_receipt,
    record_post_decision, record_selection, recycle_entries_response, reindex_artist_links,
    remember_bridge_token, resolve_existing_scan_candidate_response_with_roots,
    resolve_netdisk_staging_directory, resolve_scan_scope, restore_recycle_entry,
    clear_recycle_entries, purge_recycle_entry,
    rotate_bridge_token, run_folder_rename_all_now, run_full_library_scan_claimed,
    run_hash_batch_with_roots, run_manual_attempt, run_pawchive_reconcile, run_pawchive_sync,
    run_scan_claimed, save_netdisk_settings, save_pawchive_settings, saved_bridge_token,
    scan_candidates_response, serve_file_response, serve_text, serve_transcoded_hls,
    serve_transcoded_hls_segment, serve_video_compatible, serve_video_hls, set_folder_rename_auto,
    set_folder_rename_format_settings, set_item_favorite_response, set_netdisk_disconnected,
    set_subscription_mode, start_character_import_job_with_roots, start_video_transcode,
    submit_bridge_task, subscription_summary, suggest_artists_native, tag_search_response,
    tags_response, toggle_subscription, try_begin_pawchive_sync, unconfirm_all_artist_plans,
    unconfirm_plan, undo_folder_rename_plan, update_folder_tags_by_name_response,
    update_folder_tags_response, update_item_dates_response, update_item_tags_by_name_response,
    update_item_tags_response, update_tag, verify_bridge_token, verify_post_files,
    video_frame_jpeg, video_transcode_status, AttemptError, BridgeConflict, BridgeExchangePayload,
    BridgeInvalid, BridgeTask, CancelOutcome, DatePrecision, DbConfig, DbPool, DecisionOutcome,
    ExternalReceipt, LegacyScopeOutcome, MediaRoots, NamingApplyError, NamingApplyRequest,
    NetdiskSettings, PawchiveSettings, PostDecisionAction, PostListFilter, ReceiptOutcome,
    ScanControl, SelectionError, StatsRefreshGate, SubscriptionMode, SyncTrigger,
    WorkNamingContext, WorkerStatus, BRIDGE_TASK_SETTLED, BRIDGE_TASK_SUBMITTED,
    DEFAULT_ITEM_PAGE_LIMIT, LEDGER_REASON_UNKNOWN_WORK, MAX_CLUSTER_SCORE_VECTORS,
    MAX_ITEM_PAGE_LIMIT, NETDISK_BRIDGE_PAYLOAD_MAX_BYTES, NETDISK_PROTOCOL_VERSION,
    PAWCHIVE_FILTER_SELECTION_MAX_POSTS,
};
use rusqlite::OptionalExtension;
use serde_json::{json, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tower_http::services::{ServeDir, ServeFile};

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
    upstream: Option<Upstream>,
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
    #[cfg(test)]
    pub fn new(
        db_path: PathBuf,
        config: DbConfig,
        capabilities: Capabilities,
    ) -> anyhow::Result<Self> {
        // Held across construction on purpose. `DATA_DIR` is process-wide and
        // read per call, both here and by the logging module, so a construction
        // that ran while another test had pointed `DATA_DIR` at its own
        // directory would read that directory and write its `ANALYZE` line into
        // it — breaking a test that asserts its directory has no log yet.
        // `ENV_LOCK` is re-entrant, so a test that already holds it is unaffected.
        let _env_lock = crate::test_support::ENV_LOCK.lock();
        Self::with_options(db_path, config, capabilities, None, false)
    }

    pub fn with_options(
        db_path: PathBuf,
        config: DbConfig,
        capabilities: Capabilities,
        upstream: Option<Upstream>,
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
            upstream,
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

    /// Build a state that requires a management token, without touching the
    /// process environment.
    #[cfg(test)]
    pub fn with_management_token(mut self, token: Option<&str>) -> Self {
        self.management_token = token.map(str::to_string);
        self
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
    let mut app = Router::new()
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
        .route("/api/recycle", get(api_recycle_entries).delete(api_recycle_clear))
        .route("/api/recycle/clear", post(api_recycle_clear))
        .route(
            "/api/recycle/{entry_id}",
            delete(api_recycle_purge),
        )
        .route(
            "/api/recycle/{entry_id}/delete",
            post(api_recycle_purge),
        )
        .route("/api/recycle/{entry_id}/restore", post(api_recycle_restore))
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

    // Optional debug upstream only when explicitly configured (not required for product).
    if state.primary && state.upstream.is_some() {
        app = app.fallback(any(api_upstream_fallback));
    }

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
                    .filter(|path| path.starts_with(virtual_root) || path.starts_with(real_root))
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
    // Native fields are complete in product mode. An optional upstream may only fill gaps.
    if let Some(upstream) = state.upstream.as_ref() {
        // Gap-filling must never stall /api/health itself: a dead upstream answers
        // within seconds instead of blocking the endpoint past NAS watchdog limits.
        let remote = tokio::time::timeout(
            std::time::Duration::from_secs(2),
            upstream.get_json("/api/health"),
        )
        .await;
        if let Ok(Ok(remote)) = remote {
            if let Some(obj) = body.as_object_mut() {
                for key in [
                    "scan",
                    "scan_schedule",
                    "backup_schedule",
                    "backups",
                    "logs",
                    "recent_errors",
                ] {
                    if !obj.contains_key(key) {
                        if let Some(value) = remote.get(key) {
                            obj.insert(key.to_string(), value.clone());
                        }
                    }
                }
            }
        }
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
    if let Some(value) = path
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| chrono::NaiveDateTime::parse_from_str(name, "%Y%m%d-%H%M%S").ok())
        .map(|value| value.and_utc().timestamp() as f64)
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
    request: Request,
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
                // Path allowlist / missing file: never proxy raw client path to residual.
                if code == StatusCode::NOT_FOUND {
                    return Err((code, Json(body)));
                }
                // Optional residual only for decode failures when explicitly configured.
                if let Some(upstream) = state.upstream.clone() {
                    return proxy_request(upstream, request).await;
                }
                return Err((code, Json(body)));
            }
        }
    }
    if let Some(upstream) = state.upstream.clone() {
        return proxy_request(upstream, request).await;
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
    let conn = state.pool.get().map_err(to_http_error)?;
    move_candidates_response(
        &conn,
        &state.roots,
        query.status.as_deref().unwrap_or("pending"),
        query.hide_grouped.unwrap_or(false),
        query.limit,
        query.offset,
    )
    .map(Json)
    .map_err(to_http_error)
}

async fn api_scan_candidates(
    State(state): State<AppState>,
    Query(query): Query<ScanCandidateQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let status = query.status.as_deref().unwrap_or("pending");
    if !matches!(
        status,
        "pending" | "candidate" | "new" | "resolved" | "ignored"
    ) {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "invalid scan candidate status"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    scan_candidates_response(&conn, status, query.limit, query.offset)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_move_candidate_groups(
    State(state): State<AppState>,
    Query(query): Query<GroupQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    move_candidate_groups_response(
        &conn,
        &state.roots,
        query.status.as_deref().unwrap_or("pending"),
        query.sample_limit,
    )
    .map(Json)
    .map_err(to_http_error)
}

async fn api_move_history(
    State(state): State<AppState>,
    Query(query): Query<HistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    move_history_response(
        &conn,
        &state.roots,
        query.status.as_deref(),
        query.limit,
        query.offset,
    )
    .map(Json)
    .map_err(to_http_error)
}

async fn api_operation_history(
    State(state): State<AppState>,
    Query(query): Query<OperationHistoryQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    operation_history_response(&conn, &state.roots, query.limit)
        .map(Json)
        .map_err(to_http_error)
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
    let artist_id = q.artist_id.ok_or_else(|| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "artist_id required"})),
        )
    })?;
    let conn = state.pool.get().map_err(to_http_error)?;
    folder_rename_auto_run(&conn, &state.roots, artist_id)
        .map(Json)
        .map_err(to_http_error)
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
            items_page_cursor_query_response(
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
                query.cursor.as_deref(),
            )
        } else {
            items_page_query_response(
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
            let remaining = value.get("remaining").and_then(Value::as_u64).unwrap_or(0);
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
                current.remaining = remaining;
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
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    update_item_dates_response(
        &conn,
        payload.artist_id,
        &payload.item_ids,
        payload.manual_date.as_deref(),
    )
    .map(Json)
    .map_err(to_http_error)
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    update_item_tags_response(
        &conn,
        payload.artist_id,
        &payload.item_ids,
        &payload.tag_ids,
        &payload.mode,
    )
    .map(Json)
    .map_err(to_tag_write_http_error)
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
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    update_item_tags_by_name_response(&conn, &payload.item_ids, &payload.tag_names, &payload.mode)
        .map(Json)
        .map_err(to_tag_write_http_error)
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
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "read mode not enabled"})),
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
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "read only mode not enabled"})),
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
    let conn = state.pool.get().map_err(to_http_error)?;
    recycle_entries_response(
        &conn,
        &state.roots,
        query.status.as_deref(),
        query.limit,
        query.offset,
    )
    .map(Json)
    .map_err(to_http_error)
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

async fn api_recycle_purge(
    State(state): State<AppState>,
    Path(entry_id): Path<i64>,
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
        purge_recycle_entry(&conn, &roots, entry_id)
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
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    let status = query.status;
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        clear_recycle_entries(&conn, &roots, status.as_deref())
            .map(Json)
            .map_err(|(status, body)| (status, Json(body)))
    })
    .await
    .map_err(blocking_http_error)?
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
    let conn = state.pool.get().map_err(to_http_error)?;
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
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
    let conn = state.pool.get().map_err(to_http_error)?;
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
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
            .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    let name = body.name.trim();
    if name.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "name required"})),
        ));
    }
    conn.execute(
        "INSERT INTO characters (name) VALUES (?)",
        rusqlite::params![name],
    )
    .map_err(|e| to_http_error(e.into()))?;
    let id = conn.last_insert_rowid();
    Ok(Json(json!({"id": id, "name": name})))
}

async fn api_delete_character(
    State(state): State<AppState>,
    Path(character_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    // Uploaded reference photos live on disk, so dropping the rows is not
    // enough. Best effort: a cleanup failure must not block the delete.
    if let Err(error) =
        gallery_accel::product_ui::remove_character_reference_images(&conn, character_id)
    {
        log_warn!("character delete: reference image cleanup failed: {error:#}");
    }
    conn.execute(
        "DELETE FROM character_references WHERE character_id=?",
        rusqlite::params![character_id],
    )
    .map_err(|e| to_http_error(e.into()))?;
    let n = conn
        .execute(
            "DELETE FROM characters WHERE id=?",
            rusqlite::params![character_id],
        )
        .map_err(|e| to_http_error(e.into()))?;
    Ok(Json(json!({"ok": n > 0, "id": character_id})))
}

async fn api_delete_character_reference(
    State(state): State<AppState>,
    Path((character_id, reference_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
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
    // A malformed body must not silently fall back to `{}`: that maps to the
    // most expensive scope ("all" — full-library import).
    let payload = body.map(|j| j.0).map_err(|err| {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({"error": format!("invalid JSON body: {err}")})),
        )
    })?;
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::task::spawn_blocking(move || {
        let conn = pool.get().map_err(to_http_error)?;
        start_character_import_job_with_roots(&conn, &roots, &payload)
            .map(Json)
            .map_err(to_http_error)
    })
    .await
    .map_err(blocking_http_error)?
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
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(1000);
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

fn preview_cache_control(versioned: bool) -> &'static str {
    if versioned {
        "public, max-age=31536000, immutable"
    } else {
        "no-cache"
    }
}

#[derive(serde::Deserialize)]
struct PawchiveSubscriptionBody {
    url: String,
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    since_date: Option<String>,
}

#[derive(serde::Deserialize)]
struct PawchiveToggleBody {
    enabled: bool,
}

/// The mode a subscription is being switched to. Absent means manual, the same
/// answer an unrecognised value gets: the mode that downloads less.
#[derive(serde::Deserialize)]
struct PawchiveModeBody {
    #[serde(default)]
    mode: Option<String>,
}

fn write_mode_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "write mode not enabled"})),
    )
}

async fn api_get_pawchive_settings(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = get_pawchive_settings(&conn).map_err(to_http_error)?;
    // The defaults ship with the response so the panel can offer a reset
    // without hard-coding the template strings a second time in the frontend.
    Ok(Json(json!({
        "settings": settings,
        "defaults": PawchiveSettings::default(),
    })))
}

async fn api_save_pawchive_settings(
    State(state): State<AppState>,
    Json(payload): Json<PawchiveSettings>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    save_pawchive_settings(&conn, &payload).map_err(to_http_error)?;
    Ok(Json(json!({"settings": payload})))
}

async fn api_list_pawchive_subscriptions(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let subscriptions = list_subscriptions(&conn).map_err(to_http_error)?;
    Ok(Json(json!({"subscriptions": subscriptions})))
}

async fn api_add_pawchive_subscription(
    State(state): State<AppState>,
    Json(body): Json<PawchiveSubscriptionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let subscription = add_subscription_from_url(
        &conn,
        &body.url,
        body.target_dir.as_deref(),
        body.since_date.as_deref(),
        &state.roots,
    )
    .map_err(|error| {
        // A malformed creator link is the user's input, not a server fault:
        // report it as a 400 so the panel can point at the URL field.
        let message = error.to_string();
        if message.starts_with("unrecognized archive domain")
            || message.starts_with("invalid creator url")
            || message.starts_with("expected URL pattern")
        {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message})));
        }
        to_http_error(error)
    })?;
    Ok(Json(json!({"subscription": subscription})))
}

async fn api_delete_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let deleted = delete_subscription(&conn, id).map_err(to_http_error)?;
    if !deleted {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "subscription not found"})),
        ));
    }
    Ok(Json(json!({"deleted": id})))
}

async fn api_toggle_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveToggleBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let updated = toggle_subscription(&conn, id, body.enabled).map_err(to_http_error)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "subscription not found"})),
        ));
    }
    Ok(Json(json!({"id": id, "enabled": body.enabled})))
}

/// Switch a subscription between automatic and manual claiming (plan 6.11).
///
/// This never starts a download: switching to manual stops the next automatic
/// claim from happening, and switching to auto lets the next round decide again
/// from the acquisition ledger. The ledger, the decisions and any task already
/// running are untouched.
async fn api_set_pawchive_subscription_mode(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveModeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let mode = SubscriptionMode::parse(body.mode.as_deref().unwrap_or("manual"));
    let conn = state.pool.get().map_err(to_http_error)?;
    let updated = set_subscription_mode(&conn, id, mode).map_err(to_http_error)?;
    if !updated {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "subscription not found"})),
        ));
    }
    Ok(Json(json!({"id": id, "mode": mode.as_str()})))
}

async fn api_pawchive_sync(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    // A sync round fetches remote listings and downloads files, so it is far
    // too slow to await inside the request. Claim the single sync slot, run it
    // on a background task, and let the panel follow `/api/pawchive/status`.
    if !try_begin_pawchive_sync(SyncTrigger::Manual) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "sync already running"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::spawn(async move {
        finish_pawchive_sync(run_pawchive_sync(pool, roots, SyncTrigger::Manual, None).await);
    });
    Ok(Json(json!({
        "started": true,
        "status": pawchive_sync_status(),
    })))
}

/// Discovery and coverage only: same round as a sync with the download phase
/// skipped, so the panel can answer "which days are still short" without
/// claiming any pending post. Shares the single sync slot with sync rounds.
async fn api_pawchive_check(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    if !try_begin_pawchive_sync(SyncTrigger::Check) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "sync already running"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::spawn(async move {
        finish_pawchive_sync(run_pawchive_sync(pool, roots, SyncTrigger::Check, None).await);
    });
    Ok(Json(json!({
        "started": true,
        "status": pawchive_sync_status(),
    })))
}

/// Library items that may already hold this post's content.
///
/// Read-only: it lists candidates for a human decision and never settles the
/// post from a path or a name.
async fn api_pawchive_post_candidates(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<PawchiveCandidatesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let candidates = list_post_candidates(&conn, id, query.limit).map_err(to_http_error)?;
    Ok(Json(json!({"candidates": candidates})))
}

#[derive(serde::Deserialize)]
struct PawchiveCandidatesQuery {
    limit: Option<i64>,
}

/// Hash a post's delivered files against the hashes the source published.
///
/// Reads the files, so it is a write-path capability action rather than a plain
/// query. Resources with nothing to compare against are reported as skipped
/// instead of being claimed as verified.
async fn api_pawchive_verify_post(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let pool = std::sync::Arc::clone(&state.pool);
    let report = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        verify_post_files(&conn, id)
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(to_http_error)?;
    Ok(Json(json!({"report": report})))
}

/// Record what an external downloader reported about one delivery.
///
/// This is the contract a paired JD bridge posts to. No bridge is paired yet, so
/// the call below passes no bridge identity and the receipt is stored as history
/// without settling anything: a caller that merely names an existing file inside
/// the media roots must not be able to declare a work delivered. Wiring a real
/// bridge means passing its identity here, not relaxing the checks.
///
/// The bridge runs beside this server, so the peer has to be local. Without that
/// bound any host on the network could fill the receipt table — harmless today
/// because nothing settles, but the route is not a public one.
async fn api_pawchive_external_receipt(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(receipt): Json<ExternalReceipt>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    if !peer.ip().is_loopback() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "receipts are accepted from the local bridge only"})),
        ));
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    match record_external_receipt(&conn, &receipt, &state.roots, None) {
        Ok(ReceiptOutcome::Recorded) => Ok((
            StatusCode::OK,
            Json(json!({"recorded": true, "settled": true})),
        )),
        Ok(ReceiptOutcome::Duplicate) => Ok((
            StatusCode::OK,
            Json(json!({"recorded": false, "duplicate": true})),
        )),
        Ok(ReceiptOutcome::Stale) => Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "receipt does not cover the post's current manifest"})),
        )),
        Ok(ReceiptOutcome::Rejected(reason)) => Ok((
            StatusCode::ACCEPTED,
            Json(json!({"recorded": true, "settled": false, "reason": reason})),
        )),
        Ok(ReceiptOutcome::NotFound) => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        )),
        Err(error) => {
            // A path outside the media roots is the caller's input, not a
            // server fault.
            let message = error.to_string();
            if message.contains("outside authorized media roots") {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error": message}))));
            }
            Err(to_http_error(error))
        }
    }
}

async fn api_pawchive_status(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({"status": pawchive_sync_status()}))
}

/// Observation-only reconciliation. Safe to run on a live library: it reads
/// folders and the ledger, writes the derived assessments, and claims nothing.
async fn api_pawchive_reconcile(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    // The pass walks the library, so it runs on the same single slot as a sync
    // round and on a background task; the panel follows `/api/pawchive/status`
    // for the round and re-reads the subscriptions for the results.
    if !try_begin_pawchive_sync(SyncTrigger::Check) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "sync already running"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::spawn(async move {
        // Walking the library is blocking filesystem work, so it goes to the
        // blocking pool rather than occupying a request worker.
        let result = match tokio::task::spawn_blocking(move || {
            run_pawchive_reconcile(&pool, "manual", None)
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("reconcile task failed: {error}")),
        };
        finish_pawchive_sync(result);
    });
    Ok(Json(json!({
        "started": true,
        "status": pawchive_sync_status(),
    })))
}

/// 检查缺失 for one subscription: the panel-wide check's discovery round,
/// narrowed to that artist.
///
/// An unknown id is answered with 404 rather than starting a round that walks
/// nothing and then reports a clean check for a subscription that is not there.
async fn api_check_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    {
        let conn = state.pool.get().map_err(to_http_error)?;
        if get_subscription(&conn, id)
            .map_err(to_http_error)?
            .is_none()
        {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "subscription not found"})),
            ));
        }
    }
    if !try_begin_pawchive_sync(SyncTrigger::Check) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "sync already running"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::spawn(async move {
        finish_pawchive_sync(run_pawchive_sync(pool, roots, SyncTrigger::Check, Some(id)).await);
    });
    Ok(Json(json!({
        "started": true,
        "status": pawchive_sync_status(),
    })))
}

/// 核对本地 for one subscription: the same observation-only pass as the panel
/// button, scoped to that artist's posts.
async fn api_reconcile_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    {
        let conn = state.pool.get().map_err(to_http_error)?;
        if get_subscription(&conn, id)
            .map_err(to_http_error)?
            .is_none()
        {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": "subscription not found"})),
            ));
        }
    }
    if !try_begin_pawchive_sync(SyncTrigger::Check) {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "sync already running"})),
        ));
    }
    let pool = Arc::clone(&state.pool);
    tokio::spawn(async move {
        // Walking the library is blocking filesystem work, so it goes to the
        // blocking pool rather than occupying a request worker.
        let result = match tokio::task::spawn_blocking(move || {
            run_pawchive_reconcile(&pool, "manual", Some(id))
        })
        .await
        {
            Ok(result) => result,
            Err(error) => Err(anyhow::anyhow!("reconcile task failed: {error}")),
        };
        finish_pawchive_sync(result);
    });
    Ok(Json(json!({
        "started": true,
        "status": pawchive_sync_status(),
    })))
}

#[derive(Debug, serde::Deserialize)]
struct AuditEvidenceQuery {
    limit: Option<usize>,
}

async fn api_pawchive_audit_evidence(
    State(state): State<AppState>,
    Query(query): Query<AuditEvidenceQuery>,
) -> Result<Json<gallery_accel::evidence_audit::EvidenceAuditReport>, (StatusCode, Json<Value>)> {
    let limit = query.limit.unwrap_or(500);
    let conn = state.pool.get().map_err(to_http_error)?;
    let report = gallery_accel::evidence_audit::audit_evidence_bindings(&conn, limit)
        .map_err(|e| to_http_error(anyhow::anyhow!(e)))?;
    Ok(Json(report))
}

async fn api_pawchive_subscription_summary(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let summary = subscription_summary(&conn, id).map_err(|error| {
        let message = error.to_string();
        if message.contains("Query returned no rows") {
            return (
                StatusCode::NOT_FOUND,
                Json(json!({"error": "subscription not found"})),
            );
        }
        to_http_error(anyhow::anyhow!(message))
    })?;
    Ok(Json(json!({"summary": summary})))
}

#[derive(serde::Deserialize)]
struct PawchivePostsQuery {
    day: Option<String>,
    cursor: Option<i64>,
    limit: Option<i64>,
}

async fn api_pawchive_subscription_posts(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<PawchivePostsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let page = list_subscription_posts(&conn, id, query.day.as_deref(), query.cursor, query.limit)
        .map_err(to_http_error)?;
    Ok(Json(json!({
        "posts": page.posts,
        "total": page.total,
        "next_cursor": page.next_cursor,
    })))
}

#[derive(serde::Deserialize)]
struct PawchiveDecisionBody {
    action: String,
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    bound_path: Option<String>,
    /// The manifest version the user was looking at. A mismatch is a conflict,
    /// not an error: the resource list changed under the decision.
    #[serde(default)]
    expected_manifest_version: Option<i64>,
}

async fn api_pawchive_post_decision(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveDecisionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let Some(action) = PostDecisionAction::parse(&body.action) else {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "unknown decision action"})),
        ));
    };
    let conn = state.pool.get().map_err(to_http_error)?;
    let outcome = record_post_decision(
        &conn,
        id,
        action,
        body.reason.as_deref().unwrap_or(""),
        body.bound_path.as_deref().unwrap_or(""),
        body.expected_manifest_version,
    )
    .map_err(to_http_error)?;
    match outcome {
        DecisionOutcome::Applied(assessment) => Ok(Json(json!({
            "applied": true,
            "action": action.as_str(),
            "assessment": assessment,
        }))),
        DecisionOutcome::Conflict {
            current_manifest_version,
        } => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "manifest changed",
                "current_manifest_version": current_manifest_version,
            })),
        )),
        DecisionOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        )),
    }
}

#[derive(serde::Deserialize)]
struct AcceptanceBody {
    #[serde(default)]
    reason: Option<String>,
    #[serde(default)]
    bound_path: Option<String>,
}

/// Accept a local content group as this work's copy.
///
/// This is the act that turns the pairing preview's offer into a policy: the
/// work's current resource set becomes its covered scope, so an automatic round
/// stops asking for it. It writes a decision, never an acquisition — the plan is
/// explicit that accepting an old library is not a download.
async fn api_pawchive_accept_legacy_scope(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AcceptanceBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let reason = body.reason.as_deref().unwrap_or("");
    let bound_path = body.bound_path.as_deref().unwrap_or("");
    match accept_legacy_scope(&conn, id, reason, bound_path).map_err(to_http_error)? {
        LegacyScopeOutcome::Accepted { covered } => Ok(Json(json!({
            "accepted": true,
            "post_id": id,
            "covered_resource_versions": covered,
        }))),
        LegacyScopeOutcome::Covered => Ok(Json(json!({
            "accepted": false,
            "post_id": id,
            "reason": "this work already carries an exact accepted scope",
        }))),
        LegacyScopeOutcome::NotFound => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        )),
    }
}

async fn api_pawchive_post_acquisition(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = get_pawchive_settings(&conn).map_err(to_http_error)?;
    let Some(demand) = demand_set(&conn, id, &settings).map_err(to_http_error)? else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        ));
    };
    let assessment = assess_work_from_ledger(&conn, &demand.work_id).map_err(to_http_error)?;

    // The plan asks the derived API to answer all four facts separately rather
    // than fold them into one `verified`: acquisition is history, association is
    // where the work is filed, auto_policy is why nothing is being fetched, and
    // integrity is whether the located original still matches. A caller that
    // only ever sees `state` cannot tell "already downloaded" from "already
    // filed elsewhere" from "downloaded once and now damaged".
    let integrity = assess_post(&conn, id, &settings).map_err(to_http_error)?;

    // The work's standing group links, with the basis each one was recorded
    // under. This is what makes "已下过" distinguishable from "在别处" for the
    // panel, and it is read straight from the ledger rather than inferred from
    // a folder name.
    let links = list_work_group_links(&conn, id).map_err(to_http_error)?;
    let association: Vec<Value> = links
        .iter()
        .map(|(group_id, basis, _created_at, shared)| {
            json!({"group_id": group_id, "basis": basis, "shared": shared})
        })
        .collect();
    // One recorded basis, or `null` when the live links disagree. Reporting the
    // first one would claim a single provenance the ledger does not have.
    let mut bases: Vec<&str> = links
        .iter()
        .map(|(_, basis, _, _)| basis.as_str())
        .collect();
    bases.sort_unstable();
    bases.dedup();
    let match_basis = match bases.as_slice() {
        [only] => Value::String((*only).to_string()),
        _ => Value::Null,
    };

    Ok(Json(json!({
        "work_id": demand.work_id,
        "manifest_id": demand.manifest_id,
        "completeness": assessment.as_ref().map(|a| a.completeness.as_str()).unwrap_or("unknown"),
        "manifest_complete": assessment.as_ref().map(|a| a.manifest_complete).unwrap_or(false),
        "requested_resources": demand.requested_resources,
        "acquired_resources": demand.acquired_resources,
        "user_confirmed_resources": demand.user_confirmed_resources,
        "user_ignored_resources": demand.user_ignored_resources,
        "required_resources": demand.required_resources,
        "blocked_by_ambiguity": demand.blocked_by_ambiguity,
        "blocked_reason": demand.blocked_reason,
        // --- derived projection: acquisition / association / auto_policy / integrity ---
        "acquired_resource_versions": assessment
            .as_ref()
            .map(|a| a.acquired_resource_versions.clone())
            .unwrap_or_default(),
        "covered_resource_versions": assessment
            .as_ref()
            .map(|a| a.covered_resource_versions.clone())
            .unwrap_or_default(),
        "unproven_resource_versions": assessment
            .as_ref()
            .map(|a| a.unproven_resource_versions.clone())
            .unwrap_or_default(),
        "requires_fetch": assessment
            .as_ref()
            .map(|a| a.requires_fetch.clone())
            .unwrap_or_default(),
        "needs_confirmation": assessment.as_ref().is_some_and(|a| a.needs_confirmation),
        "reason_codes": assessment
            .as_ref()
            .map(|a| a.reason_codes.clone())
            .unwrap_or_else(|| vec![LEDGER_REASON_UNKNOWN_WORK.to_string()]),
        "association": association,
        "match_basis": match_basis,
        "integrity": {
            "state": integrity.state.as_str(),
            "reason": integrity.reason,
            "required_assets": integrity.required_assets,
            "proven_assets": integrity.proven_assets,
            "unverified_assets": integrity.unverified_assets,
            "missing_assets": integrity.missing_assets,
            "deferred_assets": integrity.deferred_assets,
            "integrity_failed_assets": integrity.integrity_failed_assets,
            "external_pending": integrity.external_pending,
        },
    })))
}

#[derive(serde::Deserialize)]
struct PawchiveEventsQuery {
    limit: Option<i64>,
}

/// The reconciliation reads: content groups, the stable work list, the naming
/// preview and the same-day pairing view. All of them are read-only; the two
/// writers (marking a location manual, applying a pairing decision) are separate
/// handlers below.
#[derive(serde::Deserialize)]
struct ContentGroupsQuery {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    day: Option<String>,
    #[serde(default)]
    include_gone: Option<bool>,
}

/// Content groups of one artist scope, or of one day.
async fn api_content_groups(
    State(state): State<AppState>,
    Query(query): Query<ContentGroupsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    if let Some(day) = query
        .day
        .as_deref()
        .map(str::trim)
        .filter(|day| !day.is_empty())
    {
        let scope = query.scope.as_deref().unwrap_or("");
        let groups = content_groups_for_day(&conn, scope, day).map_err(to_http_error)?;
        return Ok(Json(json!({"day": day, "groups": groups})));
    }
    let groups = list_content_groups(
        &conn,
        query.scope.as_deref(),
        query.include_gone.unwrap_or(false),
    )
    .map_err(to_http_error)?;
    Ok(Json(json!({"groups": groups})))
}

/// One group's members and known locations.
async fn api_content_group_detail(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let members = content_group_members(&conn, &group_id).map_err(to_http_error)?;
    let locations = content_group_locations(&conn, &group_id).map_err(to_http_error)?;
    Ok(Json(json!({
        "group_id": group_id,
        "members": members,
        "locations": locations,
    })))
}

#[derive(serde::Deserialize)]
struct GroupLocationBody {
    relative_path: String,
    #[serde(default)]
    manual: Option<bool>,
    #[serde(default)]
    source_operation: Option<String>,
}

/// Record that the user, not a later grouping pass, decides where this group
/// lives. Without this a manual move would be re-stamped by the next grouping.
async fn api_mark_group_location(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    Json(body): Json<GroupLocationBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let changed = mark_group_location_manual(
        &conn,
        &group_id,
        &body.relative_path,
        body.manual.unwrap_or(true),
        body.source_operation.as_deref().unwrap_or("user"),
    )
    .map_err(to_http_error)?;
    Ok(Json(json!({
        "group_id": group_id,
        "relative_path": body.relative_path,
        "changed": changed,
    })))
}

/// The stable, filterable work list (plan §7.1). A cursor is the sort key, so a
/// post inserted while a review is open cannot shift the pages already read.
#[derive(serde::Deserialize)]
struct StablePostsQuery {
    #[serde(default)]
    subscription_id: Option<i64>,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default)]
    limit: Option<i64>,
    #[serde(default)]
    day: Option<String>,
    #[serde(default)]
    state: Option<String>,
    #[serde(default)]
    search: Option<String>,
    #[serde(default)]
    artist_id: Option<i64>,
}

async fn api_pawchive_post_list(
    State(state): State<AppState>,
    Query(query): Query<StablePostsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let filter = PostListFilter {
        day: query.day,
        state: query.state,
        search: query.search,
        artist_id: query.artist_id,
    };
    let page = list_artist_posts_page(
        &conn,
        query.subscription_id,
        query.cursor.as_deref(),
        query.limit,
        filter,
    )
    .map_err(|error| {
        // A malformed cursor is the caller's input, not an internal fault.
        let message = error.to_string();
        if message.contains("cursor") {
            return (StatusCode::BAD_REQUEST, Json(json!({"error": message})));
        }
        to_http_error(error)
    })?;
    Ok(Json(json!({
        "posts": page.posts,
        "total": page.total,
        "next_cursor": page.next_cursor,
        "snapshot_revision": page.snapshot_revision,
        "truncated": page.truncated,
    })))
}

#[derive(serde::Deserialize)]
struct NamingPreviewQuery {
    work_key: i64,
}

/// What the stored naming rules would render for one work, under all four rule
/// sources the migration has to reconcile. Read-only: the operator reviews it
/// before anything is applied.
///
/// The context is built from the working row's own facts — the remote identity,
/// the title and the published value — and not from any rendered path, so the
/// preview describes the work rather than last time's output.
async fn api_pawchive_naming_preview(
    State(state): State<AppState>,
    Query(query): Query<NamingPreviewQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let row: Option<(
        String,
        String,
        String,
        String,
        Option<String>,
        Option<String>,
        Option<String>,
        Option<i64>,
    )> = conn
        .query_row(
            "SELECT s.site_id, s.service, s.user_id, p.post_id, p.title, p.published_at,
                    a.name, s.artist_id
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             LEFT JOIN artists a ON a.id = s.artist_id
             WHERE p.id = ?1",
            rusqlite::params![query.work_key],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                    row.get(7)?,
                ))
            },
        )
        .optional()
        .map_err(|error| to_http_error(error.into()))?;
    let Some((site, service, creator_id, post_id, title, published, artist, artist_id)) = row
    else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        ));
    };
    let mut context = WorkNamingContext {
        site,
        service,
        creator_id,
        post_id,
        artist: artist.unwrap_or_default(),
        title: title.unwrap_or_default(),
        date: published
            .as_deref()
            .map(|value| value.get(..10).unwrap_or_default().to_string())
            .unwrap_or_default(),
        raw_published: published.unwrap_or_default(),
        date_basis: "published".to_string(),
        ..WorkNamingContext::default()
    };
    if !context.date.is_empty() {
        context.date_precision = DatePrecision::Day;
    }
    let preview =
        plan_naming_migration(&conn, query.work_key, &context, artist_id).map_err(to_http_error)?;
    Ok(Json(json!({
        "work_key": query.work_key,
        "preview": preview,
    })))
}

/// Apply a reviewed naming rule migration or switch to the shared naming authority.
///
/// Requires expected_revision for CAS consistency; blocks if folder archive operations
/// are actively confirmed/executing (B5 gate coordination).
async fn api_pawchive_naming_apply(
    State(state): State<AppState>,
    Json(body): Json<NamingApplyRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    match apply_naming_migration(&conn, &body) {
        Ok(outcome) => Ok(Json(json!({
            "status": "applied",
            "revision": outcome.revision,
            "previous_revision": outcome.previous_revision,
            "semantic_version": outcome.semantic_version,
            "folder_template": outcome.folder_template,
            "image_template": outcome.image_template,
            "attachment_template": outcome.attachment_template,
        }))),
        Err(NamingApplyError::RevisionConflict { expected, current }) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "naming revision conflict",
                "expected_revision": expected,
                "current_revision": current,
            })),
        )),
        Err(NamingApplyError::ArchiveOperationInProgress(detail)) => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "archive operation in progress",
                "detail": detail,
            })),
        )),
        Err(NamingApplyError::Database(err)) => Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            Json(json!({"error": err})),
        )),
        Err(NamingApplyError::Other(msg)) => {
            Err((StatusCode::BAD_REQUEST, Json(json!({"error": msg}))))
        }
    }
}

async fn api_netdisk_get_settings(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let mut settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    if settings.staging_dir.trim().is_empty() {
        if let Some(default_staging) = resolve_netdisk_staging_directory(&state.roots) {
            let _ = std::fs::create_dir_all(&default_staging);
            settings.staging_dir = default_staging.to_string_lossy().to_string();
        }
    }
    Ok((StatusCode::OK, Json(json!(settings))))
}

async fn api_netdisk_save_settings(
    State(state): State<AppState>,
    Json(mut settings): Json<NetdiskSettings>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    if settings.staging_dir.trim().is_empty() {
        if let Some(default_staging) = resolve_netdisk_staging_directory(&state.roots) {
            let _ = std::fs::create_dir_all(&default_staging);
            settings.staging_dir = default_staging.to_string_lossy().to_string();
        }
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    save_netdisk_settings(&conn, &settings).map_err(|error| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": error.to_string()})),
        )
    })?;
    Ok((
        StatusCode::OK,
        Json(json!({"saved": true, "staging_dir": settings.staging_dir})),
    ))
}

async fn api_netdisk_rotate_token(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let token = rotate_bridge_token(&conn).map_err(to_http_error)?;
    Ok((StatusCode::OK, Json(json!({"token": token}))))
}

#[derive(serde::Deserialize)]
struct NetdiskScriptRequest {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

async fn api_netdisk_get_script(
    State(state): State<AppState>,
    Json(req): Json<NetdiskScriptRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let token = if let Some(t) = req.token.filter(|s| !s.trim().is_empty()) {
        if !verify_bridge_token(&conn, &t).map_err(to_http_error)? {
            return Err((
                StatusCode::UNAUTHORIZED,
                Json(json!({"error": "invalid bridge pairing token"})),
            ));
        }
        remember_bridge_token(&conn, &t).map_err(to_http_error)?;
        t.trim().to_string()
    } else if let Some(token) = saved_bridge_token(&conn).map_err(to_http_error)? {
        token
    } else if load_netdisk_settings(&conn)
        .map_err(to_http_error)?
        .bridge_configured
    {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "等待 JD 回传现有配对；如旧脚本已丢失，请重置配对"})),
        ));
    } else {
        rotate_bridge_token(&conn).map_err(to_http_error)?
    };
    let base_url = req
        .base_url
        .unwrap_or_else(|| "http://127.0.0.1:8899".to_string());
    let script = generate_event_scripter_script(&token, &base_url);
    Ok((
        StatusCode::OK,
        Json(json!({"script": script, "token": token})),
    ))
}

/// The connection state as a plain read.
///
/// `connect` and `disconnect` already return it as the result of their own
/// action, but a settings panel has to be able to ask "where do we stand"
/// without performing an action to find out — and the page re-reads on a timer.
/// This is the non-secret projection the plan's §6.10 table calls a GET that
/// does not log in, scan or test anything.
async fn api_netdisk_connection(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    Ok(Json(
        netdisk_connection_state(&conn, &settings).map_err(to_http_error)?,
    ))
}

async fn api_netdisk_test(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    let connection_state = netdisk_connection_state(&conn, &settings).map_err(to_http_error)?;
    let mut result = connection_state;
    result["enabled"] = json!(settings.enabled);
    result["peer"] = json!(peer.to_string());
    Ok((StatusCode::OK, Json(result)))
}

/// The connection state, as the settings view reports it.
///
/// Connected requires a recent, trusted handshake for the configured bridge.
/// A token, an old session or a different bridge never proves reachability.
fn netdisk_connection_state(
    conn: &rusqlite::Connection,
    settings: &NetdiskSettings,
) -> Result<Value, anyhow::Error> {
    let disconnected = netdisk_is_disconnected(conn)?;
    let bridge_id = default_bridge_identity();
    let session = latest_bridge_session(conn, bridge_id)?;
    let handshaked = settings.bridge_configured && !disconnected && session.is_some();
    use rusqlite::OptionalExtension;
    let last_seen: Option<i64> = conn
        .query_row(
            "SELECT CAST(updated_at AS INTEGER) FROM netdisk_bridge_sessions
             WHERE bridge_id = ?1 AND handshaked = 1
             ORDER BY updated_at DESC, rowid DESC LIMIT 1",
            [bridge_id],
            |row| row.get(0),
        )
        .optional()?;
    let auto_start_enabled: Option<bool> = if handshaked {
        conn.query_row(
            "SELECT capabilities FROM netdisk_bridge_sessions
             WHERE bridge_id = ?1 AND session_id = ?2",
            rusqlite::params![bridge_id, session],
            |row| row.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str::<BridgeCapabilities>(&raw).ok())
        .and_then(|capabilities| capabilities.linkgrabber_auto_start_enabled)
    } else {
        None
    };
    // The pre-check §6.6 requires is a property of the install, not of one
    // request: the settings view has to be able to say why 开始 is refused
    // instead of leaving the user to guess.
    let move_isolated = bridge_move_is_isolated(conn, bridge_id)?;
    let state = if !settings.bridge_configured {
        "未配置"
    } else if disconnected {
        "已断开"
    } else if handshaked {
        "已连接"
    } else if last_seen.is_some() {
        "设备离线"
    } else {
        "连接中"
    };
    Ok(json!({
        "state": state,
        "connected": handshaked,
        "disconnected": disconnected,
        "bridge_configured": settings.bridge_configured,
        "handshaked": handshaked,
        "move_isolated": handshaked && move_isolated,
        "auto_start_enabled": auto_start_enabled,
        "last_seen": last_seen,
        "session_id": if handshaked { session } else { None },
        "protocol_version": NETDISK_PROTOCOL_VERSION,
    }))
}

async fn api_netdisk_connect(
    State(state): State<AppState>,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    if !settings.bridge_configured {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "尚未生成配对密钥，无法连接"})),
        ));
    }
    set_netdisk_disconnected(&conn, false).map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    let state_json = netdisk_connection_state(&conn, &settings).map_err(to_http_error)?;
    Ok((StatusCode::OK, Json(state_json)))
}

async fn api_netdisk_disconnect(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    // Only the pairing is dropped. Tasks already handed to the downloader keep
    // running there; Gallery simply stops accepting reports until reconnected,
    // so it never claims to be tracking something it is not.
    set_netdisk_disconnected(&conn, true).map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    let state_json = netdisk_connection_state(&conn, &settings).map_err(to_http_error)?;
    Ok((StatusCode::OK, Json(state_json)))
}

#[derive(serde::Deserialize)]
struct NetdiskPathCheckRequest {
    #[serde(default)]
    staging_dir: Option<String>,
    #[serde(default)]
    import_dir: Option<String>,
}

/// Check the two directories the settings page asks the user to choose.
///
/// The staging directory has to sit outside every authorized media root, and
/// the probe file is created there and removed again — never in the media
/// roots, where a stray file would be picked up by the scanner. The import
/// directory is only inspected, never written to.
async fn api_netdisk_path_check(
    State(state): State<AppState>,
    Json(req): Json<NetdiskPathCheckRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;

    let staging_dir = req
        .staging_dir
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            if !settings.staging_dir.trim().is_empty() {
                Some(settings.staging_dir.clone())
            } else {
                resolve_netdisk_staging_directory(&state.roots)
                    .map(|p| p.to_string_lossy().to_string())
            }
        })
        .unwrap_or_default();
    let import_dir = req
        .import_dir
        .unwrap_or_else(|| settings.import_dir.clone());

    let staging = check_staging_dir(&staging_dir, &state.roots);
    let import = check_import_dir(&import_dir, &state.roots);

    let ok = staging["ok"] == json!(true) && import["ok"] == json!(true);
    Ok((
        StatusCode::OK,
        Json(json!({ "ok": ok, "staging": staging, "import": import })),
    ))
}

fn check_staging_dir(raw: &str, roots: &MediaRoots) -> Value {
    let trimmed = raw.trim();
    let path = if trimmed.is_empty() {
        match resolve_netdisk_staging_directory(roots) {
            Some(p) => p,
            None => return json!({"ok": false, "reason": "暂存目录未填写"}),
        }
    } else {
        std::path::PathBuf::from(trimmed)
    };
    if !path.is_absolute() {
        return json!({"ok": false, "reason": "暂存目录必须是绝对路径"});
    }
    let is_dot_dir = path
        .file_name()
        .and_then(|n| n.to_str())
        .map_or(false, |n| n.starts_with('.'));
    if !is_dot_dir && path_under_authorized_roots(&path, roots) {
        return json!({"ok": false, "reason": "暂存目录不能在媒体扫描根内"});
    }
    if let Err(error) = std::fs::create_dir_all(&path) {
        return json!({"ok": false, "reason": format!("暂存目录不可创建：{error}")});
    }
    let probe = path.join(format!(
        ".gallery-netdisk-probe-{}",
        uuid::Uuid::new_v4().simple()
    ));
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
    {
        Ok(file) => {
            drop(file);
            let _ = std::fs::remove_file(&probe);
            json!({"ok": true, "reason": ""})
        }
        Err(error) => json!({"ok": false, "reason": format!("暂存目录不可写：{error}")}),
    }
}

fn check_import_dir(raw: &str, roots: &MediaRoots) -> Value {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        // Optional: an empty value means the task's own target directory is
        // used, which is not an error.
        return json!({"ok": true, "reason": "未指定，使用任务目标目录"});
    }
    let path = std::path::PathBuf::from(trimmed);
    if !path.is_absolute() {
        return json!({"ok": false, "reason": "入库目录必须是绝对路径"});
    }
    if !path_under_authorized_roots(&path, roots) {
        return json!({"ok": false, "reason": "入库目录不在授权媒体根内"});
    }
    match std::fs::symlink_metadata(&path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            json!({"ok": true, "reason": ""})
        }
        Ok(_) => json!({"ok": false, "reason": "入库目录不是普通目录"}),
        Err(error) => json!({"ok": false, "reason": format!("入库目录不可访问：{error}")}),
    }
}

async fn api_netdisk_bridge_exchange(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body_bytes: bytes::Bytes,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    if !peer.ip().is_loopback() {
        return Err((
            StatusCode::FORBIDDEN,
            Json(json!({"error": "bridge exchange is accepted from loopback only"})),
        ));
    }

    if body_bytes.len() > 1024 * 1024 {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({"error": "payload exceeds 1MB limit"})),
        ));
    }

    let conn = state.pool.get().map_err(to_http_error)?;

    // An explicit disconnect stops the exchange outright. A bridge that keeps
    // posting would otherwise look like it is still being tracked.
    if netdisk_is_disconnected(&conn).map_err(to_http_error)? {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "网盘下载已断开连接，不再接受桥接交换"})),
        ));
    }

    let content_type = headers
        .get(axum::http::header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("");

    let mut token_opt = None;
    let mut payload_opt = None;

    if content_type.contains("application/x-www-form-urlencoded")
        || body_bytes.starts_with(b"token=")
    {
        for (k, v) in url::form_urlencoded::parse(&body_bytes) {
            if k == "token" {
                token_opt = Some(v.into_owned());
            } else if k == "payload" {
                let p: BridgeExchangePayload = serde_json::from_str(&v).map_err(|e| {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(json!({"error": format!("invalid payload JSON: {e}")})),
                    )
                })?;
                payload_opt = Some(p);
            }
        }
    } else {
        #[derive(serde::Deserialize)]
        struct JsonInput {
            token: String,
            payload: serde_json::Value,
        }
        let input: JsonInput = serde_json::from_slice(&body_bytes).map_err(|e| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("invalid request JSON: {e}")})),
            )
        })?;
        token_opt = Some(input.token);
        let p: BridgeExchangePayload = if input.payload.is_string() {
            serde_json::from_str(input.payload.as_str().unwrap()).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid payload string JSON: {e}")})),
                )
            })?
        } else {
            serde_json::from_value(input.payload).map_err(|e| {
                (
                    StatusCode::BAD_REQUEST,
                    Json(json!({"error": format!("invalid payload JSON: {e}")})),
                )
            })?
        };
        payload_opt = Some(p);
    }

    let (token, payload) = match (token_opt, payload_opt) {
        (Some(t), Some(p)) => (t, p),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "missing token or payload"})),
            ))
        }
    };

    if !verify_bridge_token(&conn, &token).map_err(to_http_error)? {
        return Err((
            StatusCode::UNAUTHORIZED,
            Json(json!({"error": "invalid bridge pairing token"})),
        ));
    }

    // The decoded payload has its own ceiling. The body limit above allows for
    // form encoding inflation; this one bounds what the parser is asked to
    // handle, and is the number the protocol documents.
    let decoded_len = serde_json::to_vec(&payload)
        .map(|bytes| bytes.len())
        .unwrap_or(0);
    if decoded_len > NETDISK_BRIDGE_PAYLOAD_MAX_BYTES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            Json(json!({
                "error": format!(
                    "decoded payload exceeds {} bytes",
                    NETDISK_BRIDGE_PAYLOAD_MAX_BYTES
                )
            })),
        ));
    }

    let response = match process_bridge_exchange(&conn, &payload, &state.roots) {
        Ok(response) => response,
        Err(error) => {
            // A reused sequence number with different content is a protocol
            // conflict, not a malformed request: the bridge has to reconcile
            // rather than be told to fix its JSON.
            if let Some(conflict) = error.downcast_ref::<BridgeConflict>() {
                return Err((StatusCode::CONFLICT, Json(json!({"error": conflict.0}))));
            }
            if let Some(invalid) = error.downcast_ref::<BridgeInvalid>() {
                return Err((StatusCode::BAD_REQUEST, Json(json!({"error": invalid.0}))));
            }
            return Err(to_http_error(error));
        }
    };

    remember_bridge_token(&conn, &token).map_err(to_http_error)?;
    let pending = gallery_accel::netdisk_import::pending_auto_imports(&conn, &payload.bridge_id)
        .map_err(to_http_error)?;
    if !pending.is_empty() {
        let pool = state.pool.clone();
        let roots = state.roots.clone();
        let scan = Arc::clone(&state.scan);
        tokio::task::spawn_blocking(move || {
            let Ok(conn) = pool.get() else {
                return;
            };
            for id in pending {
                // Re-read the switch after any previous task completed.
                if !load_netdisk_settings(&conn).is_ok_and(|s| s.enabled && s.auto_import) {
                    break;
                }
                if let Ok(Some(task)) = load_bridge_task(&conn, &id) {
                    if let Err(error) =
                        gallery_accel::netdisk_import::import_task(&conn, &task, &roots, &scan)
                    {
                        log_warn!("JD task {} import deferred: {error}", task.task_id);
                    }
                }
            }
        });
    }
    Ok((StatusCode::OK, Json(json!(response))))
}

#[derive(serde::Deserialize)]
struct NetdiskJobRequest {
    #[serde(default)]
    post_id: Option<i64>,
    #[serde(default)]
    post_ids: Vec<i64>,
    #[serde(default)]
    bridge_id: Option<String>,
    #[serde(default)]
    links: Vec<String>,
    #[serde(default)]
    link: Option<String>,
    #[serde(default)]
    password: Option<String>,
    #[serde(default)]
    auto_start: Option<bool>,
    #[serde(default)]
    force: bool,
}

async fn api_netdisk_create_job(
    State(state): State<AppState>,
    Json(mut req): Json<NetdiskJobRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    if !settings.enabled {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "网盘下载未开启"})),
        ));
    }
    let bridge_id = req
        .bridge_id
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| default_bridge_identity().to_string());
    let Some(session_id) = latest_bridge_session(&conn, &bridge_id).map_err(to_http_error)? else {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "本地下载器桥尚未配对，无法登记任务"})),
        ));
    };

    if let Some(single_link) = req.link.take() {
        let trimmed = single_link.trim().to_string();
        if !trimmed.is_empty() {
            req.links.push(trimmed);
        }
    }
    for link in &req.links {
        let trimmed = link.trim();
        if !trimmed.is_empty()
            && !trimmed.starts_with("http://")
            && !trimmed.starts_with("https://")
        {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "仅支持 http:// 或 https:// 协议链接"})),
            ));
        }
    }

    let mut target_post_ids = req.post_ids.clone();
    if let Some(pid) = req.post_id {
        if !target_post_ids.contains(&pid) {
            target_post_ids.insert(0, pid);
        }
    }
    if target_post_ids.is_empty() && !req.links.is_empty() {
        let found_id: Option<i64> = conn
            .query_row(
                "SELECT id FROM kemono_posts WHERE instr(external_links, ?1) > 0 ORDER BY id DESC LIMIT 1",
                rusqlite::params![&req.links[0].trim()],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| to_http_error(e.into()))?;
        if let Some(pid) = found_id {
            target_post_ids.push(pid);
        } else {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": "未在画库中找到包含此链接的作品记录，无法确定归属"})),
            ));
        }
    }
    if target_post_ids.is_empty() {
        return Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "缺少作品 ID 或分享链接"})),
        ));
    }

    let should_auto_start = req.auto_start.unwrap_or(settings.auto_start);
    let effective_staging_dir = if !settings.staging_dir.trim().is_empty() {
        settings.staging_dir.clone()
    } else {
        resolve_netdisk_staging_directory(&state.roots)
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_default()
    };

    if target_post_ids.len() == 1 {
        let pid = target_post_ids[0];
        if !req.force {
            let active: Option<(String, String)> = conn
                .query_row(
                    "SELECT task_id, state FROM netdisk_bridge_tasks
                     WHERE post_id = ?1 AND state NOT IN ('settled', 'cancelled', 'failed')
                     ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![pid],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|e| to_http_error(e.into()))?;
            if let Some((task_id, state)) = active {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "该作品已有投递任务正在进行中",
                        "existing_task_id": task_id,
                        "state": state
                    })),
                ));
            }
        }

        let task = create_bridge_task(
            &conn,
            &bridge_id,
            &session_id,
            pid,
            &req.links,
            req.password.as_deref(),
        )
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error.to_string()})),
            )
        })?;

        let mut submitted = false;
        let mut submit_error = String::new();
        let mut command_id = String::new();
        if should_auto_start && !task.links.is_empty() {
            match submit_bridge_task(&conn, &task, &effective_staging_dir) {
                Ok(id) => {
                    submitted = true;
                    command_id = id;
                }
                Err(error) => submit_error = error.to_string(),
            }
        }

        let state_label = if submitted {
            BRIDGE_TASK_SUBMITTED.to_string()
        } else {
            task.state.clone()
        };

        return Ok((
            StatusCode::CREATED,
            Json(json!({
                "task_id": task.task_id,
                "job_ids": vec![task.task_id.clone()],
                "package_name": task.package_name(),
                "bridge_id": task.bridge_id,
                "session_id": task.session_id,
                "work_id": task.work_id,
                "manifest_version": task.manifest_version,
                "links": task.links,
                "state": state_label,
                "status": "queued",
                "submitted": submitted,
                "skipped": Vec::<Value>::new(),
                "command_id": command_id,
                "submit_error": submit_error,
                "auto_start": should_auto_start,
            })),
        ));
    }

    // Batch handling
    let mut job_ids = Vec::new();
    let mut skipped = Vec::new();
    let mut first_task = None;
    let mut submitted_count = 0;
    for pid in target_post_ids {
        if !req.force {
            let active: Option<String> = conn
                .query_row(
                    "SELECT task_id FROM netdisk_bridge_tasks
                     WHERE post_id = ?1 AND state NOT IN ('settled', 'cancelled', 'failed')
                     ORDER BY created_at DESC LIMIT 1",
                    rusqlite::params![pid],
                    |row| row.get(0),
                )
                .optional()
                .map_err(|e| to_http_error(e.into()))?;
            if let Some(task_id) = active {
                skipped
                    .push(json!({"post_id": pid, "reason": "已有进行中任务", "task_id": task_id}));
                continue;
            }
        }

        match create_bridge_task(
            &conn,
            &bridge_id,
            &session_id,
            pid,
            &[],
            req.password.as_deref(),
        ) {
            Ok(task) => {
                let mut submitted = false;
                if should_auto_start && !task.links.is_empty() {
                    if submit_bridge_task(&conn, &task, &effective_staging_dir).is_ok() {
                        submitted = true;
                        submitted_count += 1;
                    }
                }
                job_ids.push(task.task_id.clone());
                if first_task.is_none() {
                    first_task = Some((task, submitted));
                }
            }
            Err(err) => {
                skipped.push(json!({"post_id": pid, "reason": err.to_string()}));
            }
        }
    }

    let (first_t, first_sub) = first_task.unwrap_or_else(|| {
        let dummy = BridgeTask {
            task_id: String::new(),
            bridge_id: bridge_id.clone(),
            session_id: session_id.clone(),
            work_id: String::new(),
            post_id: 0,
            manifest_version: 0,
            expected: Vec::new(),
            links: Vec::new(),
            password: None,
            state: String::new(),
        };
        (dummy, false)
    });

    let state_label = if first_sub {
        BRIDGE_TASK_SUBMITTED.to_string()
    } else {
        first_t.state.clone()
    };

    Ok((
        StatusCode::CREATED,
        Json(json!({
            "task_id": first_t.task_id,
            "job_ids": job_ids,
            "package_name": first_t.package_name(),
            "bridge_id": first_t.bridge_id,
            "session_id": first_t.session_id,
            "work_id": first_t.work_id,
            "manifest_version": first_t.manifest_version,
            "links": first_t.links,
            "state": state_label,
            "status": "queued",
            "submitted": submitted_count > 0,
            "submitted_count": submitted_count,
            "skipped": skipped,
            "command_id": "",
            "submit_error": "",
            "auto_start": should_auto_start,
        })),
    ))
}

async fn api_netdisk_list_jobs(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let tasks = list_bridge_tasks(&conn, 200).map_err(to_http_error)?;
    let mut jobs: Vec<Value> = Vec::with_capacity(tasks.len());
    for task in &tasks {
        jobs.push(bridge_task_view(&conn, task).map_err(to_http_error)?);
    }
    Ok((StatusCode::OK, Json(json!({"jobs": jobs}))))
}

#[derive(serde::Deserialize)]
struct NetdiskJobControl {
    #[serde(default)]
    link_ids: Vec<String>,
}

/// The job actions the bridge protocol defines.
///
/// Parsed before the task lookup so an unknown action is a malformed request
/// rather than something that depends on whether the job happens to exist.
#[derive(Clone, Copy, PartialEq, Eq)]
enum NetdiskJobAction {
    Start,
    Retry,
    Import,
    Pause,
    Resume,
    Remove,
}

async fn api_netdisk_job_control(
    State(state): State<AppState>,
    Path((task_id, action)): Path<(String, String)>,
    Json(body): Json<NetdiskJobControl>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;

    // An action outside the protocol is a malformed request whether or not the
    // task exists, so it is parsed before the task is looked up. Otherwise the
    // same typo would answer 400 for a real task and 404 for a missing one, and
    // the caller would read the 404 as "the job is gone".
    let action = match action.as_str() {
        "start" => NetdiskJobAction::Start,
        "retry" => NetdiskJobAction::Retry,
        "import" => NetdiskJobAction::Import,
        "pause" => NetdiskJobAction::Pause,
        "resume" => NetdiskJobAction::Resume,
        "remove" => NetdiskJobAction::Remove,
        other => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({"error": format!("unknown job action: {other}")})),
            ))
        }
    };

    let Some(task) = load_bridge_task(&conn, &task_id).map_err(to_http_error)? else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "job not found"})),
        ));
    };

    match action {
        // 开始 submits a registered task. It is deliberately not gated by
        // 自动开始: that switch decides the automatic default, and the button the
        // user pressed is an explicit instruction.
        NetdiskJobAction::Start => {
            let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
            let effective_staging_dir = if !settings.staging_dir.trim().is_empty() {
                settings.staging_dir.clone()
            } else {
                resolve_netdisk_staging_directory(&state.roots)
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_default()
            };
            let command_id =
                submit_bridge_task(&conn, &task, &effective_staging_dir).map_err(|error| {
                    (
                        StatusCode::CONFLICT,
                        Json(json!({"error": error.to_string()})),
                    )
                })?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "command_id": command_id,
                    "action": "start",
                    "state": BRIDGE_TASK_SUBMITTED,
                })),
            ));
        }
        // 重试 has to say which half failed. A settled task whose resources are
        // not in the library is an import retry; anything else is an engine
        // retry, which is a resume command rather than a fresh addLinks.
        NetdiskJobAction::Retry => {
            let (linked, total) = gallery_accel::netdisk_import::task_import_state(&conn, &task)
                .map_err(to_http_error)?;
            if task.state == BRIDGE_TASK_SETTLED && total > 0 && linked == total {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({"error": "任务资源已入库，无需重试"})),
                ));
            }
            if task.state == BRIDGE_TASK_SETTLED {
                let linked_now = run_netdisk_import(&conn, &task, &state.roots, &state.scan)?;
                return Ok((
                    StatusCode::OK,
                    Json(json!({
                        "action": "retry",
                        "retried": "import",
                        "linked": linked_now,
                        "total": total,
                    })),
                ));
            }
            let command_id = queue_bridge_command(
                &conn,
                &task.bridge_id,
                &task.task_id,
                "resume",
                &json!({ "link_ids": body.link_ids }),
            )
            .map_err(to_http_error)?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "command_id": command_id,
                    "action": "retry",
                    "retried": "engine",
                })),
            ));
        }
        // 入库 only makes sense for a task whose receipt was accepted against
        // its frozen manifest. Anything else is refused with the reason instead
        // of reporting an import that never happened.
        NetdiskJobAction::Import => {
            if task.state != BRIDGE_TASK_SETTLED {
                return Err((
                    StatusCode::CONFLICT,
                    Json(json!({
                        "error": "任务尚未结算，无法入库",
                        "state": task.state,
                    })),
                ));
            }
            let (_, total) = gallery_accel::netdisk_import::task_import_state(&conn, &task)
                .map_err(to_http_error)?;
            let linked = run_netdisk_import(&conn, &task, &state.roots, &state.scan)?;
            return Ok((
                StatusCode::OK,
                Json(json!({ "action": "import", "linked": linked, "total": total })),
            ));
        }
        NetdiskJobAction::Pause | NetdiskJobAction::Resume | NetdiskJobAction::Remove => {
            let command_action = match action {
                NetdiskJobAction::Pause => "pause",
                NetdiskJobAction::Resume => "resume",
                _ => "remove",
            };
            let command_id = queue_bridge_command(
                &conn,
                &task.bridge_id,
                &task.task_id,
                command_action,
                &json!({ "link_ids": body.link_ids }),
            )
            .map_err(to_http_error)?;
            return Ok((
                StatusCode::ACCEPTED,
                Json(json!({"command_id": command_id, "action": command_action})),
            ));
        }
    }
}

/// Run the ingest handoff for one task's post and report how many of its
/// delivered resources are now bound to a library item.
///
/// The linking pass itself is library-wide and idempotent; this reports the
/// task's own post so the caller sees a per-task answer rather than a global
/// count that says nothing about this job.
fn run_netdisk_import(
    conn: &rusqlite::Connection,
    task: &BridgeTask,
    roots: &MediaRoots,
    scan: &Arc<ScanControl>,
) -> Result<i64, (StatusCode, Json<Value>)> {
    gallery_accel::netdisk_import::import_task_manual(conn, task, roots, scan)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct PairingQuery {
    #[serde(default)]
    scope: Option<String>,
    day: String,
    #[serde(default)]
    range_incomplete: Option<bool>,
}

/// Rebuild one artist's content groups from the media index.
///
/// The index is the entry point the plan names, and this is the pass that turns
/// it into the groups a work can be associated with. It is synchronous because
/// the work is one indexed query plus one transaction per artist, and it is
/// bounded per call: a caller that wants every artist asks for pages of them
/// rather than holding a request open over the whole library.
#[derive(serde::Deserialize)]
struct GroupIndexBody {
    #[serde(default)]
    artist_id: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

async fn api_pawchive_index_groups(
    State(state): State<AppState>,
    Json(body): Json<GroupIndexBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let limit = body.limit.unwrap_or(20).clamp(1, MAX_ITEM_PAGE_LIMIT);
    let conn = state.pool.get().map_err(to_http_error)?;
    ensure_content_group_schema(&conn).map_err(to_http_error)?;

    let artists: Vec<(i64, String)> = match body.artist_id {
        Some(artist_id) => {
            let row: Option<(i64, String)> = conn
                .query_row(
                    "SELECT id, path FROM artists WHERE id = ?1",
                    rusqlite::params![artist_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()
                .map_err(|error| to_http_error(error.into()))?;
            match row {
                Some(row) => vec![row],
                None => {
                    return Err((
                        StatusCode::NOT_FOUND,
                        Json(json!({"error": "artist not found"})),
                    ))
                }
            }
        }
        None => {
            let mut stmt = conn
                .prepare("SELECT id, path FROM artists ORDER BY id ASC LIMIT ?1")
                .map_err(|error| to_http_error(error.into()))?;
            let rows = stmt
                .query_map(rusqlite::params![limit], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })
                .map_err(|error| to_http_error(error.into()))?
                .collect::<rusqlite::Result<Vec<(i64, String)>>>()
                .map_err(|error| to_http_error(error.into()))?;
            rows
        }
    };

    let mut applied = Vec::new();
    for (artist_id, artist_root) in artists {
        // The scope id is the artist identity, not the display name: a renamed
        // artist must keep one scope, and two artists that happen to share a
        // name must not share one.
        let scope = format!("artist:{artist_id}");
        let result = group_index_entries(&conn, artist_id).map_err(to_http_error)?;
        let report = apply_grouping(&conn, &scope, &artist_root, &result).map_err(to_http_error)?;
        applied.push(json!({
            "artist_id": artist_id,
            "artist_root": artist_root,
            "groups": result.groups.len(),
            "created": report.created,
            "updated": report.updated,
            "unchanged": report.unchanged,
            "gone": report.gone,
            "conflicts": result.conflicts.len(),
            "empty_roots": result.empty_roots.len(),
            "unreadable": result.unreadable.len(),
        }));
    }
    Ok(Json(json!({"applied": applied})))
}

/// The same-day candidate graph for one artist scope. It reports what it could
/// pair, what it refuses to pair, and which works it froze; it never decides
/// for the user what is still owed.
async fn api_pawchive_pairing_preview(
    State(state): State<AppState>,
    Query(query): Query<PairingQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let result = preview_day_pairing(
        &conn,
        query.scope.as_deref(),
        &query.day,
        query.range_incomplete.unwrap_or(false),
    )
    .map_err(to_http_error)?;
    Ok(Json(json!({
        "day": query.day,
        "edges": result.edges,
        "unpaired_works": result.unpaired_works,
        "unpaired_groups": result.unpaired_groups,
        "frozen_works": result.frozen_works,
        "questions": result.questions,
        "verdict_withheld": result.verdict_withheld,
    })))
}

/// The body of one explicit selection request.
///
/// `request_id` is the caller's idempotency key: the same request id with the
/// same body is answered with the same selection, and the same id with a
/// different body is a conflict. That is what makes a double click, a retried
/// request and a stale tab one task instead of three.
///
/// A request names its works one of two ways, never both: `post_ids` is an
/// explicit list (the per-work and per-day buttons), and `filter` is the list
/// view's own filter, which the server resolves to the same ids. The second form
/// exists because "全选当前筛选" stands for tens of thousands of works that must
/// not travel in a request body.
#[derive(serde::Deserialize)]
struct PawchiveSelectionBody {
    request_id: String,
    #[serde(default)]
    post_ids: Vec<i64>,
    #[serde(default)]
    file_ids: Vec<i64>,
    #[serde(default)]
    subscription_id: Option<i64>,
    #[serde(default)]
    filter: Option<PostListFilter>,
}

/// Resolve a selection body to the works it names.
///
/// Both selection routes accept the same two forms, so the override rule (an
/// empty `file_ids` means "everything this work asks for") and the "one form
/// only" rule live here instead of in each handler. Naming both is refused
/// rather than merged: the two lists would have to be intersected or unioned, and
/// either answer would be a guess about which one the caller meant.
fn resolve_selection_post_ids(
    body: &PawchiveSelectionBody,
    conn: &rusqlite::Connection,
) -> Result<(Vec<i64>, bool), (StatusCode, Json<Value>)> {
    match (&body.filter, body.post_ids.is_empty()) {
        (Some(_), false) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": "post_ids and filter are mutually exclusive"})),
        )),
        (Some(filter), true) => list_filtered_post_ids(
            conn,
            body.subscription_id,
            filter,
            PAWCHIVE_FILTER_SELECTION_MAX_POSTS,
        )
        .map_err(to_http_error),
        (None, _) => Ok((body.post_ids.clone(), false)),
    }
}

/// The one response both selection routes give when the body named a work this
/// ledger does not hold.
///
/// The two routes used to disagree — the preview answered `200` with a list, the
/// freezing route answered `400` with `post_ids` — so a caller had to know which
/// one it was talking to before it could read the same fact. Both now answer with
/// `unknown_posts`, the token the preview already used, and the request is
/// refused in both cases: a selection that silently drops a work, or a preview
/// that quietly plans fewer works than asked, are the same problem seen twice.
fn selection_unknown_posts_response(ids: Vec<i64>) -> (StatusCode, Json<Value>) {
    (
        StatusCode::UNPROCESSABLE_ENTITY,
        Json(json!({"error": "unknown posts", "unknown_posts": ids})),
    )
}

/// Preview a selection without recording a selection or an attempt.
///
/// It is not a pure read, and the router does not treat it as one: planning a
/// work materializes its long-term ledger rows (`plan_manual_post` calls
/// `materialize_work_ledger`), so this route writes the ledger tables. What it
/// never does is freeze a decision: no `pawchive_selections` row and no attempt
/// is created, which is why the panel can run it on every selection change.
async fn api_pawchive_selection_preview(
    State(state): State<AppState>,
    Json(body): Json<PawchiveSelectionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = get_pawchive_settings(&conn).map_err(to_http_error)?;
    let (mut post_ids, truncated) = resolve_selection_post_ids(&body, &conn)?;
    post_ids.sort();
    post_ids.dedup();
    let mut plans = Vec::new();
    let mut missing = Vec::new();
    for &post_id in &post_ids {
        match plan_manual_post(&conn, post_id, &settings).map_err(to_http_error)? {
            Some(plan) => plans.push(plan),
            None => missing.push(post_id),
        }
    }
    if !missing.is_empty() {
        return Err(selection_unknown_posts_response(missing));
    }
    let resources: usize = plans.iter().map(|plan| plan.resources.len()).sum();
    let external_posts: Vec<i64> = {
        let mut ext = Vec::new();
        for &pid in &post_ids {
            let has_ext: bool = conn
                .query_row(
                    "SELECT CASE WHEN TRIM(COALESCE(external_links, '')) NOT IN ('', '[]') THEN 1 ELSE 0 END FROM kemono_posts WHERE id = ?1",
                    [pid],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap_or(0) == 1;
            if has_ext {
                ext.push(pid);
            }
        }
        ext
    };
    Ok(Json(json!({
        "posts": plans.len(),
        "resources": resources,
        "no_direct_resources": plans.iter().filter(|plan| plan.no_direct_resources)
            .map(|plan| plan.post_id).collect::<Vec<_>>(),
        "external_posts": external_posts,
        "unknown_posts": Vec::<i64>::new(),
        "truncated": truncated,
        "plans": plans,
    })))
}

/// Freeze a selection for a request id.
async fn api_pawchive_create_selection(
    State(state): State<AppState>,
    Json(body): Json<PawchiveSelectionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = get_pawchive_settings(&conn).map_err(to_http_error)?;
    let (post_ids, truncated) = resolve_selection_post_ids(&body, &conn)?;
    match record_selection(
        &conn,
        &body.request_id,
        &post_ids,
        &body.file_ids,
        &settings,
    ) {
        Ok(outcome) => Ok(Json(json!({
            "selection_id": outcome.selection_id,
            "request_id": outcome.request_id,
            "reused": outcome.reused,
            "posts": outcome.posts,
            "resources": outcome.resources,
            "expires_at": outcome.expires_at,
            "unknown_posts": Vec::<i64>::new(),
            "matched": post_ids.len(),
            "truncated": truncated,
        }))),
        Err(SelectionError::RequestConflict) => Err((
            StatusCode::CONFLICT,
            Json(json!({"error": "request id already used for another request"})),
        )),
        Err(SelectionError::UnknownPosts(ids)) => Err(selection_unknown_posts_response(ids)),
        Err(other) => Err((
            StatusCode::BAD_REQUEST,
            Json(json!({"error": other.to_string()})),
        )),
    }
}

/// Create the task a frozen selection was made for, and run it in the
/// background so the request itself stays short.
async fn api_pawchive_create_attempt(
    State(state): State<AppState>,
    Json(body): Json<PawchiveAttemptBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let outcome = match create_attempt(
        &conn,
        body.request_id.as_deref().unwrap_or(""),
        &body.selection_id,
    ) {
        Ok(outcome) => outcome,
        Err(AttemptError::SelectionExpired(id)) => {
            return Err((
                StatusCode::CONFLICT,
                Json(json!({"error": "selection expired", "selection_id": id})),
            ))
        }
        Err(AttemptError::SelectionEmpty(id)) => {
            return Err((
                StatusCode::BAD_REQUEST,
                Json(json!({
                    "error": "selection holds no direct resource to fetch",
                    "selection_id": id,
                })),
            ))
        }
        Err(other) => {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error": other.to_string()})),
            ))
        }
    };
    // A manual task is the user's explicit request, so it is not gated on the
    // automatic master switch or the subscription's mode. It still cannot run
    // when the app is read-only, which is checked above.
    spawn_pawchive_attempt(&state, outcome.attempt_id.clone());
    Ok(Json(json!({
        "attempt_id": outcome.attempt_id,
        "selection_id": outcome.selection_id,
        "request_id": outcome.request_id,
        "posts": outcome.posts,
        "resources": outcome.resources,
        "started": true,
    })))
}

/// Run an explicit request in the background.
///
/// Shared by the selection flow and by 单文件重试 so both use one client and one
/// runner: a bare total deadline would mean "no file larger than N seconds of
/// transfer can ever be received", and a large attachment asked for by hand
/// would fail here while the scheduled round fetched it fine.
fn spawn_pawchive_attempt(state: &AppState, attempt_id: String) {
    let pool = Arc::clone(&state.pool);
    let roots = state.roots.clone();
    tokio::spawn(async move {
        let client = match pawchive_http_client() {
            Ok(client) => client,
            Err(_) => return,
        };
        let _ = run_manual_attempt(&pool, &client, &roots, &attempt_id).await;
    });
}

/// The resources of one work, as the panel has to show them.
///
/// 单文件重试 takes a file id, so a screen that offers it has to be able to
/// name the files it is offering them for. `has_evidence` is what tells the
/// panel whether a file actually owes a retry: it is the same fact the retry
/// route itself reads, and showing 重试 next to a file that is already in the
/// library would invite a click that can only be refused.
async fn api_pawchive_list_post_files(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let has_evidence_column: bool = conn
        .query_row(
            "SELECT COUNT(*) FROM pragma_table_info('kemono_files')
             WHERE name = 'evidence_item_id'",
            [],
            |row| Ok(row.get::<_, i64>(0)? > 0),
        )
        .map_err(|error| to_http_error(error.into()))?;
    let evidence_select = if has_evidence_column {
        "COALESCE(evidence_item_id IS NOT NULL, 0)"
    } else {
        "0"
    };
    let sql = format!(
        "SELECT id, file_name, file_type, status, COALESCE(expected_length, 0),
                COALESCE(error_message, ''), COALESCE(target_path, ''), {evidence_select}
         FROM kemono_files WHERE post_id = ?1 ORDER BY id ASC"
    );
    let mut stmt = conn
        .prepare(&sql)
        .map_err(|error| to_http_error(error.into()))?;
    let files: Vec<Value> = stmt
        .query_map(rusqlite::params![id], |row| {
            Ok(json!({
                "file_id": row.get::<_, i64>(0)?,
                "file_name": row.get::<_, String>(1)?,
                "file_type": row.get::<_, String>(2)?,
                "status": row.get::<_, String>(3)?,
                "expected_length": row.get::<_, i64>(4)?,
                "error": row.get::<_, String>(5)?,
                "target_path": row.get::<_, String>(6)?,
                "has_evidence": row.get::<_, i64>(7)? > 0,
            }))
        })
        .map_err(|error| to_http_error(error.into()))?
        .collect::<rusqlite::Result<Vec<_>>>()
        .map_err(|error| to_http_error(error.into()))?;
    Ok(Json(json!({"files": files})))
}

/// 单文件重试: fetch one resource of one work again.
///
/// This goes through the same selection → attempt path as an explicit manual
/// request rather than reaching into the download loop. That is what keeps the
/// re-fetch honest: the new delivery gets its own attempt identity and a
/// non-overwriting target, so the copy the user already arranged is not
/// replaced. A row a live pass currently holds is refused instead of being
/// yanked out from under it.
async fn api_pawchive_file_retry(
    State(state): State<AppState>,
    Path((post_id, file_id)): Path<(i64, i64)>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;

    let row: Option<(String, String)> = conn
        .query_row(
            "SELECT status, COALESCE(claimed_by, '') FROM kemono_files
             WHERE id = ?1 AND post_id = ?2",
            rusqlite::params![file_id, post_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| to_http_error(error.into()))?;
    let Some((status, claimed_by)) = row else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "file not found for this post"})),
        ));
    };
    if !claimed_by.is_empty() || status == "downloading" {
        return Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "该文件正由一次下载持有，请先停止该任务",
                "status": status,
            })),
        ));
    }

    let settings = get_pawchive_settings(&conn).map_err(to_http_error)?;
    let request_id = format!(
        "retry-{post_id}-{file_id}-{}",
        uuid::Uuid::new_v4().simple()
    );
    let selection = record_selection(&conn, &request_id, &[post_id], &[file_id], &settings)
        .map_err(|error| {
            (
                StatusCode::BAD_REQUEST,
                Json(json!({"error": error.to_string()})),
            )
        })?;
    let outcome = create_attempt(&conn, &request_id, &selection.selection_id).map_err(|error| {
        (
            StatusCode::CONFLICT,
            Json(json!({"error": error.to_string()})),
        )
    })?;
    spawn_pawchive_attempt(&state, outcome.attempt_id.clone());

    Ok((
        StatusCode::ACCEPTED,
        Json(json!({
            "attempt_id": outcome.attempt_id,
            "selection_id": outcome.selection_id,
            "post_id": post_id,
            "file_id": file_id,
            "started": true,
        })),
    ))
}

/// Scan this post's published files and report its own evidence counts.
async fn api_pawchive_post_import(
    State(state): State<AppState>,
    Path(post_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    tokio::task::spawn_blocking(move || {
        let conn = state.pool.get().map_err(to_http_error)?;
        let exists: bool = conn
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM kemono_posts WHERE id=?1)",
                [post_id],
                |r| r.get(0),
            )
            .map_err(|e| to_http_error(e.into()))?;
        if !exists {
            return Err((
                StatusCode::NOT_FOUND,
                Json(json!({"error":"post not found"})),
            ));
        }
        gallery_accel::pawchive_import::import_post(
            &conn,
            post_id,
            &state.roots,
            &state.scan,
            false,
        )
        .map_err(to_http_error)?;
        let (linked, total) = post_evidence_state(&conn, post_id).map_err(to_http_error)?;
        Ok(Json(
            json!({"post_id":post_id,"linked":linked,"total":total}),
        ))
    })
    .await
    .map_err(|e| to_http_error(e.into()))?
}

#[derive(serde::Deserialize)]
struct PawchiveAttemptBody {
    selection_id: String,
    #[serde(default)]
    request_id: Option<String>,
}

/// Stop an explicit request.
///
/// Cancelling undoes nothing: what was already published stays where it is, and
/// the resource already receiving is allowed to reach its own safe point. It
/// stops the pass from claiming anything further, so the response reports how
/// many works were still outstanding when the request was stopped.
async fn api_pawchive_cancel_attempt(
    State(state): State<AppState>,
    Path(attempt_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let outcome = cancel_attempt(&conn, &attempt_id).map_err(to_http_error)?;
    match outcome {
        CancelOutcome::Cancelled { posts } => Ok(Json(json!({
            "attempt_id": attempt_id,
            "state": "cancelled",
            "outstanding_posts": posts,
        }))),
        // Repeating a cancel is not an error: the caller asked for a state the
        // task is already in.
        CancelOutcome::AlreadyCancelled => Ok(Json(json!({
            "attempt_id": attempt_id,
            "state": "cancelled",
            "outstanding_posts": 0,
        }))),
        // Answering "cancelled" for a finished task would misdescribe it, and
        // there is nothing left to stop.
        CancelOutcome::AlreadyFinished => Err((
            StatusCode::CONFLICT,
            Json(json!({
                "error": "attempt already finished",
                "attempt_id": attempt_id,
                "state": "done",
            })),
        )),
        CancelOutcome::Unknown => Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "unknown attempt", "attempt_id": attempt_id})),
        )),
    }
}

/// The attempts a post has, with the state of each.
///
/// Every row answers for *this work*: how many files this attempt delivered for
/// it, and the error it recorded when it delivered none. The attempt's own total
/// resource count is deliberately not repeated here — on a row for a work the
/// attempt could not touch, it reads as a delivery that never happened.
async fn api_pawchive_post_attempts(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let rows = list_post_attempts(&conn, id).map_err(to_http_error)?;
    let attempts: Vec<Value> = rows
        .into_iter()
        .map(|(attempt_id, intent, state, fetched, error)| {
            json!({
                "attempt_id": attempt_id,
                "intent": intent,
                "state": state,
                "fetched": fetched,
                "error": error,
            })
        })
        .collect();
    Ok(Json(json!({"attempts": attempts})))
}

async fn api_pawchive_events(
    State(state): State<AppState>,
    Query(query): Query<PawchiveEventsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let events = list_pawchive_events(&conn, query.limit).map_err(to_http_error)?;
    Ok(Json(json!({"events": events})))
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
    let status = match message.as_str() {
        "artist not found" => StatusCode::NOT_FOUND,
        "tag name must not be empty" | "Bad mode" => StatusCode::BAD_REQUEST,
        _ => return to_http_error(error),
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
    if state.capabilities.read_only {
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
        conn.query_row(
            "SELECT file_path FROM items WHERE id = ?",
            [id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| (StatusCode::NOT_FOUND, Json(json!({"error": "item not found"}))))?
    } else if let Some(p) = payload.file_path {
        p
    } else {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "item_id or file_path is required"}))));
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
        conn.query_row(
            "SELECT file_path FROM items WHERE id = ?",
            [id],
            |row| row.get::<_, String>(0),
        )
        .map_err(|_| (StatusCode::NOT_FOUND, Json(json!({"error": "item not found"}))))?
    } else if let Some(p) = query.file_path {
        p
    } else {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "item_id or file_path is required"}))));
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

    let mime = mime_guess::from_path(&query.entry)
        .first_or_octet_stream()
        .essence_str()
        .to_string();

    Ok(Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, mime)
        .header(header::CACHE_CONTROL, "private, max-age=3600")
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
}

async fn api_archive_extract(
    State(state): State<AppState>,
    Json(payload): Json<ArchiveExtractRequest>,
) -> Result<Json<gallery_accel::archive_ops::ExtractResponse>, (StatusCode, Json<Value>)> {
    if state.capabilities.read_only {
        return Err((
            StatusCode::NOT_IMPLEMENTED,
            Json(json!({"error": "write mode not enabled"})),
        ));
    }
    let roots = state.roots.clone();
    let conn = state.pool.get().map_err(to_http_error)?;

    let (path_str, artist_id) = if let Some(id) = payload.item_id {
        conn.query_row(
            "SELECT file_path, artist_id FROM items WHERE id = ?",
            [id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .map_err(|_| (StatusCode::NOT_FOUND, Json(json!({"error": "item not found"}))))?
    } else if let Some(p) = payload.file_path {
        (p, None)
    } else {
        return Err((StatusCode::BAD_REQUEST, Json(json!({"error": "item_id or file_path is required"}))));
    };

    let full_path = gallery_accel::media_serve::resolve_allowed_path(&path_str, &roots)
        .map_err(|e| (StatusCode::NOT_FOUND, Json(json!({"error": e.to_string()}))))?;
    drop(conn);

    let pool = state.pool.clone();
    let roots_clone = roots.clone();
    let target_mode = payload.target_mode.unwrap_or_else(|| "current_folder".to_string());
    let custom_name = payload.custom_folder_name;
    let recycle_source = payload.recycle_source.unwrap_or(false);
    let pwd = payload.password;

    let res = tokio::task::spawn_blocking(move || {
        let conn = pool.get()?;
        gallery_accel::archive_ops::extract_archive(
            &conn,
            &roots_clone,
            &full_path,
            pwd.as_deref(),
            &target_mode,
            custom_name.as_deref(),
            recycle_source,
        )
    })
    .await
    .map_err(blocking_http_error)?
    .map_err(to_archive_http_error)?;

    // Trigger instant scoped scan on the artist or folder so extracted images appear immediately
    let scan_roots = state.roots.clone();
    let scan_control = state.scan.clone();
    let scan_pool = state.pool.clone();
    tokio::task::spawn_blocking(move || {
        if let Ok(c) = scan_pool.get() {
            let _ = gallery_accel::scan::run_scan(&c, &scan_roots, &scan_control, artist_id, None);
        }
    });

    Ok(Json(res))
}

fn to_archive_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let msg = error.to_string();
    if msg.contains("密码错误") || msg.contains("Wrong password") {
        return (StatusCode::BAD_REQUEST, Json(json!({"error": "解压密码错误", "wrong_password": true})));
    }
    if msg.contains("not found") {
        return (StatusCode::NOT_FOUND, Json(json!({"error": msg})));
    }
    (StatusCode::INTERNAL_SERVER_ERROR, Json(json!({"error": msg})))
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

async fn api_upstream_fallback(State(state): State<AppState>, request: Request) -> Response {
    match state.upstream.clone() {
        Some(upstream) => match proxy_request(upstream, request).await {
            Ok(response) => response,
            Err((_, Json(err))) => proxy_error(err["error"].as_str().unwrap_or("upstream error")),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(json!({"error": "no residual upstream for this route"})),
        )
            .into_response(),
    }
}

/// Proxy forwarding is for residual JSON APIs; refuse to buffer unbounded
/// upload bodies (an accidental large POST must not exhaust NAS memory).
const PROXY_BODY_MAX_BYTES: usize = 32 * 1024 * 1024;

async fn proxy_request(
    upstream: Upstream,
    request: Request,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let (parts, body) = request.into_parts();
    let mut stream = body.into_data_stream();
    let mut buffered: Vec<u8> = Vec::new();
    while let Some(chunk) = futures_util::StreamExt::next(&mut stream).await {
        let chunk = chunk.map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("read body: {err}")})),
            )
        })?;
        if buffered.len() + chunk.len() > PROXY_BODY_MAX_BYTES {
            return Err((
                StatusCode::PAYLOAD_TOO_LARGE,
                Json(json!({"error": "request body exceeds proxy forwarding limit"})),
            ));
        }
        buffered.extend_from_slice(&chunk);
    }
    upstream
        .forward(
            parts.method,
            &parts.uri,
            parts.headers,
            Bytes::from(buffered),
        )
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err.to_string()})),
            )
        })
}

#[cfg(test)]
// Env serialization (ENV_LOCK) is intentionally held across awaits: the
// async tests set process-global env vars before driving the router.
#[allow(clippy::await_holding_lock)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

    #[tokio::test]
    async fn static_ui_serves_artist_paths_without_shadowing_static_assets() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("index.html"), "gallery-index").unwrap();
        std::fs::write(dir.path().join("style.css"), "gallery-style").unwrap();
        let app = with_static_ui(Router::new(), dir.path().to_path_buf());

        for (uri, expected) in [
            ("/Artist%20Name", "gallery-index"),
            ("/static/style.css", "gallery-style"),
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
            let body = response.into_body().collect().await.unwrap().to_bytes();
            assert_eq!(body.as_ref(), expected.as_bytes(), "{uri}");
        }
    }

    #[tokio::test]
    async fn dimension_backfill_runs_in_background_and_reports_completion() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pictures = dir.path().join("pictures").join("Artist");
        std::fs::create_dir_all(&pictures).unwrap();
        let image_path = pictures.join("wide.png");
        image::RgbImage::new(640, 360).save(&image_path).unwrap();
        let _root = crate::test_support::EnvVar::set("PICTURES_ROOT", dir.path().join("pictures"));
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        state
            .pool
            .get()
            .unwrap()
            .execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', ?1)",
                [dir.path()
                    .join("pictures")
                    .join("Artist")
                    .to_string_lossy()
                    .as_ref()],
            )
            .unwrap();
        state
            .pool
            .get()
            .unwrap()
            .execute(
                "INSERT INTO items (artist_id, file_path, file_name, media_type) VALUES (1, ?1, 'wide.png', 'image')",
                [image_path.to_string_lossy().as_ref()],
            )
            .unwrap();
        let app = router(state);
        let (status, started) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/items/dimensions/backfill")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert!(started.get("running").is_some());

        let mut finished = Value::Null;
        for _ in 0..30 {
            tokio::time::sleep(Duration::from_millis(10)).await;
            let (status, body) = json_response(
                &app,
                Request::builder()
                    .uri("/api/items/dimensions/backfill/status")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            if body["running"] == json!(false) {
                finished = body;
                break;
            }
        }
        assert_eq!(finished["complete"], json!(true));
        assert_eq!(finished["updated"], json!(1));
        assert_eq!(finished["remaining"], json!(0));
    }

    #[test]
    fn folder_archive_health_counts_unresolved_execution_failures() {
        let conn = rusqlite::Connection::open_in_memory().unwrap();
        conn.execute_batch(
            "CREATE TABLE folder_rename_plans (status TEXT, execution_log TEXT);
             INSERT INTO folder_rename_plans VALUES ('manual_review', '[]');
             INSERT INTO folder_rename_plans VALUES
                ('manual_review', '[{\"status\":\"failed\",\"reason\":\"source_missing\"}]');
             INSERT INTO folder_rename_plans VALUES
                ('ready', '[{\"status\":\"failed\",\"reason\":\"stale_target\"}]');",
        )
        .unwrap();

        assert_eq!(folder_archive_failed_plans_count(&conn).unwrap(), 2);
    }

    fn tag_test_app() -> (tempfile::TempDir, Router) {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        state
            .pool
            .get()
            .unwrap()
            .execute_batch(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist');
                 INSERT INTO items (id, artist_id, file_path, file_name)
                 VALUES (1, 1, '/pictures/Artist/one.jpg', 'one.jpg');",
            )
            .unwrap();
        (dir, router(state))
    }

    async fn json_response(app: &Router, request: Request) -> (StatusCode, Value) {
        let response = app.clone().oneshot(request).await.unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body = serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| Value::String(String::from_utf8_lossy(&bytes).into_owned()));
        (status, body)
    }

    #[tokio::test]
    async fn media_roots_route_lists_configured_roots() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let _roots = crate::test_support::EnvVar::set("PICTURES_ROOT", "/pictures1,/pictures2");
        let _labels = crate::test_support::EnvVar::set("PICTURES_ROOT_LABELS", "主媒体,归档媒体");
        let (_dir, app) = tag_test_app();

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/media-roots")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            body["roots"][0],
            json!({"index": 0, "path": "/pictures1", "label": "主媒体"})
        );
        assert_eq!(
            body["roots"][1],
            json!({"index": 1, "path": "/pictures2", "label": "归档媒体"})
        );
    }

    #[tokio::test]
    async fn media_root_directories_route_is_non_recursive_and_rejects_parent_paths() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("Alpha").join("Child")).unwrap();
        std::fs::create_dir_all(root.path().join("Beta")).unwrap();
        let _roots = crate::test_support::EnvVar::set("PICTURES_ROOT", root.path());
        let (_dir, app) = tag_test_app();

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/media-roots/directories?root_index=0")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["directories"], json!(["Alpha", "Beta"]));

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/media-roots/directories?root_index=0&path=Alpha")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["directories"], json!(["Child"]));

        let (status, _) = json_response(
            &app,
            Request::builder()
                .uri("/api/media-roots/directories?root_index=0&path=..%2Foutside")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn favorite_put_is_idempotent_and_missing_item_is_not_found() {
        let (_dir, app) = tag_test_app();
        for _ in 0..2 {
            let request = Request::builder()
                .method(Method::PUT)
                .uri("/api/items/1/favorite")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"favorite":true}"#))
                .unwrap();
            let (status, body) = json_response(&app, request).await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["favorite"], true);
        }

        let request = Request::builder()
            .method(Method::PUT)
            .uri("/api/items/999/favorite")
            .header("content-type", "application/json")
            .body(Body::from(r#"{"favorite":true}"#))
            .unwrap();
        let (status, body) = json_response(&app, request).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "item not found");
    }

    #[tokio::test]
    async fn create_tag_accepts_query_parameters_without_a_body() {
        let (_dir, app) = tag_test_app();

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags?artist_id=1&name=%E6%A0%87%E7%AD%BE")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "标签");
        assert_eq!(body["sort_order"], 1);
    }

    #[tokio::test]
    async fn create_tag_keeps_json_body_compatibility() {
        let (_dir, app) = tag_test_app();

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"artist_id":1,"name":" json-tag "}"#))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["name"], "json-tag");
        assert_eq!(body["sort_order"], 1);
    }

    #[tokio::test]
    async fn create_tag_query_parameters_win_and_duplicates_return_the_existing_tag() {
        let (_dir, app) = tag_test_app();
        let uri = "/api/tags?artist_id=1&name=query-tag";
        let (_, first) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        let (status, duplicate) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::from(r#"{"artist_id":1,"name":"ignored-json-tag"}"#))
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(duplicate, first);
    }

    #[tokio::test]
    async fn create_tag_rejects_missing_or_empty_input_with_bad_request() {
        let (_dir, app) = tag_test_app();
        for request in [
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags?artist_id=1&name=%20%20")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"artist_id":1,"name":""}"#))
                .unwrap(),
        ] {
            let (status, _) = json_response(&app, request).await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn tag_writes_report_missing_parents_and_invalid_input_as_client_errors() {
        let (_dir, app) = tag_test_app();

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags?artist_id=999&name=orphan")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["error"], "artist not found");

        let (_, created) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/tags?artist_id=1&name=valid")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        let tag_id = created["id"].as_i64().unwrap();
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::PUT)
                .uri(format!("/api/tags/{tag_id}"))
                .header("content-type", "application/json")
                .body(Body::from(r#"{"artist_id":1,"name":"  "}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "tag name must not be empty");

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::PUT)
                .uri("/api/items/tags")
                .header("content-type", "application/json")
                .body(Body::from(
                    r#"{"artist_id":1,"item_ids":[1],"tag_ids":[],"mode":"bogus"}"#,
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["error"], "Bad mode");
    }

    #[tokio::test]
    async fn cluster_scores_rejects_invalid_vector_shapes_with_bad_request() {
        let (_dir, app) = tag_test_app();
        for body in [
            r#"{"vectors":[[],[1.0]]}"#,
            r#"{"vectors":[[1.0,0.0],[1.0]]}"#,
        ] {
            let (status, _) = json_response(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/cluster-scores")
                    .header("content-type", "application/json")
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::BAD_REQUEST);
        }
    }

    #[tokio::test]
    async fn content_hash_path_failures_are_not_internal_server_errors() {
        let (_dir, app) = tag_test_app();
        for path in ["/etc/hosts", "../etc/hosts", "/pictures/missing.jpg"] {
            let (status, body) = json_response(
                &app,
                Request::builder()
                    .uri(format!("/api/content-hash?path={path}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::NOT_FOUND, "path={path}: {body}");
        }

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/file/video-transcode?path=/etc/hosts")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "transcode path: {body}");

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/file/video-transcode-status?path=/etc/hosts")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(
            status,
            StatusCode::NOT_FOUND,
            "transcode status path: {body}"
        );
    }

    #[tokio::test]
    async fn logs_tail_reads_a_bounded_tail_window() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        let mut content = "old\n".repeat(2_000);
        content.push_str("tail-one\ntail-two\n");
        std::fs::write(logs.join("ui-actions.log"), content).unwrap();

        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .uri("/api/logs/tail?source=ui&lines=2&max_bytes=32")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(body["lines"], json!(["tail-one", "tail-two"]));
        assert_eq!(body["truncated"], true);
        assert_eq!(body["max_bytes"], 32);
    }

    #[tokio::test]
    async fn health_reports_native_status_without_upstream() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let _scan_interval = crate::test_support::EnvVar::set("SCAN_INTERVAL", "21600");
        let _backup_interval = crate::test_support::EnvVar::set("DB_BACKUP_INTERVAL", "43200");
        let logs = dir.path().join("logs");
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(
            logs.join("gallery.log"),
            "INFO failed=0\n[ERROR] disk\nTraceback: broken\n",
        )
        .unwrap();
        std::fs::write(
            logs.join("ui-actions.log"),
            "frontend_error rejected\nfrontend_rejection promise\n",
        )
        .unwrap();
        let backup_dir = dir.path().join("db-backups/20260712-010203");
        std::fs::create_dir_all(&backup_dir).unwrap();
        std::fs::write(backup_dir.join("gallery.db"), b"backup").unwrap();
        std::fs::write(
            backup_dir.join("metadata.json"),
            r#"{"created_at":1234,"label":"20260712-010203"}"#,
        )
        .unwrap();

        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let conn = state.pool.get().unwrap();
        gallery_accel::update_scan_state(
            &conn,
            &[
                ("status", json!("idle")),
                ("phase", json!("complete")),
                ("scanned_count", json!(481)),
                ("total_estimate", json!(482)),
            ],
        )
        .unwrap();
        state
            .workers
            .record("scan", true, json!({"status":"waiting"}), Some(2000.0));
        state.workers.record(
            "backup",
            true,
            json!({"ok":false,"error":"backup failed"}),
            Some(3000.0),
        );
        state
            .workers
            .record("hash", true, json!({"status":"waiting"}), Some(4000.0));
        drop(conn);

        let (status, body) = json_response(
            &router(state),
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        for key in [
            "database",
            "backups",
            "logs",
            "recent_errors",
            "scan",
            "scan_schedule",
            "backup_schedule",
            "hash",
            "workers",
        ] {
            assert!(body.get(key).is_some(), "missing health field: {key}");
        }
        assert_eq!(body["backups"]["count"], 1);
        assert_eq!(body["backups"]["latest"]["size_bytes"], 6);
        assert_eq!(body["backups"]["latest"]["updated_at"], 1234.0);
        assert_eq!(body["backups"]["last_backup_time"], 1234.0);
        assert!(body["media_roots"]["per_root_artists"].is_array());
        assert_eq!(body["recycle"]["awaiting_reconciliation"], 0);
        assert_eq!(body["scan"]["scanned_count"], 481);
        assert_eq!(body["scan_schedule"]["enabled"], true);
        assert_eq!(body["scan_schedule"]["interval"], 21600);
        assert_eq!(body["backup_schedule"]["last_error"], "backup failed");
        assert_eq!(body["logs"]["gallery_log"]["size_bytes"], 45);
        let errors = body["recent_errors"].as_array().unwrap();
        assert!(errors.iter().any(|row| row["line"] == "[ERROR] disk"));
        assert!(errors
            .iter()
            .any(|row| row["line"] == "frontend_rejection promise"));
        assert!(!errors
            .iter()
            .any(|row| row["line"].as_str().unwrap_or("").contains("failed=0")));
    }

    #[tokio::test]
    async fn health_reports_empty_backups_and_logs_without_upstream() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();

        let (status, body) = json_response(
            &router(state),
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["backups"]["count"], 0);
        assert!(body["backups"]["latest"].is_null());
        assert_eq!(body["logs"]["gallery_log"]["exists"], false);
        assert_eq!(body["logs"]["ui_actions_log"]["exists"], false);
        assert_eq!(body["recent_errors"], json!([]));
    }

    #[tokio::test]
    async fn items_reject_invalid_global_search_cursor_with_bad_request() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let (status, _) = json_response(
            &router(state),
            Request::builder()
                .uri("/api/items?search=image&cursor=not-json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
    }

    #[tokio::test]
    async fn health_read_only_does_not_try_to_create_scan_state() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let db_path = dir.path().join("gallery.db");
        {
            let state = AppState::new(
                db_path.clone(),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
                Capabilities {
                    read_only: false,
                    writes: true,
                    media: true,
                    ml: true,
                },
            )
            .unwrap();
            state
                .pool
                .get()
                .unwrap()
                .execute(
                    "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist')",
                    [],
                )
                .unwrap();
        }

        let state = AppState::new(
            db_path,
            DbConfig {
                read_only: true,
                pool_size: 1,
            },
            Capabilities {
                read_only: true,
                writes: false,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let (status, body) = json_response(
            &router(state),
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert_eq!(body["scan"]["status"], "idle");
        assert!(body.get("scan_error").is_none());
    }

    #[tokio::test]
    async fn legacy_database_is_readable_immediately_after_writable_startup() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let db_path = dir.path().join("gallery.db");
        {
            let conn = rusqlite::Connection::open(&db_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE artists (
                    id INTEGER PRIMARY KEY AUTOINCREMENT,
                    name TEXT NOT NULL,
                    path TEXT UNIQUE NOT NULL,
                    missing INTEGER NOT NULL DEFAULT 0,
                    missing_at REAL,
                    created_at INTEGER NOT NULL DEFAULT (strftime('%s','now'))
                 );
                 INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist');
                 CREATE TABLE scan_state (
                    id INTEGER PRIMARY KEY CHECK (id = 1),
                    artist_id INTEGER,
                    status TEXT NOT NULL DEFAULT 'idle',
                    phase TEXT NOT NULL DEFAULT '',
                    scanned_count INTEGER NOT NULL DEFAULT 0,
                    total_estimate INTEGER NOT NULL DEFAULT 0,
                    current_path TEXT NOT NULL DEFAULT '',
                    started_at REAL,
                    updated_at REAL
                 );
                 INSERT INTO scan_state (id, status) VALUES (1, 'idle');",
            )
            .unwrap();
        }

        let state = AppState::new(
            db_path,
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let app = router(state);
        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/scan/state")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["status"], "idle");
        assert_eq!(body["scan_id"], "");

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/health")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["ok"], true);
        assert!(body.get("database_error").is_none());
        assert!(body.get("scan_error").is_none());
    }

    #[tokio::test]
    async fn ui_log_rotates_at_configured_size() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        let _max_bytes = crate::test_support::EnvVar::set("UI_LOG_MAX_BYTES", "128");
        let _backup_count = crate::test_support::EnvVar::set("UI_LOG_BACKUP_COUNT", "2");

        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let app = router(state);
        for _ in 0..4 {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri("/api/ui-log")
                        .header("content-type", "application/json")
                        .body(Body::from(
                            r#"{"event":"test","data":"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"}"#,
                        ))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK);
        }

        let path = dir.path().join("logs").join("ui-actions.log");
        assert!(path.is_file());
        assert!(std::path::PathBuf::from(format!("{}.1", path.display())).is_file());
        assert!(std::path::PathBuf::from(format!("{}.2", path.display())).is_file());
    }

    /// A manual run is the work a background loop would otherwise have done, so it
    /// must refresh the planner statistics the same way the loop does — otherwise
    /// `SCAN_INTERVAL=0` plus `HASH_INTERVAL=0` and only the manual buttons leave
    /// the planner on the cardinalities the startup bootstrap collected, until the
    /// next restart.
    ///
    /// The refresh stays gated, so a second manual run inside the interval does
    /// not analyze again: the gap was the missing call, not a broken gate.
    #[tokio::test]
    async fn manual_runs_refresh_planner_statistics() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());
        // Pin the gate interval: the state reads it from the environment, and a
        // stray value from another test would otherwise decide whether the second
        // run refreshes.
        let _interval = crate::test_support::EnvVar::set("GALLERY_STATS_REFRESH_INTERVAL", "3600");

        /// A fresh process state, and therefore a fresh statistics gate: the gate
        /// is deliberately shared between the loops and the manual routes, so
        /// proving each route calls it needs one state per route.
        fn state_in(dir: &std::path::Path) -> (AppState, Router) {
            let state = AppState::new(
                dir.join("gallery.db"),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
                Capabilities {
                    read_only: false,
                    writes: true,
                    media: true,
                    ml: false,
                },
            )
            .unwrap();
            let app = router(state.clone());
            (state, app)
        }

        /// Make the recorded baseline stale without touching the items schema: an
        /// empty library that claims to have held a million items is no longer
        /// described by its statistics, so a refresh is due.
        fn stale(state: &AppState) {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "UPDATE app_settings SET value='1000000'
                 WHERE key IN ('query_planner_analyzed_items','query_planner_analyzed_item_tags')",
                [],
            )
            .unwrap();
        }
        fn baseline(state: &AppState) -> String {
            let conn = state.pool.get().unwrap();
            conn.query_row(
                "SELECT value FROM app_settings WHERE key='query_planner_analyzed_items'",
                [],
                |row| row.get(0),
            )
            .unwrap()
        }
        async fn post(app: &Router, uri: &str) {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(Method::POST)
                        .uri(uri)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::OK, "{uri}");
        }

        let (hash_state, hash_app) = state_in(dir.path());
        stale(&hash_state);
        assert_eq!(
            baseline(&hash_state),
            "1000000",
            "the stale baseline is in place"
        );

        post(&hash_app, "/api/hash/run").await;
        assert_eq!(
            baseline(&hash_state),
            "0",
            "a manual hash run must refresh the planner statistics like the hash loop"
        );

        stale(&hash_state);
        post(&hash_app, "/api/hash/run").await;
        assert_eq!(
            baseline(&hash_state),
            "1000000",
            "the refresh stays gated, so a second manual run inside the interval \
             does not analyze again"
        );

        // The scan route refreshes through its own gate, because the state above
        // has already spent its budget. The route answers before the walk
        // finishes, so wait for the recorded baseline rather than the response.
        let scan_dir = tempfile::tempdir().unwrap();
        let (scan_state, scan_app) = state_in(scan_dir.path());
        stale(&scan_state);
        post(&scan_app, "/api/scan").await;
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while baseline(&scan_state) != "0" && std::time::Instant::now() < deadline {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert_eq!(
            baseline(&scan_state),
            "0",
            "a manual scan must refresh the planner statistics like the scan loop"
        );
    }

    /// `product_ui::log_line_timestamp_millis` only parses 23-character stamps
    /// carrying a `,mmm` / `.mmm` fraction. A bare `%H:%M:%S` stamp makes every
    /// Rust-era line undateable, so fresh `frontend_error` rows inherit an old
    /// timestamp and the health error window filters them out.
    #[tokio::test]
    async fn ui_log_lines_carry_a_parseable_timestamp() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path());

        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let response = router(state)
            .oneshot(
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/ui-log")
                    .header("content-type", "application/json")
                    .body(Body::from(r#"{"event":"frontend_error"}"#))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let content =
            std::fs::read_to_string(dir.path().join("logs").join("ui-actions.log")).unwrap();
        let stamp = content
            .get(..23)
            .expect("line starts with a 23-char timestamp");
        assert!(
            chrono::NaiveDateTime::parse_from_str(stamp, "%Y-%m-%d %H:%M:%S,%3f").is_ok(),
            "ui-actions.log must use the shared log format: {stamp:?}"
        );
    }

    #[tokio::test]
    async fn disabled_capabilities_reject_media_ml_and_writes() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: true,
                writes: false,
                media: false,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);
        for request in [
            Request::builder()
                .method(Method::POST)
                .uri("/api/scan")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::GET)
                .uri("/api/file/text?path=/x.txt")
                .body(Body::empty())
                .unwrap(),
            Request::builder()
                .method(Method::POST)
                .uri("/api/cluster-scores")
                .body(Body::empty())
                .unwrap(),
        ] {
            let response = app.clone().oneshot(request).await.unwrap();
            assert!(matches!(
                response.status(),
                StatusCode::FORBIDDEN | StatusCode::NOT_IMPLEMENTED
            ));
        }
    }

    /// The receipt route is the downloader bridge's contract, and the bridge
    /// runs beside this server. A peer arriving over the network must not be
    /// able to write receipts at all, even though nothing settles without a
    /// paired bridge.
    #[tokio::test]
    async fn pawchive_receipts_are_only_accepted_from_a_local_peer() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);
        let body = serde_json::json!({
            "post_id": 1,
            "manifest_version": 1,
            "task_id": "task-1",
            "result": "completed",
            "output_paths": [],
            "bridge_id": "",
            "note": "",
        })
        .to_string();
        let receipt = |peer: SocketAddr| {
            let mut request = Request::builder()
                .method(Method::POST)
                .uri("/api/pawchive/receipts")
                .header("content-type", "application/json")
                .body(Body::from(body.clone()))
                .unwrap();
            request.extensions_mut().insert(ConnectInfo(peer));
            request
        };

        let remote = app
            .clone()
            .oneshot(receipt(SocketAddr::from(([203, 0, 113, 9], 41234))))
            .await
            .unwrap();
        assert_eq!(
            remote.status(),
            StatusCode::FORBIDDEN,
            "a receipt from the network is refused before it reaches the ledger"
        );

        // A local peer passes the gate and is answered by the handler itself:
        // there is no such post in this database, so the answer is 404.
        let local = app
            .oneshot(receipt(SocketAddr::from(([127, 0, 0, 1], 41235))))
            .await
            .unwrap();
        assert_eq!(local.status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn artist_suggestion_confirm_is_mutating() {
        assert!(is_nonmutating_post("/api/items/1/artist-suggestions"));
        assert!(!is_nonmutating_post(
            "/api/items/1/artist-suggestions/2/confirm"
        ));
    }

    /// The two selection routes answer "some of those works do not exist" the
    /// same way.
    ///
    /// They used to disagree: the preview answered `200` with `unknown_posts` and
    /// the freezing route answered `400` with `post_ids`, so a caller had to know
    /// which route it was talking to before it could read the same fact — and the
    /// panel, reading `unknown_posts`, saw nothing at all on the freezing route.
    /// Both now refuse the request with the one shape.
    #[tokio::test]
    async fn both_selection_routes_report_unknown_works_the_same_way() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        // One real work, so the request is refused for naming the other one and
        // not for naming nothing.
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO kemono_subscriptions
                     (service, user_id, target_dir, enabled, created_at, updated_at)
                 VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at)
                 VALUES (1, '900', 'pending', 't', 't')",
                [],
            )
            .unwrap();
        }
        let app = router(state);
        let body = json!({
            "request_id": "req-unknown",
            "post_ids": [1, 9_999],
            "file_ids": [],
        })
        .to_string();

        for uri in [
            "/api/pawchive/selections/preview",
            "/api/pawchive/selections",
        ] {
            let (status, response) = json_response(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.clone()))
                    .unwrap(),
            )
            .await;
            assert_eq!(
                status,
                StatusCode::UNPROCESSABLE_ENTITY,
                "{uri} must refuse a request naming a work this ledger does not hold: {response}"
            );
            assert_eq!(response["error"], "unknown posts", "{uri}");
            assert_eq!(
                response["unknown_posts"],
                json!([9_999]),
                "{uri} names the works it could not find, under one key"
            );
        }
    }

    /// The cancel entry point answers with the state the task is actually in.
    ///
    /// The three answers are the contract, not a nicety. A caller told
    /// "cancelled" for a task that had already finished would show the user a
    /// request they stopped as one that completed, and a caller told
    /// "cancelled" again for a repeated cancel would report work as just
    /// stopped that was stopped some time ago.
    #[tokio::test]
    async fn the_cancel_route_answers_the_state_the_task_is_actually_in() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO pawchive_attempts
                     (attempt_id, selection_id, request_id, state, created_at, updated_at)
                 VALUES ('att_open', 'sel_1', 'req-open', 'queued', 't', 't'),
                        ('att_done', 'sel_2', 'req-done', 'done', 't', 't')",
                [],
            )
            .unwrap();
            // One work already delivered and one never reached, so the count
            // cannot be read as "how many works the task had".
            conn.execute(
                "INSERT INTO pawchive_attempt_posts
                     (attempt_id, post_id, work_id, state, updated_at)
                 VALUES ('att_open', 1, 'w1', 'done', 't'),
                        ('att_open', 2, 'w2', 'queued', 't')",
                [],
            )
            .unwrap();
        }
        let app = router(state);
        let cancel = |path: &'static str| {
            let app = app.clone();
            async move {
                json_response(
                    &app,
                    Request::builder()
                        .method(Method::POST)
                        .uri(path)
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
            }
        };

        let (status, body) = cancel("/api/pawchive/attempts/att_open/cancel").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "cancelled", "{body}");
        assert_eq!(
            body["outstanding_posts"],
            json!(1),
            "only the work the task never reached is outstanding: {body}"
        );

        // Repeating it asks for a state the task is already in, which is not a
        // second cancel.
        let (status, body) = cancel("/api/pawchive/attempts/att_open/cancel").await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["state"], "cancelled", "{body}");
        assert_eq!(body["outstanding_posts"], json!(0), "{body}");

        // A task that finished is not reported as one that was stopped.
        let (status, body) = cancel("/api/pawchive/attempts/att_done/cancel").await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "attempt already finished", "{body}");
        assert_eq!(body["state"], "done", "{body}");

        // An id this ledger does not hold is not a task that stopped.
        let (status, body) = cancel("/api/pawchive/attempts/att_missing/cancel").await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "unknown attempt", "{body}");
    }

    #[tokio::test]
    async fn pawchive_post_acquisition_route_returns_demand_and_completeness() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let post_id = {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO kemono_subscriptions
                     (service, user_id, target_dir, enabled, created_at, updated_at)
                 VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at)
                 VALUES (1, '900', 'pending', 't', 't')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        let app = router(state);
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/pawchive/posts/{post_id}/acquisition"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["work_id"],
            "w_97ddb792bf415aa9a9effa3b5720c8239b7b9c3513c8560b84ca6bc89fbf3c91"
        );
        assert_eq!(body["completeness"], "unknown");
        assert_eq!(body["blocked_by_ambiguity"], false);

        // Missing post returns 404
        let (missing_status, _) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/pawchive/posts/99999/acquisition")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(missing_status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn pawchive_audit_evidence_route_returns_report() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state);
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/pawchive/audit-evidence?limit=10")
                .body(Body::empty())
                .unwrap(),
        )
        .await;

        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["audited_total"], 0);
        assert_eq!(body["verified_match"], 0);
    }

    /// The derived API has to answer acquisition, association, auto_policy and
    /// integrity as four separate facts. A caller that only receives `state`
    /// cannot tell "already downloaded" from "filed in a group I no longer
    /// track" from "downloaded once and now damaged" — which is exactly the
    /// distinction the backend plan forbids collapsing into one `verified`.
    #[tokio::test]
    async fn pawchive_acquisition_reports_the_four_facts_separately() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let post_id = {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO kemono_subscriptions
                     (service, user_id, target_dir, enabled, created_at, updated_at)
                 VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at)
                 VALUES (1, '901', 'pending', 't', 't')",
                [],
            )
            .unwrap();
            conn.last_insert_rowid()
        };

        let app = router(state.clone());
        let fetch = |uri: String| {
            Request::builder()
                .method(Method::GET)
                .uri(uri)
                .body(Body::empty())
                .unwrap()
        };

        // Case 1: nothing is known about this work yet. Every derived field has
        // to be present and honest about the absence, not silently omitted.
        let (status, body) = json_response(
            &app,
            fetch(format!("/api/pawchive/posts/{post_id}/acquisition")),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        // The route reports the long-term work identity itself, so the test uses
        // that instead of recomputing it: if the two ever disagreed, the
        // association assertions below would be testing a different work.
        let work_id = body["work_id"].as_str().unwrap().to_string();
        assert!(!work_id.is_empty(), "{body}");
        assert_eq!(body["reason_codes"], json!(["unknown_work"]), "{body}");
        assert_eq!(body["requires_fetch"], json!([]), "{body}");
        assert_eq!(body["needs_confirmation"], false, "{body}");
        assert_eq!(body["association"], json!([]), "{body}");
        assert!(body["match_basis"].is_null(), "{body}");
        assert!(
            body["integrity"]["state"].is_string(),
            "integrity must be its own fact, not folded into state: {body}"
        );

        // Case 2: one live group link. The basis the ledger recorded is what the
        // panel shows, and it comes from the ledger rather than a folder name.
        {
            let conn = state.pool.get().unwrap();
            ensure_content_group_schema(&conn).unwrap();
            for (group_id, relative) in [("grp-a", "2026-09-14 A"), ("grp-b", "2026-09-14 B")] {
                conn.execute(
                    "INSERT INTO content_groups
                         (group_id, artist_scope_id, root_relative, artist_root, date, precision,
                          generation, state, created_at, updated_at)
                     VALUES (?1, 'artist:1', ?2, '/pictures1/artist', '2026-09-14', 'day',
                             1, 'active', '', '')",
                    rusqlite::params![group_id, relative],
                )
                .unwrap();
            }
            conn.execute(
                "INSERT INTO work_group_links (work_id, group_id, basis, created_at, revoked_at)
                 VALUES (?1, 'grp-a', 'user', '', '')",
                rusqlite::params![work_id],
            )
            .unwrap();
        }

        let (_, body) = json_response(
            &app,
            fetch(format!("/api/pawchive/posts/{post_id}/acquisition")),
        )
        .await;
        assert_eq!(body["association"].as_array().unwrap().len(), 1, "{body}");
        assert_eq!(body["association"][0]["group_id"], "grp-a", "{body}");
        assert_eq!(body["match_basis"], "user", "{body}");

        // Case 3: the work is claimed by a second group under a different basis.
        // There is no single provenance any more, so the field has to say so
        // rather than report whichever row happened to come back first.
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO work_group_links (work_id, group_id, basis, shared, created_at, revoked_at)
                 VALUES (?1, 'grp-b', 'structured_identity', 1, '', '')",
                rusqlite::params![work_id],
            )
            .unwrap();
        }

        let (_, body) = json_response(
            &app,
            fetch(format!("/api/pawchive/posts/{post_id}/acquisition")),
        )
        .await;
        assert_eq!(body["association"].as_array().unwrap().len(), 2, "{body}");
        assert!(
            body["match_basis"].is_null(),
            "two disagreeing bases must not be reported as one: {body}"
        );
    }

    #[tokio::test]
    async fn pawchive_naming_apply_route_checks_cas_and_archive_coordination() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state.clone());

        // 1. Revision mismatch returns 409
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/pawchive/naming/apply")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": 999,
                        "folder_template": "{user}/{date}_{title}/"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "naming revision conflict");

        // 2. Active archive operation in progress returns 409
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures1/artist')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO folder_rename_plans (id, artist_id, source_folder, status, plan_kind)
                 VALUES (1, 1, 'artist_folder', 'confirmed', 'split_by_tag')",
                [],
            )
            .unwrap();
        }

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/pawchive/naming/apply")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": 1,
                        "folder_template": "{user}/{date}_{title}/"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["error"], "archive operation in progress");

        // 3. Clear active archive operation, now apply succeeds with 200
        {
            let conn = state.pool.get().unwrap();
            conn.execute("DELETE FROM folder_rename_plans", []).unwrap();
        }

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/pawchive/naming/apply")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "expected_revision": 1,
                        "folder_template": "{user}/{date}_{title}/"
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"], "applied");
        assert_eq!(body["revision"], 2);
        assert_eq!(body["previous_revision"], 1);
        assert_eq!(body["folder_template"], "{user}/{date}_{title}/");
    }

    #[tokio::test]
    async fn api_v1_compatibility_routes_match_pawchive_endpoints() {
        finish_pawchive_sync(Ok(serde_json::json!({})));
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state.clone());

        // /api/v1/status
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/status")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["status"]["running"], false);

        // /api/v1/settings
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["settings"].get("folder_template").is_some());

        // /api/v1/subscriptions
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/v1/subscriptions")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body["subscriptions"].is_array());
    }

    /// The management surface is reachable from this UI's own origin, and from
    /// no other page the browser might be showing.
    ///
    /// The install has no accounts. Without this, any site the operator happens
    /// to have open could post to the loopback address and start downloads or
    /// rewrite the target of one: the routes are exactly the ones that begin
    /// work, change where it writes, or record a decision.
    #[tokio::test]
    async fn the_management_surface_refuses_a_cross_site_origin() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);
        let host = "127.0.0.1:8787";

        let cross_site = Request::builder()
            .method(Method::POST)
            .uri("/api/pawchive/sync")
            .header("host", host)
            .header("origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, cross_site).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "cross-site management request is refused");

        // The panel's own request carries this origin and passes the gate. What
        // answers after that is the handler's business: this asserts the gate
        // was not the thing that refused it.
        //
        // Deliberately not `/api/pawchive/sync`: that route claims the one
        // process-wide sync slot, and starting a sync here would make
        // `status.running` read `true` for every other test running in this
        // binary at the same time.
        let same_site = Request::builder()
            .method(Method::POST)
            .uri("/api/pawchive/selections/preview")
            .header("host", host)
            .header("origin", format!("http://{host}"))
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, same_site).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
        assert_ne!(body["error"], "cross-site management request is refused");

        // A non-browser caller sends no origin at all; the capability check is
        // the thing that governs it, not this gate.
        let headless = Request::builder()
            .method(Method::POST)
            .uri("/api/pawchive/selections/preview")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, headless).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
        assert_ne!(body["error"], "cross-site management request is refused");

        // Reading is untouched: the panel loads these on every render, and none
        // of them changes anything.
        let read = Request::builder()
            .method(Method::GET)
            .uri("/api/pawchive/settings")
            .header("host", host)
            .header("origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, read).await;
        assert_eq!(status, StatusCode::OK);
        assert!(body["settings"].get("folder_template").is_some(), "{body}");

        // "null" names no authority, so it cannot match the one it reached.
        let sandboxed = Request::builder()
            .method(Method::POST)
            .uri("/api/pawchive/subscriptions/1/toggle")
            .header("host", host)
            .header("origin", "null")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, sandboxed).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
    }

    /// Every mutating management route refuses a cross-site origin, by
    /// construction rather than by being listed somewhere.
    ///
    /// The enumeration is the point. The gate used to name handlers one by one,
    /// and two routes that start a download were added without an entry — a page
    /// on another origin could POST them. This test walks the mutating paths of
    /// both management namespaces, so the next route that is added is covered
    /// the moment it exists, and the only way to exempt one is to edit `EXEMPT`
    /// in `is_management_path` deliberately.
    #[tokio::test]
    async fn every_mutating_management_route_refuses_a_cross_site_origin() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);
        let host = "127.0.0.1:8787";
        let mutating = [
            "/api/pawchive/sync",
            "/api/pawchive/check",
            "/api/pawchive/reconcile",
            "/api/pawchive/settings",
            "/api/pawchive/subscriptions",
            "/api/pawchive/subscriptions/1",
            "/api/pawchive/subscriptions/1/toggle",
            "/api/pawchive/subscriptions/1/mode",
            "/api/pawchive/posts/1/decisions",
            "/api/pawchive/posts/1/verify",
            "/api/pawchive/selections",
            "/api/pawchive/selections/preview",
            "/api/pawchive/attempts",
            "/api/pawchive/attempts/att_1/cancel",
            "/api/admin/rebuild-character-index",
        ];
        for path in mutating {
            assert!(
                is_management_path(path),
                "{path} must be a management path; if it is genuinely public, exempt it explicitly"
            );
            let request = Request::builder()
                .method(Method::POST)
                .uri(path)
                .header("host", host)
                .header("origin", "http://evil.example")
                .body(Body::empty())
                .unwrap();
            let (status, body) = json_response(&app, request).await;
            assert_eq!(status, StatusCode::FORBIDDEN, "{path}: {body}");
            assert_eq!(
                body["error"], "cross-site management request is refused",
                "{path}: {body}"
            );
        }

        // The external-downloader receipt endpoint is deliberately not the
        // operator's route: it is refused on its own contract, not by origin.
        let receipt = Request::builder()
            .method(Method::POST)
            .uri("/api/pawchive/receipts")
            .header("host", host)
            .header("origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, receipt).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
        assert_ne!(body["error"], "cross-site management request is refused");

        // Reads keep working from anywhere: the panel loads them on every render.
        let read = Request::builder()
            .method(Method::GET)
            .uri("/api/pawchive/status")
            .header("host", host)
            .header("origin", "http://evil.example")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, read).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
        assert_ne!(body["error"], "cross-site management request is refused");
    }

    /// When the operator configures a management token, the management routes
    /// require it — the origin check alone only stops a browser, not a program.
    #[tokio::test]
    async fn a_configured_management_token_is_required() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap()
        // The token belongs to the instance, not to the process. Setting
        // `GALLERY_MANAGEMENT_TOKEN` here instead would make every other
        // mutating management request in this test binary answer `403` for as
        // long as this test runs, because the environment is process-wide and
        // `cargo test` runs tests on parallel threads.
        .with_management_token(Some("s3cret-token"));
        let app = router(state);
        let host = "127.0.0.1:8787";

        // A mutating management route with no process-wide side effect. The
        // sync and check routes claim the one global sync slot, and a test that
        // starts a sync makes `status.running` read `true` for every other test
        // running in this binary at the same time.
        let uri = "/api/pawchive/selections/preview";

        let without = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, without).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(body["error"], "management token is required for this route");

        let wrong = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("host", host)
            .header("x-gallery-management-token", "not-the-token")
            .body(Body::empty())
            .unwrap();
        assert_eq!(json_response(&app, wrong).await.0, StatusCode::FORBIDDEN);

        let right = Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("host", host)
            .header("x-gallery-management-token", "s3cret-token")
            .body(Body::empty())
            .unwrap();
        let (status, body) = json_response(&app, right).await;
        assert_ne!(status, StatusCode::FORBIDDEN, "{body}");
    }

    #[tokio::test]
    async fn archive_undo_is_write_gated() {
        let dir = tempfile::tempdir().unwrap();
        let db_path = dir.path().join("gallery.db");
        let writable = AppState::new(
            db_path.clone(),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        drop(writable);
        let state = AppState::new(
            db_path,
            DbConfig {
                read_only: true,
                pool_size: 1,
            },
            Capabilities {
                read_only: true,
                writes: false,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let (status, body) = json_response(
            &router(state),
            Request::builder()
                .method(Method::POST)
                .uri("/api/folder-renames/plans/1/undo")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body["error"], "write capability is disabled");
    }

    #[tokio::test]
    async fn archive_undo_returns_machine_readable_missing_plan() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        let (status, body) = json_response(
            &router(state),
            Request::builder()
                .method(Method::POST)
                .uri("/api/folder-renames/plans/999/undo")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        assert_eq!(body["reason"], "plan_not_found");
    }

    #[tokio::test]
    async fn api_netdisk_settings_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state);

        // 1. Initial settings are defaults
        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/netdisk/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], false);
        assert_eq!(body["check_interval_secs"], 10);
        assert_eq!(body["reserve_space_gib"], 10);

        // 2. Update settings
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::PUT)
                .uri("/api/netdisk/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "enabled": true,
                        "staging_dir": "D:/Downloads",
                        "check_interval_secs": 30,
                        "reserve_space_gib": 20,
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["saved"], true);

        // 3. Read back verified
        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/netdisk/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["enabled"], true);
        assert_eq!(body["staging_dir"], "D:/Downloads");
        assert_eq!(body["check_interval_secs"], 30);
        assert_eq!(body["reserve_space_gib"], 20);
    }

    #[tokio::test]
    async fn api_netdisk_token_rotate_and_script() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state);

        // 1. Rotate token
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/token/rotate")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let token = body["token"].as_str().unwrap().to_string();
        assert_eq!(token.len(), 64);

        // 2. Request script with existing token
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/script")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "token": token,
                        "base_url": "http://127.0.0.1:8899",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["token"], token);
        let script = body["script"].as_str().unwrap();
        assert!(script.contains("JDownloader Event Scripter"));
        assert!(script.contains(&token));
        assert!(script.contains("http://127.0.0.1:8899"));
        // Opening the script repeatedly (including after a page reload) must
        // reuse the configured key, never rotate as a side effect.
        for _ in 0..2 {
            let (status, body) = json_response(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/netdisk/script")
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::OK);
            assert_eq!(body["token"], token);
        }
        let (status, _) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/script")
                .header("content-type", "application/json")
                .body(Body::from(r#"{"token":"wrong"}"#))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED);
    }

    #[tokio::test]
    async fn api_netdisk_bridge_exchange_loopback_and_auth() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let app = router(state.clone());

        // Rotate token to have a valid pairing token
        let token = {
            let conn = state.pool.get().unwrap();
            rotate_bridge_token(&conn).unwrap()
        };

        let exchange_req = |peer: SocketAddr, body: String, content_type: &str| {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/bridge/exchange")
                .header("content-type", content_type)
                .body(Body::from(body))
                .unwrap();
            req.extensions_mut().insert(ConnectInfo(peer));
            req
        };

        // 1. Non-loopback peer is rejected with 403 Forbidden
        let remote_peer = SocketAddr::from(([203, 0, 113, 9], 54321));
        let (status, body) = json_response(
            &app,
            exchange_req(
                remote_peer,
                json!({
                    "token": token,
                    "payload": {
                        "bridge_id": "test",
                        "statuses": []
                    }
                })
                .to_string(),
                "application/json",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");

        // 2. Loopback peer with wrong token is rejected with 401 Unauthorized
        let local_peer = SocketAddr::from(([127, 0, 0, 1], 54321));
        let (status, body) = json_response(
            &app,
            exchange_req(
                local_peer,
                json!({
                    "token": "invalid_token_12345",
                    "payload": {
                        "bridge_id": "test",
                        "statuses": []
                    }
                })
                .to_string(),
                "application/json",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");

        // 3. Loopback peer with correct token (JSON body) returns 200 OK
        let (status, body) = json_response(
            &app,
            exchange_req(
                local_peer,
                json!({
                    "token": token,
                    "payload": {
                        "bridge_id": "jd-agent",
                        "session_id": "sess-route",
                        "seq": 1,
                        "pages": []
                    }
                })
                .to_string(),
                "application/json",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["settled_receipts"], 0);

        // 4. Form urlencoded format (as posted by Event Scripter postPage)
        let payload_str = json!({
            "bridge_id": "jd-agent",
            "session_id": "sess-route",
            "seq": 2,
            "pages": []
        })
        .to_string();
        let form_body = url::form_urlencoded::Serializer::new(String::new())
            .append_pair("token", &token)
            .append_pair("payload", &payload_str)
            .finish();
        let (status, body) = json_response(
            &app,
            exchange_req(local_peer, form_body, "application/x-www-form-urlencoded"),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["ok"], true);

        // 5. A sequence number reused for different content is a conflict, not
        //    a second execution of the same request.
        let (status, body) = json_response(
            &app,
            exchange_req(
                local_peer,
                json!({
                    "token": token,
                    "payload": {
                        "bridge_id": "jd-agent",
                        "session_id": "sess-route",
                        "seq": 2,
                        "ack": 99,
                        "pages": []
                    }
                })
                .to_string(),
                "application/json",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // 6. A report with no session identity is malformed input.
        let (status, body) = json_response(
            &app,
            exchange_req(
                local_peer,
                json!({
                    "token": token,
                    "payload": {"bridge_id": "jd-agent", "seq": 9, "pages": []}
                })
                .to_string(),
                "application/json",
            ),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    #[tokio::test]
    async fn api_netdisk_job_routes_report_their_real_state() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);

        // The list is readable before anything is registered.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/netdisk/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["jobs"].as_array().map(|v| v.len()), Some(0));

        // Registering a task while 网盘下载 is off is refused rather than
        // silently accepted and never reported on.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(json!({"post_id": 1}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // A control command for a task that does not exist is a 404, not a
        // queued command nobody will ever answer.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs/job_missing/pause")
                .header("content-type", "application/json")
                .body(Body::from(json!({"link_ids": []}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // An action outside the protocol is refused before any queueing.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs/job_missing/reboot")
                .header("content-type", "application/json")
                .body(Body::from(json!({"link_ids": []}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
    }

    /// The 网盘下载 switches: 自动开始 defaults on, 下载后入库 defaults off and
    /// cannot be turned on before the bridge has proved what it can do.
    #[tokio::test]
    async fn netdisk_switches_default_and_auto_import_needs_a_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state);

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/netdisk/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["auto_start"], true, "{body}");
        assert_eq!(body["auto_import"], false, "{body}");
        assert_eq!(body["cleanup_after_import"], false, "{body}");
        assert_eq!(body["import_dir"], "", "{body}");

        // Without a handshake the import switch is refused rather than stored.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::PUT)
                .uri("/api/netdisk/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "enabled": true,
                        "auto_start": true,
                        "auto_import": true,
                        "staging_dir": "/vol1/downloads/staging",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("能力握手"),
            "{body}"
        );

        // Without a handshake cleanup_after_import is also refused.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::PUT)
                .uri("/api/netdisk/settings")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "enabled": true,
                        "auto_start": true,
                        "auto_import": false,
                        "cleanup_after_import": true,
                        "staging_dir": "/vol1/downloads/staging",
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("能力握手"),
            "{body}"
        );

        let (status, body) = json_response(
            &app,
            Request::builder()
                .uri("/api/netdisk/settings")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(
            body["auto_import"], false,
            "the refused save stored nothing"
        );
        assert_eq!(
            body["cleanup_after_import"], false,
            "cleanup_after_import remained false"
        );
    }

    /// 连接/断开连接: the state is reported honestly, and a disconnect really
    /// stops the exchange instead of only changing a label.
    #[tokio::test]
    async fn netdisk_connect_and_disconnect_gate_the_exchange() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state.clone());

        // Settings actions work from the trusted LAN UI; only the bridge
        // exchange itself must come from loopback.
        let loopback = |uri: &str| {
            let mut req = Request::builder()
                .method(Method::POST)
                .uri(uri)
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([192, 168, 1, 10], 1234))));
            req
        };

        // Connecting before a pairing token exists is refused, not reported as
        // connected.
        let (status, body) = json_response(&app, loopback("/api/netdisk/connect")).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        let token = {
            let conn = state.pool.get().unwrap();
            rotate_bridge_token(&conn).unwrap()
        };

        let (status, body) = json_response(&app, loopback("/api/netdisk/connect")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["connected"], false, "{body}");
        assert_eq!(body["state"], "连接中", "{body}");
        assert_eq!(
            body["handshaked"], false,
            "no script has reported in yet: {body}"
        );

        let (status, body) = json_response(&app, loopback("/api/netdisk/disconnect")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["connected"], false, "{body}");
        assert_eq!(body["state"], "已断开", "{body}");

        // The exchange is refused while disconnected, so a disconnected install
        // cannot keep reporting progress it is not tracking.
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/netdisk/bridge/exchange")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "token": token,
                    "payload": {"bridge_id": "jd-local", "session_id": "s", "seq": 1, "pages": []}
                })
                .to_string(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        let (status, body) = json_response(&app, req).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Reconnecting restores it.
        let (status, body) = json_response(&app, loopback("/api/netdisk/connect")).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["connected"], false, "{body}");
        assert_eq!(body["disconnected"], false, "{body}");
        let payload = BridgeExchangePayload {
            bridge_id: "jd-local".into(),
            session_id: "fresh-session".into(),
            seq: 1,
            capabilities: Some(BridgeCapabilities {
                protocol_version: NETDISK_PROTOCOL_VERSION.into(),
                supports_commands: true,
                supports_pagination: true,
                supports_download_path: true,
                supports_snapshot: true,
                linkgrabber_auto_start_enabled: Some(false),
                ..Default::default()
            }),
            ..Default::default()
        };
        {
            let conn = state.pool.get().unwrap();
            process_bridge_exchange(&conn, &payload, &state.roots).unwrap();
        }
        let (_, body) = json_response(&app, loopback("/api/netdisk/test")).await;
        assert_eq!(body["connected"], true, "{body}");
        assert_eq!(body["state"], "已连接", "{body}");
        assert_eq!(body["auto_start_enabled"], false, "{body}");
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "UPDATE netdisk_bridge_sessions SET updated_at = updated_at - 90",
                [],
            )
            .unwrap();
        }
        let (_, body) = json_response(&app, loopback("/api/netdisk/test")).await;
        assert_eq!(body["connected"], false, "{body}");
        assert_eq!(body["state"], "设备离线", "{body}");
        assert_eq!(body["handshaked"], false, "{body}");
        {
            let conn = state.pool.get().unwrap();
            rotate_bridge_token(&conn).unwrap();
        }
        let (_, body) = json_response(&app, loopback("/api/netdisk/test")).await;
        assert_eq!(body["state"], "连接中", "{body}");
        assert_eq!(body["handshaked"], false, "{body}");
    }

    /// 检查目录 answers per field: the staging directory must sit outside the
    /// media roots and be writable, the import directory must be an existing
    /// directory inside an authorized root, and neither check writes to media.
    #[test]
    fn netdisk_path_check_separates_the_two_directories() {
        let media = tempfile::tempdir().unwrap();
        let scratch = tempfile::tempdir().unwrap();
        let roots = MediaRoots::identical(
            vec![media.path().to_string_lossy().to_string()],
            vec!["Media".to_string()],
        );

        // A staging directory outside the media roots is accepted and the probe
        // file is removed again.
        let staging = scratch.path().join("staging");
        let verdict = check_staging_dir(&staging.to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(true), "{verdict}");
        assert!(staging.is_dir());
        assert_eq!(
            std::fs::read_dir(&staging).unwrap().count(),
            0,
            "the probe file does not survive the check"
        );

        // A staging directory inside a media root is refused: the scanner would
        // index half-downloaded files.
        let inside = media.path().join("staging");
        let verdict = check_staging_dir(&inside.to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(false), "{verdict}");
        assert!(
            verdict["reason"]
                .as_str()
                .unwrap_or_default()
                .contains("媒体扫描根"),
            "{verdict}"
        );
        assert!(!inside.exists(), "a refused check creates nothing");

        // A dot-directory inside a media root is accepted: the scanner ignores dot directories
        let dot_inside = media.path().join(".staging");
        let verdict = check_staging_dir(&dot_inside.to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(true), "{verdict}");
        assert!(dot_inside.is_dir());

        // Empty staging directory auto-resolves to .staging under the media root
        let verdict = check_staging_dir("", &roots);
        assert_eq!(verdict["ok"], json!(true), "{verdict}");

        // An empty import directory means "use the task's own target".
        let verdict = check_import_dir("", &roots);
        assert_eq!(verdict["ok"], json!(true), "{verdict}");

        // An existing directory inside the root is accepted; one outside is not.
        let import = media.path().join("artist");
        std::fs::create_dir_all(&import).unwrap();
        let verdict = check_import_dir(&import.to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(true), "{verdict}");
        let verdict = check_import_dir(&scratch.path().to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(false), "{verdict}");
        let verdict = check_import_dir(&media.path().join("nope").to_string_lossy(), &roots);
        assert_eq!(verdict["ok"], json!(false), "{verdict}");
    }

    /// A task freezes its share links at registration, and 开始 submits exactly
    /// those. With 自动开始 off, registration must not submit anything on its own,
    /// and the explicit action must carry the frozen links rather than re-reading
    /// the artist's current link list.
    #[tokio::test]
    async fn netdisk_job_start_submits_the_frozen_links() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let token = {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "INSERT INTO kemono_subscriptions
                     (service, user_id, target_dir, enabled, created_at, updated_at)
                 VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at, external_links)
                 VALUES (1, '900', 'pending', 't', 't', '[\"https://provider.example/share/one\"]')",
                [],
            )
            .unwrap();
            let token = rotate_bridge_token(&conn).unwrap();
            // 自动开始 off: the task must stay registered until 开始 is pressed.
            save_netdisk_settings(
                &conn,
                &NetdiskSettings {
                    enabled: true,
                    auto_start: false,
                    staging_dir: "/vol1/downloads/staging".to_string(),
                    ..Default::default()
                },
            )
            .unwrap();
            token
        };
        let app = router(state.clone());

        // Pair the bridge so a session exists for the task to bind to.
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/netdisk/bridge/exchange")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "token": token,
                    "payload": {
                        "bridge_id": "jd-local",
                        "session_id": "sess-start",
                        "seq": 1,
                        "capabilities": {
                            "script_version": "2.0",
                            "protocol_version": NETDISK_PROTOCOL_VERSION,
                            "supports_commands": true,
                            "supports_pagination": true,
                            "supports_download_path": true,
                            "supports_snapshot": true,
                            "max_page_size": 100,
                            "linkgrabber_auto_start_enabled": false
                        },
                        "pages": []
                    }
                })
                .to_string(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        let (status, body) = json_response(&app, req).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // A task for a post the ledger does not know cannot be registered.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"post_id": 4242, "links": ["https://provider.example/x"]}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // A real task registers with its links frozen and submits nothing.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "post_id": 1,
                        "links": ["https://provider.example/share/one"]
                    })
                    .to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["submitted"], false, "{body}");
        assert_eq!(
            body["links"],
            json!(["https://provider.example/share/one"]),
            "the task echoes the links it froze: {body}"
        );
        let task_id = body["task_id"].as_str().unwrap().to_string();

        {
            let conn = state.pool.get().unwrap();
            let queued: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM netdisk_bridge_commands WHERE job_id = ?1",
                    rusqlite::params![task_id],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(queued, 0, "自动开始 off means nothing is submitted yet");
        }

        // 开始 submits the frozen links.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/netdisk/jobs/{task_id}/start"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert_eq!(body["state"], BRIDGE_TASK_SUBMITTED, "{body}");

        {
            let conn = state.pool.get().unwrap();
            let (action, params): (String, String) = conn
                .query_row(
                    "SELECT action, params FROM netdisk_bridge_commands WHERE job_id = ?1",
                    rusqlite::params![task_id],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            assert_eq!(action, "add_links");
            let params: Value = serde_json::from_str(&params).unwrap();
            assert_eq!(
                params["links"],
                json!("https://provider.example/share/one"),
                "the command carries the frozen link, not the artist's current list"
            );
            assert_eq!(
                params["destinationFolder"],
                json!(format!("/vol1/downloads/staging/gallery-{task_id}")),
                "{params}"
            );
            assert_eq!(params["autostart"], json!(false), "{params}");
            assert_eq!(params["autoExtract"], json!(false), "{params}");
            assert_eq!(params["overwritePackagizerRules"], json!(true), "{params}");
        }

        // The list is what the settings panel renders, so it has to carry the
        // plan's wording and not only the protocol state.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/netdisk/jobs")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let jobs = body["jobs"].as_array().unwrap();
        assert_eq!(jobs.len(), 1, "{body}");
        assert_eq!(jobs[0]["state"], BRIDGE_TASK_SUBMITTED, "{body}");
        assert_eq!(jobs[0]["state_label"], json!("待核对"), "{body}");
        assert_eq!(jobs[0]["link_count"], json!(1), "{body}");

        // 开始 twice is refused instead of queueing a second addLinks.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/netdisk/jobs/{task_id}/start"))
                .header("content-type", "application/json")
                .body(Body::from("{}"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Duplicate post without force is refused with 409 Conflict
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"post_id": 1, "force": false}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(body["error"].as_str().unwrap().contains("进行中"));

        // Invalid scheme is refused with 400 Bad Request
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"post_id": 1, "links": ["file:///secret"], "force": true}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        // Empty links with force automatically pulls kemono_posts.external_links
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(json!({"post_id": 1, "force": true}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["links"], json!(["https://provider.example/share/one"]));

        // Batch dispatch with post_ids returns job_ids and status
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"post_ids": [1], "force": true}).to_string(),
                ))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["status"], "queued");
        assert!(!body["job_ids"].as_array().unwrap().is_empty());
    }

    /// §6.6: moving links into JD's download list consults JD's global
    /// auto-start setting, so 开始 is refused unless the installed script has
    /// reported that setting as off. "Could not read" is refused too — it is not
    /// the same answer as "off", and only one of them is safe.
    #[tokio::test]
    async fn netdisk_start_refuses_until_jd_global_autostart_is_verified_off() {
        // Each case gets its own install. The stored handshake is per session
        // and the lookup orders on a second-resolution timestamp, so two
        // sessions written in the same second are not distinguishable.
        async fn start_with(reported: Option<bool>) -> (StatusCode, Value) {
            let dir = tempfile::tempdir().unwrap();
            let state = AppState::new(
                dir.path().join("gallery.db"),
                DbConfig {
                    read_only: false,
                    pool_size: 1,
                },
                Capabilities {
                    read_only: false,
                    writes: true,
                    media: true,
                    ml: false,
                },
            )
            .unwrap();
            let token = {
                let conn = state.pool.get().unwrap();
                conn.execute(
                    "INSERT INTO kemono_subscriptions
                         (service, user_id, target_dir, enabled, created_at, updated_at)
                     VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                    [],
                )
                .unwrap();
                conn.execute(
                    "INSERT INTO kemono_posts
                         (subscription_id, post_id, status, created_at, updated_at, external_links)
                     VALUES (1, '900', 'pending', 't', 't', '[\"https://provider.example/share/one\"]')",
                    [],
                )
                .unwrap();
                let token = rotate_bridge_token(&conn).unwrap();
                save_netdisk_settings(
                    &conn,
                    &NetdiskSettings {
                        enabled: true,
                        auto_start: false,
                        staging_dir: "/vol1/downloads/staging".to_string(),
                        ..Default::default()
                    },
                )
                .unwrap();
                token
            };
            let app = router(state.clone());

            let mut capabilities = json!({
                "script_version": "2.0",
                "protocol_version": NETDISK_PROTOCOL_VERSION,
                "supports_commands": true,
                "supports_pagination": true,
                "supports_download_path": true,
                "supports_snapshot": true,
                "max_page_size": 100
            });
            if let Some(value) = reported {
                capabilities["linkgrabber_auto_start_enabled"] = json!(value);
            }
            let mut req = Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/bridge/exchange")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "token": token,
                        "payload": {
                            "bridge_id": "jd-local",
                            "session_id": "sess-autostart",
                            "seq": 1,
                            "capabilities": capabilities,
                            "pages": []
                        }
                    })
                    .to_string(),
                ))
                .unwrap();
            req.extensions_mut()
                .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
            let (status, body) = json_response(&app, req).await;
            assert_eq!(status, StatusCode::OK, "{body}");

            let (status, body) = json_response(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri("/api/netdisk/jobs")
                    .header("content-type", "application/json")
                    .body(Body::from(
                        json!({
                            "post_id": 1,
                            "links": ["https://provider.example/share/one"]
                        })
                        .to_string(),
                    ))
                    .unwrap(),
            )
            .await;
            assert_eq!(status, StatusCode::CREATED, "{body}");
            let task_id = body["task_id"].as_str().unwrap().to_string();

            json_response(
                &app,
                Request::builder()
                    .method(Method::POST)
                    .uri(format!("/api/netdisk/jobs/{task_id}/start"))
                    .header("content-type", "application/json")
                    .body(Body::from("{}"))
                    .unwrap(),
            )
            .await
        }

        // On: the move would start whatever else the user has queued.
        let (status, body) = start_with(Some(true)).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("全局自动启动"),
            "the refusal says what to change in JD: {body}"
        );

        // Unknown: it cannot be shown to be isolated, so it is not.
        let (status, body) = start_with(None).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");

        // Verified off: the move is isolated and 开始 goes through.
        let (status, body) = start_with(Some(false)).await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    }

    /// 入库 is refused for a task that has not settled, with the state named.
    #[tokio::test]
    async fn netdisk_job_import_requires_a_settled_task() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state.clone());

        {
            let conn = state.pool.get().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
                 CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
            gallery_accel::ensure_netdisk_bridge_schema(&conn).unwrap();
            conn.execute(
                "INSERT INTO netdisk_bridge_tasks
                     (task_id, bridge_id, session_id, work_id, post_id, manifest_version,
                      expected, links, state, created_at, updated_at)
                 VALUES ('job_reg', 'jd-local', 'sess', 'w', 1, 1, '[]', '[]', 'issued', 0, 0)",
                [],
            )
            .unwrap();
        }

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs/job_reg/import")
                .header("content-type", "application/json")
                .body(Body::from(json!({"link_ids": []}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(body["state"], "issued", "{body}");

        // 开始 without links is refused rather than queued as an empty command.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/netdisk/jobs/job_reg/start")
                .header("content-type", "application/json")
                .body(Body::from(json!({"link_ids": []}).to_string()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert!(
            body["error"]
                .as_str()
                .unwrap_or_default()
                .contains("分享链接"),
            "{body}"
        );
    }

    /// The decoded bridge payload has its own ceiling, separate from the body
    /// limit that has to allow for form-encoding inflation.
    #[tokio::test]
    async fn netdisk_bridge_rejects_an_oversized_decoded_payload() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state.clone());
        let token = {
            let conn = state.pool.get().unwrap();
            rotate_bridge_token(&conn).unwrap()
        };

        let filler = "x".repeat(NETDISK_BRIDGE_PAYLOAD_MAX_BYTES + 1024);
        let mut req = Request::builder()
            .method(Method::POST)
            .uri("/api/netdisk/bridge/exchange")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "token": token,
                    "payload": {
                        "bridge_id": "jd-local",
                        "session_id": "sess-big",
                        "seq": 1,
                        "pages": [{
                            "job_id": "job",
                            "snapshot_id": "s",
                            "records": [{"link_id": filler, "name": "n"}]
                        }]
                    }
                })
                .to_string(),
            ))
            .unwrap();
        req.extensions_mut()
            .insert(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 1234))));
        let (status, body) = json_response(&app, req).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE, "{body}");
    }

    /// 单文件重试 and 手动入库: the two explicit per-work actions the plan lists
    /// as still missing. Retry goes through the same selection → attempt path as
    /// a manual request, so the new delivery gets its own identity and a
    /// non-overwriting target; import runs the linking pass and answers with
    /// this post's own count.
    #[tokio::test]
    async fn pawchive_file_retry_and_manual_import_are_per_work_actions() {
        let dir = tempfile::tempdir().unwrap();
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();
        let app = router(state.clone());

        let (post_id, file_id, other_post) = {
            let conn = state.pool.get().unwrap();
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS artists (id INTEGER PRIMARY KEY, name TEXT, path TEXT);
                 CREATE TABLE IF NOT EXISTS items (id INTEGER PRIMARY KEY);",
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_subscriptions
                     (service, user_id, target_dir, enabled, created_at, updated_at)
                 VALUES ('fanbox', '27212726', '/pictures1/artist', 1, 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at)
                 VALUES (1, '900', 'pending', 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_posts
                     (subscription_id, post_id, status, created_at, updated_at)
                 VALUES (1, '901', 'pending', 't', 't')",
                [],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO kemono_files
                     (post_id, source_identity, remote_path, file_name, file_type,
                      status, created_at, updated_at)
                 VALUES (1, 'a', '/a.png', 'a.png', 'image', 'failed', 't', 't')",
                [],
            )
            .unwrap();
            let file_id: i64 = conn
                .query_row("SELECT id FROM kemono_files WHERE post_id = 1", [], |row| {
                    row.get(0)
                })
                .unwrap();
            (1i64, file_id, 2i64)
        };

        // A file that belongs to another post is not this post's to retry.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/pawchive/posts/{other_post}/files/{file_id}/retry"
                ))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // A row a live pass holds is refused rather than pulled out from under it.
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "UPDATE kemono_files SET status = 'downloading', claimed_by = 'worker-1' WHERE id = ?1",
                rusqlite::params![file_id],
            )
            .unwrap();
        }
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/pawchive/posts/{post_id}/files/{file_id}/retry"
                ))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        {
            let conn = state.pool.get().unwrap();
            conn.execute(
                "UPDATE kemono_files SET status = 'failed', claimed_by = '' WHERE id = ?1",
                rusqlite::params![file_id],
            )
            .unwrap();
        }

        // An unclaimed resource is retried as its own attempt.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!(
                    "/api/pawchive/posts/{post_id}/files/{file_id}/retry"
                ))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::ACCEPTED, "{body}");
        assert!(
            body["attempt_id"].as_str().is_some_and(|id| !id.is_empty()),
            "{body}"
        );

        // 入库 on a work the ledger does not hold is a 404, not an import of
        // nothing.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/pawchive/posts/4242/import")
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // A real work reports its own count.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri(format!("/api/pawchive/posts/{post_id}/import"))
                .header("content-type", "application/json")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["post_id"], json!(post_id), "{body}");
        assert_eq!(body["total"], json!(1), "{body}");
        assert_eq!(body["linked"], json!(0), "{body}");

        // The read the panel needs before it can offer 单文件重试: it has to be
        // able to name the file, and `has_evidence` is the fact that says
        // whether a retry is owed at all.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/pawchive/posts/{post_id}/files"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let files = body["files"].as_array().unwrap();
        assert_eq!(files.len(), 1, "{body}");
        assert_eq!(files[0]["file_id"], json!(file_id), "{body}");
        assert_eq!(files[0]["has_evidence"], json!(false), "{body}");
        assert_eq!(
            files[0]["status"],
            json!("failed"),
            "the stored status is reported, not a guess: {body}"
        );

        // Another work's files stay out of it.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri(format!("/api/pawchive/posts/{other_post}/files"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["files"].as_array().unwrap().len(), 0, "{body}");
    }

    /// A character exists so the upload's ownership check can pass; the caller
    /// sets `DATA_DIR` and the fake-embedding flag before calling this.
    fn character_upload_state(dir: &tempfile::TempDir) -> AppState {
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: true,
            },
        )
        .unwrap();
        state
            .pool
            .get()
            .unwrap()
            .execute_batch("INSERT INTO characters (id, name) VALUES (7, 'miku');")
            .unwrap();
        state
    }

    /// Magic bytes plus filler: the upload sniffs the type and, under the fake
    /// embedding flag, never decodes the pixels.
    fn png_upload_body() -> Vec<u8> {
        let mut bytes = vec![0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A];
        bytes.extend_from_slice(b"reference-pixels");
        bytes
    }

    #[tokio::test]
    async fn reference_upload_rejects_non_images_empty_bodies_and_unknown_characters() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let _fake = crate::test_support::EnvVar::set("CHARACTER_IMPORT_FAKE_EMBEDDING", "1");
        let app = router(character_upload_state(&dir));

        // A script wearing an image content-type is still not an image.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/characters/7/references/upload")
                .header("content-type", "image/jpeg")
                .body(Body::from("<?php echo 1; ?>"))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");
        assert_eq!(body["error"], "unsupported image type");

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/characters/7/references/upload")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{body}");

        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/characters/999/references/upload")
                .body(Body::from(png_upload_body()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");
        assert_eq!(body["error"], "character not found");

        // A rejected upload must not leave a file behind.
        let uploads = gallery_accel::product_ui::character_references_dir();
        assert!(
            !uploads.join("7").exists(),
            "nothing stored for character 7"
        );
        assert!(
            !uploads.join("999").exists(),
            "nothing stored for character 999"
        );
    }

    #[tokio::test]
    async fn reference_upload_stores_a_manual_reference_and_serves_then_deletes_its_image() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let _data_dir = crate::test_support::EnvVar::set("DATA_DIR", dir.path().join("data"));
        let _fake = crate::test_support::EnvVar::set("CHARACTER_IMPORT_FAKE_EMBEDDING", "1");
        let state = character_upload_state(&dir);
        let app = router(state.clone());

        let image = png_upload_body();
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::POST)
                .uri("/api/characters/7/references/upload")
                .header("content-type", "application/octet-stream")
                .body(Body::from(image.clone()))
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reference_id = body["reference_id"].as_i64().unwrap();

        // The row is a manual reference with its own stored image, not an item.
        let conn = state.pool.get().unwrap();
        let (source_type, item_id, image_path): (String, Option<i64>, Option<String>) = conn
            .query_row(
                "SELECT source_type, item_id, image_path FROM character_references WHERE id=?",
                [reference_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .unwrap();
        assert_eq!(source_type, "manual");
        assert_eq!(item_id, None);
        let stored = PathBuf::from(image_path.expect("uploaded file path"));
        assert!(
            stored.is_file(),
            "uploaded file exists at {}",
            stored.display()
        );
        drop(conn);

        // The list response tells the UI to preview it by reference id.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri("/api/characters/7/references")
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let reference = &body["references"][0];
        assert_eq!(reference["id"], reference_id);
        assert_eq!(reference["source_type"], "manual");
        assert_eq!(reference["has_image"], true);
        assert_eq!(reference["item_id"], Value::Null);

        // The stored bytes come back untouched.
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method(Method::GET)
                    .uri(format!("/api/characters/7/references/{reference_id}/image"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get("content-type")
                .and_then(|value| value.to_str().ok()),
            Some("image/png")
        );
        let served = response.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(served.as_ref(), image.as_slice());

        // A tag_single reference has no uploaded image, so there is nothing to
        // serve even though the reference exists.
        let conn = state.pool.get().unwrap();
        conn.execute(
            "INSERT INTO character_references
             (character_id, embedding, embedding_dim, source_type, item_id, created_at)
             VALUES (7, x'00', 1, 'tag_single', NULL, 0)",
            [],
        )
        .unwrap();
        let tag_reference_id = conn.last_insert_rowid();
        drop(conn);
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::GET)
                .uri(format!(
                    "/api/characters/7/references/{tag_reference_id}/image"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{body}");

        // Deleting the reference removes the row and the uploaded file.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/characters/7/references/{reference_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], 1);
        assert!(!stored.exists(), "uploaded file removed with the reference");

        // A second delete is a no-op rather than an error.
        let (status, body) = json_response(
            &app,
            Request::builder()
                .method(Method::DELETE)
                .uri(format!("/api/characters/7/references/{reference_id}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["deleted"], 0);
    }

    #[tokio::test]
    async fn archive_endpoints_inspect_stream_and_extract() {
        let _env_lock = crate::test_support::ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        let pictures = dir.path().join("pictures").join("Artist");
        std::fs::create_dir_all(&pictures).unwrap();

        // Create sample zip in pictures/Artist/pack.zip using 7z
        let bin = gallery_accel::archive_ops::find_7z_binary().unwrap();
        let zip_path = pictures.join("pack.zip");
        let sample_file = dir.path().join("inner.png");
        std::fs::write(&sample_file, b"sample png data").unwrap();
        let mut cmd = std::process::Command::new(&bin);
        cmd.arg("a").arg("-tzip").arg(&zip_path).arg(&sample_file);
        assert!(cmd.status().unwrap().success());

        let _root = crate::test_support::EnvVar::set("PICTURES_ROOT", dir.path().join("pictures"));
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let conn = state.pool.get().unwrap();
        conn.execute(
            "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', ?1)",
            [pictures.to_string_lossy().as_ref()],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO items (id, artist_id, file_path, file_name, folder_name, is_archive, media_type)
             VALUES (101, 1, ?1, 'pack.zip', 'Artist', 1, 'archive')",
            [zip_path.to_string_lossy().as_ref()],
        )
        .unwrap();
        drop(conn);

        let app = router(state);

        // 1. Inspect
        let inspect_req = Request::builder()
            .method(Method::POST)
            .uri("/api/archives/inspect")
            .header("content-type", "application/json")
            .body(Body::from(json!({"item_id": 101}).to_string()))
            .unwrap();
        let res = app.clone().oneshot(inspect_req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["archive_name"], "pack.zip");
        assert_eq!(body["stats"]["total_files"], 1);
        assert_eq!(body["stats"]["images"], 1);

        // 2. Stream entry
        let stream_req = Request::builder()
            .method(Method::GET)
            .uri("/api/archives/entry?item_id=101&entry=inner.png")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(stream_req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let bytes = res.into_body().collect().await.unwrap().to_bytes();
        assert_eq!(bytes.as_ref(), b"sample png data");

        // 3. Extract
        let extract_req = Request::builder()
            .method(Method::POST)
            .uri("/api/archives/extract")
            .header("content-type", "application/json")
            .body(Body::from(json!({
                "item_id": 101,
                "target_mode": "new_folder",
                "custom_folder_name": "extracted_set",
                "recycle_source": false
            }).to_string()))
            .unwrap();
        let res = app.clone().oneshot(extract_req).await.unwrap();
        let status = res.status();
        let body_bytes = res.into_body().collect().await.unwrap().to_bytes();
        let body: Value = serde_json::from_slice(&body_bytes).unwrap();
        assert_eq!(status, StatusCode::OK, "extract failed with body: {body:?}");
        assert_eq!(body["ok"], true);
        assert_eq!(body["extracted_count"], 1);
        assert!(pictures.join("extracted_set").join("inner.png").is_file());
    }

    #[tokio::test]
    async fn recycle_routes_purge_and_clear() {
        let dir = tempfile::tempdir().unwrap();
        let pictures = dir.path().join("pictures");
        std::fs::create_dir_all(&pictures).unwrap();
        let recycle_store = pictures.join(".Recycle_bin");
        std::fs::create_dir_all(&recycle_store).unwrap();
        let file1 = recycle_store.join("item1.jpg");
        let file2 = recycle_store.join("item2.jpg");
        std::fs::write(&file1, b"f1").unwrap();
        std::fs::write(&file2, b"f2").unwrap();

        let _root = crate::test_support::EnvVar::set("PICTURES_ROOT", &pictures);
        let state = AppState::new(
            dir.path().join("gallery.db"),
            DbConfig {
                read_only: false,
                pool_size: 1,
            },
            Capabilities {
                read_only: false,
                writes: true,
                media: true,
                ml: false,
            },
        )
        .unwrap();

        let conn = state.pool.get().unwrap();
        gallery_accel::ensure_recycle_schema(&conn).unwrap();
        let orig1 = pictures.join("item1.jpg").to_string_lossy().to_string();
        let orig2 = pictures.join("item2.jpg").to_string_lossy().to_string();
        conn.execute(
            "INSERT INTO recycle_entries (id, original_item_id, artist_id, original_path, recycled_path, item_snapshot, status)
             VALUES (1, 10, 1, ?1, ?2, '{}', 'recycled')",
            rusqlite::params![orig1, file1.to_string_lossy().to_string()],
        ).unwrap();
        conn.execute(
            "INSERT INTO recycle_entries (id, original_item_id, artist_id, original_path, recycled_path, item_snapshot, status)
             VALUES (2, 20, 1, ?1, ?2, '{}', 'recycled')",
            rusqlite::params![orig2, file2.to_string_lossy().to_string()],
        ).unwrap();
        drop(conn);

        let app = router(state);

        // 1. DELETE /api/recycle/1
        let req = Request::builder()
            .method(Method::DELETE)
            .uri("/api/recycle/1")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["ok"], true);
        assert!(!file1.exists());

        // 2. POST /api/recycle/clear
        let req = Request::builder()
            .method(Method::POST)
            .uri("/api/recycle/clear")
            .body(Body::empty())
            .unwrap();
        let res = app.clone().oneshot(req).await.unwrap();
        assert_eq!(res.status(), StatusCode::OK);
        let body: Value = serde_json::from_slice(&res.into_body().collect().await.unwrap().to_bytes()).unwrap();
        assert_eq!(body["ok"], true);
        assert_eq!(body["cleared_count"], 1);
        assert!(!file2.exists());
    }
}
