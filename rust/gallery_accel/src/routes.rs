use std::path::PathBuf;
use std::sync::Arc;

use crate::route_params::{
    ArtistReferenceScoreRequest, CandidateQuery, CharacterSummaryQuery, CharactersQuery,
    FoldersQuery, GroupQuery, HistoryQuery, ItemsQuery, OperationHistoryQuery, ReferenceQuery,
    TagSearchQuery, TagsQuery,
};
use axum::body::Body;
use axum::extract::ws::WebSocketUpgrade;
use axum::extract::{Path, Query, Request, State};
use axum::http::{header, Method, StatusCode};
use axum::middleware::{self, Next};
use axum::response::{IntoResponse, Response};
use axum::routing::{any, delete, get, post, put};
use axum::{Json, Router};
use bytes::Bytes;
use gallery_accel::upstream::{proxy_error, Upstream};
use gallery_accel::{
    apply_hash_unique_scan_candidate_response, apply_move_candidate_response,
    apply_scan_candidate_move_response, artist_detail_response, artist_recognition_status,
    artist_reference_scores_response, artist_references_response, artist_stats_response,
    artists_response, auto_resolve_move_candidates, cancel_character_import_job,
    character_model_signature, character_recognition_status, character_references_response,
    character_response, character_summary_response, characters_response, cluster_scores_response,
    confirm_artist_suggestion, content_hash_allowed, create_db_backup, create_new_item_response,
    create_tag, delete_character_reference, delete_tag,
    delete_to_recycle,
    duplicate_artists_response, env_media_roots, execute_folder_renames, folder_paths_response,
    folder_rename_auto_response, folder_rename_auto_run, folders_response,
    get_character_import_job, get_scan_state, hash_status_response, health_summary,
    ignore_move_candidate_response, item_detail_response, items_page_query_response,
    list_folder_renames, mark_move_candidate_new_response, merge_move_candidate_group,
    move_candidate_groups_response, move_candidates_response, move_history_response,
    operation_history_response, operation_log_response, preview_jpeg_allowed,
    propagate_hash_tags_response, rebuild_character_index, recheck_plan,
    recognize_character_native_topk, reconfirm_plan, resolve_existing_scan_candidate_response,
    resolve_scan_scope, run_full_library_scan, run_hash_batch, run_scan, serve_file_response,
    serve_text, serve_transcoded_hls, serve_transcoded_hls_segment, serve_video_compatible,
    serve_video_hls,
    set_folder_rename_auto, start_character_import_job, start_video_transcode,
    suggest_artists_native, tag_search_response, tags_response, unconfirm_plan,
    update_folder_tags_by_name_response, update_folder_tags_response,
    update_item_tags_by_name_response, update_item_tags_response, update_tag,
    upsert_folder_rename_plans, video_frame_jpeg, video_transcode_status, DbConfig, DbPool,
    MediaRoots, ScanControl, WorkerStatus, MAX_CLUSTER_SCORE_VECTORS,
};
use gallery_accel::product_ui::{read_log_tail as read_bounded_log_tail, recent_log_errors};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use tower_http::services::{ServeDir, ServeFile};
use std::time::{SystemTime, UNIX_EPOCH};

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
    workers: WorkerStatus,
    data_dir: PathBuf,
    ui_log_max_bytes: u64,
    ui_log_backups: usize,
}

impl AppState {
    pub fn new(
        db_path: PathBuf,
        config: DbConfig,
        capabilities: Capabilities,
    ) -> anyhow::Result<Self> {
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
        Ok(Self {
            pool: Arc::new(DbPool::with_config(db_path.clone(), config)?),
            roots: env_media_roots(),
            capabilities,
            db_path,
            upstream,
            primary,
            scan: Arc::new(ScanControl::new()),
            workers: WorkerStatus::default(),
            data_dir,
            ui_log_max_bytes: env_positive_u64("UI_LOG_MAX_BYTES", UI_LOG_MAX_BYTES),
            ui_log_backups: env_positive_u64("UI_LOG_BACKUP_COUNT", UI_LOG_BACKUP_COUNT as u64)
                .min(10) as usize,
        })
    }

    pub fn worker_inputs(&self) -> (Arc<DbPool>, MediaRoots, Arc<ScanControl>, WorkerStatus) {
        (
            Arc::clone(&self.pool),
            self.roots.clone(),
            Arc::clone(&self.scan),
            self.workers.clone(),
        )
    }
}

pub fn router(state: AppState) -> Router {
    let mut app = Router::new()
        .route("/api/health", get(api_health))
        .route("/api/capabilities", get(api_capabilities))
        .route("/api/content-hash", get(api_content_hash))
        .route("/api/image-preview", get(api_image_preview))
        .route("/api/file/preview", get(api_file_preview))
        .route("/api/hash/status", get(api_hash_status))
        .route("/api/move-candidates", get(api_move_candidates))
        .route(
            "/api/move-candidates/groups",
            get(api_move_candidate_groups),
        )
        .route("/api/move-history", get(api_move_history))
        .route("/api/operation-log", get(api_operation_log))
        .route("/api/operation-log/history", get(api_operation_history))
        .route(
            "/api/folder-renames/auto",
            get(api_folder_rename_auto),
        )
        .route(
            "/api/folder-renames/auto/run",
            post(api_folder_rename_auto_run),
        )
        .route("/api/artists/duplicates", get(api_duplicate_artists))
        .route("/api/artists", get(api_artists))
        .route("/api/artists/{artist_id}", get(api_artist_detail))
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
        .route("/api/items/tags", put(api_update_item_tags))
        .route("/api/items/tags-by-name", put(api_update_item_tags_by_name))
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
        .route("/api/folder-renames", put(api_folder_renames_put))
        .route(
            "/api/folder-renames/execute",
            post(api_folder_renames_execute),
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
        );

    // Optional debug upstream only when explicitly configured (not required for product).
    if state.primary && state.upstream.is_some() {
        app = app.fallback(any(api_upstream_fallback));
    }

    app.layer(middleware::from_fn_with_state(
        state.clone(),
        capability_gate,
    ))
    .with_state(state)
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
        || path.starts_with("/api/characters/import-from-tags")
        || path.contains("/artist-suggestions")
        || path.contains("/character-recognition")
}

fn is_nonmutating_post(path: &str) -> bool {
    path == "/api/cluster-scores"
        || path == "/api/artist-reference-scores"
        || path.contains("/artist-suggestions")
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

/// Attach classic static UI (`index.html` + assets) for primary mode.
///
/// UI loads `/static/style.css` and `/static/js/*.js` from the same directory
/// that holds `index.html` (mirrors FastAPI StaticFiles mount).
pub fn with_static_ui(router: Router, static_dir: PathBuf) -> Router {
    let index = static_dir.join("index.html");
    router
        .route_service("/", ServeFile::new(index))
        .nest_service("/static", ServeDir::new(static_dir))
}

async fn api_health(State(state): State<AppState>) -> Json<Value> {
    let conn = state.pool.get().ok();
    let mut body = health_summary(Some(state.db_path.as_path()), conn.as_ref().map(|c| &**c));
    let workers = state.workers.snapshot();
    body["workers"] = workers.clone();
    body["scan"] = match conn.as_ref().map(|c| &**c) {
        Some(conn) => get_scan_state(conn).unwrap_or_else(|error| {
            body["scan_error"] = json!(error.to_string());
            json!({"status": "error", "phase": error.to_string()})
        }),
        None => json!({"status": "unknown", "phase": "database connection unavailable"}),
    };
    body["scan_schedule"] = worker_schedule(
        &workers,
        "scan",
        env_positive_interval("SCAN_INTERVAL"),
        false,
        "next_auto_scan_at",
    );
    body["backups"] = backup_summary(&state.data_dir.join("db-backups"));
    // Count of authorized real media roots only — never dump untrusted host paths.
    let media_root_count = state.roots.real_paths.len().max(state.roots.roots.len());
    let accessible = (0..state.roots.roots.len())
        .filter(|&i| {
            state
                .roots
                .real_root_at(i)
                .map(|p| std::path::Path::new(p).is_dir())
                .unwrap_or(false)
        })
        .count();
    body["media_roots"] = json!({
        "count": media_root_count as i64,
        "accessible": accessible as i64,
    });
    body["logs"] = health_logs_summary(&state.data_dir);
    body["recent_errors"] = json!(recent_log_errors(&state.data_dir.join("logs"), 8));
    body["backup_schedule"] = worker_schedule(
        &workers,
        "backup",
        env_positive_interval("DB_BACKUP_INTERVAL"),
        env_flag("DB_BACKUP_ON_START"),
        "next_run_at",
    );
    // Native fields are complete in product mode. An optional upstream may only fill gaps.
    if let Some(upstream) = state.upstream.as_ref() {
        if let Ok(remote) = upstream.get_json("/api/health").await {
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
        Some(if overdue { 0.0 } else { (next_at - now).max(0.0) })
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
                "latest": Value::Null,
                "recent": [],
            });
        }
        Err(error) => {
            return json!({
                "root": root.display().to_string(),
                "count": Value::Null,
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
    let recent = backups.into_iter().take(5).collect::<Vec<_>>();
    json!({
        "root": root.display().to_string(),
        "count": count,
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
    content_hash_allowed(&query.path, &state.roots)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct ImagePreviewQuery {
    path: String,
    #[serde(default)]
    max_edge: Option<u32>,
}

async fn api_image_preview(
    State(state): State<AppState>,
    Query(query): Query<ImagePreviewQuery>,
) -> Result<Response, (StatusCode, Json<Value>)> {
    match preview_jpeg_allowed(&query.path, &state.roots, query.max_edge) {
        Ok(bytes) => Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response()),
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
}

async fn api_file_preview(
    State(state): State<AppState>,
    Query(query): Query<FilePreviewQuery>,
    request: Request,
) -> Result<Response, (StatusCode, Json<Value>)> {
    if state.capabilities.media || state.primary {
        match preview_jpeg_allowed(&query.path, &state.roots, query.max.or(query.max_edge)) {
            Ok(bytes) => return Ok(([(header::CONTENT_TYPE, "image/jpeg")], bytes).into_response()),
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
    let conn = state.pool.get().map_err(to_http_error)?;
    hash_status_response(&conn).map(Json).map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    let limit = q.get("limit").and_then(|v| v.parse().ok());
    let error_limit = q.get("error_limit").and_then(|v| v.parse().ok());
    operation_log_response(&conn, &state.roots, limit, error_limit)
        .map(Json)
        .map_err(to_http_error)
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
    folder_rename_auto_run(&conn, artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_artists(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    artists_response(&conn).map(Json).map_err(to_http_error)
}

async fn api_duplicate_artists(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    duplicate_artists_response(&conn, &state.roots)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    folders_response(&conn, query.artist_id)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    let media_type = if query.archive_only.unwrap_or(false) {
        Some("archive".to_string())
    } else {
        query.media_type.clone()
    };
    items_page_query_response(
        &conn,
        query.artist_id,
        query.limit,
        query.offset,
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
    )
    .map(Json)
    .map_err(to_http_error)
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
struct HashTagPropagationPayload {
    item_ids: Vec<i64>,
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
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    resolve_existing_scan_candidate_response(&conn, payload.candidate_id)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    apply_hash_unique_scan_candidate_response(&conn, payload.candidate_id)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    apply_scan_candidate_move_response(
        &conn,
        payload.candidate_id,
        payload.item_id,
        &payload.reason,
    )
    .map(Json)
    .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    create_new_item_response(&conn, payload.candidate_id)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    apply_move_candidate_response(&conn, payload.move_candidate_id)
        .map(Json)
        .map_err(to_http_error)
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
    .map_err(to_http_error)
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
        .map_err(to_http_error)
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
    .map_err(to_http_error)?
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
    let conn = state.pool.get().map_err(to_http_error)?;
    tag_search_response(&conn, query.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_characters(
    State(state): State<AppState>,
    Query(query): Query<CharactersQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    characters_response(&conn, query.search.as_deref())
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    character_summary_response(
        &conn,
        query.artist_id,
        query.model_repo_id.as_deref().unwrap_or(""),
        query.model_variant.as_deref().unwrap_or(""),
        query.model_file.as_deref().unwrap_or(""),
    )
    .map(Json)
    .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
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
    cluster_scores_response(&payload.vectors)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_scan_start(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.scan.try_start() {
        return Ok(Json(json!({"ok": false, "message": "Already scanning"})));
    }
    let db_path = state.db_path.clone();
    let roots = state.roots.clone();
    let control = Arc::clone(&state.scan);
    tokio::task::spawn_blocking(move || {
        // Dedicated connection avoids borrowing the request pool during long walks.
        let conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => c,
            Err(err) => {
                control.set_running(false);
                eprintln!("scan open db failed: {err}");
                return;
            }
        };
        let _ = conn.execute_batch("PRAGMA busy_timeout=30000; PRAGMA journal_mode=WAL;");
        if let Err(err) = run_full_library_scan(&conn, &roots, &control) {
            eprintln!("scan failed: {err}");
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
    if !state.scan.try_start() {
        return Ok(Json(json!({"ok": false, "message": "Already scanning"})));
    }
    let db_path = state.db_path.clone();
    let roots = state.roots.clone();
    let control = Arc::clone(&state.scan);
    let folder = q.folder.clone();
    let artist_id = q.artist_id;
    tokio::task::spawn_blocking(move || {
        let conn = match rusqlite::Connection::open(&db_path) {
            Ok(c) => c,
            Err(err) => {
                control.set_running(false);
                eprintln!("scan open db failed: {err}");
                return;
            }
        };
        let _ = conn.execute_batch("PRAGMA busy_timeout=30000; PRAGMA journal_mode=WAL;");
        if let Err(err) = run_scan(&conn, &roots, &control, Some(artist_id), folder.as_deref()) {
            eprintln!("scan failed: {err}");
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
    let conn = state.pool.get().map_err(to_http_error)?;
    run_hash_batch(&conn, 32).map(Json).map_err(to_http_error)
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
    let roots = state.roots.clone();
    let pool = Arc::clone(&state.pool);
    tokio::task::spawn_blocking(move || {
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
    start_video_transcode(&path, &state.roots)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_video_transcode_status(
    State(state): State<AppState>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Json<Value> {
    let path = q.get("path").cloned().unwrap_or_default();
    Json(video_transcode_status(&path, &state.roots))
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
    list_folder_renames(&conn, q.artist_id)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct FolderPlansBody {
    artist_id: i64,
    plans: Vec<Value>,
}

async fn api_folder_renames_put(
    State(state): State<AppState>,
    Json(body): Json<FolderPlansBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    upsert_folder_rename_plans(&conn, body.artist_id, &body.plans)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    execute_folder_renames(&conn, &state.roots, body.artist_id, body.dry_run)
        .map(Json)
        .map_err(to_http_error)
}

#[derive(serde::Deserialize)]
struct AutoBody {
    enabled: bool,
}

async fn api_folder_renames_auto_put(
    State(state): State<AppState>,
    Json(body): Json<AutoBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    set_folder_rename_auto(&conn, body.enabled)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_plan_recheck(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    recheck_plan(&conn, plan_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_plan_reconfirm(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    reconfirm_plan(&conn, plan_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_folder_plan_unconfirm(
    State(state): State<AppState>,
    Path(plan_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    unconfirm_plan(&conn, plan_id)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_backup(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    create_db_backup(&conn)
        .map(|path| Json(json!({"ok": true, "path": path})))
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
    let line = format!(
        "{} {}\n",
        chrono::Local::now().format("%Y-%m-%d %H:%M:%S"),
        payload
    );
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
    let (lines, truncated) = read_bounded_log_tail(&path, line_limit, max_bytes).unwrap_or_default();
    Json(json!({
        "source": source,
        "exists": exists,
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

async fn api_artist_suggestions(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    suggest_artists_native(&conn, item_id, 3)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_character_recognize(
    State(state): State<AppState>,
    Path(item_id): Path<i64>,
    Query(q): Query<std::collections::HashMap<String, String>>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let top_k = q.get("top_k").and_then(|v| v.parse().ok()).unwrap_or(3);
    let conn = state.pool.get().map_err(to_http_error)?;
    recognize_character_native_topk(&conn, item_id, top_k)
        .map(Json)
        .map_err(to_http_error)
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

async fn api_character_import_job_current() -> Json<Value> {
    Json(get_character_import_job())
}

async fn api_character_import_job_start(
    State(state): State<AppState>,
    body: Result<Json<Value>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let payload = body.map(|j| j.0).unwrap_or_else(|_| json!({}));
    let conn = state.pool.get().map_err(to_http_error)?;
    start_character_import_job(&conn, &payload)
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
    let limit = q.get("limit").and_then(|v| v.parse().ok()).unwrap_or(1000);
    let conn = state.pool.get().map_err(to_http_error)?;
    auto_resolve_move_candidates(&conn, limit)
        .map(Json)
        .map_err(to_http_error)
}

async fn api_move_group_merge(
    State(state): State<AppState>,
    Path((old_artist_id, new_artist_id)): Path<(i64, i64)>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    merge_move_candidate_group(&conn, old_artist_id, new_artist_id)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    update_folder_tags_response(&conn, artist_id, &folder, &tag_ids, &mode)
        .map(Json)
        .map_err(to_http_error)
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
    let conn = state.pool.get().map_err(to_http_error)?;
    update_folder_tags_by_name_response(
        &conn,
        body.artist_id,
        &body.folder,
        &body.tag_names,
        &body.mode,
    )
    .map(Json)
    .map_err(to_http_error)
}

fn to_http_error(error: anyhow::Error) -> (StatusCode, Json<Value>) {
    let message = error
        .chain()
        .map(|cause| cause.to_string())
        .collect::<Vec<_>>()
        .join(": ");
    (
        StatusCode::INTERNAL_SERVER_ERROR,
        Json(json!({"error": message})),
    )
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
    let conn = state.pool.get().map_err(to_http_error)?;
    apply_move_candidate_response(&conn, candidate_id)
        .map(Json)
        .map_err(to_http_error)
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

async fn api_ws_scan(State(state): State<AppState>, ws: WebSocketUpgrade) -> Response {
    // Prefer native scan_state polling from the local DB (pure Rust product).
    let pool = Arc::clone(&state.pool);
    ws.on_upgrade(move |mut socket| async move {
        let mut last = String::new();
        loop {
            let state_json = match pool.get() {
                Ok(conn) => get_scan_state(&conn)
                    .unwrap_or_else(|_| json!({"status": "idle"}))
                    .to_string(),
                Err(_) => json!({"status": "idle"}).to_string(),
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

async fn proxy_request(
    upstream: Upstream,
    request: Request,
) -> Result<Response, (StatusCode, Json<Value>)> {
    let (parts, body) = request.into_parts();
    let bytes = body
        .collect()
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": format!("read body: {err}")})),
            )
        })?
        .to_bytes();
    upstream
        .forward(parts.method, &parts.uri, parts.headers, Bytes::from(bytes))
        .await
        .map_err(|err| {
            (
                StatusCode::BAD_GATEWAY,
                Json(json!({"error": err.to_string()})),
            )
        })
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::BodyExt;
    use tower::ServiceExt;

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
            .execute(
                "INSERT INTO artists (id, name, path) VALUES (1, 'Artist', '/pictures/Artist')",
                [],
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
        get_scan_state(&conn).unwrap();
        conn.execute(
            "UPDATE scan_state SET status='idle', phase='complete', scanned_count=481, total_estimate=482",
            [],
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
        assert_eq!(body["scan"]["scanned_count"], 481);
        assert_eq!(body["scan_schedule"]["enabled"], true);
        assert_eq!(body["scan_schedule"]["interval"], 21600);
        assert_eq!(body["backup_schedule"]["last_error"], "backup failed");
        assert_eq!(body["logs"]["gallery_log"]["size_bytes"], 45);
        let errors = body["recent_errors"].as_array().unwrap();
        assert!(errors.iter().any(|row| row["line"] == "[ERROR] disk"));
        assert!(errors.iter().any(|row| row["line"] == "frontend_rejection promise"));
        assert!(!errors.iter().any(|row| row["line"].as_str().unwrap_or("").contains("failed=0")));
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
}
