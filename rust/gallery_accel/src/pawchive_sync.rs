//! Subscription post listing, cursor pagination, discovery paging, and sync round orchestration.

use super::*;

/// One post as the panel's drill-down shows it.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PawchivePostRow {
    pub id: i64,
    pub post_id: String,
    pub title: Option<String>,
    pub published_at: Option<String>,
    pub day: String,
    pub source_url: String,
    pub state: String,
    pub reason: String,
    pub manifest_version: i64,
    pub required_assets: usize,
    pub proven_assets: usize,
    pub unverified_assets: usize,
    pub missing_assets: usize,
    pub deferred_assets: usize,
    pub observed_state: String,
    pub observed_files: i64,
    /// The status this post carried before completion was derived from evidence.
    pub legacy_status: Option<String>,
    /// Whether this post was settled by the date-based folder coverage rule.
    #[serde(default)]
    pub date_skipped: bool,
    /// The state of the most recent explicit request for this work, if any.
    /// Empty means the user has never asked for it by hand. It is reported
    /// separately from `state`, because "this request finished" and "this work
    /// is downloaded" are different answers.
    #[serde(default)]
    pub last_attempt_state: String,
    #[serde(default)]
    pub has_external_links: bool,
    #[serde(default)]
    pub has_direct_files: bool,
    #[serde(default)]
    pub external_link_count: usize,
    #[serde(default)]
    pub direct_file_count: usize,
}

/// Page of one subscription's posts, with the total the filter matches so a
/// page never changes the denominator the counts are read against.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PawchivePostPage {
    pub posts: Vec<PawchivePostRow>,
    pub total: usize,
    pub next_cursor: Option<i64>,
}

/// Posts of a subscription, newest first, optionally limited to one day.
pub fn list_subscription_posts(
    conn: &Connection,
    subscription_id: i64,
    day: Option<&str>,
    cursor: Option<i64>,
    limit: Option<i64>,
) -> Result<PawchivePostPage> {
    let settings = get_pawchive_settings(conn)?;
    let limit = limit
        .unwrap_or(crate::DEFAULT_ITEM_PAGE_LIMIT)
        .clamp(1, crate::MAX_ITEM_PAGE_LIMIT);
    let day_filter = day.filter(|value| !value.trim().is_empty());

    struct PostPageQueryRow {
        id: i64,
        post_id: String,
        title: Option<String>,
        published_at: Option<String>,
        day: String,
        state: String,
        reason: String,
        manifest_version: i64,
        observed_state: String,
        observed_files: i64,
        legacy_status: Option<String>,
        service: String,
        user_id: String,
        last_attempt_state: String,
        external_links: Option<String>,
    }

    let mut rows: Vec<PostPageQueryRow> = Vec::new();
    {
        let post_day = post_day_sql("p", post_source_date_column(conn));
        let mut stmt = conn.prepare(&format!(
            "SELECT p.id, p.post_id, p.title, p.published_at,
                    {post_day} AS day,
                    p.assessment_state, p.assessment_reason, p.manifest_version,
                    p.observed_state, p.observed_files, p.legacy_status,
                    s.service, s.user_id,
                    COALESCE((SELECT ap.state FROM pawchive_attempt_posts ap
                               JOIN pawchive_attempts a ON a.attempt_id = ap.attempt_id
                               WHERE ap.post_id = p.id
                               ORDER BY a.created_at DESC, a.attempt_id DESC
                               LIMIT 1), '') AS last_attempt_state,
                    p.external_links
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             WHERE p.subscription_id = ?1
               AND (?2 IS NULL OR {post_day} = ?2)
               AND (?3 IS NULL OR p.id > ?3)
             ORDER BY p.id ASC
             LIMIT ?4"
        ))?;
        let collected = stmt
            .query_map(
                params![subscription_id, day_filter, cursor, limit + 1],
                |row| {
                    Ok(PostPageQueryRow {
                        id: row.get(0)?,
                        post_id: row.get(1)?,
                        title: row.get(2)?,
                        published_at: row.get(3)?,
                        day: row.get(4)?,
                        state: row.get(5)?,
                        reason: row.get(6)?,
                        manifest_version: row.get(7)?,
                        observed_state: row.get(8)?,
                        observed_files: row.get(9)?,
                        legacy_status: row.get(10)?,
                        service: row.get(11)?,
                        user_id: row.get(12)?,
                        last_attempt_state: row.get(13)?,
                        external_links: row.get(14)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.extend(collected);
    }

    let next_cursor = if rows.len() as i64 > limit {
        rows.pop();
        rows.last().map(|row| row.id)
    } else {
        None
    };

    let evidence = load_post_evidence(conn, Some(subscription_id), None, &settings)?;
    let by_post: std::collections::HashMap<i64, PostAssessment> = evidence
        .iter()
        .map(|row| (row.post_db_id, derive_assessment(row)))
        .collect();

    let posts = rows
        .into_iter()
        .map(|row| {
            let PostPageQueryRow {
                id,
                post_id,
                title,
                published_at,
                day,
                state,
                reason,
                manifest_version,
                observed_state,
                observed_files,
                legacy_status,
                service,
                user_id,
                last_attempt_state,
                external_links,
            } = row;
            let assessment = by_post.get(&id).cloned().unwrap_or(PostAssessment {
                state: PostState::parse(&state).unwrap_or(PostState::Pending),
                reason: reason.clone(),
                date_skipped: reason == DAY_FOLDER_REASON,
                required_assets: 0,
                proven_assets: 0,
                unverified_assets: 0,
                missing_assets: 0,
                deferred_assets: 0,
                integrity_failed_assets: 0,
                external_pending: false,
            });
            let ext_links: Vec<String> = external_links
                .as_deref()
                .and_then(|json| serde_json::from_str(json).ok())
                .unwrap_or_default();
            let external_link_count = ext_links.len();
            let has_external_links = external_link_count > 0;
            let direct_file_count = assessment.required_assets;
            let has_direct_files = direct_file_count > 0;
            PawchivePostRow {
                id,
                source_url: build_post_url(&service, &user_id, &post_id),
                post_id,
                title,
                published_at,
                day,
                state: assessment.state.as_str().to_string(),
                reason: assessment.reason,
                manifest_version,
                required_assets: assessment.required_assets,
                proven_assets: assessment.proven_assets,
                unverified_assets: assessment.unverified_assets,
                missing_assets: assessment.missing_assets,
                deferred_assets: assessment.deferred_assets,
                observed_state,
                observed_files,
                legacy_status,
                date_skipped: assessment.date_skipped,
                last_attempt_state,
                has_external_links,
                has_direct_files,
                external_link_count,
                direct_file_count,
            }
        })
        .collect();

    let total: i64 = conn.query_row(
        &format!(
            "SELECT COUNT(*) FROM kemono_posts p
             WHERE p.subscription_id = ?1
               AND (?2 IS NULL OR {post_day} = ?2)",
            post_day = post_day_sql("p", post_source_date_column(conn))
        ),
        params![subscription_id, day_filter],
        |row| row.get(0),
    )?;

    Ok(PawchivePostPage {
        posts,
        total: total.max(0) as usize,
        next_cursor,
    })
}

/// What the manual list is narrowed to. The fields intersect: an unset field is
/// not a filter, and an empty one is treated as unset rather than as "matches
/// the empty string".
///
/// This is the whole-review list of §6.11, so the filter is deliberately not the
/// automatic round's policy: no `auto_since`, no completion, no ignore and no
/// confirmation gate is folded in here. A post that was downloaded, ignored and
/// confirmed still has to be listed, because the user is choosing what to fetch
/// explicitly and those states only inform the choice.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PostListFilter {
    /// Exact normalized publication day, `YYYY-MM-DD`.
    #[serde(default)]
    pub day: Option<String>,
    /// Exact assessment state as the evaluator last persisted it (`pending`,
    /// `verified`, …). See [`StablePostRow::state`] for why the filter and the
    /// row read the same column.
    #[serde(default)]
    pub state: Option<String>,
    /// Case-insensitive substring of the title or of the source post id.
    #[serde(default)]
    pub search: Option<String>,
    /// The artist the subscription is bound to.
    #[serde(default)]
    pub artist_id: Option<i64>,
}

/// One row of the complete manual list.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StablePostRow {
    /// `kemono_posts.id`, the stable local key.
    pub id: i64,
    /// The source's post id as text. It is opaque and can exceed the range a
    /// JSON number or a 32-bit key holds, so it never becomes an integer here.
    pub post_id: String,
    pub subscription_id: i64,
    pub artist_id: Option<i64>,
    pub artist_name: Option<String>,
    pub title: Option<String>,
    /// Normalized day, `''` when the source gave no readable date. A row with no
    /// day sorts after every dated row rather than reading as the year zero.
    pub day: String,
    /// Public URL to the post on the source service.
    pub source_url: String,
    /// The state the evaluator last persisted. The list reads the column and the
    /// `state` filter matches the same column, so a row that is shown as
    /// `pending` is exactly the set `state=pending` returns; the alternative —
    /// filtering one value and displaying a freshly derived one — can show a row
    /// the filter did not ask for.
    ///
    /// The asset counts below *are* re-derived from the live evidence through the
    /// evaluator's own predicates, because they need the file joins and the
    /// current type selection; they can therefore be newer than the state.
    pub state: String,
    pub manifest_version: i64,
    pub required_assets: usize,
    pub proven_assets: usize,
    pub missing_assets: usize,
    /// Whether this post was settled by the date-based folder coverage rule.
    #[serde(default)]
    pub date_skipped: bool,
    /// Whether an explicit manual attempt has ever named this work. Recorded
    /// because the panel has to be able to say so; it never removes the row and
    /// never disables fetching it again.
    pub manual_attempt: bool,
    #[serde(default)]
    pub has_external_links: bool,
    #[serde(default)]
    pub has_direct_files: bool,
    #[serde(default)]
    pub external_link_count: usize,
    #[serde(default)]
    pub direct_file_count: usize,
}

/// A page of the complete manual list, with the exact total and the identity of
/// the view it was read from.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StablePostPage {
    pub posts: Vec<StablePostRow>,
    /// The whole filtered set, not the page. Plan §7.1: 总数来自同一快照.
    pub total: usize,
    /// Opaque continuation token, `None` on the last page.
    pub next_cursor: Option<String>,
    /// Digest of the filtered set's identity, computed in the same read as
    /// `total` and the page, so the three describe one consistent view. A caller
    /// refetching without a write sees the same revision; the revision changes
    /// when the set actually changes.
    pub snapshot_revision: String,
    /// The page is not the last one — the filter matched more rows than this
    /// page holds. `total` still counts all of them.
    pub truncated: bool,
}

/// Cursor encoding for [`list_artist_posts_page`].
///
/// `v1` <US> day <US> row id, where `<US>` is the ASCII unit separator
/// (`\u{1f}`), which a `YYYY-MM-DD` day cannot contain. It encodes the *sort
/// key* of the last row of the page — the day, then the id — and not the row id
/// alone, because the walk is ordered by (undated-last, day, id): a bare id
/// cursor would skip or repeat rows as soon as a post is discovered with a date
/// that inserts in the middle of the order.
///
/// The `v1` prefix is what makes the string opaque: callers must not parse it,
/// and a later encoding can be introduced without the old one being mistaken for
/// it. A cursor that does not decode is an error rather than an empty result — a
/// silently ignored cursor would restart the walk forever.
const STABLE_CURSOR_VERSION: &str = "v1";
const STABLE_CURSOR_SEP: char = '\u{1f}';

fn encode_stable_cursor(day: &str, id: i64) -> String {
    format!("{STABLE_CURSOR_VERSION}{STABLE_CURSOR_SEP}{day}{STABLE_CURSOR_SEP}{id}")
}

fn decode_stable_cursor(raw: &str) -> Result<(String, i64)> {
    let body = raw
        .strip_prefix(STABLE_CURSOR_VERSION)
        .and_then(|rest| rest.strip_prefix(STABLE_CURSOR_SEP))
        .ok_or_else(|| anyhow!("unrecognised list cursor"))?;
    let (day, id) = body
        .rsplit_once(STABLE_CURSOR_SEP)
        .ok_or_else(|| anyhow!("unrecognised list cursor"))?;
    let id = id
        .parse::<i64>()
        .map_err(|_| anyhow!("unrecognised list cursor"))?;
    Ok((day.to_string(), id))
}

/// Where a day sits in the list's order: undated posts sort after dated ones.
fn stable_undated_rank(day: &str) -> u8 {
    if day.is_empty() {
        1
    } else {
        0
    }
}

/// Whether `row`'s key sorts strictly after the cursor key.
fn stable_key_after(row_day: &str, row_id: i64, cursor_day: &str, cursor_id: i64) -> bool {
    (stable_undated_rank(row_day), row_day, row_id)
        > (stable_undated_rank(cursor_day), cursor_day, cursor_id)
}

/// One row of the filtered set, before the page is cut out of it.
struct StableSetRow {
    id: i64,
    subscription_id: i64,
    post_id: String,
    title: Option<String>,
    day: String,
    state: String,
    manifest_version: i64,
    artist_id: Option<i64>,
    artist_name: Option<String>,
    service: String,
    user_id: String,
    external_links: Option<String>,
}

/// Identity digest of the filtered set, over its ordered rows.
///
/// The day is part of it on purpose: two rows with the same id and manifest
/// version but a different day are not the same walk, because the day is the
/// first half of the sort key and moving it moves the row's position.
fn stable_snapshot_revision(rows: &[StableSetRow]) -> String {
    let mut hasher = blake3::Hasher::new();
    hasher.update(&(rows.len() as u64).to_le_bytes());
    for row in rows {
        hasher.update(&row.id.to_le_bytes());
        hasher.update(&row.manifest_version.to_le_bytes());
        hasher.update(&(row.day.len() as u64).to_le_bytes());
        hasher.update(row.day.as_bytes());
    }
    format!("snap_{}", hasher.finalize().to_hex())
}

/// The complete manual list: every discovered work in scope, newest order not
/// required, pageable to the end with a stable cursor and an exact total.
///
/// Plan §6.11 / §7.1: 新增跨日期的稳定游标列表和精确总数，分页/虚拟列表可到尾，
/// 无前 N 条永久截断；游标包含排序键和固定 upper bound/snapshot revision，总数来自
/// 同一快照. The list must not be an automatic-policy filter wearing a list's
/// shape — see [`PostListFilter`].
///
/// The ids of every work the filter matches, in the list's own order.
///
/// This is what lets "全选当前筛选" be one request: the panel sends the filter it
/// is displaying instead of the tens of thousands of ids the filter stands for,
/// and the server resolves them while it holds the same view the list did. The
/// ids never travel back to the browser — they go straight into
/// [`record_selection`] — because the point of the filter form is that the
/// selection *is* the filter, not a list of ids the caller could have sent.
///
/// The order is the manual list's (`undated last, day, id`), so a selection taken
/// from a filtered view shares the order of the view it came from.
///
/// `limit` bounds what a single request can freeze. `truncated` says the filter
/// matched more than that, which the caller has to report: a selection that
/// silently holds a prefix of the filter is a request the user cannot verify.
pub fn list_filtered_post_ids(
    conn: &Connection,
    subscription_id: Option<i64>,
    filter: &PostListFilter,
    limit: usize,
) -> Result<(Vec<i64>, bool)> {
    let non_empty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let day_filter = non_empty(filter.day.as_deref());
    let state_filter = non_empty(filter.state.as_deref());
    let search_filter = non_empty(filter.search.as_deref());
    // The day is `post_day_sql`, the same expression the calendar groups by, so
    // a day the panel offers is a day this filter can find. Reading the raw
    // `substr(published_at, 1, 10)` here would disagree on a work whose local
    // day differs from its source stamp's day, and the day would look empty.
    let post_day = post_day_sql("p", post_source_date_column(conn));
    // `有网盘` shows what the row badge shows: see `has_external_links_sql`.
    let has_external_links = has_external_links_sql("p");
    let mut stmt = conn.prepare(&format!(
        "SELECT p.id
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         WHERE (?1 IS NULL OR p.subscription_id = ?1)
           AND (?2 IS NULL OR {post_day} = ?2)
           AND (?3 IS NULL
                OR (?3 = 'missing' AND p.assessment_state IN ('pending', 'partial', 'unverified', 'unavailable', 'unassessed', 'external'))
                OR (?3 = 'completed' AND p.assessment_state IN ('verified', 'confirmed'))
                OR (?3 = 'external' AND (p.assessment_state = 'external'
                                         OR {has_external_links} = 1))
                OR (?3 NOT IN ('missing', 'completed', 'external') AND p.assessment_state = ?3))
           AND (?4 IS NULL
                OR instr(lower(COALESCE(p.title, '')), lower(?4)) > 0
                OR instr(lower(p.post_id), lower(?4)) > 0
                OR p.id = CAST(?4 AS INTEGER))
           AND (?5 IS NULL OR s.artist_id = ?5)
         ORDER BY CASE WHEN {post_day} = '' THEN 1 ELSE 0 END,
                      {post_day} ASC,
                      p.id ASC
         LIMIT ?6"
    ))?;
    let rows = stmt
        .query_map(
            params![
                subscription_id,
                day_filter,
                state_filter,
                search_filter,
                filter.artist_id,
                // One past the cap, so the read itself answers whether the filter
                // holds more than the request may freeze.
                (limit as i64).saturating_add(1)
            ],
            |row| row.get::<_, i64>(0),
        )?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    let truncated = rows.len() > limit;
    let mut ids = rows;
    ids.truncate(limit);
    Ok((ids, truncated))
}

/// Read in one pass. The filtered set is read once, ordered by
/// (undated-last, day, id); `total`, `snapshot_revision` and the page are all cut
/// from that one result, which is what makes them one consistent view instead of
/// three queries that can disagree with each other while a discovery round
/// writes. Only the page's rows get the heavier per-row evidence read.
///
/// The cursor is a sort key, not an offset and not a bare row id, so a post
/// discovered mid-walk neither repeats a row that was already shown nor skips
/// one that follows: it either sorts before the cursor (behind the walk) or
/// after it (ahead of the walk, where it belongs).
///
/// Sorting is oldest day first, matching [`claimable_posts`] and the id-ordered
/// pagination of [`list_subscription_posts`]; a post with no readable date goes
/// last.
///
/// `limit` is clamped to `1..=200` with a default of 50, as elsewhere.
pub fn list_artist_posts_page(
    conn: &Connection,
    subscription_id: Option<i64>,
    cursor: Option<&str>,
    limit: Option<i64>,
    filter: PostListFilter,
) -> Result<StablePostPage> {
    let settings = get_pawchive_settings(conn)?;
    let limit = limit
        .unwrap_or(crate::DEFAULT_ITEM_PAGE_LIMIT)
        .clamp(1, crate::MAX_ITEM_PAGE_LIMIT);
    let non_empty = |value: Option<&str>| {
        value
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    };
    let day_filter = non_empty(filter.day.as_deref());
    let state_filter = non_empty(filter.state.as_deref());
    let search_filter = non_empty(filter.search.as_deref());
    let cursor_key = cursor.map(decode_stable_cursor).transpose()?;
    // The row's own day is `post_day_sql`, the same expression the calendar
    // groups by: the day a work is listed under and the day a filter matches
    // have to be one fact, or the list shows a day its own filter cannot find.
    let post_day = post_day_sql("p", post_source_date_column(conn));
    // `有网盘` shows what the row badge shows: see `has_external_links_sql`.
    let has_external_links = has_external_links_sql("p");

    let mut rows: Vec<StableSetRow> = Vec::new();
    {
        let mut stmt = conn.prepare(&format!(
            "SELECT p.id, p.subscription_id, p.post_id, p.title,
                    {post_day},
                    p.assessment_state, p.manifest_version,
                    s.artist_id, a.name, s.service, s.user_id,
                    p.external_links
             FROM kemono_posts p
             JOIN kemono_subscriptions s ON s.id = p.subscription_id
             LEFT JOIN artists a ON a.id = s.artist_id
             WHERE (?1 IS NULL OR p.subscription_id = ?1)
               AND (?2 IS NULL OR {post_day} = ?2)
               AND (?3 IS NULL
                    OR (?3 = 'missing' AND p.assessment_state IN ('pending', 'partial', 'unverified', 'unavailable', 'unassessed', 'external'))
                    OR (?3 = 'completed' AND p.assessment_state IN ('verified', 'confirmed'))
                    OR (?3 = 'external' AND (p.assessment_state = 'external'
                                             OR {has_external_links} = 1))
                    OR (?3 NOT IN ('missing', 'completed', 'external') AND p.assessment_state = ?3))
               AND (?4 IS NULL
                    OR instr(lower(COALESCE(p.title, '')), lower(?4)) > 0
                    OR instr(lower(p.post_id), lower(?4)) > 0
                    OR p.id = CAST(?4 AS INTEGER))
               AND (?5 IS NULL OR s.artist_id = ?5)
             ORDER BY CASE WHEN {post_day} = '' THEN 1 ELSE 0 END,
                      {post_day} ASC,
                      p.id ASC"
        ))?;
        let collected = stmt
            .query_map(
                params![
                    subscription_id,
                    day_filter,
                    state_filter,
                    search_filter,
                    filter.artist_id
                ],
                |row| {
                    Ok(StableSetRow {
                        id: row.get(0)?,
                        subscription_id: row.get(1)?,
                        post_id: row.get(2)?,
                        title: row.get(3)?,
                        day: row.get::<_, Option<String>>(4)?.unwrap_or_default(),
                        state: row.get(5)?,
                        manifest_version: row.get(6)?,
                        artist_id: row.get(7)?,
                        artist_name: row.get(8)?,
                        service: row.get(9)?,
                        user_id: row.get(10)?,
                        external_links: row.get(11)?,
                    })
                },
            )?
            .collect::<rusqlite::Result<Vec<_>>>()?;
        rows.extend(collected);
    }

    let total = rows.len();
    let snapshot_revision = stable_snapshot_revision(&rows);

    // The set is already ordered, so the cursor is applied as "the first row
    // that sorts strictly after it" rather than as SQL predicate: the two are
    // the same rule, and this one cannot disagree with the ORDER BY above.
    let start = match cursor_key.as_ref() {
        Some((cursor_day, cursor_id)) => rows
            .iter()
            .position(|row| stable_key_after(&row.day, row.id, cursor_day, *cursor_id))
            .unwrap_or(rows.len()),
        None => 0,
    };
    let end = start.saturating_add(limit as usize).min(rows.len());
    let page = &rows[start.min(end)..end];
    let truncated = end < rows.len();
    let next_cursor = if truncated {
        page.last()
            .map(|row| encode_stable_cursor(&row.day, row.id))
    } else {
        None
    };

    let mut posts = Vec::with_capacity(page.len());
    let mut manual_attempt_stmt = conn.prepare(
        "SELECT 1 AS recorded FROM pawchive_attempt_posts ap
         JOIN pawchive_attempts a ON a.attempt_id = ap.attempt_id
         WHERE ap.post_id = ?1 AND a.intent = 'manual' LIMIT 1",
    )?;
    for row in page {
        // The counts come from the one evidence query the evaluator uses, so the
        // list cannot invent a second definition of "已有". It is read per page
        // row instead of per subscription so the cost stays proportional to the
        // page and not to the creator's whole backlog.
        let assessment = load_post_evidence(conn, None, Some(row.id), &settings)?
            .first()
            .map(derive_assessment);
        let manual_attempt: Option<i64> = manual_attempt_stmt
            .query_row(params![row.id], |row| row.get(0))
            .optional()?;
        let manual_attempt = manual_attempt.is_some();
        let ext_links: Vec<String> = row
            .external_links
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default();
        let external_link_count = ext_links.len();
        let has_external_links = external_link_count > 0;
        let direct_file_count = assessment.as_ref().map_or(0, |value| value.required_assets);
        let has_direct_files = direct_file_count > 0;
        posts.push(StablePostRow {
            id: row.id,
            post_id: row.post_id.clone(),
            subscription_id: row.subscription_id,
            artist_id: row.artist_id,
            artist_name: row.artist_name.clone(),
            title: row.title.clone(),
            day: row.day.clone(),
            source_url: build_post_url(&row.service, &row.user_id, &row.post_id),
            state: row.state.clone(),
            manifest_version: row.manifest_version,
            required_assets: assessment.as_ref().map_or(0, |value| value.required_assets),
            proven_assets: assessment.as_ref().map_or(0, |value| value.proven_assets),
            missing_assets: assessment.as_ref().map_or(0, |value| value.missing_assets),
            date_skipped: assessment.as_ref().is_some_and(|value| value.date_skipped),
            manual_attempt,
            has_external_links,
            has_direct_files,
            external_link_count,
            direct_file_count,
        });
    }

    Ok(StablePostPage {
        posts,
        total,
        next_cursor,
        snapshot_revision,
        truncated,
    })
}

/// Per-subscription view of the evaluator's results.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubscriptionSummary {
    pub subscription_id: i64,
    pub enabled: bool,
    pub discovery_complete: bool,
    pub gap_days: usize,
    pub posts: usize,
    pub verified: usize,
    /// Posts settled by a user decision (confirmed or ignored), never an
    /// automatic verification.
    pub confirmed: usize,
    pub ignored: usize,
    pub partial: usize,
    pub pending: usize,
    pub unverified: usize,
    pub unavailable: usize,
    pub external: usize,
    pub not_required: usize,
    pub unresolved: usize,
}

/// Summarise one subscription from the derived state.
pub fn subscription_summary(
    conn: &Connection,
    subscription_id: i64,
) -> Result<SubscriptionSummary> {
    let (enabled, discovery_complete): (i64, i64) = conn.query_row(
        "SELECT enabled, discovery_complete FROM kemono_subscriptions WHERE id = ?1",
        params![subscription_id],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let mut summary = SubscriptionSummary {
        subscription_id,
        enabled: enabled != 0,
        discovery_complete: discovery_complete != 0,
        gap_days: pending_coverage_day_count(conn, subscription_id)?,
        ..SubscriptionSummary::default()
    };
    let mut stmt = conn.prepare(
        "SELECT assessment_state, COUNT(*) FROM kemono_posts
         WHERE subscription_id = ?1
         GROUP BY assessment_state",
    )?;
    let counts = stmt
        .query_map(params![subscription_id], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<rusqlite::Result<Vec<_>>>()?;
    for (state, count) in counts {
        let count = count.max(0) as usize;
        summary.posts += count;
        match PostState::parse(&state) {
            Some(PostState::Verified) => summary.verified += count,
            Some(PostState::Confirmed) => summary.confirmed += count,
            Some(PostState::Ignored) => summary.ignored += count,
            Some(PostState::Partial) => summary.partial += count,
            Some(PostState::Pending) => summary.pending += count,
            Some(PostState::Unverified) => summary.unverified += count,
            Some(PostState::Unavailable) => summary.unavailable += count,
            Some(PostState::External) => summary.external += count,
            Some(PostState::NotRequired) => summary.not_required += count,
            // `unassessed` has no decision behind it and no proof ahead of it,
            // so it is counted unresolved until a re-evaluation places it.
            None => summary.unresolved += count,
        }
    }
    summary.unresolved += summary.partial
        + summary.pending
        + summary.unverified
        + summary.unavailable
        + summary.external;
    Ok(summary)
}

/// One observation-only reconciliation pass.
///
/// Reads the library, re-derives every post and saves the results. It claims no
/// post, downloads nothing and never rewrites delivery evidence, so it is safe
/// to run on a live library to answer "what do we actually have?" before any
/// download is authorised.
pub fn run_pawchive_reconcile(
    pool: &Arc<DbPool>,
    trigger: &str,
    only_subscription: Option<i64>,
) -> Result<serde_json::Value> {
    let run_id: i64 = {
        let conn = pool.get()?;
        conn.execute(
            "INSERT INTO pawchive_reconcile_runs (trigger, started_at, status)
             VALUES (?1, ?2, 'running')",
            params![trigger, Utc::now().to_rfc3339()],
        )?;
        conn.last_insert_rowid()
    };

    // A failed observation round reports itself rather than reading as "the
    // library was checked and nothing was there".
    let (observed, truncated, observation_failed) =
        match observe_post_folders(pool, only_subscription) {
            ObserveRound::Completed {
                observed,
                truncated,
            } => (observed, truncated, false),
            ObserveRound::Failed => (0, false, true),
        };

    let (digest, recovered) = {
        let conn = pool.get()?;
        let recovered = reset_stale_download_claims(&conn, 60)?;
        // A delivery that the library has since indexed is linked here, and a
        // file the user moved keeps its identity instead of reading as lost.
        let _ = link_evidence_to_items(&conn);
        let digest = match only_subscription {
            Some(subscription_id) => reassess_subscription_posts(&conn, subscription_id)?,
            None => reassess_all_posts(&conn)?,
        };
        (digest, recovered)
    };

    let summary = serde_json::json!({
        "run_id": run_id,
        "trigger": trigger,
        "observed_folders": observed,
        "observation_truncated": truncated,
        "observation_failed": observation_failed,
        "recovered_claims": recovered,
        "posts": digest.posts,
        "verified": digest.verified,
        "confirmed": digest.confirmed,
        "partial": digest.partial,
        "pending": digest.pending,
        "unverified": digest.unverified,
        "unavailable": digest.unavailable,
        "external": digest.external,
        "not_required": digest.not_required,
        "claimable": digest.claimable(),
    });

    {
        let conn = pool.get()?;
        conn.execute(
            "UPDATE pawchive_reconcile_runs
             SET finished_at = ?1, status = 'finished', summary = ?2
             WHERE id = ?3",
            params![Utc::now().to_rfc3339(), summary.to_string(), run_id],
        )?;
        record_pawchive_event(
            &conn,
            "info",
            "reconcile",
            None,
            "",
            &format!(
                "对账完成：{} 篇作品，{} 篇已核验、{} 篇已确认、{} 篇待核验、{} 篇可下载",
                digest.posts,
                digest.verified,
                digest.confirmed,
                digest.unverified,
                digest.claimable()
            ),
            trigger,
        );
    }

    Ok(summary)
}

/// Fetch posts directly from the Pawchive API.
pub async fn fetch_creator_posts_api(
    client: &reqwest::Client,
    service: &str,
    user_id: &str,
    offset: u64,
) -> Result<Vec<PawchivePost>> {
    fetch_creator_posts_api_at(client, PAWCHIVE_API_BASE, service, user_id, offset).await
}

async fn fetch_creator_posts_api_at(
    client: &reqwest::Client,
    base: &str,
    service: &str,
    user_id: &str,
    offset: u64,
) -> Result<Vec<PawchivePost>> {
    let url = format!("{base}/{service}/user/{user_id}?o={offset}");
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "Gallery/1.0 (NAS; Pure-Rust)")
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("failed to send request to pawchive api")?;

    if !resp.status().is_success() {
        return Err(anyhow!(
            "pawchive api returned HTTP status {}",
            resp.status()
        ));
    }

    // `reqwest` is built without its `json` feature (see Cargo.toml), so the
    // body is read as text and parsed here instead of via `Response::json`.
    let body = resp
        .text()
        .await
        .context("failed to read posts response body")?;
    let posts: Vec<PawchivePost> =
        serde_json::from_str(&body).context("failed to parse posts json")?;
    Ok(posts)
}

/// Fetch one post's full record.
///
/// The list endpoint is a summary, and the fields it may leave out are the
/// resource fields: a summary that omits `attachments` has not said the post
/// has none. This is the request that answers the question, and its response is
/// read through the same presence-aware `PawchivePost`, so a detail that itself
/// omits a resource field is still recorded as an incomplete observation rather
/// than as an empty work.
pub async fn fetch_post_detail(
    client: &reqwest::Client,
    service: &str,
    user_id: &str,
    post_id: &str,
) -> Result<PawchivePost> {
    fetch_post_detail_at(client, PAWCHIVE_API_BASE, service, user_id, post_id).await
}

pub(super) async fn fetch_post_detail_at(
    client: &reqwest::Client,
    base: &str,
    service: &str,
    user_id: &str,
    post_id: &str,
) -> Result<PawchivePost> {
    let url = format!("{base}/{service}/user/{user_id}/post/{post_id}");
    let resp = client
        .get(&url)
        .header(reqwest::header::USER_AGENT, "Gallery/1.0 (NAS; Pure-Rust)")
        .header(reqwest::header::ACCEPT, "application/json")
        .send()
        .await
        .context("failed to send request to pawchive api")?;
    if !resp.status().is_success() {
        return Err(anyhow!(
            "pawchive api returned HTTP status {}",
            resp.status()
        ));
    }
    let body = resp
        .text()
        .await
        .context("failed to read post detail body")?;
    // The endpoint answers with the post object itself. A one-element array is
    // accepted too, because the summary shape is the same record and a source
    // that wraps it that way is not saying anything different.
    if let Ok(post) = serde_json::from_str::<PawchivePost>(&body) {
        return Ok(post);
    }
    let mut posts: Vec<PawchivePost> =
        serde_json::from_str(&body).context("failed to parse post detail json")?;
    posts
        .pop()
        .ok_or_else(|| anyhow!("pawchive post detail response was an empty list"))
}

/// The works whose detail this round should read, oldest gap first.
///
/// Only a work whose current observation is incomplete is queued: a summary
/// that already listed the whole resource set has been answered, and re-reading
/// it every round would make the detail cost grow with the library instead of
/// with what is still unproven. The order follows the same oldest-first rule as
/// the download queue, so a bounded round makes progress on the oldest works.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PostAwaitingDetail {
    pub post_db_id: i64,
    pub subscription_id: i64,
    pub service: String,
    pub user_id: String,
    pub post_id: String,
}

pub(super) fn posts_awaiting_detail(
    conn: &Connection,
    limit: usize,
) -> Result<Vec<PostAwaitingDetail>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.subscription_id, s.service, s.user_id, p.post_id
         FROM kemono_posts p
         JOIN kemono_subscriptions s ON s.id = p.subscription_id
         LEFT JOIN pawchive_remote_works w
                ON w.site_id = s.site_id AND w.service = s.service
               AND w.creator_id = s.user_id AND w.post_id = p.post_id
         LEFT JOIN pawchive_work_manifests m
                ON m.work_id = w.work_id AND m.is_current = 1
         WHERE COALESCE(m.completeness, '') != 'complete'
         ORDER BY CASE WHEN COALESCE(NULLIF(p.published_at, ''), p.created_at, '') = ''
                       THEN 1 ELSE 0 END,
                  substr(COALESCE(NULLIF(p.published_at, ''), p.created_at, ''), 1, 10) ASC,
                  p.id ASC
         LIMIT ?1",
    )?;
    let rows = stmt
        .query_map(params![limit as i64], |row| {
            Ok(PostAwaitingDetail {
                post_db_id: row.get(0)?,
                subscription_id: row.get(1)?,
                service: row.get(2)?,
                user_id: row.get(3)?,
                post_id: row.get(4)?,
            })
        })?
        // Propagated, not filtered: a dropped row meant that work was never
        // asked about again, so its manifest stayed unproven forever with no
        // failure recorded anywhere.
        .collect::<rusqlite::Result<Vec<_>>>()?;
    Ok(rows)
}

/// Read the detail of the works whose summary left the resource set open.
///
/// Runs after the page walk and before anything is claimed: a work whose
/// manifest is still unproven must not be judged complete on the strength of a
/// summary, and must not be claimed either. A detail that fails is left for the
/// next round — the failed read is an error, not evidence that the work is
/// empty — and the work keeps whatever manifest it already had.
///
/// Returns `(read, failed)`.
async fn fetch_pending_post_details(
    pool: &Arc<DbPool>,
    client: &reqwest::Client,
) -> Result<(usize, usize)> {
    let pending = {
        let conn = pool.get()?;
        posts_awaiting_detail(&conn, PAWCHIVE_DETAIL_MAX_POSTS)?
    };
    if pending.is_empty() {
        return Ok((0, 0));
    }
    let mut read = 0usize;
    let mut failed = 0usize;
    for PostAwaitingDetail {
        post_db_id,
        subscription_id,
        service,
        user_id,
        post_id,
    } in pending
    {
        // No database connection is held across the network await: the batch was
        // read above, and the write below checks one out for its own scope.
        match fetch_post_detail(client, &service, &user_id, &post_id).await {
            Ok(post) => {
                let conn = pool.get()?;
                // The detail is recorded through the same discovery write as a
                // summary page: it carries the resource fields (or honestly
                // reports that it omitted them), so the observation either
                // becomes the work's complete manifest or is stored beside the
                // current one.
                if record_discovered_posts(&conn, subscription_id, &[post], None).is_err() {
                    failed += 1;
                    continue;
                }
                let completeness = conn
                    .query_row(
                        "SELECT COALESCE(m.completeness, '')
                         FROM pawchive_remote_works w
                         LEFT JOIN pawchive_work_manifests m
                                ON m.work_id = w.work_id AND m.is_current = 1
                         WHERE w.post_id = ?1 AND w.service = ?2 AND w.creator_id = ?3
                         LIMIT 1",
                        params![post_id, service, user_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .unwrap_or_default();
                if completeness == ManifestCompleteness::Complete.as_str() {
                    read += 1;
                } else {
                    // The detail answered, but its own resource set was open
                    // (or the source did not return one). It stays queued for a
                    // later round rather than being counted as read.
                    failed += 1;
                }
                let _ = post_db_id;
            }
            Err(_) => {
                failed += 1;
            }
        }
    }
    if read > 0 || failed > 0 {
        let conn = pool.get()?;
        record_pawchive_event(
            &conn,
            "info",
            "detail",
            None,
            "",
            &format!("核对 {read} 篇作品详情"),
            &if failed > 0 {
                format!("{failed} 篇详情未取到，留待下一轮")
            } else {
                String::new()
            },
        );
    }
    Ok((read, failed))
}

/// Outcome of one paged discovery walk.
/// `next_offset` resumes where this walk stopped: the value is only as large
/// as the pages actually read, so a round interrupted by the page budget (a
/// huge backlog) does not restart from offset 0 on the next round. A walk
/// that reached the list tail reports `complete` so the cursor resets.
pub struct DiscoveryOutcome {
    pub posts: Vec<PawchivePost>,
    pub next_offset: u64,
    pub complete: bool,
}

/// One successfully read page, handed to the caller as soon as it arrives.
///
/// Discovery is persisted per page rather than at the end of the walk: a
/// failure on the twentieth page used to discard the nineteen that had already
/// been fetched, and the next round re-read them from the old cursor. The
/// newest page is swept every round and must not move the history cursor, so
/// `cursor_after` is `None` for it.
pub struct DiscoveryPage {
    pub offset: u64,
    pub posts: Vec<PawchivePost>,
    /// The cursor to store once this page is persisted, or `None` when the page
    /// must leave it alone.
    pub cursor_after: Option<u64>,
    /// This page ended the creator's list.
    pub reached_tail: bool,
}

/// Walk the creator's post list from the head, up to the page budget.
///
/// Kept for the manual full sweep and the unit tests: the scheduled round uses
/// `fetch_creator_posts_resumed` so a huge backlog continues where the last
/// round stopped instead of re-walking the head every time.
pub async fn fetch_creator_posts_paged(
    client: &reqwest::Client,
    service: &str,
    user_id: &str,
    max_pages: usize,
) -> Result<Vec<PawchivePost>> {
    Ok(
        fetch_creator_posts_resumed(client, service, user_id, 0, max_pages, &mut |_| Ok(()))
            .await?
            .posts,
    )
}

/// Resume a paged discovery walk from a stored offset cursor.
///
/// `start_offset` is the offset the previous round persisted. Each page is
/// handed to `on_page` the moment it arrives, so the caller persists it and
/// moves the cursor before the next request goes out; a later failure then
/// costs only the pages that were never read. The newest page is always read,
/// so a creator whose history is longer than the budget still reports new work
/// every round, and the page just before the cursor is re-read as an overlap
/// check because the list shifts between rounds.
pub async fn fetch_creator_posts_resumed(
    client: &reqwest::Client,
    service: &str,
    user_id: &str,
    start_offset: u64,
    max_pages: usize,
    on_page: &mut (dyn FnMut(DiscoveryPage) -> Result<()> + Send),
) -> Result<DiscoveryOutcome> {
    fetch_creator_posts_resumed_at(
        client,
        PAWCHIVE_API_BASE,
        service,
        user_id,
        start_offset,
        max_pages,
        on_page,
    )
    .await
}

pub(super) async fn fetch_creator_posts_resumed_at(
    client: &reqwest::Client,
    base: &str,
    service: &str,
    user_id: &str,
    start_offset: u64,
    max_pages: usize,
    on_page: &mut (dyn FnMut(DiscoveryPage) -> Result<()> + Send),
) -> Result<DiscoveryOutcome> {
    let start_page = (start_offset / PAWCHIVE_POSTS_PAGE_SIZE) as usize;
    let mut collected: Vec<PawchivePost> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    let mut complete = false;
    let mut pages_read = 0usize;
    let budget = max_pages.max(1);

    let mut absorb = |posts: Vec<PawchivePost>, into: &mut Vec<PawchivePost>| {
        let mut fresh = Vec::new();
        for post in posts {
            if seen.insert(post.id.clone()) {
                fresh.push(post);
            }
        }
        into.extend(fresh.iter().cloned());
        fresh
    };

    // A resumed walk is somewhere in the middle of a long history, and the list
    // is paged by offset rather than by a snapshot, so new posts land at the
    // head while the walk is away. Read the head first, or a creator with more
    // history than the budget would never report anything new until the walk
    // finally reaches the end. This page never moves the cursor: the history
    // walk below owns it.
    if start_page > 0 {
        let posts = fetch_creator_posts_api_at(client, base, service, user_id, 0).await?;
        let fresh = absorb(posts, &mut collected);
        on_page(DiscoveryPage {
            offset: 0,
            posts: fresh,
            cursor_after: None,
            reached_tail: false,
        })?;
    }

    // The page before the cursor is re-read as an overlap: posts can be added
    // to or removed from the middle of the list between rounds, so the boundary
    // is not guaranteed to be where the last round left it.
    if start_page > 0 {
        let overlap_page = start_page - 1;
        let offset = overlap_page as u64 * PAWCHIVE_POSTS_PAGE_SIZE;
        let posts = fetch_creator_posts_api_at(client, base, service, user_id, offset).await?;
        let fresh = absorb(posts, &mut collected);
        on_page(DiscoveryPage {
            offset,
            posts: fresh,
            cursor_after: None,
            reached_tail: false,
        })?;
    }

    for page in start_page..start_page + budget {
        let offset = page as u64 * PAWCHIVE_POSTS_PAGE_SIZE;
        let posts = fetch_creator_posts_api_at(client, base, service, user_id, offset).await?;
        let received = posts.len();
        pages_read += 1;
        let fresh = absorb(posts, &mut collected);
        // A short page is the end of the list; anything else means the walk
        // continues from the next offset.
        let reached_tail = received < PAWCHIVE_POSTS_PAGE_SIZE as usize;
        let cursor_after = if reached_tail {
            0
        } else {
            offset + PAWCHIVE_POSTS_PAGE_SIZE
        };
        on_page(DiscoveryPage {
            offset,
            posts: fresh,
            cursor_after: Some(cursor_after),
            reached_tail,
        })?;
        if reached_tail {
            complete = true;
            break;
        }
    }
    let next_offset = if complete {
        0
    } else {
        (start_page + pages_read) as u64 * PAWCHIVE_POSTS_PAGE_SIZE
    };
    Ok(DiscoveryOutcome {
        posts: collected,
        next_offset,
        complete,
    })
}

/// Run a complete Pawchive discovery and download sync cycle.
///
/// Takes the pool rather than a connection: every database phase checks out a
/// connection for its own scope so none is held across the network awaits.
///
/// `only_subscription` narrows the round to one subscription, for the per-artist
/// 检查缺失 button. The unbound case (`None`) is the whole-library round every
/// other caller runs.
const AUTO_NETDISK_BASELINE_KEY: &str = "pawchive_auto_netdisk_baseline_v1";

pub(super) fn auto_netdisk_baseline(conn: &Connection) -> Result<i64> {
    conn.execute(
        "INSERT OR IGNORE INTO app_settings(key, value, updated_at)
         VALUES (?1, (SELECT COALESCE(MAX(id), 0) FROM kemono_posts), strftime('%s','now'))",
        [AUTO_NETDISK_BASELINE_KEY],
    )?;
    Ok(conn.query_row(
        "SELECT CAST(value AS INTEGER) FROM app_settings WHERE key = ?1",
        [AUTO_NETDISK_BASELINE_KEY],
        |row| row.get(0),
    )?)
}

pub(super) fn dispatch_subscription_netdisk(
    conn: &Connection,
    roots: &MediaRoots,
    baseline: i64,
) -> Result<usize> {
    use crate::netdisk::{
        bridge_move_is_isolated, create_bridge_task, default_bridge_identity,
        latest_bridge_session, load_bridge_task, load_netdisk_settings, netdisk_is_disconnected,
        resolve_netdisk_staging_directory, submit_bridge_task,
    };

    let settings = load_netdisk_settings(conn)?;
    if !settings.enabled || !settings.bridge_configured || netdisk_is_disconnected(conn)? {
        return Ok(0);
    }
    let bridge_id = default_bridge_identity();
    let Some(session_id) = latest_bridge_session(conn, bridge_id)? else {
        return Ok(0);
    };
    if settings.auto_start && !bridge_move_is_isolated(conn, bridge_id)? {
        return Ok(0);
    }
    let staging_dir = if settings.staging_dir.trim().is_empty() {
        resolve_netdisk_staging_directory(roots)
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_default()
    } else {
        settings.staging_dir.clone()
    };
    let has_external_links = has_external_links_sql("p");
    let posts: Vec<(i64, i64, Option<String>)> = {
        let mut stmt = conn.prepare(&format!(
            "SELECT p.id, p.manifest_version,
                    (SELECT t.task_id FROM netdisk_bridge_tasks t WHERE t.post_id=p.id
                     ORDER BY t.created_at DESC, t.rowid DESC LIMIT 1)
             FROM kemono_posts p JOIN kemono_subscriptions s ON s.id=p.subscription_id
             WHERE p.id > ?1 AND s.enabled=1 AND s.mode='auto'
               AND (s.since_date IS NULL OR s.since_date=''
                    OR substr(COALESCE(NULLIF(p.published_at,''),p.created_at),1,10)>=s.since_date)
               AND {has_external_links} = 1
               AND NOT EXISTS (
                   SELECT 1 FROM netdisk_bridge_tasks t WHERE t.post_id=p.id
                     AND (t.state != 'issued' OR t.manifest_version != p.manifest_version)
               )
             ORDER BY p.id LIMIT 32",
        ))?;
        let rows = stmt
            .query_map([baseline], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<rusqlite::Result<_>>()?;
        rows
    };
    let mut created = 0;
    for (post_id, manifest_version, existing_task) in posts {
        let task = if let Some(task_id) = existing_task {
            let Some(task) = load_bridge_task(conn, &task_id)? else {
                continue;
            };
            if task.state != crate::netdisk::BRIDGE_TASK_REGISTERED
                || task.manifest_version != manifest_version
            {
                continue;
            }
            task
        } else {
            match create_bridge_task(conn, bridge_id, &session_id, post_id, &[], None) {
                Ok(task) => {
                    created += 1;
                    task
                }
                Err(_) => {
                    record_pawchive_event(
                        conn,
                        "warn",
                        "netdisk",
                        None,
                        "",
                        "网盘任务登记失败",
                        &post_id.to_string(),
                    );
                    continue;
                }
            }
        };
        if settings.auto_start && submit_bridge_task(conn, &task, &staging_dir).is_err() {
            record_pawchive_event(
                conn,
                "warn",
                "netdisk",
                None,
                "",
                "网盘任务待开始",
                &post_id.to_string(),
            );
        }
    }
    if created > 0 {
        record_pawchive_event(
            conn,
            "info",
            "netdisk",
            None,
            "",
            &format!("新建 {created} 个网盘任务"),
            "scheduled",
        );
    }
    Ok(created)
}

pub async fn run_pawchive_sync(
    pool: Arc<DbPool>,
    roots: MediaRoots,
    trigger: SyncTrigger,
    only_subscription: Option<i64>,
) -> Result<serde_json::Value> {
    let (settings, subscriptions) = {
        let conn = pool.get()?;
        (get_pawchive_settings(&conn)?, list_subscriptions(&conn)?)
    };
    // The master switch stops new claims: with it off a sync creates no
    // discovery request, no download task and no ingest. Already-downloading
    // files are left to finish on their own.
    if !settings.enabled {
        let conn = pool.get()?;
        record_pawchive_event(
            &conn,
            "warn",
            "round",
            None,
            "",
            "未执行：订阅下载总开关未开启",
            trigger.as_str(),
        );
        return Ok(serde_json::json!({
            "enabled": false,
            "trigger": trigger.as_str(),
            "synced_subscriptions": 0,
            "discovered_posts": 0,
            "downloaded_files": 0,
            "missing_days": 0,
        }));
    }
    let netdisk_baseline =
        if settings.auto_netdisk && trigger.downloads() && only_subscription.is_none() {
            let conn = pool.get()?;
            Some(auto_netdisk_baseline(&conn)?)
        } else {
            None
        };
    let client = pawchive_http_client()?;

    let mut synced_subs = 0;
    let mut discovered_posts = 0;
    let mut downloaded_files = 0;
    // The gap as the round found it, before anything was fetched. The figure the
    // round reports at the end is recomputed from the final state.
    let mut missing_days_before = 0;
    // A discovery failure is not a zero gap: the panel has to be able to tell
    // "nothing is missing" from "nothing was checked".
    let mut discovery_failures = 0;
    let mut discovery_incomplete = false;

    // A manual round and a check both cover every subscription: `enabled` gates
    // the automatic rounds, and a check is diagnostics the user asked for, so
    // silently skipping a disabled subscription would look like a dead button.
    // A scheduled round also honours the subscription's mode: a manual
    // subscription is still walkable by the operator, but the interval must not
    // treat it as one of its own.
    let covers_all = matches!(trigger, SyncTrigger::Manual | SyncTrigger::Check);
    let in_round = |sub: &&SubscriptionRecord| {
        only_subscription.is_none_or(|id| sub.id == id)
            && (covers_all || (sub.enabled && sub.mode.is_auto()))
    };
    for sub in subscriptions.iter().filter(in_round) {
        // Every round resumes the history walk from the stored cursor and reads
        // the newest page on top of it. A manual round used to restart from
        // offset 0, so a creator whose history exceeds the page budget could
        // never finish a sweep by pressing the button: it re-read the same head
        // pages and the cursor never moved. Reading the head first is what keeps
        // new posts visible while the walk is still in the back catalogue.
        let start_offset = sub.discovery_offset.max(0) as u64;
        let artist_name = sub.artist_name.clone().unwrap_or_default();
        let since_date = sub.since_date.clone();
        let mut page_posts = 0usize;
        let mut pages_saved = 0usize;
        // Each page is recorded, and the cursor moved, as soon as it arrives.
        // Collecting the whole walk first meant a failure on the last page
        // discarded every page already fetched and the next round started from
        // the old cursor again.
        let mut on_page = |page: DiscoveryPage| -> Result<()> {
            let conn = pool.get()?;
            page_posts +=
                record_discovered_posts(&conn, sub.id, &page.posts, since_date.as_deref())?;
            if let Some(next) = page.cursor_after {
                conn.execute(
                    "UPDATE kemono_subscriptions
                     SET discovery_offset = ?1, discovery_complete = ?2, updated_at = ?3
                     WHERE id = ?4",
                    params![
                        next as i64,
                        page.reached_tail as i64,
                        Utc::now().to_rfc3339(),
                        sub.id
                    ],
                )?;
            }
            pages_saved += 1;
            Ok(())
        };
        let posts_res = fetch_creator_posts_resumed(
            &client,
            &sub.service,
            &sub.user_id,
            start_offset,
            PAWCHIVE_DISCOVERY_MAX_PAGES,
            &mut on_page,
        )
        .await;
        let conn = pool.get()?;
        match posts_res {
            Ok(outcome) => {
                synced_subs += 1;
                discovered_posts += page_posts;
                let count = page_posts;
                // Re-derive every post of the subscription: a round can add an
                // attachment to a post that was already settled, and only the
                // evaluator may decide what that means for its state.
                let _ = reassess_subscription(&conn, sub.id, &settings);
                // Coverage is recomputed from the ledger (not incremented): the
                // same call also persists the durable per-day rows, so the next
                // `list_subscriptions` already carries fresh diagnostics.
                let _ = refresh_subscription_coverage(&conn, sub.id);
                let now = Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "UPDATE kemono_subscriptions
                     SET discovery_offset = ?1, last_discovery_at = ?2,
                         discovery_complete = ?3, last_error = NULL, updated_at = ?2
                     WHERE id = ?4",
                    params![
                        outcome.next_offset as i64,
                        now,
                        outcome.complete as i64,
                        sub.id
                    ],
                );
                // The count comes from its own aggregate: `pending_coverage_days`
                // is windowed at 90 days and would report exactly 90 for any
                // larger backlog. The list is still used for which days to
                // quote, where only the newest few matter.
                let pending_count = pending_coverage_day_count(&conn, sub.id).unwrap_or(0);
                missing_days_before += pending_count;
                // A walk that stopped at the page budget has not seen the whole
                // history, so "no gap" for this subscription only means "nothing
                // found yet". The round says so instead of reporting it as done.
                discovery_incomplete |= !outcome.complete;
                let pending = pending_coverage_days(&conn, sub.id).unwrap_or_default();
                record_pawchive_event(
                    &conn,
                    "info",
                    "discover",
                    Some(sub.id),
                    &artist_name,
                    &format!("发现 {} 个帖子，{} 天待补", count, pending_count),
                    &coverage_gap_summary(&pending),
                );
            }
            Err(e) => {
                let err_str = e.to_string();
                discovery_failures += 1;
                // Pages that were read before the failure are already in the
                // ledger and the cursor already moved past them, so the round
                // reports them instead of pretending nothing happened.
                discovered_posts += page_posts;
                let now = Utc::now().to_rfc3339();
                let _ = conn.execute(
                    "UPDATE kemono_subscriptions SET last_error = ?1, updated_at = ?2 WHERE id = ?3",
                    params![err_str, now, sub.id],
                );
                record_pawchive_event(
                    &conn,
                    "error",
                    "discover",
                    Some(sub.id),
                    &artist_name,
                    "检查失败",
                    &format!("{err_str}（已保存 {pages_saved} 页，{page_posts} 个帖子）"),
                );
            }
        }
    }

    // A round killed mid-file leaves rows written as `downloading` that no
    // later round would ever pick up again, because the queue only claims
    // `pending` and `failed`. Recover them before claiming anything.
    {
        let conn = pool.get()?;
        let recovered = reset_stale_download_claims(&conn, 60)?;
        // A publish whose ledger write never landed is restored from the publish
        // ledger instead of being fetched again.
        let restored = reconcile_published_files(&conn, &roots).unwrap_or(0);
        if recovered > 0 || restored > 0 {
            let _ = reassess_all_posts(&conn);
            record_pawchive_event(
                &conn,
                "warn",
                "round",
                None,
                "",
                &format!("{recovered} 个中断的下载已重新排队，{restored} 个已发布文件已按账本恢复"),
                trigger.as_str(),
            );
        }
    }

    // The summary pages answered "which posts exist"; they are allowed to omit
    // the resource fields, which is exactly what a manifest needs. Read the
    // details of the works whose summary left that open before anything is
    // judged: a work is not empty because a summary did not mention its files,
    // and it must not be claimed on the strength of a summary either. The pass
    // records its own event; a failure here is not fatal to the round, and the
    // works it could not read stay unproven instead of being read as empty.
    if let Err(error) = fetch_pending_post_details(&pool, &client).await {
        let conn = pool.get()?;
        record_pawchive_event(
            &conn,
            "warn",
            "detail",
            None,
            "",
            "作品详情核对未完成",
            &error.to_string(),
        );
    }

    // Before any fetching: read the folders of the posts that still need
    // something. Runs for every trigger, so a check reports reality instead of
    // the queue. The finding is a candidate, not a verdict — the evaluator
    // turns it into 待核验 rather than into a download.
    let (observed, observation_truncated) = match observe_post_folders(&pool, only_subscription) {
        ObserveRound::Completed {
            observed,
            truncated,
        } => (observed, truncated),
        ObserveRound::Failed => {
            let conn = pool.get()?;
            record_pawchive_event(
                &conn,
                "warn",
                "round",
                None,
                "",
                "作品目录核对未完成",
                "本轮未能读取本地目录，不据此判定缺失",
            );
            // Reporting zero here would say the folders were checked and empty.
            // They were not checked, and the queue must not read that as a gap.
            (0, false)
        }
    };
    if observed > 0 || observation_truncated {
        let conn = pool.get()?;
        record_pawchive_event(
            &conn,
            "info",
            "round",
            None,
            "",
            &format!("已核对 {observed} 篇作品的本地目录"),
            trigger.as_str(),
        );
        if observation_truncated {
            record_pawchive_event(
                &conn,
                "warn",
                "round",
                None,
                "",
                &format!("本轮只核对了前 {PAWCHIVE_OBSERVE_MAX_POSTS} 篇，其余留待下一轮"),
                trigger.as_str(),
            );
        }
    }

    // Drain every post the evaluator still considers claimable. This used to
    // stop at 10 per round, which for a few hundred posts meant the backlog
    // needed dozens of rounds (at the six hour interval, about a week) and every
    // day still queued read as missing in the meantime — the panel was
    // reporting the queue, not the library.
    //
    // `assessment_state` replaces `status = 'done'` as the queue's filter: a
    // post is claimed because the evaluator found something missing that it may
    // fetch, not because a column says it is unfinished.
    //
    // The order is the gap's own date, oldest first, so a round that cannot
    // finish the backlog still closes the days the panel has been reporting as
    // missing the longest instead of whichever post happens to have the lowest
    // row id. A post whose date could not be read sorts after every dated one
    // rather than ahead of them, and the id keeps the order total so two rounds
    // walk the queue the same way.
    //
    // Across subscriptions the order is a rotation rather than one global date
    // line: a creator with a large backlog used to own every slot until it was
    // chewed through, so the other creators' gaps never closed. One post per
    // subscription per turn keeps the global oldest-first rule *inside* each
    // subscription and gives the others their slot.
    // Read by the truncation warning below; stays zero for a check-only round,
    // which claims nothing and so cannot leave anything behind.
    let mut outstanding_posts = 0usize;
    let rows: Vec<(i64, String)> = if trigger.downloads() {
        let conn = pool.get()?;
        // A scheduled round only claims subscriptions in `auto` mode: `manual`
        // means the user takes works from the list themselves, and the round
        // must not decide for them. The two operator triggers still cover
        // everything, because pressing 同步 or 检查 is the user asking for every
        // subscription once — per plan section 6.11 the mode constrains
        // automatic claiming, not the button the operator just pressed.
        let auto_only = matches!(trigger, SyncTrigger::Scheduled);
        // A per-artist round is a 检查缺失: it walks one subscription's history
        // and recomputes coverage, and it must not start downloading on a
        // button that says otherwise. Scoping the claim phase separately would
        // also need a scoped admission count, which is the shape a check does
        // not have.
        let queue = if only_subscription.is_some() {
            RoundQueue {
                posts: Vec::new(),
                outstanding: 0,
            }
        } else {
            schedule_round_order(&conn, auto_only)?
        };
        outstanding_posts = queue.outstanding;
        // The target directory is the subscription's own column; taking it from
        // the same rows the scheduler returned would mean a second copy of the
        // admission predicate that could drift from the first.
        let mut directories: std::collections::HashMap<i64, String> =
            std::collections::HashMap::new();
        let mut stmt = conn.prepare("SELECT id, target_dir FROM kemono_subscriptions")?;
        let pairs = stmt
            .query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<rusqlite::Result<Vec<(i64, String)>>>()?;
        for (id, target) in pairs {
            directories.insert(id, target);
        }
        queue
            .posts
            .into_iter()
            .map(|scheduled| {
                let target = directories
                    .get(&scheduled.subscription_id)
                    .cloned()
                    .unwrap_or_default();
                (scheduled.post_id, target)
            })
            .collect()
    } else {
        Vec::new()
    };

    // A queue that was cut is reported, not silently short. Without this the
    // operator sees a round that processed two thousand posts and has no way to
    // learn that two hundred thousand are still waiting behind the
    // per-subscription cap — the backlog would look like it had been cleared.
    if outstanding_posts > rows.len() {
        let conn = pool.get()?;
        record_pawchive_event(
            &conn,
            "warn",
            "round",
            None,
            "",
            &format!(
                "本轮队列取了 {} / {outstanding_posts} 篇待办（每位画师每轮上限 {PAWCHIVE_ROUND_MAX_POSTS_PER_SUBSCRIPTION} 篇），其余留待下一轮",
                rows.len()
            ),
            trigger.as_str(),
        );
    }

    for (post_id, target_dir) in rows {
        let artist_dir = PathBuf::from(&target_dir);
        if let Ok(cnt) = download_post_files_with_settings(
            &pool,
            &client,
            post_id,
            &artist_dir,
            &roots,
            &settings,
        )
        .await
        {
            downloaded_files += cnt;
        }
    }
    let netdisk_jobs = if let Some(baseline) = netdisk_baseline {
        let conn = pool.get()?;
        dispatch_subscription_netdisk(&conn, &roots, baseline)?
    } else {
        0
    };

    // The gap is recomputed here, from the final state, and never reused from
    // the discovery loop: that figure was taken before the downloads ran, so the
    // panel reported a backlog the round had already cleared.
    let (final_missing_days, covered_subscriptions) = {
        let conn = pool.get()?;
        // Link what the library has indexed since, so a delivered file resolves
        // to its media item before the day counts are read.
        let _ = link_evidence_to_items(&conn);
        let mut total = 0usize;
        let mut covered = 0usize;
        for sub in subscriptions.iter().filter(in_round) {
            total += pending_coverage_day_count(&conn, sub.id).unwrap_or(0);
            covered += 1;
        }
        (total, covered)
    };

    {
        let conn = pool.get()?;
        let discovery_note = if discovery_failures > 0 {
            format!("，{discovery_failures} 个订阅发现失败")
        } else if discovery_incomplete {
            "，历史尚未遍历完".to_string()
        } else {
            String::new()
        };
        record_pawchive_event(
            &conn,
            "info",
            "round",
            None,
            "",
            &if trigger.downloads() {
                format!(
                    "同步完成：{synced_subs}/{covered_subscriptions} 个订阅，新增 {discovered_posts} 个帖子，下载 {downloaded_files} 个文件，{final_missing_days} 天待补{discovery_note}"
                )
            } else {
                format!(
                    "检查完成：{synced_subs}/{covered_subscriptions} 个订阅，新增 {discovered_posts} 个帖子，{final_missing_days} 天待补{discovery_note}"
                )
            },
            trigger.as_str(),
        );
    }

    Ok(serde_json::json!({
        "enabled": true,
        "trigger": trigger.as_str(),
        "synced_subscriptions": synced_subs,
        "covered_subscriptions": covered_subscriptions,
        "total_subscriptions": subscriptions.len(),
        "discovery_failures": discovery_failures,
        "discovery_incomplete": discovery_incomplete,
        "discovered_posts": discovered_posts,
        "downloaded_files": downloaded_files,
        "netdisk_jobs": netdisk_jobs,
        // The figure the round started from, kept only for comparison.
        "missing_days_before": missing_days_before,
        // What is still missing once the round has finished.
        "missing_days": final_missing_days,
    }))
}

/// Last observed state of the manual/background sync, reported by
/// `GET /api/pawchive/status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PawchiveSyncStatus {
    pub running: bool,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// What the running (or last) round was: a check and a sync share this slot
    /// and this status, so the panel needs the token to label it correctly.
    pub trigger: Option<String>,
    pub last_error: Option<String>,
    /// The summary the last finished round returned. Named `last_summary`
    /// rather than `last_result` because the static-UI contract test forbids
    /// the `last_result` token in the frontend bundle (it is the legacy hash
    /// worker's raw payload key); the frontend reads this field directly.
    pub last_summary: Option<serde_json::Value>,
}

static PAWCHIVE_SYNC_STATUS: OnceLock<Mutex<PawchiveSyncStatus>> = OnceLock::new();
/// When the last round started, for the scheduled-loop interval check. Kept as
/// an in-process `Instant` so a restart runs at most one catch-up round.
static PAWCHIVE_LAST_ROUND: OnceLock<Mutex<Option<std::time::Instant>>> = OnceLock::new();

fn pawchive_sync_state() -> &'static Mutex<PawchiveSyncStatus> {
    PAWCHIVE_SYNC_STATUS.get_or_init(|| Mutex::new(PawchiveSyncStatus::default()))
}

fn pawchive_last_round() -> &'static Mutex<Option<std::time::Instant>> {
    PAWCHIVE_LAST_ROUND.get_or_init(|| Mutex::new(None))
}

/// True when a scheduled round is due: nothing has run in this process yet, or
/// the configured interval has elapsed since the last round started.
pub fn pawchive_round_due(interval_hours: u32) -> bool {
    let interval = std::time::Duration::from_secs(u64::from(interval_hours.max(1)) * 3600);
    let guard = match pawchive_last_round().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    match *guard {
        None => true,
        Some(last) => last.elapsed() >= interval,
    }
}

/// Current sync status. A poisoned mutex means a previous sync panicked; the
/// status is reported as not-running so the next sync can start again.
pub fn pawchive_sync_status() -> PawchiveSyncStatus {
    match pawchive_sync_state().lock() {
        Ok(guard) => guard.clone(),
        Err(poisoned) => poisoned.into_inner().clone(),
    }
}

/// Claim the single sync slot. Returns false when a sync is already running,
/// so a repeated button press cannot start a second overlapping round.
pub fn try_begin_pawchive_sync(trigger: SyncTrigger) -> bool {
    let mut guard = match pawchive_sync_state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    if guard.running {
        return false;
    }
    guard.running = true;
    guard.started_at = Some(Utc::now().to_rfc3339());
    guard.finished_at = None;
    guard.trigger = Some(trigger.as_str().to_string());
    guard.last_error = None;
    // The interval is measured from the start of a round, so a round that runs
    // long does not immediately queue another one behind it.
    match pawchive_last_round().lock() {
        Ok(mut last) => *last = Some(std::time::Instant::now()),
        Err(poisoned) => *poisoned.into_inner() = Some(std::time::Instant::now()),
    }
    true
}

/// Record the outcome of a finished sync and release the slot.
pub fn finish_pawchive_sync(result: Result<serde_json::Value>) {
    let mut guard = match pawchive_sync_state().lock() {
        Ok(guard) => guard,
        Err(poisoned) => poisoned.into_inner(),
    };
    guard.running = false;
    guard.finished_at = Some(Utc::now().to_rfc3339());
    match result {
        Ok(value) => {
            guard.last_error = None;
            guard.last_summary = Some(value);
        }
        Err(error) => {
            // Keep the previous result: a failed round does not erase the last
            // successful counts the user is looking at.
            guard.last_error = Some(error.to_string());
        }
    }
}
