use sha2::{Digest, Sha256};

use super::*;

/// Compute a SHA-256 checksum over a canonical combination of title and body.
///
/// Canonicalization: `title + "\n\n" + body`, no normalization. This is
/// deterministic across runs and platforms — no trailing-newline stripping,
/// no locale-sensitive transforms.
pub(crate) fn content_checksum(title: &str, body: &str) -> String {
    let mut h = Sha256::new();
    h.update(title.as_bytes());
    h.update(b"\n\n");
    h.update(body.as_bytes());
    format!("{:x}", h.finalize())
}

impl WorkDb {
    /// Atomically null out `cube_lease_id`, `cube_workspace_id`, and
    /// `workspace_path` on `execution_id`. Returns the prior lease id
    /// — `Some` means the caller is responsible for issuing the cube
    /// `workspace release`, `None` means there was nothing to release
    /// (already cleared by an earlier path or never leased).
    ///
    /// Used by the engine-side release path (manual chore-done update,
    /// `bossctl agents stop`) to claim ownership of the cube release
    /// before calling out to the cube CLI, so two concurrent callers
    /// don't issue duplicate releases against the same lease.
    /// Atomically cancel a non-terminal execution and demote the owning
    /// task from `active` back to `todo`. Called from the
    /// `StopRun` / `force_stop_execution` path so that an explicitly
    /// stopped worker does not leave an `active` task or a `running`
    /// execution behind — both of which would cause the orphan sweep
    /// or `reconcile_active_dispatch` to re-dispatch the work item
    /// immediately.
    ///
    /// Idempotent: if the execution is already terminal the `UPDATE`
    /// is a no-op; if the task is already out of `active` (e.g. moved
    /// to `in_review` by `on_stop` in a concurrent path) the demote is
    /// a no-op.
    ///
    /// Returns `(execution_cancelled, task_demoted)` so callers can
    /// log what actually changed.
    /// Cancel a non-terminal execution without touching the task status.
    /// Returns `true` if the execution was actually cancelled (was in a
    /// non-terminal state), `false` if it was already terminal.
    ///
    /// Used by the `active` → Backlog kanban-drag path: the task status
    /// has already been moved to `todo` by `update_work_item_as_actor`,
    /// so we only need to mark the execution as cancelled before releasing
    /// the pane and workspace.
    pub fn cancel_running_execution(&self, execution_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let now = now_string();
        let cancelled = if !execution_status_is_terminal(&execution.status) {
            let affected = tx.execute(
                "UPDATE work_executions
                 SET status      = 'cancelled',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE id = ?1",
                params![execution_id, now],
            )?;
            affected > 0
        } else {
            false
        };
        tx.commit()?;
        Ok(cancelled)
    }

    pub fn cancel_running_execution_and_demote_task(
        &self,
        execution_id: &str,
    ) -> Result<(bool, bool)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let now = now_string();
        // Cancel the execution only if it is still non-terminal.
        let exec_cancelled = if !execution_status_is_terminal(&execution.status) {
            let affected = tx.execute(
                "UPDATE work_executions
                 SET status     = 'cancelled',
                     finished_at = COALESCE(finished_at, ?2)
                 WHERE id = ?1",
                params![execution_id, now],
            )?;
            affected > 0
        } else {
            false
        };
        // Demote the task only if it is still `active`.
        let task_demoted = {
            let affected = tx.execute(
                "UPDATE tasks
                 SET status             = 'todo',
                     last_status_actor  = 'engine',
                     updated_at         = ?2
                 WHERE id              = ?1
                   AND status          = 'active'
                   AND deleted_at      IS NULL",
                params![execution.work_item_id, now],
            )?;
            affected > 0
        };
        tx.commit()?;
        Ok((exec_cancelled, task_demoted))
    }

    pub fn clear_execution_workspace(&self, execution_id: &str) -> Result<Option<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let execution = query_execution(&tx, execution_id)?
            .with_context(|| format!("unknown execution: {execution_id}"))?;
        let prior = execution.cube_lease_id.clone();
        if prior.is_some() {
            tx.execute(
                "UPDATE work_executions
                 SET cube_lease_id = NULL,
                     cube_workspace_id = NULL,
                     workspace_path = NULL
                 WHERE id = ?1",
                params![execution_id],
            )?;
        }
        tx.commit()?;
        Ok(prior)
    }

    /// Append an `effort_escalations` row recording a worker's
    /// `[effort-escalation]` Stop-boundary signal (design §Q5). The
    /// engine assigns `id` (prefix `esc_…`) and `created_at`.
    /// `markers` is stored as a JSON array; the audit report
    /// re-parses on read. Returns the inserted row wire-shape so
    /// the RPC caller can echo it back without a re-query.
    ///
    /// Validates that `work_item_id` refers to a known leaf row
    /// (chore / project_task / design) and resolves `product_id`
    /// from it; the denormalised `product_id` column avoids a join
    /// on every audit-report read.
    pub fn record_effort_escalation(
        &self,
        work_item_id: &str,
        original_level: boss_protocol::EffortLevel,
        new_level: boss_protocol::EffortLevel,
        markers: &[String],
        rule_id: Option<&str>,
    ) -> Result<boss_protocol::EffortEscalation> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let product_id = product_id_for_work_item(&tx, work_item_id)
            .with_context(|| format!("unknown work item: {work_item_id}"))?;
        let id = next_id("esc");
        let now = now_string();
        let markers_json =
            serde_json::to_string(markers).context("serialise effort escalation markers")?;
        tx.execute(
            "INSERT INTO effort_escalations
                 (id, product_id, work_item_id, original_level, new_level, markers, rule_id, created_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                product_id,
                work_item_id,
                original_level.as_str(),
                new_level.as_str(),
                markers_json,
                rule_id,
                now,
            ],
        )?;
        tx.commit()?;
        Ok(boss_protocol::EffortEscalation {
            id,
            product_id,
            work_item_id: work_item_id.to_owned(),
            original_level,
            new_level,
            markers: markers.to_vec(),
            rule_id: rule_id.map(|s| s.to_owned()),
            created_at: now,
        })
    }

    /// Load every `effort_escalations` row for `product_id`,
    /// optionally filtered to events with `created_at >=
    /// since_epoch_secs`. Order is newest-first by `created_at`.
    /// Used by the audit report (design §Q4 follow-up).
    pub fn list_effort_escalations_for_product(
        &self,
        product_id: &str,
        since_epoch_secs: Option<i64>,
    ) -> Result<Vec<boss_protocol::EffortEscalation>> {
        let conn = self.connect()?;
        let mut sql = String::from(
            "SELECT id, product_id, work_item_id, original_level, new_level, markers, rule_id, created_at
             FROM effort_escalations
             WHERE product_id = ?1",
        );
        let mut params_vec: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(product_id.to_owned())];
        if let Some(since) = since_epoch_secs {
            sql.push_str(" AND CAST(created_at AS INTEGER) >= ?");
            params_vec.push(Box::new(since));
        }
        sql.push_str(" ORDER BY created_at DESC, id DESC");
        let mut stmt = conn.prepare(&sql)?;
        let refs: Vec<&dyn rusqlite::ToSql> = params_vec
            .iter()
            .map(|b| b.as_ref() as &dyn rusqlite::ToSql)
            .collect();
        let rows = stmt.query_map(refs.as_slice(), map_effort_escalation)?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Project `(name, description)` for every active chore on
    /// `product_id`. Used by the audit report to compute the
    /// per-marker `matches` denominator. Excludes deleted rows and
    /// non-chore kinds — the audit is a per-product chore-corpus
    /// snapshot, not a cross-kind scan.
    pub fn list_chores_for_audit(
        &self,
        product_id: &str,
    ) -> Result<Vec<crate::audit_effort::ChoreForAudit>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT name, description
             FROM tasks
             WHERE product_id = ?1
               AND kind = 'chore'
               AND deleted_at IS NULL",
        )?;
        let rows = stmt.query_map([product_id], |row| {
            Ok(crate::audit_effort::ChoreForAudit {
                name: row.get(0)?,
                description: row.get(1)?,
            })
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Most-recent execution for `work_item_id`, ordered by creation.
    /// `Ok(None)` when the work item has never had an execution.
    pub fn latest_execution_for_work_item(
        &self,
        work_item_id: &str,
    ) -> Result<Option<WorkExecution>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, work_item_id, kind, status, repo_remote_url, cube_repo_id, cube_lease_id,
                    cube_workspace_id, workspace_path, priority, preferred_workspace_id,
                    created_at, started_at, finished_at,
                    pre_start_failure_count, dispatch_not_before, pr_url, pr_head_before, prefer_is_soft, worker_branch_prefix, transient_failure_count, allow_dirty, branch_naming
             FROM work_executions
             WHERE work_item_id = ?1
             ORDER BY created_at DESC, id DESC
             LIMIT 1",
        )?;
        let mut rows = stmt.query_map([work_item_id], map_execution)?;
        match rows.next() {
            Some(row) => Ok(Some(row?)),
            None => Ok(None),
        }
    }

    // ── External-ref methods (T8) ────────────────────────────────────────────
    // Design: tools/boss/docs/designs/external-issue-tracker-sync-github-projects.md
    // §"Design Question 4" and §"Lookup methods on WorkDb".

    /// Bind `work_item_id` to the upstream issue identified by `(kind,
    /// canonical_id)`. Stores the tracker-specific `raw` blob (e.g.
    /// `{"issue_number": 560, "project_item_id": "..."}` for GitHub).
    /// Clears any prior `external_ref_unbound_at` marker so the row is
    /// treated as actively bound. Replaces an existing binding silently.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn set_external_ref(
        &self,
        work_item_id: &str,
        kind: &str,
        canonical_id: &str,
        raw: &serde_json::Value,
    ) -> Result<()> {
        let conn = self.connect()?;
        let raw_json = serde_json::to_string(raw)
            .with_context(|| format!("failed to serialise raw blob for {work_item_id}"))?;
        let n = conn.execute(
            "UPDATE tasks
             SET external_ref_kind         = ?2,
                 external_ref_canonical_id = ?3,
                 external_ref_raw          = ?4,
                 external_ref_unbound_at   = NULL,
                 updated_at                = ?5
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, kind, canonical_id, raw_json, now_string()],
        )?;
        if n == 0 {
            bail!("work item not found or soft-deleted: {work_item_id}");
        }
        Ok(())
    }

    /// Mark the external-ref binding on `work_item_id` as unbound.
    /// Retains `external_ref_kind` and `external_ref_canonical_id` so
    /// [`find_by_external_ref`][Self::find_by_external_ref] can
    /// re-bind automatically when the upstream item reappears. Sets
    /// `external_ref_unbound_at` to now and clears `external_ref_synced_at`.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn clear_external_ref(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE tasks
             SET external_ref_synced_at  = NULL,
                 external_ref_unbound_at = ?2,
                 updated_at              = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        if n == 0 {
            bail!("work item not found or soft-deleted: {work_item_id}");
        }
        Ok(())
    }

    /// Fetch a single task/chore by primary id, including the
    /// `external_ref_*` columns. Used by the `LinkWorkItemExternalRef` /
    /// `UnlinkWorkItemExternalRef` handlers so the `WorkItemUpdated`
    /// response carries the live `external_ref` snapshot.
    ///
    /// Returns an error if the work item does not exist or is soft-deleted.
    pub fn get_task_with_external_ref(&self, id: &str) -> Result<WorkItem> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state,
                    external_ref_kind, external_ref_canonical_id, external_ref_raw,
                    external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE id = ?1 AND deleted_at IS NULL",
            [id],
            map_task_with_external_ref,
        )
        .optional()
        .map_err(anyhow::Error::from)?
        .map(task_to_item)
        .with_context(|| format!("work item not found or soft-deleted: {id}"))
    }

    /// Find the work item actively bound to `(kind, canonical_id)`.
    /// Returns `None` when no matching active binding exists. Rows where
    /// `external_ref_unbound_at IS NOT NULL` are excluded (they retain
    /// their `canonical_id` for automatic re-binding, but are not
    /// considered "found" by this query). Soft-deleted tasks are always
    /// excluded.
    ///
    /// The returned `Task.external_ref` is populated, including a derived
    /// `web_url` (see [`derive_external_ref_web_url`]).
    pub fn find_by_external_ref(&self, kind: &str, canonical_id: &str) -> Result<Option<Task>> {
        let conn = self.connect()?;
        conn.query_row(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal,
                    pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor,
                    priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url,
                    effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id,
                    ci_required_state, review_required_state, ci_required_detail,
                    review_required_detail, pr_state_polled_at, merge_queue_state,
                    external_ref_kind, external_ref_canonical_id, external_ref_raw,
                    external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE external_ref_kind          = ?1
               AND external_ref_canonical_id  = ?2
               AND external_ref_unbound_at   IS NULL
               AND deleted_at               IS NULL",
            params![kind, canonical_id],
            map_task_with_external_ref,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Return every task under `product_id` that has a non-null
    /// `external_ref_canonical_id`, including previously-unbound rows
    /// (where `external_ref_unbound_at IS NOT NULL`). The reconciler
    /// uses this list to detect reappearing items (and re-bind them via
    /// [`set_external_ref`][Self::set_external_ref]) as well as to build
    /// the canonical-id → work-item map for each reconcile pass.
    ///
    /// Soft-deleted tasks are excluded.
    pub fn list_external_refs_for_product(
        &self,
        product_id: &str,
    ) -> Result<Vec<(String, StoredExternalRef)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, external_ref_kind, external_ref_canonical_id,
                    external_ref_raw, external_ref_synced_at, external_ref_unbound_at
             FROM tasks
             WHERE product_id                = ?1
               AND external_ref_canonical_id IS NOT NULL
               AND deleted_at               IS NULL",
        )?;
        let rows = stmt.query_map([product_id], |row| {
            let raw_json: Option<String> = row.get(3)?;
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                raw_json,
                row.get::<_, Option<String>>(4)?,
                row.get::<_, Option<String>>(5)?,
            ))
        })?;
        let mut result = Vec::new();
        for row in rows {
            let (id, kind, canonical_id, raw_json, synced_at, unbound_at) = row?;
            let raw: serde_json::Value = raw_json
                .as_deref()
                .and_then(|s| serde_json::from_str(s).ok())
                .unwrap_or(serde_json::Value::Null);
            result.push((
                id,
                StoredExternalRef {
                    kind,
                    canonical_id,
                    raw,
                    synced_at,
                    unbound_at,
                },
            ));
        }
        Ok(result)
    }

    /// Bump `external_ref_synced_at` to the current time for a work item.
    /// Called by the reconciler on every successful tick regardless of whether
    /// any other column changed. Does NOT update `updated_at` (keeping the
    /// reconciler tick invisible in the general-purpose "last modified" timeline).
    pub fn touch_external_ref_synced_at(&self, work_item_id: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE tasks SET external_ref_synced_at = ?2
             WHERE id = ?1 AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        Ok(())
    }

    /// Move `work_item_id` to `status = 'done'`, clearing any block reason.
    /// No-op (returns `false`) when the row is already done/archived or soft-deleted.
    /// Used by the external-tracker reconciler for close-mirror (Behavior 2) and
    /// PR-merge-close (Behavior 5). Cascades the dep-unblock sweep after commit.
    pub fn reconciler_close_work_item(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let Some(task) = query_task(&tx, work_item_id)? else {
            return Ok(false);
        };
        if task.deleted_at.is_some() || task.status == "done" || task.status == "archived" {
            return Ok(false);
        }
        let now = now_string();
        let n = tx.execute(
            "UPDATE tasks
             SET status             = 'done',
                 updated_at         = ?2,
                 last_status_actor  = 'engine',
                 blocked_reason     = NULL,
                 blocked_attempt_id = NULL
             WHERE id = ?1
               AND status NOT IN ('done', 'archived')
               AND deleted_at IS NULL",
            params![work_item_id, now],
        )?;
        if n > 0 {
            cascade_dependents_after_prereq_status_change(&tx, work_item_id, "done", &now)?;
        }
        tx.commit()?;
        Ok(n > 0)
    }

    /// Set `pr_url` on a work item if it is currently `NULL` or empty.
    /// Returns `true` when the column was written, `false` when it was
    /// already set (preserving the existing URL, which may come from a
    /// more-trusted source like the `pr_url_capture` pipeline).
    pub fn reconciler_attach_pr_url(&self, work_item_id: &str, pr_url: &str) -> Result<bool> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE tasks
             SET pr_url = ?2, updated_at = ?3
             WHERE id = ?1
               AND deleted_at IS NULL
               AND (pr_url IS NULL OR pr_url = '')",
            params![work_item_id, pr_url, now],
        )?;
        Ok(n > 0)
    }

    /// Return the stored Behavior 8 drift-detection checksums.
    ///
    /// Returns `Ok(None)` when either checksum column is `NULL` — this happens
    /// for items imported before the checksum migration. The reconciler
    /// establishes the baseline on the next pass without auto-syncing.
    ///
    /// The tuple is `(upstream_checksum, boss_checksum)` where each is a
    /// SHA-256 hex string computed by [`content_checksum`].
    pub fn reconciler_get_content_checksums(
        &self,
        work_item_id: &str,
    ) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        let result = conn
            .query_row(
                "SELECT external_ref_upstream_checksum, external_ref_boss_checksum
                 FROM tasks WHERE id = ?1 AND deleted_at IS NULL",
                params![work_item_id],
                |row| {
                    let upstream: Option<String> = row.get(0)?;
                    let boss: Option<String> = row.get(1)?;
                    Ok(upstream.zip(boss))
                },
            )
            .optional()?;
        Ok(result.flatten())
    }

    /// Persist the drift-detection checksums without modifying the task name
    /// or description.
    ///
    /// Called on the first reconcile tick for items imported before the
    /// checksum migration (`external_ref_upstream_checksum IS NULL`).
    pub fn reconciler_set_content_checksums_baseline(
        &self,
        work_item_id: &str,
        upstream_title: &str,
        upstream_body: &str,
        boss_name: &str,
        boss_description: &str,
    ) -> Result<()> {
        let upstream_checksum = content_checksum(upstream_title, upstream_body);
        let boss_checksum = content_checksum(boss_name, boss_description);
        let conn = self.connect()?;
        conn.execute(
            "UPDATE tasks
             SET external_ref_upstream_checksum = ?2,
                 external_ref_boss_checksum     = ?3
             WHERE id = ?1
               AND deleted_at IS NULL",
            params![work_item_id, upstream_checksum, boss_checksum],
        )?;
        Ok(())
    }

    /// Update a work item's `name` and `description` from upstream and record
    /// fresh checksums as the Behavior 8 sync baseline.
    ///
    /// Returns `true` if the row was found and updated, `false` if soft-deleted
    /// or not found (idempotent-safe).
    pub fn reconciler_update_name_and_description(
        &self,
        work_item_id: &str,
        new_name: &str,
        new_description: &str,
        upstream_title: &str,
        upstream_body: &str,
    ) -> Result<bool> {
        let upstream_checksum = content_checksum(upstream_title, upstream_body);
        let boss_checksum = content_checksum(new_name, new_description);
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE tasks
             SET name                               = ?2,
                 description                        = ?3,
                 external_ref_upstream_checksum     = ?4,
                 external_ref_boss_checksum         = ?5,
                 updated_at                         = ?6
             WHERE id = ?1
               AND deleted_at IS NULL",
            params![work_item_id, new_name, new_description, upstream_checksum, boss_checksum, now],
        )?;
        Ok(n > 0)
    }
}
