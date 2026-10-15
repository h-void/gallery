//! HTTP handlers for Pawchive subscriptions, acquisition, content groups, and Netdisk bridge routes.

use super::*;
use gallery_accel::post_evidence_state;
use gallery_accel::{
    accept_legacy_scope, add_subscription_from_url, apply_grouping, apply_naming_migration,
    assess_post, assess_work_from_ledger, bridge_move_is_isolated, bridge_task_view,
    cancel_attempt, content_group_locations, content_group_members, content_groups_for_day,
    create_attempt, create_bridge_task, default_bridge_identity, delete_subscription, demand_set,
    ensure_content_group_schema, finish_pawchive_sync, generate_event_scripter_script,
    get_pawchive_settings, get_subscription, group_index_entries, latest_bridge_session,
    list_artist_posts_page, list_bridge_tasks, list_content_groups, list_filtered_post_ids,
    list_pawchive_events, list_post_attempts, list_post_candidates, list_subscription_posts,
    list_subscriptions, list_work_group_links, load_bridge_task, load_netdisk_settings, log_warn,
    mark_group_location_manual, netdisk_is_disconnected, path_under_authorized_roots,
    pawchive_http_client, pawchive_sync_status, plan_manual_post, plan_naming_migration,
    preview_day_pairing, process_bridge_exchange, queue_bridge_command, record_external_receipt,
    record_post_decision, record_selection, remember_bridge_token,
    resolve_netdisk_staging_directory, rotate_bridge_token, run_manual_attempt,
    run_pawchive_reconcile, run_pawchive_sync, save_netdisk_settings, save_pawchive_settings,
    saved_bridge_token, set_netdisk_disconnected, set_subscription_mode, submit_bridge_task,
    subscription_summary, toggle_subscription, try_begin_pawchive_sync, verify_bridge_token,
    verify_post_files, AttemptError, BridgeCapabilities, BridgeConflict, BridgeExchangePayload,
    BridgeInvalid, BridgeTask, CancelOutcome, DatePrecision, DecisionOutcome, ExternalReceipt,
    LegacyScopeOutcome, MediaRoots, NamingApplyError, NamingApplyRequest, NetdiskSettings,
    PawchiveSettings, PostDecisionAction, PostListFilter, ReceiptOutcome, ScanControl,
    SelectionError, SubscriptionMode, SyncTrigger, WorkNamingContext, BRIDGE_TASK_SETTLED,
    BRIDGE_TASK_SUBMITTED, LEDGER_REASON_UNKNOWN_WORK, MAX_ITEM_PAGE_LIMIT, MAX_PAGINATION_LIMIT,
    NETDISK_BRIDGE_PAYLOAD_MAX_BYTES, NETDISK_PROTOCOL_VERSION,
    PAWCHIVE_FILTER_SELECTION_MAX_POSTS,
};

#[derive(serde::Deserialize)]
pub(super) struct PawchiveSubscriptionBody {
    url: String,
    #[serde(default)]
    target_dir: Option<String>,
    #[serde(default)]
    since_date: Option<String>,
}

#[derive(serde::Deserialize)]
pub(super) struct PawchiveToggleBody {
    enabled: bool,
}

/// The mode a subscription is being switched to. Absent means manual, the same
/// answer an unrecognised value gets: the mode that downloads less.
#[derive(serde::Deserialize)]
pub(super) struct PawchiveModeBody {
    #[serde(default)]
    mode: Option<String>,
}

fn write_mode_error() -> (StatusCode, Json<Value>) {
    (
        StatusCode::NOT_IMPLEMENTED,
        Json(json!({"error": "write mode not enabled"})),
    )
}

pub(super) async fn api_get_pawchive_settings(
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

pub(super) async fn api_save_pawchive_settings(
    State(state): State<AppState>,
    Json(payload): Json<PawchiveSettings>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    save_pawchive_settings(&conn, &payload).map_err(to_http_error)?;
    Ok(Json(json!({"settings": payload})))
}

pub(super) async fn api_list_pawchive_subscriptions(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let subscriptions = list_subscriptions(&conn).map_err(to_http_error)?;
    Ok(Json(json!({"subscriptions": subscriptions})))
}

pub(super) async fn api_add_pawchive_subscription(
    State(state): State<AppState>,
    Json(body): Json<PawchiveSubscriptionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_delete_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_toggle_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveToggleBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_set_pawchive_subscription_mode(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveModeBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_pawchive_sync(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_check(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_post_candidates(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Query(query): Query<PawchiveCandidatesQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let candidates = list_post_candidates(&conn, id, query.limit).map_err(to_http_error)?;
    Ok(Json(json!({"candidates": candidates})))
}

#[derive(serde::Deserialize)]
pub(super) struct PawchiveCandidatesQuery {
    limit: Option<i64>,
}

/// Hash a post's delivered files against the hashes the source published.
///
/// Reads the files, so it is a write-path capability action rather than a plain
/// query. Resources with nothing to compare against are reported as skipped
/// instead of being claimed as verified.
pub(super) async fn api_pawchive_verify_post(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_external_receipt(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    Json(receipt): Json<ExternalReceipt>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_pawchive_status(State(_state): State<AppState>) -> Json<Value> {
    Json(json!({"status": pawchive_sync_status()}))
}

/// Observation-only reconciliation. Safe to run on a live library: it reads
/// folders and the ledger, writes the derived assessments, and claims nothing.
pub(super) async fn api_pawchive_reconcile(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_check_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_reconcile_pawchive_subscription(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) struct AuditEvidenceQuery {
    limit: Option<usize>,
}

pub(super) async fn api_pawchive_audit_evidence(
    State(state): State<AppState>,
    Query(query): Query<AuditEvidenceQuery>,
) -> Result<Json<gallery_accel::evidence_audit::EvidenceAuditReport>, (StatusCode, Json<Value>)> {
    let limit = query.limit.unwrap_or(MAX_PAGINATION_LIMIT as usize);
    let conn = state.pool.get().map_err(to_http_error)?;
    let report = gallery_accel::evidence_audit::audit_evidence_bindings(&conn, limit)
        .map_err(|e| to_http_error(anyhow::anyhow!(e)))?;
    Ok(Json(report))
}

pub(super) async fn api_pawchive_subscription_summary(
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
pub(super) struct PawchivePostsQuery {
    day: Option<String>,
    cursor: Option<i64>,
    limit: Option<i64>,
}

pub(super) async fn api_pawchive_subscription_posts(
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
pub(super) struct PawchiveDecisionBody {
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

pub(super) async fn api_pawchive_post_decision(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<PawchiveDecisionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) struct AcceptanceBody {
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
pub(super) async fn api_pawchive_accept_legacy_scope(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(body): Json<AcceptanceBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_pawchive_post_acquisition(
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
pub(super) struct PawchiveEventsQuery {
    limit: Option<i64>,
}

/// The reconciliation reads: content groups, the stable work list, the naming
/// preview and the same-day pairing view. All of them are read-only; the two
/// writers (marking a location manual, applying a pairing decision) are separate
/// handlers below.
#[derive(serde::Deserialize)]
pub(super) struct ContentGroupsQuery {
    #[serde(default)]
    scope: Option<String>,
    #[serde(default)]
    day: Option<String>,
    #[serde(
        default,
        deserialize_with = "crate::route_params::deserialize_bool_lenient"
    )]
    include_gone: Option<bool>,
}

/// Content groups of one artist scope, or of one day.
pub(super) async fn api_content_groups(
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
pub(super) async fn api_content_group_detail(
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
pub(super) struct GroupLocationBody {
    relative_path: String,
    #[serde(default)]
    manual: Option<bool>,
    #[serde(default)]
    source_operation: Option<String>,
}

/// Record that the user, not a later grouping pass, decides where this group
/// lives. Without this a manual move would be re-stamped by the next grouping.
pub(super) async fn api_mark_group_location(
    State(state): State<AppState>,
    Path(group_id): Path<String>,
    Json(body): Json<GroupLocationBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) struct StablePostsQuery {
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

pub(super) async fn api_pawchive_post_list(
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
pub(super) struct NamingPreviewQuery {
    work_key: i64,
}

/// What the stored naming rules would render for one work, under all four rule
/// sources the migration has to reconcile. Read-only: the operator reviews it
/// before anything is applied.
///
/// The context is built from the working row's own facts — the remote identity,
/// the title and the published value — and not from any rendered path, so the
/// preview describes the work rather than last time's output.
pub(super) async fn api_pawchive_naming_preview(
    State(state): State<AppState>,
    Query(query): Query<NamingPreviewQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let row: Option<(WorkNamingContext, Option<i64>)> = conn
        .query_row(
            "SELECT s.site_id, s.service, s.user_id, p.post_id, p.title, p.published_at,
                    a.name, s.artist_id
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             LEFT JOIN artists a ON a.id = s.artist_id
             WHERE p.id = ?1",
            rusqlite::params![query.work_key],
            |row| {
                let site: String = row.get(0)?;
                let service: String = row.get(1)?;
                let creator_id: String = row.get(2)?;
                let post_id: String = row.get(3)?;
                let title: Option<String> = row.get(4)?;
                let published: Option<String> = row.get(5)?;
                let artist: Option<String> = row.get(6)?;
                let artist_id: Option<i64> = row.get(7)?;
                let date = published
                    .as_deref()
                    .map(|value| value.get(..10).unwrap_or_default().to_string())
                    .unwrap_or_default();
                let date_precision = if !date.is_empty() {
                    DatePrecision::Day
                } else {
                    DatePrecision::Unknown
                };
                let context = WorkNamingContext {
                    site,
                    service,
                    creator_id,
                    post_id,
                    artist: artist.unwrap_or_default(),
                    title: title.unwrap_or_default(),
                    date,
                    raw_published: published.unwrap_or_default(),
                    date_basis: "published".to_string(),
                    date_precision,
                    ..WorkNamingContext::default()
                };
                Ok((context, artist_id))
            },
        )
        .optional()
        .map_err(|error| to_http_error(error.into()))?;
    let Some((context, artist_id)) = row else {
        return Err((
            StatusCode::NOT_FOUND,
            Json(json!({"error": "post not found"})),
        ));
    };
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
pub(super) async fn api_pawchive_naming_apply(
    State(state): State<AppState>,
    Json(body): Json<NamingApplyRequest>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_netdisk_get_settings(
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

pub(super) async fn api_netdisk_save_settings(
    State(state): State<AppState>,
    Json(mut settings): Json<NetdiskSettings>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_netdisk_rotate_token(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
        return Err(write_mode_error());
    }
    let conn = state.pool.get().map_err(to_http_error)?;
    let token = rotate_bridge_token(&conn).map_err(to_http_error)?;
    Ok((StatusCode::OK, Json(json!({"token": token}))))
}

#[derive(serde::Deserialize)]
pub(super) struct NetdiskScriptRequest {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    base_url: Option<String>,
}

pub(super) async fn api_netdisk_get_script(
    State(state): State<AppState>,
    Json(req): Json<NetdiskScriptRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_netdisk_connection(
    State(state): State<AppState>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let settings = load_netdisk_settings(&conn).map_err(to_http_error)?;
    Ok(Json(
        netdisk_connection_state(&conn, &settings).map_err(to_http_error)?,
    ))
}

pub(super) async fn api_netdisk_test(
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

pub(super) async fn api_netdisk_connect(
    State(state): State<AppState>,
    ConnectInfo(_peer): ConnectInfo<SocketAddr>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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

pub(super) async fn api_netdisk_disconnect(
    State(state): State<AppState>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) struct NetdiskPathCheckRequest {
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
pub(super) async fn api_netdisk_path_check(
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

pub(super) fn check_staging_dir(raw: &str, roots: &MediaRoots) -> Value {
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
        .is_some_and(|n| n.starts_with('.'));
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

pub(super) fn check_import_dir(raw: &str, roots: &MediaRoots) -> Value {
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

pub(super) async fn api_netdisk_bridge_exchange(
    State(state): State<AppState>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    body_bytes: bytes::Bytes,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
        let p: BridgeExchangePayload = if let Some(payload_str) = input.payload.as_str() {
            serde_json::from_str(payload_str).map_err(|e| {
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
pub(super) struct NetdiskJobRequest {
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

pub(super) async fn api_netdisk_create_job(
    State(state): State<AppState>,
    Json(mut req): Json<NetdiskJobRequest>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
    req.links = req
        .links
        .into_iter()
        .map(|l| l.trim().to_string())
        .filter(|l| !l.is_empty())
        .collect();
    for link in &req.links {
        if !link.starts_with("http://") && !link.starts_with("https://") {
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
                if should_auto_start
                    && !task.links.is_empty()
                    && submit_bridge_task(&conn, &task, &effective_staging_dir).is_ok()
                {
                    submitted = true;
                    submitted_count += 1;
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

pub(super) async fn api_netdisk_list_jobs(
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
pub(super) struct NetdiskJobControl {
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

pub(super) async fn api_netdisk_job_control(
    State(state): State<AppState>,
    Path((task_id, action)): Path<(String, String)>,
    Json(body): Json<NetdiskJobControl>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "command_id": command_id,
                    "action": "start",
                    "state": BRIDGE_TASK_SUBMITTED,
                })),
            ))
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
            let _ = conn.execute(
                "UPDATE netdisk_import_status SET retry_count = 0 WHERE task_id = ?1",
                rusqlite::params![task.task_id],
            );
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
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({
                    "command_id": command_id,
                    "action": "retry",
                    "retried": "engine",
                })),
            ))
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
            let _ = conn.execute(
                "UPDATE netdisk_import_status SET retry_count = 0 WHERE task_id = ?1",
                rusqlite::params![task.task_id],
            );
            let (_, total) = gallery_accel::netdisk_import::task_import_state(&conn, &task)
                .map_err(to_http_error)?;
            let linked = run_netdisk_import(&conn, &task, &state.roots, &state.scan)?;
            Ok((
                StatusCode::OK,
                Json(json!({ "action": "import", "linked": linked, "total": total })),
            ))
        }
        NetdiskJobAction::Pause | NetdiskJobAction::Resume | NetdiskJobAction::Remove => {
            let command_action = match action {
                NetdiskJobAction::Pause => "pause",
                NetdiskJobAction::Resume => "resume",
                _ => "remove",
            };
            if action == NetdiskJobAction::Remove {
                let _ = conn.execute(
                    "DELETE FROM netdisk_import_status WHERE task_id = ?1",
                    rusqlite::params![task.task_id],
                );
            }
            let command_id = queue_bridge_command(
                &conn,
                &task.bridge_id,
                &task.task_id,
                command_action,
                &json!({ "link_ids": body.link_ids }),
            )
            .map_err(to_http_error)?;
            Ok((
                StatusCode::ACCEPTED,
                Json(json!({"command_id": command_id, "action": command_action})),
            ))
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
pub(super) struct PairingQuery {
    #[serde(default)]
    scope: Option<String>,
    day: String,
    #[serde(
        default,
        deserialize_with = "crate::route_params::deserialize_bool_lenient"
    )]
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
pub(super) struct GroupIndexBody {
    #[serde(default)]
    artist_id: Option<i64>,
    #[serde(default)]
    limit: Option<i64>,
}

pub(super) async fn api_pawchive_index_groups(
    State(state): State<AppState>,
    Json(body): Json<GroupIndexBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_pairing_preview(
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
pub(super) struct PawchiveSelectionBody {
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
pub(super) async fn api_pawchive_selection_preview(
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
        // The same predicate the list's `含网盘资源` badge is built from, so a
        // selected work reported as external is one the badge agrees with.
        let has_links_sql = gallery_accel::pawchive::has_external_links_sql("kemono_posts");
        let sql = format!("SELECT {has_links_sql} FROM kemono_posts WHERE id = ?1");
        let mut ext = Vec::new();
        for &pid in &post_ids {
            let has_ext: bool = conn
                .query_row(&sql, [pid], |row| row.get::<_, i64>(0))
                .unwrap_or(0)
                == 1;
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
pub(super) async fn api_pawchive_create_selection(
    State(state): State<AppState>,
    Json(body): Json<PawchiveSelectionBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_create_attempt(
    State(state): State<AppState>,
    Json(body): Json<PawchiveAttemptBody>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_list_post_files(
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
pub(super) async fn api_pawchive_file_retry(
    State(state): State<AppState>,
    Path((post_id, file_id)): Path<(i64, i64)>,
) -> Result<(StatusCode, Json<Value>), (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_post_import(
    State(state): State<AppState>,
    Path(post_id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) struct PawchiveAttemptBody {
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
pub(super) async fn api_pawchive_cancel_attempt(
    State(state): State<AppState>,
    Path(attempt_id): Path<String>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    if !state.capabilities.allows_writes() {
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
pub(super) async fn api_pawchive_post_attempts(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let attempts = list_post_attempts(&conn, id).map_err(to_http_error)?;
    Ok(Json(json!({ "attempts": attempts })))
}

pub(super) async fn api_pawchive_events(
    State(state): State<AppState>,
    Query(query): Query<PawchiveEventsQuery>,
) -> Result<Json<Value>, (StatusCode, Json<Value>)> {
    let conn = state.pool.get().map_err(to_http_error)?;
    let events = list_pawchive_events(&conn, query.limit).map_err(to_http_error)?;
    Ok(Json(json!({"events": events})))
}
