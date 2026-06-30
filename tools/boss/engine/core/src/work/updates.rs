use super::*;

impl WorkDb {
    pub(crate) fn update_product(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut product = query_product(&tx, id).require("product", id)?;

        apply_text_patch(&mut product.name, patch.name);
        apply_text_patch(&mut product.description, patch.description);
        apply_repo_remote_url_patch(&mut product.repo_remote_url, patch.repo_remote_url);
        apply_repo_remote_url_patch(&mut product.design_repo, patch.design_repo);
        apply_repo_remote_url_patch(&mut product.docs_repo, patch.docs_repo);
        apply_text_patch(&mut product.status, patch.status);
        apply_optional_string_patch(&mut product.default_model, patch.default_model);
        apply_optional_string_patch(&mut product.default_driver, patch.default_driver);
        apply_optional_string_patch(&mut product.dispatch_preamble, patch.dispatch_preamble);
        apply_optional_string_patch(&mut product.worker_branch_prefix, patch.worker_branch_prefix);
        // Re-canonicalise so a patched (or pre-existing) prefix always
        // carries its trailing `/`; idempotent on already-canonical
        // values and on `None`.
        product.worker_branch_prefix = canonicalize_worker_branch_prefix(product.worker_branch_prefix.take());
        product.slug = unique_product_slug_for_update(&tx, id, &slugify(&product.name))?;
        product.updated_at = now_string();

        tx.execute(
            "UPDATE products
             SET name = ?2, slug = ?3, description = ?4, repo_remote_url = ?5, status = ?6, updated_at = ?7, default_model = ?8, dispatch_preamble = ?9, design_repo = ?10, worker_branch_prefix = ?11, docs_repo = ?12, default_driver = ?13
             WHERE id = ?1",
            params![
                product.id,
                product.name,
                product.slug,
                product.description,
                product.repo_remote_url,
                product.status,
                product.updated_at,
                product.default_model,
                product.dispatch_preamble,
                product.design_repo,
                product.worker_branch_prefix,
                product.docs_repo,
                product.default_driver,
            ],
        )?;

        let updated = query_product(&tx, id).require("product", id)?;
        tx.commit()?;
        Ok(WorkItem::Product(updated))
    }

    pub(crate) fn update_project(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut project = query_project(&tx, id).require("project", id)?;
        let previous_status = project.status;
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut project.name, patch.name);
        apply_text_patch(&mut project.description, patch.description);
        apply_text_patch(&mut project.goal, patch.goal);
        if let Some(status_str) = patch.status {
            project.status = status_str.parse::<ProjectStatus>().map_err(|e| anyhow::anyhow!(e))?;
        }
        apply_text_patch(&mut project.priority, patch.priority);
        project.slug = unique_project_slug_for_update(&tx, &project.product_id, id, &slugify(&project.name))?;
        project.updated_at = now_string();

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, previous_status.as_str(), project.status.as_str())?;
        }
        let actor_stamp = if status_changed && previous_status != project.status {
            actor
        } else {
            ""
        };

        tx.execute(
            "UPDATE projects
             SET name = ?2, slug = ?3, description = ?4, goal = ?5, status = ?6, priority = ?7, updated_at = ?8,
                 last_status_actor = CASE WHEN ?9 = '' THEN last_status_actor ELSE ?9 END
             WHERE id = ?1",
            params![
                project.id,
                project.name,
                project.slug,
                project.description,
                project.goal,
                project.status.as_str(),
                project.priority,
                project.updated_at,
                actor_stamp,
            ],
        )?;

        if status_changed && previous_status != project.status {
            cascade_dependents_after_prereq_status_change(&tx, id, project.status.as_str(), &project.updated_at)?;
        }

        let updated = query_project(&tx, id).require("project", id)?;
        tx.commit()?;
        Ok(WorkItem::Project(updated))
    }

    pub(crate) fn update_task(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let mut task = query_task(&tx, id).require("task", id)?;
        if task.deleted_at.is_some() {
            bail!("cannot update a deleted task: {id}");
        }
        let previous_status = task.status.clone();
        let previous_blocked_reason = task.blocked_reason.clone();
        let status_changed = patch.status.is_some();

        apply_text_patch(&mut task.name, patch.name);
        apply_text_patch(&mut task.description, patch.description);
        if let Some(status_str) = patch.status {
            task.status = status_str.parse::<TaskStatus>().map_err(|e| anyhow::anyhow!(e))?;
        }
        apply_optional_patch(&mut task.pr_url, patch.pr_url);
        // Reject non-empty repo override when the product has its own repo.
        if let Some(ref repo_patch) = patch.repo_remote_url
            && !repo_patch.trim().is_empty()
        {
            let product = query_product(&tx, &task.product_id)?
                .with_context(|| format!("orphan task {id}: parent product {} missing", task.product_id))?;
            if let Some(product_repo) = product.repo_remote_url.as_deref() {
                bail!(
                    "cannot set per-task repo override on product `{}`: \
                         product has its own repo (`{}`). \
                         Clear the product's repo first, or omit --repo to inherit.",
                    product.slug,
                    product_repo,
                );
            }
        }
        apply_repo_remote_url_patch(&mut task.repo_remote_url, patch.repo_remote_url);
        if let Some(priority_patch) = patch.priority {
            task.priority = normalize_priority(Some(&priority_patch))?;
        }
        if let Some(effort_patch) = patch.effort_level {
            // Empty string clears the column; anything else must
            // parse as one of the five allowed levels. Invalid
            // values reject the whole patch — no half-updates.
            let trimmed = effort_patch.trim();
            task.effort_level = if trimmed.is_empty() {
                None
            } else {
                Some(trimmed.parse::<EffortLevel>().map_err(|e| anyhow::anyhow!(e))?)
            };
        }
        apply_optional_string_patch(&mut task.model_override, patch.model_override);
        apply_optional_string_patch(&mut task.driver, patch.driver);
        apply_optional_string_patch(&mut task.blocked_reason, patch.blocked_reason);
        if let Some(autostart) = patch.autostart {
            task.autostart = autostart;
        }
        if let Some(ordinal) = patch.ordinal {
            task.ordinal = Some(ordinal);
        }
        task.updated_at = now_string();

        // Invariant: blocked_reason and blocked_attempt_id must be NULL for any
        // non-blocked status. Enforce this here so every write path honours it,
        // not just the engine's targeted CI/conflict-resolution helpers.
        if task.status != TaskStatus::Blocked {
            task.blocked_reason = None;
            task.blocked_attempt_id = None;
        }

        if status_changed {
            refuse_manual_move_off_blocked_while_gated(&tx, id, previous_status.as_str(), task.status.as_str())?;
        }
        let actor_stamp = if status_changed && previous_status != task.status {
            actor
        } else {
            ""
        };

        let effort_level_value = task.effort_level.map(|level| level.as_str().to_owned());

        tx.execute(
            "UPDATE tasks
             SET name = ?2, description = ?3, status = ?4, ordinal = ?5, pr_url = ?6, updated_at = ?7,
                 priority = ?9, repo_remote_url = ?10,
                 effort_level = ?11, model_override = ?12, autostart = ?13,
                 blocked_reason = ?14, blocked_attempt_id = ?15, driver = ?16,
                 last_status_actor = CASE WHEN ?8 = '' THEN last_status_actor ELSE ?8 END,
                 completed_at = CASE
                     WHEN ?4 IN ('done', 'archived', 'cancelled') THEN COALESCE(completed_at, ?7)
                     ELSE NULL
                 END
             WHERE id = ?1",
            params![
                task.id,
                task.name,
                task.description,
                task.status.as_str(),
                task.ordinal,
                task.pr_url,
                task.updated_at,
                actor_stamp,
                task.priority,
                task.repo_remote_url,
                effort_level_value,
                task.model_override,
                task.autostart as i64,
                task.blocked_reason,
                task.blocked_attempt_id,
                task.driver,
            ],
        )?;

        if status_changed && previous_status != task.status {
            cascade_dependents_after_prereq_status_change(&tx, id, task.status.as_str(), &task.updated_at)?;
        }

        // Manual-override suppression for `blocked: ci_failure` /
        // `ci_failure_exhausted` (design §Q5 / Phase 12 #38). A human
        // pulling a chore out of the CI-failure column is a signal that
        // the engine should keep its hands off the current head sha —
        // otherwise the very next probe re-observes the failure and
        // immediately re-flips the row. We honour the override by:
        //   1) inserting a `ci_failure_suppressions` row keyed on the
        //      head_sha of the most recent CI attempt (a fresh push
        //      changes the key and naturally invalidates suppression),
        //   2) resetting `ci_attempts_used` so a future probe (on a
        //      new head) starts with a fresh budget — mirrors the
        //      `boss engine ci retry` reset rule.
        if status_changed
            && previous_status == TaskStatus::Blocked
            && task.status != TaskStatus::Blocked
            && matches!(
                previous_blocked_reason.as_deref(),
                Some("ci_failure") | Some("ci_failure_exhausted")
            )
        {
            record_ci_failure_suppression_in_tx(&tx, id, &task.updated_at)?;
        }

        let updated = query_task(&tx, id).require("task", id)?;
        tx.commit()?;
        Ok(task_to_item(updated))
    }
}
