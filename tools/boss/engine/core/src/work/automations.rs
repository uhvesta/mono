use super::*;

/// One scheduler decision to persist via
/// [`WorkDb::record_automation_run_and_advance`]. All timestamps are UTC
/// epoch seconds (stored as strings, matching the rest of the schema).
///
/// Uses the repo builder convention (`bon`) since it carries 8+ fields;
/// `Option` fields default to `None`, so a caller only sets what applies to
/// the decision it is recording.
#[derive(Debug, Clone)]
#[derive(bon::Builder)]
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

pub(crate) fn query_automation(
    conn: &Connection,
    id: &str,
) -> Result<Option<boss_protocol::Automation>> {
    let sql = format!("{AUTOMATION_SELECT} WHERE id = ?1");
    conn.query_row(&sql, [id], map_automation)
        .optional()
        .map_err(Into::into)
}

impl WorkDb {
    /// Create a new automation and return the inserted row.
    pub fn create_automation(
        &self,
        input: CreateAutomationInput,
    ) -> Result<boss_protocol::Automation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_product_exists(&tx, &input.product_id)?;

        let id = next_id("auto");
        let now = now_string();
        let short_id = allocate_automation_short_id(&tx, &input.product_id)?;
        let (trigger_kind, trigger_config) = automation_trigger_to_db(&input.trigger)?;
        let repo_remote_url = canonicalize_repo_remote_url(input.repo_remote_url);
        let created_via = input
            .created_via
            .unwrap_or_else(|| CREATED_VIA_UNKNOWN.to_owned());

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

        let automation = query_automation(&tx, &id)?
            .with_context(|| format!("missing automation after insert: {id}"))?;
        tx.commit()?;
        Ok(automation)
    }

    /// List all automations for a product, ordered by `created_at ASC`.
    pub fn list_automations(
        &self,
        product_id: &str,
    ) -> Result<Vec<boss_protocol::Automation>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        let sql = format!(
            "{AUTOMATION_SELECT} WHERE product_id = ?1 ORDER BY created_at ASC, id ASC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map([product_id], map_automation)?;
        collect_rows(rows)
    }

    /// Fetch a single automation by its canonical id.
    pub fn get_automation(&self, id: &str) -> Result<Option<boss_protocol::Automation>> {
        let conn = self.connect()?;
        query_automation(&conn, id)
    }

    /// Apply a patch to an automation. Only `Some` fields are updated.
    pub fn update_automation(
        &self,
        id: &str,
        patch: AutomationPatch,
    ) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let existing = query_automation(&conn, id)?
            .with_context(|| format!("unknown automation: {id}"))?;

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
        let mut params_raw: Vec<Box<dyn rusqlite::ToSql>> =
            vec![Box::new(now.clone())];
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
        if trigger_kind.is_some() {
            sets.push(format!("trigger_kind = ?{idx}"));
            params_raw.push(Box::new(trigger_kind.unwrap()));
            idx += 1;
            sets.push(format!("trigger_config = ?{idx}"));
            params_raw.push(Box::new(trigger_config.unwrap()));
            idx += 1;
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

        let sql = format!(
            "UPDATE automations SET {} WHERE id = ?{id_idx}",
            sets.join(", ")
        );

        let params_refs: Vec<&dyn rusqlite::ToSql> =
            params_raw.iter().map(|b| b.as_ref() as &dyn rusqlite::ToSql).collect();
        conn.execute(&sql, params_refs.as_slice())?;

        query_automation(&conn, id)?
            .with_context(|| format!("missing automation after update: {id}"))
    }

    /// Set `enabled = true` on an automation.
    pub fn enable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id)?
            .with_context(|| format!("unknown automation: {id}"))?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 1, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?
            .with_context(|| format!("missing automation after enable: {id}"))
    }

    /// Set `enabled = false` on an automation.
    pub fn disable_automation(&self, id: &str) -> Result<boss_protocol::Automation> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, id)?
            .with_context(|| format!("unknown automation: {id}"))?;
        let now = now_string();
        conn.execute(
            "UPDATE automations SET enabled = 0, updated_at = ?2 WHERE id = ?1",
            params![id, now],
        )?;
        query_automation(&conn, id)?
            .with_context(|| format!("missing automation after disable: {id}"))
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
        let _existing = query_automation(&tx, id)?
            .with_context(|| format!("unknown automation: {id}"))?;
        tx.execute("DELETE FROM automation_runs WHERE automation_id = ?1", [id])?;
        tx.execute("DELETE FROM automations WHERE id = ?1", [id])?;
        tx.commit()?;
        Ok(())
    }

    /// Count how many tasks produced by `automation_id` are currently open.
    /// "Open" = `status IN (todo, ready, doing, in_review, blocked)`.
    /// Used by the scheduler to enforce `open_task_limit` at fire time.
    pub fn count_open_tasks_for_automation(&self, automation_id: &str) -> Result<i64> {
        let conn = self.connect()?;
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM tasks
              WHERE source_automation_id = ?1
                AND status IN ('todo', 'ready', 'doing', 'in_review', 'blocked')
                AND deleted_at IS NULL",
            [automation_id],
            |row| row.get(0),
        )?;
        Ok(count)
    }

    /// List `automation_runs` rows for an automation, newest first.
    pub fn list_automation_runs(
        &self,
        automation_id: &str,
    ) -> Result<Vec<boss_protocol::AutomationRun>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id)?
            .with_context(|| format!("unknown automation: {automation_id}"))?;

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
    pub fn list_tasks_for_automation(
        &self,
        automation_id: &str,
    ) -> Result<Vec<boss_protocol::Task>> {
        let conn = self.connect()?;
        let _existing = query_automation(&conn, automation_id)?
            .with_context(|| format!("unknown automation: {automation_id}"))?;

        let mut stmt = conn.prepare(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state,
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
    pub fn list_due_automations(
        &self,
        now_epoch: i64,
    ) -> Result<Vec<boss_protocol::Automation>> {
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
    pub fn initialize_automation_next_due_at(
        &self,
        id: &str,
        next_due_epoch: i64,
    ) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "UPDATE automations SET next_due_at = ?2 WHERE id = ?1",
            params![id, next_due_epoch.to_string()],
        )?;
        Ok(())
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
    pub fn record_automation_run_and_advance(
        &self,
        record: AutomationFireRecord,
    ) -> Result<()> {
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
                    params![
                        record.automation_id,
                        record.started_at.to_string(),
                        record.outcome,
                    ],
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
    pub fn source_automation_id_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<String>> {
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
}
