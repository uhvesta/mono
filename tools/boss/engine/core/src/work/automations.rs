use super::*;

/// One scheduler decision to persist via
/// [`WorkDb::record_automation_run_and_advance`]. All timestamps are UTC
/// epoch seconds (stored as strings, matching the rest of the schema).
///
/// Uses the repo builder convention (`bon`) since it carries 8+ fields;
/// `Option` fields default to `None`, so a caller only sets what applies to
/// the decision it is recording.
#[derive(Debug, Clone, bon::Builder)]
#[builder(on(String, into))]
pub struct AutomationFireRecord {
    pub automation_id: String,
    /// The cron occurrence this run satisfies (UTC epoch seconds). Doubles
    /// as the at-most-once dedupe key with `automation_id`.
    pub scheduled_for: i64,
    /// When the scheduler recorded this decision (UTC epoch seconds).
    pub started_at: i64,
    /// One of the `AUTOMATION_OUTCOME_*` discriminators.
    pub outcome: String,
    pub triage_execution_id: Option<String>,
    pub produced_task_id: Option<String>,
    pub finished_at: Option<i64>,
    pub detail: Option<String>,
    /// `Some(next_occurrence)` advances `automations.next_due_at`; `None`
    /// holds the current occurrence (used for transient-failure retry).
    pub next_due_at: Option<i64>,
}

const AUTOMATION_SELECT: &str = "
    SELECT id, short_id, product_id, name, repo_remote_url,
           trigger_kind, trigger_config, standing_instruction,
           open_task_limit, catch_up_window_secs, enabled,
           created_via, created_at, updated_at,
           last_fired_at, last_outcome, next_due_at
    FROM automations";

pub(crate) fn query_automation(conn: &Connection, id: &str) -> Result<Option<boss_protocol::Automation>> {
    let sql = format!("{AUTOMATION_SELECT} WHERE id = ?1");
    conn.query_row(&sql, [id], map_automation)
        .optional()
        .map_err(Into::into)
}

impl WorkDb {
    /// Create a new automation and return the inserted row.
    pub fn create_automation(&self, input: CreateAutomationInput) -> Result<boss_protocol::Automation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("auto");
        let now = now_string();
        let short_id = allocate_automation_short_id(&tx, &input.product_id)?;
        let (trigger_kind, trigger_config) = automation_trigger_to_db(&input.trigger)?;
        let repo_remote_url = canonicalize_repo_remote_url(input.repo_remote_url);
        let created_via = input.created_via.unwrap_or_else(|| CREATED_VIA_UNKNOWN.to_owned());

        tx.execute(
            "INSERT INTO automations
                 (id, short_id, product_id, name, repo_remote_url,
                  trigger_kind, trigger_config, standing_instruction,
                  open_task_limit, catch_up_window_secs, enabled,
                  created_via, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?13)",
            params![
                id,
                short_id,
                input.product_id,
                input.name,
                repo_remote_url,
                trigger_kind,
                trigger_config,
                input.standing_instruction,
                input.open_task_limit,
                input.catch_up_window_secs,
                input.enabled as i64,
                created_via,
                now,
            ],
        )?;

        let automation =
            query_automation(&tx, &id)?.with_context(|| format!("missing automation after insert: {id}"))?;
        tx.commit()?;
        Ok(automation)
    }

    /// List all automations for a product, ordered by `created_at ASC`.
    pub fn list_automations(&self, product_id: &str) -> Result<Vec<boss_protocol::Automation>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let sql = format!("{AUTOMATION_SELECT} WHERE product_id = ?1 ORDER BY created_at ASC, id ASC");
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([product_id], map_automation)?;
        collect_rows(rows)
    }

    /// Like [`list_automations`] but also returns each automation's current
    /// open-task count in one round-trip using a correlated subquery.
    pub fn list_automations_with_open_task_counts(
        &self,
        product_id: &str,
    ) -> Result<Vec<(boss_protocol::Automation, i64)>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let sql = "SELECT id, short_id, product_id, name, repo_remote_url,
                    trigger_kind, trigger_config, standing_instruction,
                    open_task_limit, catch_up_window_secs, enabled,
                    created_via, created_at, updated_at,
                    last_fired_at, last_outcome, next_due_at,
                    (SELECT COUNT(*) FROM tasks
                      WHERE source_automation_id = automations.id
                        AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
                        AND deleted_at IS NULL) AS open_task_count
             FROM automations
             WHERE product_id = ?1
             ORDER BY created_at ASC, id ASC";
        let mut stmt = conn.prepare(sql)?;
        let rows = stmt.query_map([product_id], |row| {
            let automation = map_automation(row)?;
            let count: i64 = row.get(17)?;
            Ok((automation, count))
        })?;
        collect_rows(rows)
    }

    /// Fetch a single automation by its canonical id.
    pub fn get_automation(&self, id: &str) -> Result<Option<boss_protocol::Automation>> {
        let conn = self.connect()?;
        query_automation(&conn, id)
    }

    /// Apply a patch to an automation. Only `Some` fields are updated.
    pub fn update_automation(&self, id: &str, patch: AutomationPatch) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let existing = query_automation(&conn, id).require("automation", id)?;

        let now = now_string();

        // Resolve trigger columns only when the trigger is being updated.
        let (trigger_kind, trigger_config) = if let Some(ref trigger) = patch.trigger {
            let (k, c) = automation_trigger_to_db(trigger)?;
            (Some(k), Some(c))
        } else {
            (None, None)
        };

        // Build SET clauses dynamically so we only touch provided fields.
        let mut sets: Vec<String> = vec!["updated_at = ?1".to_owned()];
        let mut params_raw: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now.clone())];
        let mut idx = 2usize;

        macro_rules! push_opt {
            ($field:expr, $val:expr) => {
                if let Some(v) = $val {
                    sets.push(format!("{} = ?{idx}", $field));
                    params_raw.push(Box::new(v));
                    idx += 1;
                }
            };
        }

        push_opt!("name", patch.name.clone());
        push_opt!(
            "repo_remote_url",
            canonicalize_repo_remote_url(patch.repo_remote_url.clone())
        );
        if let (Some(trigger_kind), Some(trigger_config)) = (trigger_kind, trigger_config) {
            sets.push(format!("trigger_kind = ?{idx}"));
            params_raw.push(Box::new(trigger_kind));
            idx += 1;
            sets.push(format!("trigger_config = ?{idx}"));
            params_raw.push(Box::new(trigger_config));
            idx += 1;
            // Reset next_due_at so the scheduler recomputes the first occurrence
            // from the new cron expression instead of using a stale value from
            // the old schedule.
            sets.push("next_due_at = NULL".to_owned());
        }
        push_opt!("standing_instruction", patch.standing_instruction.clone());
        push_opt!("open_task_limit", patch.open_task_limit);
        // catch_up_window_secs: Option<Option<i64>> would be needed for
        // "clear to null", but AutomationPatch uses Option<i64> which means
        // "set to this value" (None = leave unchanged). Scheduler can still
        // fall back to the engine default if the column is NULL.
        push_opt!("catch_up_window_secs", patch.catch_up_window_secs);
        push_opt!("enabled", patch.enabled.map(|b| b as i64));

        // id param goes at the end
        params_raw.push(Box::new(existing.id.clone()));
        let id_idx = idx;

        let sql = format!("UPDATE automations SET {} WHERE id = ?{id_idx}", sets.join(", "));

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_raw.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params_refs.as_slice())?;

        query_automation(&conn, id)?.with_context(|| format!("missing automation after update: {id}"))
    }

    /// Set `enabled = true` on an automation.
    pub fn enable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id).require("automation", id)?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 1, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?.with_context(|| format!("missing automation after enable: {id}"))
    }

    /// Set `enabled = false` on an automation.
    pub fn disable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id).require("automation", id)?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 0, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?.with_context(|| format!("missing automation after disable: {id}"))
    }

    /// Hard-delete an automation row. Also removes any `automation_runs` rows
    /// (ON DELETE CASCADE would handle this, but the FK is not `ON DELETE`
    /// constrained in the schema; we delete explicitly for safety).
    /// Tasks that were produced by this automation keep their
    /// `source_automation_id` value — they are orphaned from the automation
    /// but continue through their lifecycle normally.
    pub fn delete_automation(&self, id: &str) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _existing = query_automation(&tx, id).require("automation", id)?;
        tx.execute("DELETE FROM automation_runs WHERE automation_id = ?1", [id])?;
        tx.execute("DELETE FROM automations WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Count how many tasks produced by `automation_id` are currently open.
    /// "Open" = any non-terminal status: `todo`, `ready`, `active` (doing),
    /// `in_review`, `blocked`. Terminal statuses (`done`, `cancelled`,
    /// `archived`) are excluded. Note: the kanban label "doing" maps to the
    /// DB value `active`; the query uses the stored value.
    /// Used by the scheduler to enforce `open_task_limit` at fire time.
    pub fn count_open_tasks_for_automation(&self, automation_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks
              WHERE source_automation_id = ?1
                AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
                AND deleted_at IS NULL",
            [automation_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// List `automation_runs` rows for an automation, newest first.
    pub fn list_automation_runs(&self, automation_id: &str) -> Result<Vec<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id).require("automation", automation_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE automation_id = ?1
              ORDER BY scheduled_for DESC, started_at DESC",
        )?;
        let rows = stmt.query_map([automation_id], map_automation_run)?;
        collect_rows(rows)
    }

    /// List tasks produced by an automation (`source_automation_id = ?`),
    /// ordered by `created_at DESC`. Includes non-deleted rows only.
    pub fn list_tasks_for_automation(&self, automation_id: &str) -> Result<Vec<boss_protocol::Task>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id).require("automation", automation_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state, driver,
                    source_automation_id
               FROM tasks
              WHERE source_automation_id = ?1
                AND deleted_at IS NULL
              ORDER BY created_at DESC",
        )?;
        let rows = stmt.query_map([automation_id], map_task_with_source_automation_id)?;
        collect_rows(rows)
    }

    /// List automations the scheduler should evaluate this tick: enabled,
    /// `trigger_kind = 'schedule'`, and either never-scheduled
    /// (`next_due_at IS NULL`, needs initialisation) or due
    /// (`next_due_at <= now_epoch`). Ordered oldest-first for stable
    /// iteration. `now_epoch` is UTC seconds; `next_due_at` is stored as an
    /// epoch-seconds string, so the comparison casts it to INTEGER.
    pub fn list_due_automations(&self, now_epoch: i64) -> Result<Vec<boss_protocol::Automation>> {
        let conn = self.connect()?;
        let sql = format!(
            "{AUTOMATION_SELECT}
              WHERE enabled = 1
                AND trigger_kind = 'schedule'
                AND (next_due_at IS NULL OR CAST(next_due_at AS INTEGER) <= ?1)
              ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([now_epoch], map_automation)?;
        collect_rows(rows)
    }

    /// Initialise an automation's `next_due_at` (epoch seconds) without
    /// recording a fire. Used the first time the scheduler sees an
    /// automation whose `next_due_at` is still NULL: it computes the next
    /// occurrence and parks it here so the next tick can fire on time.
    /// Deliberately does NOT touch `updated_at` (which tracks user/config
    /// edits) or the `last_*` fire bookkeeping.
    pub fn initialize_automation_next_due_at(&self, id: &str, next_due_epoch: i64) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE automations SET next_due_at = ?2 WHERE id = ?1",
            params![id, next_due_epoch.to_string()],
        )?;
        Ok(())
    }

    /// Return scheduling data for the automation scheduler's sleep computation:
    /// the minimum `next_due_at` epoch across all enabled `schedule` automations
    /// whose `next_due_at` has been initialized, and whether any enabled
    /// `schedule` automations are still uninitialized (`next_due_at IS NULL`).
    ///
    /// Used by the scheduler after each pass to compute how long to sleep before
    /// the next evaluation: sleep until `min_next_due`, capped at a maximum,
    /// but use a short poll interval when uninitialized automations are present.
    pub fn list_min_next_due_at_for_scheduler(&self) -> Result<(Option<i64>, bool)> {
        let conn = self.connect()?;
        let min_next_due: Option<i64> = conn.query_row(
            "SELECT MIN(CAST(next_due_at AS INTEGER))
               FROM automations
              WHERE enabled = 1 AND trigger_kind = 'schedule' AND next_due_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;
        let uninitialized_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM automations
              WHERE enabled = 1 AND trigger_kind = 'schedule' AND next_due_at IS NULL",
            [],
            |row| row.get(0),
        )?;
        Ok((min_next_due, uninitialized_count > 0))
    }

    /// Fetch the `automation_runs` row for a specific occurrence, if one
    /// exists. The `(automation_id, scheduled_for)` pair is the
    /// at-most-once dedupe key for a fired occurrence.
    pub fn automation_run_for_occurrence(
        &self,
        automation_id: &str,
        scheduled_for_epoch: i64,
    ) -> Result<Option<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE automation_id = ?1 AND scheduled_for = ?2",
            params![automation_id, scheduled_for_epoch.to_string()],
            map_automation_run,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Record one scheduler decision and advance the automation's
    /// bookkeeping, atomically.
    ///
    /// The `automation_runs` write is an **upsert** keyed on
    /// `(automation_id, scheduled_for)`: a fresh occurrence inserts a row;
    /// re-recording the same occurrence (e.g. a held `failed_will_retry`
    /// the scheduler re-attempts) updates the existing row in place rather
    /// than piling up duplicates — preserving the at-most-once-per-occurrence
    /// invariant.
    ///
    /// `last_fired_at` and `last_outcome` are always updated to mirror this
    /// decision. `next_due_at` advances only when `record.next_due_at` is
    /// `Some` — a transient pre-start failure passes `None` to *hold* the
    /// occurrence for retry rather than skip past it.
    pub fn record_automation_run_and_advance(&self, record: AutomationFireRecord) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let scheduled_for = record.scheduled_for.to_string();
        let started_at = record.started_at.to_string();
        let finished_at = record.finished_at.map(|v| v.to_string());

        let existing_id: Option<String> = tx
            .query_row(
                "SELECT id FROM automation_runs
                  WHERE automation_id = ?1 AND scheduled_for = ?2",
                params![record.automation_id, scheduled_for],
                |row| row.get(0),
            )
            .optional()?;

        match existing_id {
            Some(id) => {
                tx.execute(
                    "UPDATE automation_runs
                        SET started_at = ?2, finished_at = ?3,
                            triage_execution_id = ?4, outcome = ?5,
                            produced_task_id = ?6, detail = ?7
                      WHERE id = ?1",
                    params![
                        id,
                        started_at,
                        finished_at,
                        record.triage_execution_id,
                        record.outcome,
                        record.produced_task_id,
                        record.detail,
                    ],
                )?;
            }
            None => {
                let run_id = next_id("autorun");
                tx.execute(
                    "INSERT INTO automation_runs
                         (id, automation_id, scheduled_for, started_at, finished_at,
                          triage_execution_id, outcome, produced_task_id, detail)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
                    params![
                        run_id,
                        record.automation_id,
                        scheduled_for,
                        started_at,
                        finished_at,
                        record.triage_execution_id,
                        record.outcome,
                        record.produced_task_id,
                        record.detail,
                    ],
                )?;
            }
        }

        // Advance bookkeeping. `next_due_at` is only rewritten when the
        // caller wants to move past this occurrence.
        match record.next_due_at {
            Some(next_due) => {
                tx.execute(
                    "UPDATE automations
                        SET last_fired_at = ?2, last_outcome = ?3, next_due_at = ?4
                      WHERE id = ?1",
                    params![
                        record.automation_id,
                        record.started_at.to_string(),
                        record.outcome,
                        next_due.to_string(),
                    ],
                )?;
            }
            None => {
                tx.execute(
                    "UPDATE automations
                        SET last_fired_at = ?2, last_outcome = ?3
                      WHERE id = ?1",
                    params![record.automation_id, record.started_at.to_string(), record.outcome,],
                )?;
            }
        }

        tx.commit()?;
        Ok(())
    }

    /// Stamp a task's `source_automation_id` (and status) directly. Used by
    /// scheduler tests to drive the open-task-limit gate without the
    /// `boss task create --automation` path (Maint task 6).
    #[cfg(test)]
    pub fn stamp_task_source_automation_for_test(
        &self,
        task_id: &str,
        automation_id: &str,
        status: &str,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks SET source_automation_id = ?2, status = ?3
              WHERE id = ?1 AND deleted_at IS NULL",
            params![task_id, automation_id, status],
        )?;
        Ok(())
    }

    /// Return the `source_automation_id` for `work_item_id`, or `None` if the
    /// task is not automation-produced (or the id is not a task at all).
    /// Used by the dispatcher to route automation-produced task executions to
    /// the automation pool. Returns `Ok(None)` rather than an error when the
    /// id is not found in `tasks` (e.g. it references a project or product).
    pub fn source_automation_id_for_work_item(&self, work_item_id: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT source_automation_id FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
            [work_item_id],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map(|opt| opt.flatten())
        .map_err(Into::into)
    }

    /// Create a `ready` `automation_triage` work_execution bound to an
    /// automation (Maint task 6).
    ///
    /// A triage execution's `work_item_id` is the `automations.id`, not a
    /// task — so it cannot go through the task-centric `insert_execution`
    /// resolvers (which require the work_item to resolve to a product/task).
    /// We insert the row directly with the automation's already-resolved
    /// repo. Downstream: the dispatcher routes it to the automations pool on
    /// `kind`, the runner renders the triage preamble, and the outcome
    /// detector finalises the matching `automation_runs` row on Stop. The row
    /// starts `ready` so the coordinator's normal drain picks it up (and the
    /// existing `dispatch_not_before` / `pre_start_failure_count` machinery
    /// retries it transparently on a transient pre-start failure).
    pub fn create_automation_triage_execution(
        &self,
        automation_id: &str,
        repo_remote_url: &str,
    ) -> Result<WorkExecution> {
        let conn = self.connect()?;
        let id = next_id("exec");
        let now = now_string();
        let branch_naming_json = serde_json::to_string(&boss_protocol::BranchNaming::default()).unwrap_or_default();
        // Column list mirrors `insert_execution`; every column it omits has a
        // schema DEFAULT (pre_start_failure_count=0, dispatch_not_before=NULL,
        // transient_failure_count=0, host_id='local', …).
        conn.execute(
            "INSERT INTO work_executions (
                id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                created_at, started_at, finished_at, prefer_is_soft, pr_url, worker_branch_prefix,
                allow_dirty, branch_naming
             ) VALUES (?1, ?2, ?3, 'ready', ?4, NULL, NULL, NULL, NULL, 0, NULL, ?5, NULL, NULL, 0, NULL, NULL, 0, ?6)",
            params![
                id,
                automation_id,
                boss_protocol::EXECUTION_KIND_AUTOMATION_TRIAGE,
                repo_remote_url,
                now,
                branch_naming_json,
            ],
        )?;
        query_execution(&conn, &id)?.with_context(|| format!("missing automation triage execution after insert: {id}"))
    }

    /// Fetch the `automation_runs` row whose triage `work_execution` is
    /// `triage_execution_id`, if one exists. Used by the outcome detector on
    /// Stop to map a finished triage execution back to the occurrence it
    /// fired for. Newest occurrence first as a tie-break (a retried execution
    /// id is unique, so at most one row normally matches).
    pub fn automation_run_for_triage_execution(
        &self,
        triage_execution_id: &str,
    ) -> Result<Option<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, automation_id, scheduled_for, started_at, finished_at,
                    triage_execution_id, outcome, produced_task_id, detail
               FROM automation_runs
              WHERE triage_execution_id = ?1
              ORDER BY scheduled_for DESC, started_at DESC
              LIMIT 1",
            [triage_execution_id],
            map_automation_run,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Finalise the `automation_runs` row for a finished triage execution
    /// (Maint task 6 outcome detection). Sets the terminal `outcome`,
    /// `produced_task_id`, `finished_at`, and (when `Some`) `detail`, and
    /// mirrors the outcome onto `automations.last_outcome`.
    ///
    /// Deliberately does NOT touch `next_due_at`: the scheduler already
    /// advanced the schedule past this occurrence when it fired the triage.
    /// Returns `false` when no run matches the execution id (the scheduler
    /// never recorded it — e.g. a manual fire that failed before recording).
    pub fn finalize_automation_triage_run(
        &self,
        triage_execution_id: &str,
        outcome: &str,
        produced_task_id: Option<&str>,
        detail: Option<&str>,
    ) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let row: Option<(String, String)> = tx
            .query_row(
                "SELECT id, automation_id FROM automation_runs
                  WHERE triage_execution_id = ?1
                  ORDER BY scheduled_for DESC, started_at DESC LIMIT 1",
                [triage_execution_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((run_id, automation_id)) = row else {
            return Ok(false);
        };
        let now_epoch = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        tx.execute(
            "UPDATE automation_runs
                SET outcome = ?2,
                    produced_task_id = ?3,
                    detail = COALESCE(?4, detail),
                    finished_at = ?5
              WHERE id = ?1",
            params![run_id, outcome, produced_task_id, detail, now_epoch.to_string()],
        )?;
        tx.execute(
            "UPDATE automations SET last_outcome = ?2 WHERE id = ?1",
            params![automation_id, outcome],
        )?;
        tx.commit()?;
        Ok(true)
    }

    /// Stamp a human-readable reason onto the `automation_runs` row for a
    /// triage execution that has been deferred (e.g. pool exhausted) but not
    /// yet finalised. Only writes when the existing `detail` is NULL or empty
    /// so that a later `finalize_automation_triage_run` call carrying a more
    /// specific reason always wins.
    ///
    /// Returns `true` when a row was updated, `false` when no matching row
    /// exists yet (the scheduler records the row at fire time, but there is a
    /// brief window before that write completes).
    pub fn update_automation_run_detail_for_triage_execution(
        &self,
        triage_execution_id: &str,
        detail: &str,
    ) -> Result<bool> {
        let conn = self.connect()?;
        let rows_changed = conn.execute(
            "UPDATE automation_runs
                SET detail = ?2
              WHERE triage_execution_id = ?1
                AND (detail IS NULL OR detail = '')",
            params![triage_execution_id, detail],
        )?;
        Ok(rows_changed > 0)
    }

    /// Mark a triage execution's `automation_runs` row as `pool_throttled` —
    /// the triage execution is queued in `ready` status waiting for an
    /// automation pool slot. Also updates `automations.last_outcome` so the
    /// sidebar reflects the correct non-failure state.
    ///
    /// Only transitions from `failed_will_retry` (the pessimistic initial
    /// state the scheduler writes) so it is idempotent: a second call while
    /// still throttled is a no-op. Returns `true` when a row was updated.
    pub fn update_automation_run_for_pool_throttle(&self, triage_execution_id: &str, detail: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows_changed = tx.execute(
            "UPDATE automation_runs
                SET outcome = 'pool_throttled', detail = ?2
              WHERE triage_execution_id = ?1
                AND outcome = 'failed_will_retry'",
            params![triage_execution_id, detail],
        )?;
        if rows_changed > 0 {
            tx.execute(
                "UPDATE automations
                    SET last_outcome = 'pool_throttled'
                  WHERE id = (SELECT automation_id FROM automation_runs
                               WHERE triage_execution_id = ?1 LIMIT 1)",
                params![triage_execution_id],
            )?;
        }
        tx.commit()?;
        Ok(rows_changed > 0)
    }

    /// Mark a triage execution's `automation_runs` row as `triage_running` —
    /// a pool slot was claimed and the triage agent is now active. Also
    /// updates `automations.last_outcome`. Transitions from `pool_throttled`
    /// (if the run was previously queued) or `failed_will_retry` (if it was
    /// dispatched immediately). Returns `true` when a row was updated.
    pub fn mark_automation_run_triage_started(&self, triage_execution_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let rows_changed = tx.execute(
            "UPDATE automation_runs
                SET outcome = 'triage_running'
              WHERE triage_execution_id = ?1
                AND outcome IN ('failed_will_retry', 'pool_throttled')",
            params![triage_execution_id],
        )?;
        if rows_changed > 0 {
            tx.execute(
                "UPDATE automations
                    SET last_outcome = 'triage_running'
                  WHERE id = (SELECT automation_id FROM automation_runs
                               WHERE triage_execution_id = ?1 LIMIT 1)",
                params![triage_execution_id],
            )?;
        }
        tx.commit()?;
        Ok(rows_changed > 0)
    }

    /// Create the single maintenance task produced by an automation's triage
    /// phase (`boss task create --automation`). Maint task 6.
    ///
    /// Runs in one immediate transaction:
    /// 1. **Open-task-cap re-check** — the backstop against fan-out. The
    ///    scheduler already gated at fire time, but a misbehaving triage
    ///    agent could call this repeatedly within one run; re-checking the
    ///    cap transactionally guarantees at most `open_task_limit` open
    ///    produced tasks regardless of agent behaviour.
    /// 2. Insert a product-level chore (`kind='chore'`, `project_id=NULL`)
    ///    inheriting the automation's repo override, `autostart=true` so
    ///    phase 2 starts automatically.
    /// 3. Stamp `source_automation_id` for provenance, backlog exclusion,
    ///    pool routing, and the open-task-limit denominator.
    ///
    /// Returns an error (surfaced to the agent) when the cap is already met,
    /// so the marker the agent then emits can be reconciled by the detector.
    pub fn create_automation_task(&self, automation_id: &str, name: &str, description: Option<&str>) -> Result<Task> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let automation = query_automation(&tx, automation_id).require("automation", automation_id)?;

        let open: i64 = tx.query_row(
            "SELECT COUNT(*) FROM tasks
              WHERE source_automation_id = ?1
                AND status IN ('todo', 'ready', 'active', 'in_review', 'blocked')
                AND deleted_at IS NULL",
            [automation_id],
            |row| row.get(0),
        )?;
        if open >= automation.open_task_limit {
            anyhow::bail!(
                "automation {automation_id} is at its open-task limit \
                 ({open}/{}); refusing to create another task (fan-out backstop)",
                automation.open_task_limit
            );
        }

        // `force_duplicate` so a recurring maintenance instruction that
        // produces same-named tasks across fires is not blocked by the
        // 60-second recent-duplicate guard.
        let mut task = insert_chore_in_tx(
            &tx,
            CreateChoreInput::builder()
                .product_id(automation.product_id.clone())
                .name(name)
                .maybe_description(description.map(str::to_owned))
                .created_via(boss_protocol::CREATED_VIA_ENGINE_AUTO)
                .maybe_repo_remote_url(automation.repo_remote_url.clone())
                .force_duplicate(true)
                .build(),
        )?;
        tx.execute(
            "UPDATE tasks SET source_automation_id = ?2 WHERE id = ?1",
            params![task.id, automation_id],
        )?;
        tx.commit()?;
        task.source_automation_id = Some(automation_id.to_owned());
        Ok(task)
    }
}
