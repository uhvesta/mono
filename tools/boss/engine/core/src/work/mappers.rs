use super::*;

pub(crate) fn collect_rows<T>(
    rows: rusqlite::MappedRows<'_, impl FnMut(&Row<'_>) -> rusqlite::Result<T>>,
) -> Result<Vec<T>> {
    let mut values = Vec::new();
    for row in rows {
        values.push(row?);
    }
    Ok(values)
}

/// Parse a text column into `T`, surfacing a parse failure as a loud
/// `FromSqlConversionFailure` keyed to the source column index rather
/// than silently dropping the value. Used by the `map_*` row mappers for
/// enum/string columns whose value set is enforced in engine code, not by
/// SQL: an out-of-set value is data corruption that must fail the read.
fn parse_text_column<T>(col: usize, raw: &str) -> rusqlite::Result<T>
where
    T: std::str::FromStr,
    T::Err: Into<Box<dyn std::error::Error + Send + Sync>>,
{
    raw.parse::<T>().map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            col,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e)),
        )
    })
}

/// Deserialize an optional JSON text column into `T`, falling back to
/// `T::default()` when the column is NULL or the JSON fails to parse.
/// Centralizes the `as_deref().and_then(from_str).unwrap_or_default()`
/// chain shared by the `map_*` mappers and a few ad-hoc row readers.
/// Note `serde_json::Value::default()` is `Value::Null`, so this covers
/// the `Value` call sites that previously wrote `unwrap_or(Value::Null)`.
pub(crate) fn deserialize_json_or_default<T: serde::de::DeserializeOwned + Default>(json: Option<&str>) -> T {
    json.and_then(|s| serde_json::from_str(s).ok()).unwrap_or_default()
}

pub(crate) fn map_product(row: &Row<'_>) -> rusqlite::Result<Product> {
    let external_tracker_kind: Option<String> = row.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty());
    let external_tracker_config: Option<serde_json::Value> = row
        .get::<_, Option<String>>(11)?
        .and_then(|s| serde_json::from_str(&s).ok());
    Ok(Product {
        id: row.get(0)?,
        name: row.get(1)?,
        slug: row.get(2)?,
        description: row.get(3)?,
        repo_remote_url: row.get(4)?,
        design_repo: row.get::<_, Option<String>>(12)?.filter(|s| !s.is_empty()),
        docs_repo: row.get::<_, Option<String>>(13)?.filter(|s| !s.is_empty()),
        worker_branch_prefix: row.get::<_, Option<String>>(14)?.filter(|s| !s.is_empty()),
        status: row.get(5)?,
        created_at: row.get(6)?,
        updated_at: row.get(7)?,
        default_model: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
        dispatch_preamble: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
        external_tracker_kind,
        external_tracker_config,
        editorial_rules: row
            .get::<_, Option<String>>(15)?
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok()),
        default_driver: row.get::<_, Option<String>>(16)?.filter(|s| !s.is_empty()),
    })
}

pub(crate) fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    let status_raw: String = row.get(6)?;
    Ok(Project {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        slug: row.get(3)?,
        description: row.get(4)?,
        goal: row.get(5)?,
        status: parse_text_column(6, &status_raw)?,
        priority: row.get(7)?,
        created_at: row.get(8)?,
        updated_at: row.get(9)?,
        last_status_actor: row.get(10)?,
        design_doc_repo_remote_url: row.get(11)?,
        design_doc_branch: row.get(12)?,
        design_doc_path: row.get(13)?,
        short_id: row.get(14)?,
    })
}

/// Maps a `work_comments` row. Column order must match the SELECT used by
/// every comment query in `work/comments.rs`:
/// `id, artifact_kind, artifact_id, doc_version, anchor_json, body, author,
///  status, status_actor, last_resolved_with, plain_text_projection_version,
///  created_at, updated_at, dismissed_at`.
/// A corrupt `anchor_json` degrades to an empty anchor (the comment still
/// lists; it simply orphans on the next resolve) rather than failing the
/// whole list query.
pub(crate) fn map_comment(row: &Row<'_>) -> rusqlite::Result<WorkComment> {
    let anchor_json: String = row.get(4)?;
    let anchor: CommentAnchor = serde_json::from_str(&anchor_json).unwrap_or_default();
    Ok(WorkComment {
        id: row.get(0)?,
        artifact_kind: row.get(1)?,
        artifact_id: row.get(2)?,
        doc_version: row.get(3)?,
        anchor,
        body: row.get(5)?,
        author: row.get(6)?,
        status: row.get(7)?,
        status_actor: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
        last_resolved_with: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
        plain_text_projection_version: row.get(10)?,
        created_at: row.get(11)?,
        updated_at: row.get(12)?,
        dismissed_at: row.get::<_, Option<String>>(13)?.filter(|s| !s.is_empty()),
    })
}

/// Maps a `magic_wand_dispatches` row. Column order must match the SELECT in
/// `work/comments.rs`:
/// `id, comment_id, artifact_kind, artifact_id, doc_version, status,
///  input_tokens, output_tokens, result_md, error_kind, anchor_warning,
///  created_at, resolved_at`.
pub(crate) fn map_magic_wand_dispatch(row: &Row<'_>) -> rusqlite::Result<MagicWandDispatch> {
    Ok(MagicWandDispatch {
        id: row.get(0)?,
        comment_id: row.get(1)?,
        artifact_kind: row.get(2)?,
        artifact_id: row.get(3)?,
        doc_version: row.get(4)?,
        status: row.get(5)?,
        input_tokens: row.get(6)?,
        output_tokens: row.get(7)?,
        result_md: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
        error_kind: row.get::<_, Option<String>>(9)?.filter(|s| !s.is_empty()),
        anchor_warning: row.get::<_, i64>(10)? != 0,
        created_at: row.get(11)?,
        resolved_at: row.get::<_, Option<String>>(12)?.filter(|s| !s.is_empty()),
        chore_id: row.get::<_, Option<String>>(13)?.filter(|s| !s.is_empty()),
    })
}

pub(crate) fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let effort_raw: Option<String> = row.get(19)?;
    let effort_level = match effort_raw.as_deref() {
        None | Some("") => None,
        // The column is constrained in code, not by SQL. A row carrying an
        // out-of-set value is engine-side data corruption: surface it loudly
        // rather than silently dropping the level.
        Some(s) => Some(parse_text_column::<EffortLevel>(19, s)?),
    };
    let kind_raw: String = row.get(3)?;
    let kind = parse_text_column::<TaskKind>(3, &kind_raw)?;
    let status_raw: String = row.get(6)?;
    let status = parse_text_column::<TaskStatus>(6, &status_raw)?;
    Ok(Task {
        id: row.get(0)?,
        product_id: row.get(1)?,
        project_id: row.get(2)?,
        kind,
        name: row.get(4)?,
        description: row.get(5)?,
        status,
        ordinal: row.get(7)?,
        pr_url: row.get(8)?,
        deleted_at: row.get(9)?,
        created_at: row.get(10)?,
        updated_at: row.get(11)?,
        autostart: row.get::<_, i64>(12)? != 0,
        last_status_actor: row.get(13)?,
        priority: row.get(14)?,
        created_via: row.get(15)?,
        blocked_reason: row.get(16)?,
        blocked_attempt_id: row.get(17)?,
        repo_remote_url: row.get(18)?,
        effort_level,
        model_override: row.get::<_, Option<String>>(20)?.filter(|s| !s.is_empty()),
        ci_attempt_budget: row.get(21)?,
        ci_attempts_used: row.get(22)?,
        short_id: row.get(23)?,
        // The multi-signal projection is built from the
        // `task_blocked_signals` side table by the engine's signal-
        // aggregation path (`merge-conflict-handling-in-review.md` §Q2),
        // which lands in a later phase. Until then the wire field is
        // always empty; consumers fall back to the scalar
        // `blocked_reason` / `blocked_attempt_id` cache above.
        blocked_signals: Vec::new(),
        ci_required_state: row.get::<_, Option<String>>(24)?.filter(|s| !s.is_empty()),
        review_required_state: row.get::<_, Option<String>>(25)?.filter(|s| !s.is_empty()),
        ci_required_detail: row.get::<_, Option<String>>(26)?.filter(|s| !s.is_empty()),
        review_required_detail: row.get::<_, Option<String>>(27)?.filter(|s| !s.is_empty()),
        pr_state_polled_at: row.get::<_, Option<String>>(28)?.filter(|s| !s.is_empty()),
        merge_queue_state: row.get::<_, Option<String>>(29)?.filter(|s| !s.is_empty()),
        driver: row.get::<_, Option<String>>(30)?.filter(|s| !s.is_empty()),
        // Standard queries omit the external_ref columns; the T8 methods
        // use map_task_with_external_ref which adds columns 31-35.
        // T1 schema columns; populated by T8 WorkDb methods when the migration
        // has run. Until then the protocol field carries None.
        external_ref: None,
        parent_task_id: None,
        // completed_at is not in the base 31-column SELECT; extended
        // mappers (map_task_with_parent_and_provenance and
        // map_task_with_external_ref_parent_source_and_provenance) read it
        // from the appended column in their respective SELECTs.
        completed_at: None,
        // Revision projections are computed in attach_revision_projections
        // (get_work_tree); they are never stored as columns.
        revision_seq: None,
        revision_parent_pr_url: None,
        // Computed by attach_in_progress_revision_flag in get_work_tree.
        has_in_progress_revision: false,
        // Populated by map_task_with_source_automation_id when the SELECT
        // includes that column; None for all standard task queries.
        source_automation_id: None,
        // review_cycle and last_reviewed_sha are not included in the standard
        // SELECT to avoid bumping all mapper column indices. Use
        // WorkDb::get_task_review_cycle_state for targeted reads (P992 task 9).
        review_cycle: 0,
        last_reviewed_sha: None,
        // Computed by attach_ai_reviewing_flag in get_work_tree; always false
        // in single-item query paths (get_work_item etc.) where the derived
        // projection is not computed.
        ai_reviewing: false,
        // Resolved per-task doc-link state for project-less docs-backed
        // items (investigations). Computed by attach_task_doc_links in
        // get_work_tree from the task's `doc_*` columns; None everywhere
        // else (the standard SELECT omits those columns).
        doc_link_state: None,
        // Followup provenance; populated by map_task_with_parent_and_provenance
        // and map_task_with_external_ref_parent_source_and_provenance when the
        // SELECT includes those columns. None in all standard query paths.
        origin_task_short_id: None,
        origin_pr_number: None,
    })
}

/// Like [`map_task`] but reads a trailing `source_automation_id` column
/// at index 31 (after the `driver` column at 30). Used by
/// `list_tasks_for_automation` so produced tasks carry their provenance
/// on the wire.
pub(crate) fn map_task_with_source_automation_id(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    task.source_automation_id = row.get::<_, Option<String>>(31)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Like [`map_task`] but also reads a trailing `parent_task_id` column
/// (index 31, i.e. appended right after `driver` at 30). Used by
/// `query_task` / `get_work_item_by_short_id`; the wider
/// [`map_task_with_external_ref_and_parent`] reads `parent_task_id` from
/// index 36 instead (after the external-ref columns at 31-35).
pub(crate) fn map_task_with_parent(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    task.parent_task_id = row.get::<_, Option<String>>(31)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Like [`map_task_with_parent`] but also reads `origin_task_short_id`
/// (index 32), `origin_pr_number` (index 33), and `completed_at`
/// (index 34). Used by `query_task`, `get_work_item_by_short_id`, and
/// `list_chores` when those columns are appended to the standard SELECT.
pub(crate) fn map_task_with_parent_and_provenance(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task_with_parent(row)?;
    task.origin_task_short_id = row.get(32)?;
    task.origin_pr_number = row.get(33)?;
    task.completed_at = row.get::<_, Option<String>>(34)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Derives the canonical browser URL from an external-ref kind and
/// canonical_id at read time. For GitHub (`kind="github"`) the
/// canonical_id encodes `"owner/repo#number"`, which maps to
/// `https://github.com/owner/repo/issues/number`. Returns an empty
/// string for unknown trackers so callers can still surface the ref.
pub(crate) fn derive_external_ref_web_url(kind: &str, canonical_id: &str) -> String {
    if kind == "github"
        && let Some((repo, number)) = canonical_id.rsplit_once('#')
    {
        return format!("https://github.com/{repo}/issues/{number}");
    }
    String::new()
}

/// Like [`map_task`] but reads columns 31–35 carrying the external-ref
/// data and populates `Task.external_ref`. Used whenever the SELECT
/// explicitly includes those columns (e.g. `get_work_tree`, `find_by_external_ref`).
/// (Column 30 is `driver`, part of the base SELECT.)
pub(crate) fn map_task_with_external_ref(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    let kind: Option<String> = row.get(31)?;
    let canonical_id: Option<String> = row.get(32)?;
    if let (Some(kind), Some(canonical_id)) = (kind, canonical_id) {
        let raw_json: Option<String> = row.get(33)?;
        let raw: serde_json::Value = deserialize_json_or_default(raw_json.as_deref());
        let web_url = derive_external_ref_web_url(&kind, &canonical_id);
        task.external_ref = Some(WorkItemExternalRef {
            kind,
            canonical_id,
            raw,
            web_url,
            synced_at: row.get(34)?,
            unbound_at: row.get(35)?,
        });
    }
    Ok(task)
}

/// Like [`map_task_with_external_ref`] but also reads column 36 carrying
/// `parent_task_id`. Used in `get_work_tree` where the SELECT explicitly
/// includes the external-ref columns (31–35) followed by `parent_task_id`.
pub(crate) fn map_task_with_external_ref_and_parent(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task_with_external_ref(row)?;
    task.parent_task_id = row.get::<_, Option<String>>(36)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Like [`map_task_with_external_ref_and_parent`] but also reads column 37
/// carrying `source_automation_id`. Used in `get_work_tree` so automation-
/// produced tasks carry their provenance to the client (icon display + kanban
/// filtering both key off this field).
pub(crate) fn map_task_with_external_ref_parent_and_source_automation_id(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task_with_external_ref_and_parent(row)?;
    task.source_automation_id = row.get::<_, Option<String>>(37)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Like [`map_task_with_external_ref_parent_and_source_automation_id`] but
/// also reads `origin_task_short_id` (index 38), `origin_pr_number`
/// (index 39), and `completed_at` (index 40). Used by `get_work_tree`
/// for both task and chore queries, which append these columns at the end.
pub(crate) fn map_task_with_external_ref_parent_source_and_provenance(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task_with_external_ref_parent_and_source_automation_id(row)?;
    task.origin_task_short_id = row.get(38)?;
    task.origin_pr_number = row.get(39)?;
    task.completed_at = row.get::<_, Option<String>>(40)?.filter(|s| !s.is_empty());
    Ok(task)
}

pub(crate) fn map_execution(row: &Row<'_>) -> rusqlite::Result<WorkExecution> {
    let branch_naming: BranchNaming = deserialize_json_or_default(row.get::<_, Option<String>>(22)?.as_deref());
    let kind_raw: String = row.get(2)?;
    let kind = parse_text_column::<ExecutionKind>(2, &kind_raw)?;
    let status_raw: String = row.get(3)?;
    let status = parse_text_column::<ExecutionStatus>(3, &status_raw)?;
    Ok(WorkExecution {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        kind,
        status,
        repo_remote_url: row.get(4)?,
        cube_repo_id: row.get(5)?,
        cube_lease_id: row.get(6)?,
        cube_workspace_id: row.get(7)?,
        workspace_path: row.get(8)?,
        priority: row.get(9)?,
        preferred_workspace_id: row.get(10)?,
        created_at: row.get(11)?,
        started_at: row.get(12)?,
        finished_at: row.get(13)?,
        pre_start_failure_count: row.get(14)?,
        dispatch_not_before: row.get(15)?,
        pr_url: row.get(16)?,
        pr_head_before: row.get(17)?,
        prefer_is_soft: row.get::<_, i64>(18)? != 0,
        worker_branch_prefix: row.get::<_, Option<String>>(19)?.filter(|s| !s.is_empty()),
        transient_failure_count: row.get(20)?,
        allow_dirty: row.get::<_, i64>(21)? != 0,
        branch_naming,
    })
}

pub(crate) fn map_run(row: &Row<'_>) -> rusqlite::Result<WorkRun> {
    Ok(WorkRun {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        agent_id: row.get(2)?,
        status: row.get(3)?,
        error_text: row.get(4)?,
        result_summary: row.get(5)?,
        transcript_path: row.get(6)?,
        artifacts_path: row.get(7)?,
        created_at: row.get(8)?,
        started_at: row.get(9)?,
        finished_at: row.get(10)?,
    })
}

pub(crate) fn map_attention_item(row: &Row<'_>) -> rusqlite::Result<WorkAttentionItem> {
    Ok(WorkAttentionItem {
        id: row.get(0)?,
        execution_id: row.get(1)?,
        work_item_id: row.get(2)?,
        kind: row.get(3)?,
        status: row.get(4)?,
        title: row.get(5)?,
        body_markdown: row.get(6)?,
        created_at: row.get(7)?,
        resolved_at: row.get(8)?,
    })
}

pub(crate) fn map_effort_escalation(row: &Row<'_>) -> rusqlite::Result<boss_protocol::EffortEscalation> {
    let id: String = row.get(0)?;
    let product_id: String = row.get(1)?;
    let work_item_id: String = row.get(2)?;
    let original_level_str: String = row.get(3)?;
    let new_level_str: String = row.get(4)?;
    let markers_json: String = row.get(5)?;
    let rule_id: Option<String> = row.get(6)?;
    let created_at: String = row.get(7)?;
    // Both level columns and the markers JSON were validated at
    // insert time; on read we treat schema-level corruption as a
    // row-level error so an unexpected value doesn't silently
    // poison the audit.
    let original_level = parse_text_column::<boss_protocol::EffortLevel>(3, &original_level_str)?;
    let new_level = parse_text_column::<boss_protocol::EffortLevel>(4, &new_level_str)?;
    let markers: Vec<String> = serde_json::from_str(&markers_json)
        .map_err(|e| rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into()))?;
    Ok(boss_protocol::EffortEscalation {
        id,
        product_id,
        work_item_id,
        original_level,
        new_level,
        markers,
        rule_id,
        created_at,
    })
}

pub(crate) fn map_conflict_resolution(row: &Row<'_>) -> rusqlite::Result<ConflictResolution> {
    Ok(ConflictResolution {
        id: row.get(0)?,
        product_id: row.get(1)?,
        work_item_id: row.get(2)?,
        pr_url: row.get(3)?,
        pr_number: row.get(4)?,
        head_branch: row.get(5)?,
        base_branch: row.get(6)?,
        base_sha_at_trigger: row.get(7)?,
        head_sha_before: row.get(8)?,
        head_sha_after: row.get(9)?,
        status: row.get(10)?,
        failure_reason: row.get(11)?,
        cube_lease_id: row.get(12)?,
        cube_workspace_id: row.get(13)?,
        worker_id: row.get(14)?,
        conflict_diagnosis: row.get(15)?,
        created_at: row.get(16)?,
        started_at: row.get(17)?,
        finished_at: row.get(18)?,
        revision_task_id: row.get(19)?,
    })
}

pub(crate) fn query_conflict_resolution(conn: &Connection, id: &str) -> Result<Option<ConflictResolution>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                base_sha_at_trigger, head_sha_before, head_sha_after, status, failure_reason,
                cube_lease_id, cube_workspace_id, worker_id, conflict_diagnosis,
                created_at, started_at, finished_at, revision_task_id
         FROM conflict_resolutions
         WHERE id = ?1",
    )?;
    let row = stmt.query_row([id], map_conflict_resolution).optional()?;
    Ok(row)
}

/// Pre-insert payload for [`WorkDb::insert_conflict_resolution`].
/// Fields mirror the `conflict_resolutions` schema; everything the
/// engine knows at detection time is required, everything the engine
/// stamps post-spawn (`head_sha_after`, `cube_lease_id`, …) is
/// omitted.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct ConflictResolutionInsertInput {
    pub product_id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub base_branch: String,
    pub base_sha_at_trigger: Option<String>,
    pub head_sha_before: Option<String>,
}

/// Pre-insert payload for [`WorkDb::insert_ci_remediation`]. Mirrors
/// the `ci_remediations` schema for the engine-known fields at
/// detection time. `consumes_budget` is `1` for `attempt_kind='fix'`
/// and `0` for `'retrigger'` per design §Q3.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct CiRemediationInsertInput {
    pub product_id: String,
    pub work_item_id: String,
    pub pr_url: String,
    pub pr_number: i64,
    pub head_branch: String,
    pub head_sha_at_trigger: String,
    pub attempt_kind: String,
    pub consumes_budget: i64,
    /// JSON-encoded list of failing-check snapshots captured at
    /// trigger time. The engine writes this on detection; the worker
    /// reads it via the spawned prompt.
    pub failed_checks: String,
    /// `'pr_branch_ci'` for normal per-PR CI failures; `'merge_queue_rebounce'`
    /// when the PR was dequeued from GitHub's merge queue with
    /// `reason=FAILED_CHECKS`. See `CiRemediation.failure_kind` for semantics.
    pub failure_kind: String,
    /// For `'merge_queue_rebounce'`: the synthetic merge SHA (`beforeCommit.oid`)
    /// from the `RemovedFromMergeQueueEvent`. `None` for `'pr_branch_ci'`.
    pub before_commit_sha: Option<String>,
}

/// One row of `ci_inflight_observations`. The engine tracks the
/// first-observed-InFlight time per `(work_item_id, head_sha)` and
/// the most-recently-emitted alert bucket so the never-starts soft
/// alert (Phase 12 #39) doesn't churn on every probe.
#[derive(Debug, Clone)]
pub struct CiInFlightObservation {
    pub work_item_id: String,
    pub head_sha: String,
    /// Unix epoch seconds (decimal-string), the canonical timestamp
    /// shape across the boss DB.
    pub first_observed_at: String,
    /// `'none'`, `'warn'`, or `'alert'` — the highest bucket the
    /// engine has emitted for this `(work_item_id, head_sha)` pair.
    pub alert_level_emitted: String,
}

impl CiInFlightObservation {
    /// Parse the decimal-string `first_observed_at` into Unix epoch
    /// seconds. A malformed value (shouldn't happen — the engine
    /// always writes the canonical form) falls back to `0` so an
    /// elapsed-time check that hits a corrupt row still terminates.
    pub fn first_observed_at_secs(&self) -> i64 {
        self.first_observed_at.parse::<i64>().unwrap_or(0)
    }
}

pub(crate) fn map_ci_remediation(row: &Row<'_>) -> rusqlite::Result<CiRemediation> {
    Ok(CiRemediation {
        id: row.get(0)?,
        product_id: row.get(1)?,
        work_item_id: row.get(2)?,
        pr_url: row.get(3)?,
        pr_number: row.get(4)?,
        head_branch: row.get(5)?,
        head_sha_at_trigger: row.get(6)?,
        head_sha_after: row.get(7)?,
        attempt_kind: row.get(8)?,
        consumes_budget: row.get(9)?,
        failed_checks: row.get(10)?,
        triage_class: row.get(11)?,
        log_excerpt: row.get(12)?,
        status: row.get(13)?,
        failure_reason: row.get(14)?,
        cube_lease_id: row.get(15)?,
        cube_workspace_id: row.get(16)?,
        worker_id: row.get(17)?,
        created_at: row.get(18)?,
        started_at: row.get(19)?,
        finished_at: row.get(20)?,
        failure_kind: row.get(21)?,
        before_commit_sha: row.get(22)?,
        revision_task_id: row.get(23)?,
    })
}

/// Map one `attention_groups` row into [`AttentionGroup`].
///
/// Column order must match the canonical SELECT used by all attention-group
/// queries:
/// `id(0), product_id(1), short_id(2), kind(3),
///  association_project_id(4), association_task_id(5),
///  source_kind(6), source_task_id(7), source_run_id(8),
///  source_doc_path(9), source_doc_repo_remote_url(10), source_doc_branch(11),
///  grouping_key(12), generation(13), state(14),
///  produced_artifact_kind(15), produced_artifact_ref(16),
///  created_at(17), actioned_at(18), dismissed_at(19)`.
#[allow(dead_code)] // prepared for attention-group query handlers (app.rs stubs)
pub(crate) fn map_attention_group(row: &Row<'_>) -> rusqlite::Result<AttentionGroup> {
    Ok(AttentionGroup {
        id: row.get(0)?,
        product_id: row.get(1)?,
        short_id: row.get(2)?,
        kind: row.get(3)?,
        association_project_id: row.get(4)?,
        association_task_id: row.get(5)?,
        source_kind: row.get(6)?,
        source_task_id: row.get(7)?,
        source_run_id: row.get(8)?,
        source_doc_path: row.get(9)?,
        source_doc_repo_remote_url: row.get(10)?,
        source_doc_branch: row.get(11)?,
        grouping_key: row.get(12)?,
        generation: row.get(13)?,
        state: row.get(14)?,
        produced_artifact_kind: row.get::<_, Option<String>>(15)?.filter(|s| !s.is_empty()),
        produced_artifact_ref: row.get::<_, Option<String>>(16)?.filter(|s| !s.is_empty()),
        created_at: row.get(17)?,
        actioned_at: row.get(18)?,
        dismissed_at: row.get(19)?,
    })
}

/// Map one `attentions` row into [`Attention`].
///
/// Column order must match the canonical SELECT:
/// `id(0), group_id(1), ordinal(2), source_anchor(3),
///  answer_state(4), created_at(5), answered_at(6),
///  question_type(7), prompt_text(8), choice_options(9), answer(10),
///  proposed_name(11), proposed_description(12), proposed_effort(13),
///  proposed_work_kind(14), rationale(15), confidence_source(16)`.
#[allow(dead_code)] // prepared for attention query handlers (app.rs stubs)
pub(crate) fn map_attention(row: &Row<'_>) -> rusqlite::Result<Attention> {
    Ok(Attention {
        id: row.get(0)?,
        group_id: row.get(1)?,
        ordinal: row.get(2)?,
        source_anchor: row.get(3)?,
        answer_state: row.get(4)?,
        created_at: row.get(5)?,
        answered_at: row.get(6)?,
        question_type: row.get(7)?,
        prompt_text: row.get(8)?,
        choice_options: row.get(9)?,
        answer: row.get(10)?,
        proposed_name: row.get(11)?,
        proposed_description: row.get(12)?,
        proposed_effort: row.get::<_, Option<String>>(13)?.filter(|s| !s.is_empty()),
        proposed_work_kind: row.get(14)?,
        rationale: row.get(15)?,
        confidence_source: row.get(16)?,
    })
}

pub(crate) fn query_ci_remediation(conn: &Connection, id: &str) -> Result<Option<CiRemediation>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, work_item_id, pr_url, pr_number,
                head_branch, head_sha_at_trigger, head_sha_after,
                attempt_kind, consumes_budget, failed_checks,
                triage_class, log_excerpt, status, failure_reason,
                cube_lease_id, cube_workspace_id, worker_id,
                created_at, started_at, finished_at,
                failure_kind, before_commit_sha, revision_task_id
         FROM ci_remediations
         WHERE id = ?1",
    )?;
    let row = stmt.query_row([id], map_ci_remediation).optional()?;
    Ok(row)
}

/// Reconstruct an [`AutomationTrigger`] from the two DB columns
/// (`trigger_kind` discriminator + `trigger_config` JSON body).
///
/// The body does NOT contain the `"kind"` field — the discriminator is
/// stored separately for cheap index filtering. We inject it back before
/// deserialising.
pub(crate) fn automation_trigger_from_db(
    kind: &str,
    config_json: &str,
) -> anyhow::Result<boss_protocol::AutomationTrigger> {
    let mut config: serde_json::Map<String, serde_json::Value> = serde_json::from_str(config_json)
        .with_context(|| format!("failed to parse trigger_config JSON: {config_json}"))?;
    config.insert("kind".to_owned(), serde_json::Value::String(kind.to_owned()));
    let trigger = serde_json::from_value::<boss_protocol::AutomationTrigger>(serde_json::Value::Object(config))
        .with_context(|| format!("failed to deserialise AutomationTrigger with kind={kind}"))?;
    Ok(trigger)
}

/// Split an [`AutomationTrigger`] into `(trigger_kind, trigger_config_json)`
/// for DB storage. The `"kind"` field is removed from the config body so the
/// discriminator can be stored separately without duplication.
pub(crate) fn automation_trigger_to_db(trigger: &boss_protocol::AutomationTrigger) -> anyhow::Result<(String, String)> {
    let tagged = serde_json::to_value(trigger).context("failed to serialise AutomationTrigger")?;
    let mut map = match tagged {
        serde_json::Value::Object(m) => m,
        other => anyhow::bail!("unexpected AutomationTrigger JSON shape: {other}"),
    };
    let kind = map
        .remove("kind")
        .and_then(|v| v.as_str().map(|s| s.to_owned()))
        .context("AutomationTrigger serialised without 'kind' field")?;
    let config = serde_json::to_string(&map).context("failed to re-serialise trigger body")?;
    Ok((kind, config))
}

/// Map a row from the canonical `automations` SELECT column order:
/// 0  id, 1 short_id, 2 product_id, 3 name, 4 repo_remote_url,
/// 5  trigger_kind, 6 trigger_config, 7 standing_instruction,
/// 8  open_task_limit, 9 catch_up_window_secs, 10 enabled,
/// 11 created_via, 12 created_at, 13 updated_at,
/// 14 last_fired_at, 15 last_outcome, 16 next_due_at
pub(crate) fn map_automation(row: &Row<'_>) -> rusqlite::Result<boss_protocol::Automation> {
    let trigger_kind: String = row.get(5)?;
    let trigger_config: String = row.get(6)?;
    let trigger = automation_trigger_from_db(&trigger_kind, &trigger_config).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(
            5,
            rusqlite::types::Type::Text,
            Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string())),
        )
    })?;
    Ok(boss_protocol::Automation {
        id: row.get(0)?,
        short_id: row.get(1)?,
        product_id: row.get(2)?,
        name: row.get(3)?,
        repo_remote_url: row.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
        trigger,
        standing_instruction: row.get(7)?,
        open_task_limit: row.get(8)?,
        catch_up_window_secs: row.get(9)?,
        enabled: row.get::<_, i64>(10)? != 0,
        created_via: row.get(11)?,
        created_at: row.get(12)?,
        updated_at: row.get(13)?,
        last_fired_at: row.get::<_, Option<String>>(14)?.filter(|s| !s.is_empty()),
        last_outcome: row.get::<_, Option<String>>(15)?.filter(|s| !s.is_empty()),
        next_due_at: row.get::<_, Option<String>>(16)?.filter(|s| !s.is_empty()),
    })
}

/// Map a row from the canonical `automation_runs` SELECT column order:
/// 0 id, 1 automation_id, 2 scheduled_for, 3 started_at, 4 finished_at,
/// 5 triage_execution_id, 6 outcome, 7 produced_task_id, 8 detail
pub(crate) fn map_automation_run(row: &Row<'_>) -> rusqlite::Result<boss_protocol::AutomationRun> {
    Ok(boss_protocol::AutomationRun {
        id: row.get(0)?,
        automation_id: row.get(1)?,
        scheduled_for: row.get(2)?,
        started_at: row.get(3)?,
        finished_at: row.get::<_, Option<String>>(4)?.filter(|s| !s.is_empty()),
        triage_execution_id: row.get::<_, Option<String>>(5)?.filter(|s| !s.is_empty()),
        outcome: row.get(6)?,
        produced_task_id: row.get::<_, Option<String>>(7)?.filter(|s| !s.is_empty()),
        detail: row.get::<_, Option<String>>(8)?.filter(|s| !s.is_empty()),
    })
}
