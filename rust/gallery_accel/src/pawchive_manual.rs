//! Manual post selection, acquisition attempts, scoped file download, and post decisions.

use super::*;

/// One working-ledger resource the selected post still holds.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualResourcePlan {
    pub file_id: i64,
    pub post_id: i64,
    pub resource_key: String,
    pub file_type: String,
    /// A resource the work still needs, or one that is already covered. The
    /// distinction is what the panel shows and what a manual request can choose
    /// to override.
    pub planned_state: String,
    pub target_path: String,
}

/// What a selectable post looks like before a request is built from it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManualPostPlan {
    pub post_id: i64,
    pub post_id_value: String,
    pub work_id: String,
    pub title: Option<String>,
    pub day: String,
    pub state: String,
    pub manifest_version: i64,
    pub resources: Vec<ManualResourcePlan>,
    /// The post has no direct resource this side may fetch — an external-only
    /// work. A manual request against it answers with this reason instead of
    /// reporting a run that fetched zero files.
    pub no_direct_resources: bool,
}

/// A frozen selection of works and resources.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SelectionOutcome {
    pub selection_id: String,
    pub request_id: String,
    /// True when this request id had already been recorded with the same body,
    /// so the caller is being answered with the task it already has.
    pub reused: bool,
    pub posts: usize,
    pub resources: usize,
    pub expires_at: String,
    pub plans: Vec<ManualPostPlan>,
}

/// Why an explicit selection could not be recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SelectionError {
    /// The same request id was already used for a different request body.
    RequestConflict,
    /// The selection named no post this ledger knows.
    UnknownPosts(Vec<i64>),
    /// A frozen selection is a short-lived object; the caller has to re-read and
    /// re-review rather than have an old one applied silently.
    ExpiredSelection(String),
    NotFound(String),
}

impl std::fmt::Display for SelectionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::RequestConflict => write!(f, "request id already used for another request"),
            Self::UnknownPosts(ids) => write!(f, "unknown posts: {ids:?}"),
            Self::ExpiredSelection(id) => write!(f, "selection expired: {id}"),
            Self::NotFound(id) => write!(f, "not found: {id}"),
        }
    }
}

impl std::error::Error for SelectionError {}

/// What the user said about one work, and when.
///
/// A manual request is a decision the user just made, so it is not filtered by
/// the automatic admission rules (master switch, subscription switch, mode,
/// start date, derived assessment). It still obeys the read-only mode, the
/// authorized-root check and the no-overwrite publish rule — those are the
/// safety boundary, not a scheduling policy.
#[derive(Debug, Clone, Copy)]
pub(crate) enum DownloadPostPolicy<'a> {
    /// A round claims what the evaluator says is still open, under the
    /// admission rules that read the switches as they stand now.
    Automatic,
    /// An explicit request: only these resources, whatever the switches or the
    /// derived state say. `attempt_id` names the task the work belongs to so
    /// the publish ledger keys the job on this attempt rather than on the post.
    Manual {
        attempt_id: &'a str,
        force_file_ids: &'a [i64],
    },
}

impl DownloadPostPolicy<'_> {
    /// Whether the derived assessment may stop the pass before any file runs.
    pub(super) fn honours_assessment(&self) -> bool {
        matches!(self, Self::Automatic)
    }

    pub(super) fn is_manual(&self) -> bool {
        matches!(self, Self::Manual { .. })
    }

    /// The explicit request this pass belongs to, if any.
    ///
    /// A cancel is a property of that request, so only a manual pass can observe
    /// one: an automatic round has no task row for a user to cancel.
    pub(super) fn attempt_id(&self) -> Option<&str> {
        match self {
            Self::Automatic => None,
            Self::Manual { attempt_id, .. } => Some(attempt_id),
        }
    }

    /// Whether a file id is outside this pass's scope.
    pub(super) fn excludes_file(&self, file_id: i64) -> bool {
        match self {
            Self::Automatic => false,
            Self::Manual { force_file_ids, .. } => {
                !force_file_ids.is_empty() && !force_file_ids.contains(&file_id)
            }
        }
    }

    /// The task key the publish ledger records this delivery under.
    ///
    /// Automatic rounds keep the post-scoped key they always had. A manual
    /// attempt gets its own key, which is what makes a genuine re-fetch a new
    /// job instead of a hit on the previous delivery's ledger row.
    pub(super) fn job_scope(&self, post_id: &str, file_id: i64) -> String {
        match self {
            Self::Automatic => format!("{post_id}:{file_id}"),
            Self::Manual { attempt_id, .. } => format!("{post_id}:{file_id}:{attempt_id}"),
        }
    }
}

/// The stable suffix a manual re-fetch wears when its natural name is taken.
///
/// Stable per attempt and resource: the retry of the same attempt lands on the
/// same target, so an interrupted receive is resumed into its own part file
/// instead of multiplying copies, while a later attempt gets its own name.
pub(super) fn attempt_target_suffix(
    post_db_id: i64,
    file_id: i64,
    policy: DownloadPostPolicy<'_>,
) -> String {
    match policy {
        DownloadPostPolicy::Automatic => target_suffix(post_db_id, &file_id.to_string()),
        DownloadPostPolicy::Manual { attempt_id, .. } => ledger_digest(
            "at",
            &[&post_db_id.to_string(), &file_id.to_string(), attempt_id],
        )
        .chars()
        .take(8)
        .collect(),
    }
}

/// How long a frozen selection stays applicable.
///
/// Long enough to review a large result and press download, short enough that a
/// manifest which moved on is re-read rather than applied from a stale snapshot.
pub const PAWCHIVE_SELECTION_TTL_MINUTES: i64 = 30;

/// The most works one filter-shaped selection request may freeze.
///
/// "全选当前筛选" is answered by the server resolving the filter, so the request
/// body stays one filter however many works it stands for. The freeze itself is
/// not free — every work is planned and its members written — so the read is
/// capped and the response says when the filter held more. A filter matching
/// more than this is still walkable: the panel asks for the remainder with a
/// second request rather than one unbounded one.
pub const PAWCHIVE_FILTER_SELECTION_MAX_POSTS: usize = 5000;

/// Plan one post for the manual list: every resource it currently asks for, and
/// whether this side already holds proof of it.
///
/// The plan is built from the same facts the evaluator uses — the current
/// manifest and the strong acquisition events — so the panel and the queue
/// cannot disagree about what "already fetched" means.
pub fn plan_manual_post(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
) -> Result<Option<ManualPostPlan>> {
    let post: Option<(String, Option<String>, String, i64, String)> = conn
        .query_row(
            "SELECT p.post_id, p.title,
                    substr(COALESCE(NULLIF(p.published_at, ''), p.created_at, ''), 1, 10),
                    p.manifest_version, p.assessment_state
             FROM kemono_posts p WHERE p.id = ?1",
            params![post_db_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?;
    let Some((post_id_value, title, day, manifest_version, state)) = post else {
        return Ok(None);
    };
    let work_id = work_id_for_post(conn, post_db_id)?.unwrap_or_default();
    // The plan reads the same facts the evaluator reads, so the working row has
    // to be on the long-term ledger before its acquisition events can answer
    // "do we have this". The call is gated on the manifest revision, so an
    // up-to-date work costs a query rather than a re-derivation.
    if !work_id.is_empty() {
        let _ = materialize_work_ledger(conn, post_db_id);
    }

    // Strong evidence for this work, from the long-term ledger. Presence alone
    // is not proof, so it is not part of this set.
    let mut acquired: HashSet<String> = HashSet::new();
    if !work_id.is_empty() {
        let mut stmt = conn.prepare(
            "SELECT resource_version_id FROM pawchive_acquisition_events
             WHERE work_id = ?1 AND evidence_strength = 'strong'",
        )?;
        let rows = stmt.query_map(params![work_id], |row| row.get::<_, String>(0))?;
        for row in rows {
            acquired.insert(row?);
        }
    }

    let files = list_post_files(conn, post_db_id)?;
    let mut resources = Vec::new();
    for file in files {
        // A resource the source withdrew is not part of what the work asks for.
        if !file.present_in_manifest {
            continue;
        }
        // The type selection is applied at the plan, so the panel shows what
        // this install would actually fetch. A type switched off is visible as
        // absent, not as a silent no-op.
        if !wants_type(settings, &file.file_type) {
            continue;
        }
        // A delivered resource is recorded at the version the source never
        // characterised (`unknown`), because this install's own digest is not a
        // source version. Both sides of the comparison derive the identity the
        // same way, so a covered resource is found rather than looked for under
        // a key nothing wrote.
        let resource_version_id =
            resource_version_id_for(&work_id, &file.remote_path, UNKNOWN_VERSION);
        let covered = acquired.contains(&resource_version_id);
        resources.push(ManualResourcePlan {
            file_id: file.id,
            post_id: post_db_id,
            resource_key: file.remote_path.clone(),
            file_type: file.file_type.clone(),
            planned_state: if covered { "covered" } else { "fetch" }.to_string(),
            target_path: file.target_path.clone(),
        });
    }

    Ok(Some(ManualPostPlan {
        post_id: post_db_id,
        post_id_value,
        work_id,
        title,
        day,
        state,
        manifest_version,
        no_direct_resources: resources.is_empty(),
        resources,
    }))
}

/// The request body of one explicit selection, as a stable fingerprint.
fn selection_fingerprint(post_ids: &[i64], file_ids: &[i64]) -> String {
    let mut posts: Vec<String> = post_ids.iter().map(|id| id.to_string()).collect();
    posts.sort();
    posts.dedup();
    let mut files: Vec<String> = file_ids.iter().map(|id| id.to_string()).collect();
    files.sort();
    files.dedup();
    let mut parts: Vec<&str> = vec!["selection", "v1"];
    parts.extend(posts.iter().map(String::as_str));
    parts.push("|");
    parts.extend(files.iter().map(String::as_str));
    ledger_digest("sel", &parts)
}

/// What a filter-shaped selection request resolved to.
#[derive(Debug, Clone)]
pub struct FilterSelectionOutcome {
    pub outcome: SelectionOutcome,
    /// How many works the filter matched in total, including any beyond the cap.
    pub matched: usize,
    /// The filter matched more works than this request may freeze, so the
    /// selection holds the first `PAWCHIVE_FILTER_SELECTION_MAX_POSTS` of them.
    pub truncated: bool,
}

/// Freeze a selection from a filter instead of an id list.
///
/// The panel's "全选当前筛选" cannot post the ids it stands for: a creator with a
/// hundred thousand works would put the whole review into one request body. It
/// posts the filter it is displaying, and this is the server-side half — the ids
/// are resolved against the same columns the manual list filters on, then frozen
/// by [`record_selection`] exactly as an explicit list would be.
///
/// The cap is a durability boundary, not a UI limit: a frozen selection is
/// materialised per work and per resource, so the request that may freeze at once
/// is bounded and the caller is told when the filter holds more.
pub fn record_selection_for_filter(
    conn: &Connection,
    request_id: &str,
    subscription_id: Option<i64>,
    filter: &PostListFilter,
    file_ids: &[i64],
    settings: &PawchiveSettings,
) -> Result<FilterSelectionOutcome, SelectionError> {
    let (post_ids, truncated) = list_filtered_post_ids(
        conn,
        subscription_id,
        filter,
        PAWCHIVE_FILTER_SELECTION_MAX_POSTS,
    )
    .map_err(|error| SelectionError::NotFound(error.to_string()))?;
    if post_ids.is_empty() {
        return Err(SelectionError::UnknownPosts(Vec::new()));
    }
    let matched = if truncated {
        PAWCHIVE_FILTER_SELECTION_MAX_POSTS
    } else {
        post_ids.len()
    };
    let outcome = record_selection(conn, request_id, &post_ids, file_ids, settings)?;
    Ok(FilterSelectionOutcome {
        outcome,
        matched,
        truncated,
    })
}

/// Freeze what the user selected, once per request id.
///
/// The selection is stored as rows rather than as one blob: the panel's "全选"
/// can name thousands of works, and materialising that as a single JSON value
/// would put the whole review into one write. Re-sending the same request id
/// with the same body answers with the same selection; re-sending it with a
/// different body is a conflict, because the id is the caller's promise that it
/// is the same request.
pub fn record_selection(
    conn: &Connection,
    request_id: &str,
    post_ids: &[i64],
    file_ids: &[i64],
    settings: &PawchiveSettings,
) -> Result<SelectionOutcome, SelectionError> {
    let request_id = request_id.trim();
    if request_id.is_empty() {
        return Err(SelectionError::NotFound(
            "request id is required".to_string(),
        ));
    }
    let mut wanted_posts: Vec<i64> = post_ids.to_vec();
    wanted_posts.sort();
    wanted_posts.dedup();
    if wanted_posts.is_empty() {
        return Err(SelectionError::UnknownPosts(Vec::new()));
    }
    let fingerprint = selection_fingerprint(&wanted_posts, file_ids);

    let existing: Option<(String, String)> = conn
        .query_row(
            "SELECT selection_id, request_fingerprint FROM pawchive_selections
             WHERE request_id = ?1",
            params![request_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()
        .map_err(|error| SelectionError::NotFound(error.to_string()))?;
    if let Some((selection_id, stored_fingerprint)) = existing {
        if stored_fingerprint != fingerprint {
            return Err(SelectionError::RequestConflict);
        }
        let (posts, resources, expires_at) = conn
            .query_row(
                "SELECT posts, resources, expires_at FROM pawchive_selections
                 WHERE selection_id = ?1",
                params![selection_id],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)? as usize,
                        row.get::<_, i64>(1)? as usize,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .map_err(|error| SelectionError::NotFound(error.to_string()))?;
        return Ok(SelectionOutcome {
            selection_id,
            request_id: request_id.to_string(),
            reused: true,
            posts,
            resources,
            expires_at,
            plans: Vec::new(),
        });
    }

    // Every named post has to exist: a selection that silently drops one is a
    // request the user cannot verify.
    let mut missing = Vec::new();
    let mut plans = Vec::new();
    for post_id in &wanted_posts {
        match plan_manual_post(conn, *post_id, settings) {
            Ok(Some(plan)) => plans.push(plan),
            Ok(None) => missing.push(*post_id),
            Err(error) => return Err(SelectionError::NotFound(error.to_string())),
        }
    }
    if !missing.is_empty() {
        return Err(SelectionError::UnknownPosts(missing));
    }

    // An explicit resource list narrows each post to those resources. A file id
    // that belongs to another post is not silently accepted.
    let requested_files: HashSet<i64> = file_ids.iter().copied().collect();
    if !requested_files.is_empty() {
        for plan in &mut plans {
            plan.resources
                .retain(|resource| requested_files.contains(&resource.file_id));
            plan.no_direct_resources = plan.resources.is_empty();
        }
    }

    let now = Utc::now();
    let created_at = now.to_rfc3339();
    let expires_at = (now + Duration::minutes(PAWCHIVE_SELECTION_TTL_MINUTES)).to_rfc3339();
    let selection_id = ledger_digest("selid", &[request_id, &fingerprint, &created_at]);
    let resource_count: usize = plans.iter().map(|plan| plan.resources.len()).sum();

    let tx = conn
        .unchecked_transaction()
        .map_err(|error| SelectionError::NotFound(error.to_string()))?;
    tx.execute(
        "INSERT INTO pawchive_selections
             (selection_id, request_id, request_fingerprint, posts, resources, created_at, expires_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            selection_id,
            request_id,
            fingerprint,
            plans.len() as i64,
            resource_count as i64,
            created_at,
            expires_at
        ],
    )
    .map_err(|error| SelectionError::NotFound(error.to_string()))?;
    for plan in &plans {
        for resource in &plan.resources {
            tx.execute(
                "INSERT INTO pawchive_selection_members
                     (selection_id, post_id, work_id, manifest_version, resource_key,
                      resource_version_id, target_path, planned_state)
                 VALUES (?1, ?2, ?3, ?4, ?5, '', ?6, ?7)
                 ON CONFLICT(selection_id, post_id, resource_key) DO NOTHING",
                params![
                    selection_id,
                    plan.post_id,
                    plan.work_id,
                    plan.manifest_version,
                    resource.resource_key,
                    resource.target_path,
                    resource.planned_state
                ],
            )
            .map_err(|error| SelectionError::NotFound(error.to_string()))?;
        }
    }
    tx.commit()
        .map_err(|error| SelectionError::NotFound(error.to_string()))?;

    Ok(SelectionOutcome {
        selection_id,
        request_id: request_id.to_string(),
        reused: false,
        posts: plans.len(),
        resources: resource_count,
        expires_at,
        plans,
    })
}

/// What an explicit selection became: one task per post, or none when the
/// request described nothing fetchable.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AttemptOutcome {
    pub attempt_id: String,
    pub selection_id: String,
    pub request_id: String,
    pub posts: usize,
    pub resources: usize,
    /// Posts whose resources are all external. The caller reports this reason
    /// instead of a run that fetched zero files.
    pub no_direct_resources: Vec<i64>,
}

/// Why an explicit attempt could not be created or run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AttemptError {
    SelectionExpired(String),
    SelectionEmpty(String),
    NotFound(String),
}

impl std::fmt::Display for AttemptError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::SelectionExpired(id) => write!(f, "selection expired: {id}"),
            Self::SelectionEmpty(id) => write!(f, "selection holds no resource to fetch: {id}"),
            Self::NotFound(id) => write!(f, "not found: {id}"),
        }
    }
}

impl std::error::Error for AttemptError {}

/// Create the task a selection was made for.
///
/// Keyed on the request id, so a repeated click or a retried request returns
/// the same attempt instead of starting a second fetch of the same thing. The
/// attempt is immutable once created: it names the manifest versions that were
/// reviewed, so a request cannot have new resources joined to it afterwards.
pub fn create_attempt(
    conn: &Connection,
    request_id: &str,
    selection_id: &str,
) -> Result<AttemptOutcome, AttemptError> {
    let selection: Option<(String, String, i64)> = conn
        .query_row(
            "SELECT request_id, expires_at, resources FROM pawchive_selections
             WHERE selection_id = ?1",
            params![selection_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    let Some((stored_request, expires_at, resource_count)) = selection else {
        return Err(AttemptError::NotFound(selection_id.to_string()));
    };
    // The session request id is the authority for "this is the same request":
    // an attempt created under a different id is a different task.
    let request_id = if request_id.trim().is_empty() {
        stored_request
    } else {
        request_id.trim().to_string()
    };
    if Utc::now().to_rfc3339() > expires_at {
        return Err(AttemptError::SelectionExpired(selection_id.to_string()));
    }

    let now = Utc::now().to_rfc3339();
    let attempt_id: Option<String> = conn
        .query_row(
            "SELECT attempt_id FROM pawchive_attempts WHERE request_id = ?1",
            params![request_id],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    if let Some(attempt_id) = attempt_id {
        let (posts, resources) = attempt_counts(conn, &attempt_id)?;
        return Ok(AttemptOutcome {
            attempt_id,
            selection_id: selection_id.to_string(),
            request_id,
            posts,
            resources,
            no_direct_resources: Vec::new(),
        });
    }

    // The posts this selection names, and how many resources each holds.
    // A selection holding nothing fetchable is refused rather than run and
    // reported as "downloaded 0 files".
    let mut stmt = conn
        .prepare(
            "SELECT post_id, COUNT(*) FROM pawchive_selection_members
             WHERE selection_id = ?1 GROUP BY post_id ORDER BY post_id",
        )
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    let rows = stmt
        .query_map(params![selection_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
        })
        .map_err(|error| AttemptError::NotFound(error.to_string()))?
        .collect::<rusqlite::Result<Vec<(i64, i64)>>>()
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    if rows.is_empty() {
        return Err(AttemptError::SelectionEmpty(selection_id.to_string()));
    }

    let attempt_id = ledger_digest("att", &[selection_id, &request_id, &now]);
    let tx = conn
        .unchecked_transaction()
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    tx.execute(
        "INSERT INTO pawchive_attempts
             (attempt_id, selection_id, request_id, intent, state, posts, resources,
              created_at, updated_at)
         VALUES (?1, ?2, ?3, 'manual', 'queued', ?4, ?5, ?6, ?6)
         ON CONFLICT(request_id) DO NOTHING",
        params![
            attempt_id,
            selection_id,
            request_id,
            rows.len() as i64,
            resource_count,
            now
        ],
    )
    .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    for (post_id, _) in &rows {
        let work_id: String = tx
            .query_row(
                "SELECT work_id FROM pawchive_selection_members
                 WHERE selection_id = ?1 AND post_id = ?2 LIMIT 1",
                params![selection_id, post_id],
                |row| row.get(0),
            )
            .unwrap_or_default();
        tx.execute(
            "INSERT INTO pawchive_attempt_posts
                 (attempt_id, post_id, work_id, state, fetched, error, updated_at)
             VALUES (?1, ?2, ?3, 'queued', 0, '', ?4)
             ON CONFLICT(attempt_id, post_id) DO NOTHING",
            params![attempt_id, post_id, work_id, now],
        )
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    }
    tx.commit()
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;

    Ok(AttemptOutcome {
        attempt_id,
        selection_id: selection_id.to_string(),
        request_id,
        posts: rows.len(),
        resources: resource_count.max(0) as usize,
        no_direct_resources: Vec::new(),
    })
}

fn attempt_counts(conn: &Connection, attempt_id: &str) -> Result<(usize, usize), AttemptError> {
    let rows = conn
        .query_row(
            "SELECT posts, resources FROM pawchive_attempts WHERE attempt_id = ?1",
            params![attempt_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .map_err(|error| AttemptError::NotFound(error.to_string()))?;
    Ok((rows.0.max(0) as usize, rows.1.max(0) as usize))
}

/// The attempts a post has, newest first, for the panel's history line.
///
/// `fetched` and `error` are per post, not per attempt: the attempt's own
/// `resources` column is the whole request's count, and reporting it on every row
/// told the panel that a work with nothing delivered had "fetched 40". What the
/// row knows is what this attempt delivered *for this work* and why it did not.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PostAttemptSummary {
    pub attempt_id: String,
    pub intent: String,
    pub state: String,
    pub fetched: i64,
    pub error: String,
}

pub fn list_post_attempts(conn: &Connection, post_db_id: i64) -> Result<Vec<PostAttemptSummary>> {
    let mut stmt = conn.prepare(
        "SELECT a.attempt_id, a.intent, p.state, p.fetched, p.error
         FROM pawchive_attempt_posts p
         JOIN pawchive_attempts a ON a.attempt_id = p.attempt_id
         WHERE p.post_id = ?1 ORDER BY a.created_at DESC, a.attempt_id DESC",
    )?;
    let rows = stmt
        .query_map(params![post_db_id], |row| {
            Ok(PostAttemptSummary {
                attempt_id: row.get(0)?,
                intent: row.get(1)?,
                state: row.get(2)?,
                fetched: row.get(3)?,
                error: row.get(4)?,
            })
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Run one explicit attempt.
///
/// What the attempt names is what is fetched: the resources the selection
/// reviewed, at the manifest versions it reviewed them at. A post whose manifest
/// has moved on is not silently widened or narrowed — the request is refused
/// for that post and the attempt reports the divergence, because the snapshot
/// the user approved no longer describes the work.
///
/// The explicit request is not filtered by the automatic admission rules:
/// disabled subscriptions, manual mode, auto start dates and a derived state
/// that says "already covered" all belong to the round's policy, not to the
/// button the user just pressed. The read-only capability and the authorized
/// root checks are applied by the caller and by the publish step.
pub async fn run_manual_attempt(
    pool: &Arc<DbPool>,
    client: &reqwest::Client,
    roots: &MediaRoots,
    attempt_id: &str,
) -> Result<usize> {
    let settings = {
        let conn = pool.get()?;
        get_pawchive_settings(&conn)?
    };
    let attempt: Option<(String, String, String)> = {
        let conn = pool.get()?;
        conn.query_row(
            "SELECT selection_id, state, request_id FROM pawchive_attempts WHERE attempt_id = ?1",
            params![attempt_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
    };
    let Some((selection_id, state, _request_id)) = attempt else {
        return Err(anyhow!("unknown attempt: {attempt_id}"));
    };
    if state == "done" || state == "cancelled" {
        return Ok(0);
    }

    let posts: Vec<(i64, String, String)> = {
        let conn = pool.get()?;
        let mut stmt = conn.prepare(
            "SELECT ap.post_id, ap.work_id, s.target_dir
             FROM pawchive_attempt_posts ap
             JOIN kemono_posts p ON p.id = ap.post_id
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE ap.attempt_id = ?1 AND ap.state != 'done'
             ORDER BY ap.post_id ASC",
        )?;
        let rows = stmt
            .query_map(params![attempt_id], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows
    };
    if posts.is_empty() {
        let conn = pool.get()?;
        mark_attempt_done(&conn, attempt_id, "done", None)?;
        return Ok(0);
    }

    let mut fetched_total = 0usize;
    for (post_db_id, _work_id, target_dir) in posts {
        // A cancel takes effect between works: the resource already receiving is
        // allowed to reach its own safe point, and nothing new is claimed.
        // Anything already published stays where it is.
        {
            let conn = pool.get()?;
            if attempt_state(&conn, attempt_id)?.as_deref() == Some("cancelled") {
                break;
            }
        }
        // The reviewed manifest against the one that stands now. A change means
        // the resource set the user approved is no longer the work's set.
        let divergence: Option<(String, i64, i64)> = {
            let conn = pool.get()?;
            conn.query_row(
                "SELECT m.resource_key, m.manifest_version, p.manifest_version
                 FROM pawchive_selection_members m
                 JOIN kemono_posts p ON p.id = m.post_id
                 WHERE m.selection_id = ?1 AND m.post_id = ?2
                 ORDER BY m.resource_key
                 LIMIT 1",
                params![selection_id, post_db_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
        };
        let Some((_, reviewed_version, current_version)) = divergence else {
            // The selection no longer names this post: it was recorded, so a
            // mismatch here means the row was removed, not that the request
            // changed.
            let conn = pool.get()?;
            mark_attempt_post(
                &conn,
                attempt_id,
                post_db_id,
                "failed",
                0,
                "selection no longer names this post",
            )?;
            continue;
        };
        if reviewed_version != current_version {
            let conn = pool.get()?;
            mark_attempt_post(
                &conn,
                attempt_id,
                post_db_id,
                "conflict",
                0,
                &format!("清单已从 v{reviewed_version} 变为 v{current_version}"),
            )?;
            continue;
        }

        let resource_rows = {
            let conn = pool.get()?;
            attempt_resource_rows(&conn, &selection_id, post_db_id)?
        };
        let file_ids: Vec<i64> = resource_rows.iter().map(|(id, _)| *id).collect();
        if file_ids.is_empty() {
            let conn = pool.get()?;
            mark_attempt_post(&conn, attempt_id, post_db_id, "done", 0, "")?;
            continue;
        }

        // The attempt names the resources; the download pass writes the rows.
        // A manual attempt re-fetches what it names, so the rows it is about to
        // fetch are put back in flight here, while the rows outside its scope
        // are left exactly as they are.
        {
            let conn = pool.get()?;
            for (file_id, resource_key) in &resource_rows {
                conn.execute(
                    "UPDATE kemono_files
                     SET status = 'pending', next_attempt_at = NULL, updated_at = ?1
                     WHERE id = ?2 AND remote_path = ?3",
                    params![Utc::now().to_rfc3339(), file_id, resource_key],
                )?;
            }
        }

        let policy = DownloadPostPolicy::Manual {
            attempt_id,
            force_file_ids: &file_ids,
        };
        let artist_dir = PathBuf::from(&target_dir);
        match download_post_files_scoped(
            pool,
            client,
            post_db_id,
            &artist_dir,
            roots,
            &settings,
            policy,
        )
        .await
        {
            Ok(count) => {
                fetched_total += count;
                let conn = pool.get()?;
                mark_attempt_post(&conn, attempt_id, post_db_id, "done", count as i64, "")?;
            }
            Err(error) => {
                let conn = pool.get()?;
                mark_attempt_post(
                    &conn,
                    attempt_id,
                    post_db_id,
                    "failed",
                    0,
                    &error.to_string(),
                )?;
            }
        }
    }

    {
        let conn = pool.get()?;
        // A cancelled request keeps its state. The works it never reached were
        // marked cancelled by the cancel itself, so counting them as outstanding
        // would settle the task as done or failed and report a request the user
        // stopped as one that finished.
        if attempt_state(&conn, attempt_id)?.as_deref() == Some("cancelled") {
            return Ok(fetched_total);
        }
        let outstanding: i64 = conn.query_row(
            "SELECT COUNT(*) FROM pawchive_attempt_posts
             WHERE attempt_id = ?1 AND state IN ('queued', 'failed', 'conflict')",
            params![attempt_id],
            |row| row.get(0),
        )?;
        let state = if outstanding > 0 { "failed" } else { "done" };
        mark_attempt_done(&conn, attempt_id, state, None)?;
    }
    Ok(fetched_total)
}

pub(super) fn mark_attempt_post(
    conn: &Connection,
    attempt_id: &str,
    post_db_id: i64,
    state: &str,
    fetched: i64,
    error: &str,
) -> Result<()> {
    conn.execute(
        "UPDATE pawchive_attempt_posts
         SET state = ?1, fetched = ?2, error = ?3, updated_at = ?4
         WHERE attempt_id = ?5 AND post_id = ?6",
        params![
            state,
            fetched,
            error,
            Utc::now().to_rfc3339(),
            attempt_id,
            post_db_id
        ],
    )?;
    conn.execute(
        "UPDATE pawchive_attempts SET updated_at = ?1 WHERE attempt_id = ?2",
        params![Utc::now().to_rfc3339(), attempt_id],
    )?;
    Ok(())
}

pub(super) fn mark_attempt_done(
    conn: &Connection,
    attempt_id: &str,
    state: &str,
    error: Option<&str>,
) -> Result<()> {
    // Cancellation is terminal. Without this a pass that was stopped would settle
    // itself afterwards and the panel would show a request the user cancelled as
    // one that completed.
    conn.execute(
        "UPDATE pawchive_attempts SET state = ?1, updated_at = ?2
         WHERE attempt_id = ?3 AND state != 'cancelled'",
        params![state, Utc::now().to_rfc3339(), attempt_id],
    )?;
    if let Some(error) = error {
        conn.execute(
            "UPDATE pawchive_attempt_posts SET error = ?1 WHERE attempt_id = ?2",
            params![error, attempt_id],
        )?;
    }
    Ok(())
}

/// The file rows one selection names for a post, as `(file_id, resource_key)`.
///
/// The selection stores the resource key it reviewed; the working row is looked
/// up by that key, so a file whose row was re-created by a re-discovery is
/// still the same resource rather than a new one.
fn attempt_resource_rows(
    conn: &Connection,
    selection_id: &str,
    post_db_id: i64,
) -> Result<Vec<(i64, String)>> {
    let mut stmt = conn.prepare(
        "SELECT f.id, m.resource_key
         FROM pawchive_selection_members m
         JOIN kemono_files f ON f.post_id = m.post_id
                            AND f.remote_path = m.resource_key
         WHERE m.selection_id = ?1 AND m.post_id = ?2
         ORDER BY f.id",
    )?;
    let rows = stmt
        .query_map(params![selection_id, post_db_id], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?
        // Propagated, not filtered: a row-level failure used to disappear,
        // which dropped that resource from `force_file_ids` and from the rows
        // the pass walks — so the resource was skipped and the post was still
        // marked done as if it had been fetched.
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// The settings one pass may claim under, or `None` when the pass stops here.
///
/// An automatic pass is admitted only while the master switch, the
/// subscription switch, the mode and the start date all still say yes —
/// `claim_scope` reads them at the moment of the claim, because a round can
/// sit queued for hours and the switches it started under are not the ones
/// the user has now.
///
/// An explicit request is not filtered by any of that: it is the user's own
/// decision, and those switches are the round's policy, not the button the
/// user just pressed (`DownloadPostPolicy`'s contract, `run_manual_attempt`'s
/// doc, and the attempt route all three say so). The pass used to apply the
/// automatic gate to manual requests too, and the attempt came back "done"
/// with zero files whenever a switch said no. The type selection was applied
/// when the selection was planned (`plan_manual_post`), so the settings the
/// caller passes are the ones the per-file type check has to answer to.
pub(super) fn pass_admission_settings(
    conn: &Connection,
    post_db_id: i64,
    settings: &PawchiveSettings,
    policy: &DownloadPostPolicy<'_>,
) -> Result<Option<PawchiveSettings>> {
    if policy.is_manual() {
        return Ok(Some(settings.clone()));
    }
    match claim_scope(conn, post_db_id)? {
        Some(claim) if claim.enabled && claim.in_range => Ok(Some(claim.settings)),
        // Already-published files stay where they are; only new claims stop.
        _ => Ok(None),
    }
}

/// Download the resources this pass may fetch, with specific settings.
///
/// Every resource is judged on its own: it is downloaded, or it is already at
/// its own reserved target, or it failed — and only the first two produce
/// evidence. Per-resource judgement decides what this pass fetches; it does not
/// decide whether the pass runs at all.
///
/// An automatic pass stops before the network when the work's folder already
/// holds content, because the date-level admission treats that as had. What that
/// costs is deliberate and documented on `resolve_post_folder`: a second work on
/// the same day, or an attachment added after the day's folder was filled, is
/// settled by that folder too. A manual attempt is not filtered by admission
/// (`pass_admission_settings`), so the same partial delivery can still be
/// supplemented by hand, under the authorized-root and no-overwrite rules.
///
/// The pooled connection is checked out per database phase and released before
/// each network await. Borrowing a checked-out guard across an await would make
/// this future `!Send` (a `rusqlite::Connection` is `Send` but not `Sync`), and
/// holding it for a whole download would occupy a pool slot for minutes.
pub(crate) async fn download_post_files_scoped(
    pool: &Arc<DbPool>,
    client: &reqwest::Client,
    post_db_id: i64,
    artist_dir: &Path,
    roots: &MediaRoots,
    settings: &PawchiveSettings,
    policy: DownloadPostPolicy<'_>,
) -> Result<usize> {
    // Query post and subscription info plus every file row, so the `{index}`
    // token stays the row's position in the post even when only some rows are
    // retried.
    struct PostExportSource {
        post_id: String,
        service: String,
        user_id: String,
        artist_name: Option<String>,
        title: Option<String>,
        published_at: Option<String>,
        content: Option<String>,
        manifest_version: i64,
        files: Vec<PostFileRow>,
    }

    let PostExportSource {
        post_id,
        service,
        user_id,
        artist_name,
        title,
        published_at,
        content,
        manifest_version,
        files,
    } = {
        let conn = pool.get()?;
        let (
            post_id,
            service,
            user_id,
            artist_name,
            title,
            published_at,
            content,
            manifest_version,
        ) = conn.query_row(
            "SELECT p.post_id, s.service, s.user_id, a.name, p.title, p.published_at, p.content,
                    p.manifest_version
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             LEFT JOIN artists a ON a.id = s.artist_id
             WHERE p.id = ?1",
            params![post_db_id],
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
        )?;

        let files = list_post_files(&conn, post_db_id)?;
        PostExportSource {
            post_id,
            service,
            user_id,
            artist_name,
            title,
            published_at,
            content,
            manifest_version,
            files,
        }
    };

    // The creator id must survive the display-name fallback below so
    // `{userID}` / `{user_id}` keep rendering the real id.
    let creator_id = user_id.clone();
    let user = artist_name.unwrap_or(user_id);
    let naming = PostNamingContext::new(
        &user,
        published_at.as_deref(),
        title.as_deref(),
        &post_id,
        Some(&service),
    )
    .with_identity(&creator_id, &service);

    let folder_rel = naming.format_folder_path_with_template(&settings.folder_template);
    let template_post_dir = artist_dir.join(&folder_rel);

    // Where a new resource of this work belongs.
    //
    // The plan's order is: this attempt's already-reserved path (per file, below)
    // → the group's verified current location → the shared render rules, which
    // are for a *new* work. Rendering the template unconditionally is what split
    // one work across two directories: the user renames the folder, and the next
    // attachment of the same work lands in a freshly rendered one, because the
    // template still says the old name. The ledger's location is the only source
    // here that is *verified* rather than guessed from the disk.
    let (verified_dir, post_dir) = {
        let conn = pool.get()?;
        let verified =
            crate::pawchive_groups::verified_location_for_post(&conn, post_db_id, roots)?;
        // A work split across several groups has no single destination. The
        // plan asks for `location_ambiguous` here rather than a guess: picking
        // one group, or the longest common parent, is a claim about content
        // nobody made. Refusing is the whole point of the third outcome.
        if let Some(reason) = verified.ambiguous_reason() {
            return Err(anyhow::anyhow!(reason));
        }
        let v_path = verified.path();
        let p_dir = v_path.clone().unwrap_or_else(|| template_post_dir.clone());
        (v_path, p_dir)
    };

    let derived = derived_text_names(&naming);
    let resolved = resolve_post_folder_with_verified(
        artist_dir.to_str().unwrap_or(""),
        &naming,
        &settings.folder_template,
        &naming.date,
        &derived,
        verified_dir.as_deref(),
    );
    let observation = observation_after_search(resolved.observation, &resolved);
    {
        let conn = pool.get()?;
        record_folder_observation(&conn, post_db_id, observation, resolved.source)?;
    }
    if policy.honours_assessment() {
        // Unknown before absent. The search could not read a directory it
        // needed, so it has not shown the work is missing — and a pass that
        // fetches on that basis is the one that duplicates a library sitting on
        // an unmounted share. The other two answers are decisions this side can
        // actually make: content on the day is the user's skip policy, and
        // anything else falls through to the resource-level judgement below.
        if matches!(observation.state, OBSERVED_UNREADABLE | OBSERVED_AMBIGUOUS) {
            return Ok(0);
        }
        // The folder holds files, and this side has no receipt for any of them,
        // so there is nothing here it can prove or improve by fetching.
        // Claiming the post anyway is what re-downloaded a JDownloader library;
        // treating the fetch as complete is what hid a real gap. Neither: the
        // observation and the assessment above are what the UI reports, and an
        // automatic pass stops here without claiming. A manual attempt ignores
        // this gate.
        if observation.state == OBSERVED_CONTENT {
            let conn = pool.get()?;
            let assessment = reassess_post(&conn, post_db_id, settings)?;
            if !assessment.state.is_claimable() {
                return Ok(0);
            }
        }
    }

    // Admit the post before publishing text or claiming media. Text must use
    // the same current toggles as media, not the round's stale snapshot.
    // Recheck switches again before each resource: a single post can take hours.
    // Folder presence is assessed only above, so this pass's first output does
    // not make it skip the remaining attachments of the same work.
    let admission_settings = {
        let conn = pool.get()?;
        pass_admission_settings(&conn, post_db_id, settings, &policy)?
    };
    let Some(admission_settings) = admission_settings else {
        return Ok(0);
    };

    // Publish content and links text files. These are metadata *about* the
    // post, never evidence that its media arrived: counting them is exactly how
    // a post with no media at all used to read as downloaded.
    let dummy_post = PawchivePost {
        id: post_id.clone(),
        user: creator_id.clone(),
        service: service.clone(),
        title: title.clone(),
        content: content.clone(),
        published: published_at.clone(),
        added: None,
        edited: None,
        file: None,
        attachments: Vec::new(),
        attachments_present: true,
        file_present: true,
        content_present: true,
        has_full: None,
    };
    // The generated documents go beside the work's media, which is where a
    // reserved resource path already points; the rendered folder is the answer
    // only for a work that has not reserved anything yet.
    let text_dir = {
        let conn = pool.get()?;
        reserved_media_dir(&conn, post_db_id, artist_dir)?.unwrap_or_else(|| post_dir.clone())
    };
    if admission_settings.download_content || admission_settings.download_links {
        let _ = publish_post_text_files(
            &text_dir,
            &dummy_post,
            &naming,
            roots,
            admission_settings.download_content,
            admission_settings.download_links,
        );
    }

    let mut downloaded_count = 0;

    // One owner for the whole pass: the lease is per row, but the pass is what a
    // reclaim supersedes, so one token revokes every claim it holds.
    let claim = DownloadClaim::new();

    for (file_idx, file) in files.into_iter().enumerate() {
        // A cancelled request stops claiming here, at the next resource. The
        // receive already in flight reaches its own safe point, and anything
        // already published stays where it is.
        if let Some(attempt_id) = policy.attempt_id() {
            let conn = pool.get()?;
            if attempt_state(&conn, attempt_id)?.as_deref() == Some("cancelled") {
                break;
            }
        }
        // `file_idx` is the position in the post's full file list, taken before
        // any row is skipped, so a retried file keeps the name it first used.
        // A resource the source has withdrawn keeps its position too, and is
        // simply not fetched.
        if !file.present_in_manifest {
            continue;
        }
        // An explicit request names its resources. A file outside that list is
        // not part of this pass, however open the assessment says it is.
        if policy.excludes_file(file.id) {
            continue;
        }
        // The automatic pass only works on rows that are still open. A manual
        // request is the user naming the resource, which is the one case where
        // re-fetching an already-delivered file is the point: the row stays
        // `done` and this pass gives it a new, non-overwriting target.
        if !policy.is_manual() && file.status != "pending" && file.status != "failed" {
            continue;
        }
        // Held back by the retry rules: claimed again only once its own wait
        // has elapsed, not on every round.
        if file
            .next_attempt_at
            .as_deref()
            .map(|next| next > Utc::now().to_rfc3339().as_str())
            .unwrap_or(false)
        {
            continue;
        }
        let PostFileRow {
            id: file_id,
            remote_path,
            file_name,
            file_type,
            expected_blake3,
            target_path: reserved_path,
            ..
        } = file;
        // Manual selections retain their frozen types. Automatic work observes
        // the current switches/range before claiming the next file. Naming and
        // already-reserved paths keep their snapshot independently.
        let current_settings = {
            let conn = pool.get()?;
            pass_admission_settings(&conn, post_db_id, settings, &policy)?
        };
        let Some(current_settings) = current_settings else {
            break;
        };
        if !wants_type(&current_settings, &file_type) {
            continue;
        }

        // The path is reserved once and reused: a retry must not renumber the
        // resource onto whatever the current template would render today.
        let target_file_path = if reserved_path.trim().is_empty() {
            let target_filename = if file_type == "image" {
                let ext = file_name.rsplit('.').next().unwrap_or("png");
                naming.format_image_filename_with_name(
                    file_idx,
                    ext,
                    &file_name,
                    &settings.image_template,
                )
            } else {
                naming.format_attachment_filename_with_index(
                    &file_name,
                    file_idx,
                    &settings.attachment_template,
                )
            };
            let desired = post_dir.join(&target_filename);
            let conn = pool.get()?;
            reserve_target_path(&conn, file_id, &desired)?
        } else {
            PathBuf::from(&reserved_path)
        };
        // A manual re-fetch must not land on the file the earlier delivery
        // published: the publish rule is create-if-absent, so writing to the
        // same name would fail — which is the correct refusal, but it would
        // make "download this again" impossible. The attempt gets a stable
        // suffixed target instead, so the user's arranged copy is kept and the
        // new attempt has a name of its own to prove itself at.
        let target_file_path = if policy.is_manual() && target_file_path.exists() {
            with_stable_suffix(
                &target_file_path,
                &attempt_target_suffix(post_db_id, file_id, policy),
            )
        } else {
            target_file_path
        };

        // Take the row for this pass. The claim records who holds it and returns
        // the fencing token that every write below is conditional on, so a
        // reclaim can revoke this pass without having to know whether it is
        // still running.
        let claim_token = {
            let conn = pool.get()?;
            match claim.claim(&conn, file_id)? {
                Some(token) => token,
                // The row disappeared; there is nothing left to fetch into.
                None => continue,
            }
        };

        // The file is already at its own target: a previous run, JDownloader,
        // or the user's own extraction put it there. That is worth recording,
        // but it is presence, not proof — the downloader refuses to overwrite
        // (correctly), so recording the refusal as a failure left the row
        // retried and re-failed every round and the post could never close.
        if target_file_path.exists() && !policy.is_manual() {
            let conn = pool.get()?;
            let settled = settle_claimed_file(
                &conn,
                file_id,
                &claim,
                claim_token,
                "status = 'done', delivery_source = 'present', error_message = NULL,
                 error_class = '', next_attempt_at = NULL, updated_at = ?",
                vec![Utc::now().to_rfc3339().into()],
            )?;
            if settled {
                downloaded_count += 1;
            }
            continue;
        }

        let remote_url = build_file_url(&remote_path, Some(&file_name));

        // Receive into a private part file next to the reserved target, then
        // hand the verified bytes to the shared publish protocol. The receive is
        // resumable and never touches the final name; the publish is the only
        // step that does, and it cannot replace anything.
        let part_path = staging_path_for(&target_file_path);
        let resume = {
            let conn = pool.get()?;
            load_resume_validator(&conn, file_id)?
        };
        // The validator is written the moment the response headers are read, not
        // when the transfer ends. A first response that drops mid-body then
        // still leaves a resumable part instead of a file the next round has to
        // fetch from zero.
        let validator_pool = Arc::clone(pool);
        let validator_owner = claim.owner().to_string();
        let mut on_validator = move |validator: &ResumeValidator| {
            if let Ok(conn) = validator_pool.get() {
                let _ = conn.execute(
                    "UPDATE kemono_files
                     SET resume_validator = ?1, expected_length = ?2, updated_at = ?3
                     WHERE id = ?4 AND claimed_by = ?5 AND claim_token = ?6",
                    params![
                        validator.encode(),
                        validator.total_length as i64,
                        Utc::now().to_rfc3339(),
                        file_id,
                        validator_owner.as_str(),
                        claim_token
                    ],
                );
            }
        };
        let outcome = download_resumable_stream(
            client,
            &remote_url,
            &part_path,
            roots,
            expected_blake3.as_deref(),
            resume.as_ref(),
            &mut on_validator,
        )
        .await;

        let updated_now = Utc::now().to_rfc3339();
        let conn = pool.get()?;
        match outcome {
            Ok(res) => {
                if let Some(validator) = res.validator.as_ref() {
                    conn.execute(
                        "UPDATE kemono_files SET resume_validator = ?1
                         WHERE id = ?2 AND claimed_by = ?3 AND claim_token = ?4",
                        params![validator.encode(), file_id, claim.owner(), claim_token],
                    )?;
                }
                // The check the plan names, and the last one before the file is
                // named. A reclaim took the row past this pass's token, so the
                // bytes on disk belong to a resource somebody else now owns:
                // stop rather than publish a file the row no longer points at.
                // The staged part is left alone for the new owner to resume.
                if !still_claimed(&conn, file_id, &claim, claim_token)? {
                    record_pawchive_event(
                        &conn,
                        "info",
                        "download",
                        None,
                        naming.user,
                        "跳过已失效的认领",
                        &format!("{remote_path}: 认领已被回收，本次不再发布"),
                    );
                    continue;
                }
                // Publishing is the only step that names the final file, and it
                // is what writes the publish ledger row this resource points at.
                let source_job_id = policy.job_scope(&post_id, file_id);
                let publish = crate::ingest_publish::publish_completed_file(
                    &conn,
                    &crate::ingest_publish::IngestPublishRequest {
                        engine: "pawchive",
                        source_job_id: &source_job_id,
                        manifest_version,
                        source_identity: &remote_path,
                        source_path: &res.staged_path,
                        expected_length: res.bytes_written,
                        expected_blake3: &res.blake3_hash,
                        target_path: &target_file_path,
                    },
                    roots,
                );
                match publish {
                    Ok(published) => {
                        // The staged part has served its purpose: the bytes are
                        // under their final name now, so leaving it behind would
                        // accumulate one `.gallery_pawchive_*.part` per delivered
                        // file in the media folder. A failed publish keeps it for
                        // the retry (the error arm below does not reach here).
                        let _ = std::fs::remove_file(&res.staged_path);
                        // Guarded as well: the check above and this write are not
                        // one step, and a reclaim landing between them must not
                        // let this pass settle the row its successor owns.
                        let settled = settle_claimed_file(
                            &conn,
                            file_id,
                            &claim,
                            claim_token,
                            "status = 'done', delivery_source = 'download',
                             expected_length = ?, expected_blake3 = ?, local_hash = ?,
                             integrity = 'verified', hash_source = 'local',
                             verified_at = ?, publish_job_id = ?, target_path = ?,
                             error_message = NULL, error_class = '',
                             next_attempt_at = NULL, resume_validator = '', updated_at = ?",
                            vec![
                                (res.bytes_written as i64).into(),
                                res.blake3_hash.clone().into(),
                                res.blake3_hash.into(),
                                updated_now.clone().into(),
                                published.job_id.into(),
                                target_file_path.to_string_lossy().to_string().into(),
                                updated_now.into(),
                            ],
                        )?;
                        if settled {
                            downloaded_count += 1;
                        }
                    }
                    Err(err) => {
                        // The bytes arrived but could not be published: a
                        // stranger holds the target, or the destination cannot
                        // be published to safely. Keep the staged file so the
                        // retry does not re-fetch it.
                        let message = err.to_string();
                        let class = classify_download_error(&message);
                        let attempt = current_attempt(&conn, file_id);
                        let next_attempt = (Utc::now()
                            + Duration::minutes(class.backoff_minutes(attempt)))
                        .to_rfc3339();
                        settle_claimed_file(
                            &conn,
                            file_id,
                            &claim,
                            claim_token,
                            "status = 'failed', error_message = ?, error_class = ?,
                             next_attempt_at = ?, updated_at = ?",
                            vec![
                                format!("publish failed: {message}").into(),
                                class.as_str().to_string().into(),
                                next_attempt.into(),
                                updated_now.clone().into(),
                            ],
                        )?;
                        record_pawchive_event(
                            &conn,
                            "error",
                            "download",
                            None,
                            naming.user,
                            "文件发布失败",
                            &format!("{}: {}", class.as_str(), message),
                        );
                    }
                }
            }
            Err(err) => {
                let message = err.to_string();
                let class = classify_download_error(&message);
                let attempt = current_attempt(&conn, file_id);
                let next_attempt = (Utc::now()
                    + Duration::minutes(backoff_minutes_for(&message, class, attempt)))
                .to_rfc3339();
                // Built before the settle because the settle consumes `message`.
                let event_message = format!("{}: {}", class.as_str(), message);
                settle_claimed_file(
                    &conn,
                    file_id,
                    &claim,
                    claim_token,
                    "status = 'failed', error_message = ?, error_class = ?,
                     next_attempt_at = ?, updated_at = ?",
                    vec![
                        message.into(),
                        class.as_str().to_string().into(),
                        next_attempt.into(),
                        updated_now.clone().into(),
                    ],
                )?;
                // One line per failed file, not per success: a single post can
                // carry dozens of files and the retry loop would flood the
                // panel list. Failures are the rare, actionable case.
                record_pawchive_event(
                    &conn,
                    "error",
                    "download",
                    None,
                    naming.user,
                    "文件下载失败",
                    &event_message,
                );
            }
        }
    }

    // Re-derive the post from what is now proven. No branch here writes a
    // completion state directly: the evaluator is the only writer of it, so a
    // post cannot be settled by a step that did not check the whole manifest.
    {
        let conn = pool.get()?;
        let assessment = reassess_post(&conn, post_db_id, settings)?;
        if let Ok(subscription_id) = conn.query_row(
            "SELECT subscription_id FROM kemono_posts WHERE id = ?1",
            params![post_db_id],
            |row| row.get::<_, i64>(0),
        ) {
            let _ = refresh_subscription_coverage(&conn, subscription_id);
            if assessment.state.is_settled() && downloaded_count > 0 {
                record_pawchive_event(
                    &conn,
                    "info",
                    "download",
                    Some(subscription_id),
                    naming.user,
                    "作品下载完成",
                    &format!("{} 个文件 {}", downloaded_count, naming.title),
                );
            }
        }
    }

    Ok(downloaded_count)
}

/// Public post page for one discovered work.
pub fn build_post_url(service: &str, user_id: &str, post_id: &str) -> String {
    format!("https://pawchive.pw/{service}/user/{user_id}/post/{post_id}")
}

/// A reversible user decision about one post manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PostDecisionAction {
    /// The user states the current manifest is already in their library.
    Confirm,
    /// The user states this post is not wanted. Not a download of it.
    Ignore,
    /// Withdraw an earlier decision and go back to the evidence.
    Revoke,
}

impl PostDecisionAction {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Confirm => "confirm",
            Self::Ignore => "ignore",
            Self::Revoke => "revoke",
        }
    }

    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "confirm" => Some(Self::Confirm),
            "ignore" => Some(Self::Ignore),
            "revoke" => Some(Self::Revoke),
            _ => None,
        }
    }
}

/// Result of recording a decision.
pub enum DecisionOutcome {
    Applied(PostAssessment),
    /// The post's manifest changed since the request was prepared, so the
    /// decision would have been about a list the user never saw.
    Conflict {
        current_manifest_version: i64,
    },
    NotFound,
}

/// Record a user decision and re-derive the post from it.
///
/// The decision is stored, never applied to the delivery evidence: revoking it
/// must put the post back exactly where the evidence says it stands, which is
/// impossible if the decision overwrote the evidence on the way in.
pub fn record_post_decision(
    conn: &Connection,
    post_db_id: i64,
    action: PostDecisionAction,
    reason: &str,
    bound_path: &str,
    expected_manifest_version: Option<i64>,
) -> Result<DecisionOutcome> {
    let current: Option<(i64, i64)> = conn
        .query_row(
            "SELECT manifest_version, subscription_id FROM kemono_posts WHERE id = ?1",
            params![post_db_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((manifest_version, subscription_id)) = current else {
        return Ok(DecisionOutcome::NotFound);
    };
    if let Some(expected) = expected_manifest_version {
        if expected != manifest_version {
            return Ok(DecisionOutcome::Conflict {
                current_manifest_version: manifest_version,
            });
        }
    }
    let now = Utc::now().to_rfc3339();
    conn.execute(
        "INSERT INTO pawchive_post_decisions
             (post_id, manifest_version, action, reason, bound_path, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            post_db_id,
            manifest_version,
            action.as_str(),
            reason,
            bound_path,
            now
        ],
    )?;
    record_decision_on_ledger(conn, post_db_id, conn.last_insert_rowid())?;
    let settings = get_pawchive_settings(conn)?;
    let assessment = reassess_post(conn, post_db_id, &settings)?;
    let _ = refresh_subscription_coverage(conn, subscription_id);
    let verb = match action {
        PostDecisionAction::Confirm => "已确认",
        PostDecisionAction::Ignore => "已忽略",
        PostDecisionAction::Revoke => "已撤销",
    };
    record_pawchive_event(
        conn,
        "info",
        "decision",
        Some(subscription_id),
        "",
        &format!("{verb} 1 篇作品"),
        reason,
    );
    Ok(DecisionOutcome::Applied(assessment))
}

/// The user accepted a local content group as this work's copy.
///
/// This is the write half of the pairing preview: `preview_day_pairing` can only
/// ever *offer* a same-day association, and this is the act that turns an offer
/// into a policy. What it records is a decision, not evidence: the resource
/// versions named here become the work's covered scope, so an automatic round
/// stops asking for them, while the acquisition ledger stays untouched — the
/// plan is explicit that accepting an old library is not a download and must not
/// be written as one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LegacyScopeOutcome {
    /// Recorded: the current manifest's members are now the covered scope.
    Accepted {
        covered: usize,
    },
    /// The work already carries an exact decision, so there is nothing to
    /// accept and the existing scope is left alone.
    Covered,
    NotFound,
}

pub fn accept_legacy_scope(
    conn: &Connection,
    post_db_id: i64,
    reason: &str,
    bound_path: &str,
) -> Result<LegacyScopeOutcome> {
    // An exact decision already answers for this work. Re-accepting would move
    // the covered set to today's manifest, which is the opposite of freezing the
    // range the user actually looked at.
    let existing: Option<String> = conn
        .query_row(
            "SELECT d.scope_kind FROM kemono_files f
             JOIN kemono_posts p ON p.id = f.post_id
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             JOIN pawchive_remote_works w
                  ON w.site_id = s.site_id AND w.service = s.service
                 AND w.creator_id = s.user_id AND w.post_id = p.post_id
             JOIN pawchive_user_decisions d ON d.work_id = w.work_id AND d.revoked_by = ''
             WHERE f.post_id = ?1 AND d.action = 'confirm'
             ORDER BY d.created_at DESC, d.decision_id DESC LIMIT 1",
            params![post_db_id],
            |row| row.get(0),
        )
        .optional()?;
    if existing.as_deref() == Some("exact") {
        return Ok(LegacyScopeOutcome::Covered);
    }
    let subscription_id: Option<i64> = conn
        .query_row(
            "SELECT subscription_id FROM kemono_posts WHERE id = ?1",
            params![post_db_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(subscription_id) = subscription_id else {
        return Ok(LegacyScopeOutcome::NotFound);
    };
    // The decision itself is the ordinary working-ledger decision, so the person
    // who accepted the range sees it the same way as any other confirmation.
    let outcome = record_post_decision(
        conn,
        post_db_id,
        PostDecisionAction::Confirm,
        reason,
        bound_path,
        None,
    )?;
    if matches!(outcome, DecisionOutcome::NotFound) {
        return Ok(LegacyScopeOutcome::NotFound);
    }
    let covered: i64 = conn.query_row(
        "SELECT (SELECT COUNT(*) FROM pawchive_decision_members m
                  WHERE m.decision_id = d.decision_id)
         FROM pawchive_user_decisions d
         JOIN pawchive_remote_works w ON w.work_id = d.work_id
         JOIN kemono_subscriptions s ON s.id = ?1
              AND s.service = w.service AND s.user_id = w.creator_id
         WHERE w.post_id = (SELECT post_id FROM kemono_posts WHERE id = ?2)
           AND d.action = 'confirm' AND d.revoked_by = ''
         ORDER BY d.created_at DESC, d.decision_id DESC LIMIT 1",
        params![subscription_id, post_db_id],
        |row| row.get(0),
    )?;
    let _ = refresh_subscription_coverage(conn, subscription_id);
    Ok(LegacyScopeOutcome::Accepted {
        covered: covered.max(0) as usize,
    })
}

/// Record one decision on the long-term ledger, at the moment it is made.
///
/// The backfill copies decisions when a post is re-discovered and when the
/// ledger is walked at startup, so a decision made in between would live only in
/// the working ledger — and a decision is exactly what dropping a subscription
/// must not take with it. Writing it here closes that window.
/// the working ledger — and a decision is exactly what dropping a subscription
/// must not take with it. Writing it here closes that window.
///
/// What the backfill cannot say, and this call can, is what the decision
/// covered: the manifest that was current when the user made it. The working row
/// has no scope column, so the copy the backfill makes is `legacy_unknown` and
/// names no members; this call records the same decision id with the set it
/// actually decided about, and a later backfill leaves that row as it found it.
fn record_decision_on_ledger(
    conn: &Connection,
    post_db_id: i64,
    working_decision_id: i64,
) -> Result<()> {
    let Some(work_id) = materialize_work_ledger(conn, post_db_id)? else {
        return Ok(());
    };
    let decision_id = decision_id_for(&work_id, working_decision_id);
    let current: Option<String> = conn
        .query_row(
            "SELECT manifest_id FROM pawchive_work_manifests
             WHERE work_id = ?1 AND is_current = 1",
            params![work_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(manifest_id) = current else {
        return Ok(());
    };
    // Members first: a scope that names a set must never exist without it.
    let members = conn.execute(
        "INSERT INTO pawchive_decision_members (decision_id, resource_version_id)
         SELECT ?1, resource_version_id FROM pawchive_manifest_members WHERE manifest_id = ?2
         ON CONFLICT DO NOTHING",
        params![decision_id, manifest_id],
    )?;
    if members == 0 {
        // A manifest that requires nothing is not a set anyone decided about,
        // and an empty scope reads the same as an unknown one. Leave it unknown
        // rather than record a scope that cannot be told from no answer.
        return Ok(());
    }
    conn.execute(
        "UPDATE pawchive_user_decisions
         SET scope_kind = 'exact', manifest_id = ?2
         WHERE decision_id = ?1 AND scope_kind = 'legacy_unknown'",
        params![decision_id, manifest_id],
    )?;
    Ok(())
}
