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

pub(crate) fn map_product(row: &Row<'_>) -> rusqlite::Result<Product> {
    let external_tracker_kind: Option<String> =
        row.get::<_, Option<String>>(10)?.filter(|s| !s.is_empty());
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
    })
}

pub(crate) fn map_project(row: &Row<'_>) -> rusqlite::Result<Project> {
    Ok(Project {
        id: row.get(0)?,
        product_id: row.get(1)?,
        name: row.get(2)?,
        slug: row.get(3)?,
        description: row.get(4)?,
        goal: row.get(5)?,
        status: row.get(6)?,
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

pub(crate) fn map_task(row: &Row<'_>) -> rusqlite::Result<Task> {
    let effort_raw: Option<String> = row.get(19)?;
    let effort_level = match effort_raw.as_deref() {
        None | Some("") => None,
        Some(s) => match s.parse::<EffortLevel>() {
            Ok(level) => Some(level),
            Err(err) => {
                // The column is constrained in code, not by SQL. A row
                // carrying an out-of-set value is engine-side data
                // corruption: surface it loudly rather than silently
                // dropping the level.
                return Err(rusqlite::Error::FromSqlConversionFailure(
                    19,
                    rusqlite::types::Type::Text,
                    Box::new(std::io::Error::new(std::io::ErrorKind::InvalidData, err)),
                ));
            }
        },
    };
    Ok(Task {
        id: row.get(0)?,
        product_id: row.get(1)?,
        project_id: row.get(2)?,
        kind: row.get(3)?,
        name: row.get(4)?,
        description: row.get(5)?,
        status: row.get(6)?,
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
        // Standard queries omit the external_ref columns; the T8 methods
        // use map_task_with_external_ref which adds columns 30-34.
        // T1 schema columns; populated by T8 WorkDb methods when the migration
        // has run. Until then the protocol field carries None.
        external_ref: None,
        investigation_doc_path: None,
        investigation_doc_repo_remote_url: None,
        investigation_doc_branch: None,
        parent_task_id: None,
        // Revision projections are computed in attach_revision_projections
        // (get_work_tree); they are never stored as columns.
        revision_seq: None,
        revision_parent_pr_url: None,
    })
}

/// Like [`map_task`] but also reads a trailing `parent_task_id` column
/// (index 30, i.e. appended right after `merge_queue_state`). Used by the
/// narrow single-row lookups (`query_task`, `get_work_item_by_short_id`)
/// that back `boss task show` / `boss chore show`, so a revision's parent
/// linkage is present in that projection instead of always coming back
/// `null`. The wider `map_task_with_external_ref_and_investigation_doc`
/// variant populates `parent_task_id` from a different column index
/// (38); this helper is only for the 30-column SELECT + one extra column.
pub(crate) fn map_task_with_parent(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    task.parent_task_id = row.get::<_, Option<String>>(30)?.filter(|s| !s.is_empty());
    Ok(task)
}

/// Derives the canonical browser URL from an external-ref kind and
/// canonical_id at read time. For GitHub (`kind="github"`) the
/// canonical_id encodes `"owner/repo#number"`, which maps to
/// `https://github.com/owner/repo/issues/number`. Returns an empty
/// string for unknown trackers so callers can still surface the ref.
pub(crate) fn derive_external_ref_web_url(kind: &str, canonical_id: &str) -> String {
    if kind == "github" {
        if let Some((repo, number)) = canonical_id.rsplit_once('#') {
            return format!("https://github.com/{repo}/issues/{number}");
        }
    }
    String::new()
}

/// Like [`map_task`] but reads columns 30–34 carrying the external-ref
/// data and populates `Task.external_ref`. Used whenever the SELECT
/// explicitly includes those columns (e.g. `get_work_tree`, `find_by_external_ref`).
pub(crate) fn map_task_with_external_ref(row: &Row<'_>) -> rusqlite::Result<Task> {
    let mut task = map_task(row)?;
    let kind: Option<String> = row.get(30)?;
    let canonical_id: Option<String> = row.get(31)?;
    if let (Some(kind), Some(canonical_id)) = (kind, canonical_id) {
        let raw_json: Option<String> = row.get(32)?;
        let raw: serde_json::Value = raw_json
            .as_deref()
            .and_then(|s| serde_json::from_str(s).ok())
            .unwrap_or(serde_json::Value::Null);
        let web_url = derive_external_ref_web_url(&kind, &canonical_id);
        task.external_ref = Some(WorkItemExternalRef {
            kind,
            canonical_id,
            raw,
            web_url,
            synced_at: row.get(33)?,
            unbound_at: row.get(34)?,
        });
    }
    Ok(task)
}

/// Like [`map_task_with_external_ref`] but also reads columns 35–38
/// carrying the investigation-doc pointer fields and `parent_task_id`.
/// Used in `get_work_tree` where the SELECT explicitly includes those columns.
pub(crate) fn map_task_with_external_ref_and_investigation_doc(
    row: &Row<'_>,
) -> rusqlite::Result<Task> {
    let mut task = map_task_with_external_ref(row)?;
    task.investigation_doc_path = row.get::<_, Option<String>>(35)?.filter(|s| !s.is_empty());
    task.investigation_doc_repo_remote_url =
        row.get::<_, Option<String>>(36)?.filter(|s| !s.is_empty());
    task.investigation_doc_branch = row.get::<_, Option<String>>(37)?.filter(|s| !s.is_empty());
    task.parent_task_id = row.get::<_, Option<String>>(38)?.filter(|s| !s.is_empty());
    Ok(task)
}

pub(crate) fn map_execution(row: &Row<'_>) -> rusqlite::Result<WorkExecution> {
    Ok(WorkExecution {
        id: row.get(0)?,
        work_item_id: row.get(1)?,
        kind: row.get(2)?,
        status: row.get(3)?,
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

pub(crate) fn map_effort_escalation(
    row: &Row<'_>,
) -> rusqlite::Result<boss_protocol::EffortEscalation> {
    use std::str::FromStr;
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
    let original_level =
        boss_protocol::EffortLevel::from_str(&original_level_str).map_err(|e| {
            rusqlite::Error::FromSqlConversionFailure(3, rusqlite::types::Type::Text, e.into())
        })?;
    let new_level = boss_protocol::EffortLevel::from_str(&new_level_str).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(4, rusqlite::types::Type::Text, e.into())
    })?;
    let markers: Vec<String> = serde_json::from_str(&markers_json).map_err(|e| {
        rusqlite::Error::FromSqlConversionFailure(5, rusqlite::types::Type::Text, e.into())
    })?;
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
    })
}

pub(crate) fn query_conflict_resolution(
    conn: &Connection,
    id: &str,
) -> Result<Option<ConflictResolution>> {
    let mut stmt = conn.prepare(
        "SELECT id, product_id, work_item_id, pr_url, pr_number, head_branch, base_branch,
                base_sha_at_trigger, head_sha_before, head_sha_after, status, failure_reason,
                cube_lease_id, cube_workspace_id, worker_id, conflict_diagnosis,
                created_at, started_at, finished_at
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
#[derive(Debug, Clone)]
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
#[derive(Debug, Clone)]
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
                failure_kind, before_commit_sha
         FROM ci_remediations
         WHERE id = ?1",
    )?;
    let row = stmt.query_row([id], map_ci_remediation).optional()?;
    Ok(row)
}
