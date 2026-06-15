use super::*;

impl WorkDb {
    pub fn create_attention_item(&self, input: CreateAttentionItemInput) -> Result<WorkAttentionItem> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let (execution_id, work_item_id) = attention_target_from_input(&tx, &input)?;

        let id = next_id("attn");
        let now = now_string();
        let status = input.status.unwrap_or_else(|| "open".to_owned());
        let resolved_at = normalize_optional_text(input.resolved_at);

        tx.execute(
            "INSERT INTO work_attention_items (
                id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                id,
                execution_id,
                work_item_id,
                input.kind,
                status,
                input.title,
                input.body_markdown,
                now,
                resolved_at,
            ],
        )?;

        let item =
            query_attention_item(&tx, &id)?.with_context(|| format!("missing attention item after insert: {id}"))?;
        tx.commit()?;
        Ok(item)
    }

    pub fn list_attention_items(&self, execution_id: &str) -> Result<Vec<WorkAttentionItem>> {
        let conn = self.connect()?;
        ensure_execution_exists(&conn, execution_id)?;

        let mut stmt = conn.prepare(
            "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE execution_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([execution_id], map_attention_item)?;
        collect_rows(rows)
    }

    /// List the sticky, pre-dispatch attention items attached to a
    /// work item (i.e. `work_item_id IS NOT NULL`). Used by the
    /// `repo_unresolved` surface and any future work-item-scoped
    /// attention flows. Errors if the work item id is unknown so
    /// callers can't accidentally silently no-op on a typo.
    pub fn list_attention_items_for_work_item(&self, work_item_id: &str) -> Result<Vec<WorkAttentionItem>> {
        let conn = self.connect()?;
        let _ = product_id_for_work_item(&conn, work_item_id)?;
        let mut stmt = conn.prepare(
            "SELECT id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
             FROM work_attention_items
             WHERE work_item_id = ?1
             ORDER BY created_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([work_item_id], map_attention_item)?;
        collect_rows(rows)
    }

    pub fn get_attention_item(&self, id: &str) -> Result<WorkAttentionItem> {
        let conn = self.connect()?;
        query_attention_item(&conn, id).require("attention item", id)
    }

    /// Create an external-tracker attention item for `work_item_id` unless one
    /// with the same `kind` is already open. Idempotent: repeated reconciler
    /// ticks for the same failure do not pile up rows.
    pub fn upsert_external_tracker_attention(
        &self,
        work_item_id: &str,
        kind: &str,
        title: &str,
        body_markdown: &str,
    ) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = product_id_for_work_item(&tx, work_item_id)?;
        let already_open: i64 = tx.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_attention_items
                 WHERE work_item_id = ?1
                   AND kind = ?2
                   AND status = 'open'
             )",
            params![work_item_id, kind],
            |row| row.get(0),
        )?;
        if already_open == 0 {
            let id = next_id("attn");
            let now = now_string();
            tx.execute(
                "INSERT INTO work_attention_items (
                    id, execution_id, work_item_id, kind, status, title, body_markdown, created_at, resolved_at
                 ) VALUES (?1, NULL, ?2, ?3, 'open', ?4, ?5, ?6, NULL)",
                params![id, work_item_id, kind, title, body_markdown, now],
            )?;
        }
        tx.commit()?;
        Ok(())
    }

    /// Mark all open attention items of `kind` for `work_item_id` as resolved.
    /// A no-op when none are open. Used by the external-tracker reconciler to
    /// clear stale failure items once a product recovers.
    pub fn resolve_external_tracker_attention(&self, work_item_id: &str, kind: &str) -> Result<()> {
        let conn = self.connect()?;
        let now = now_string();
        conn.execute(
            "UPDATE work_attention_items
             SET status = 'resolved', resolved_at = ?1
             WHERE work_item_id = ?2
               AND kind = ?3
               AND status = 'open'",
            params![now, work_item_id, kind],
        )?;
        Ok(())
    }

    pub fn update_work_item(&self, id: &str, patch: WorkItemPatch) -> Result<WorkItem> {
        self.update_work_item_as_actor(id, patch, "human")
    }

    /// Like `update_work_item` but stamps `last_status_actor` with `actor`
    /// when the status actually changes. Engine-internal writers use direct
    /// SQL with `last_status_actor = 'engine'`; this path is for peer RPCs
    /// where the caller tier has already been resolved to `"human"` or
    /// `"boss"`.
    pub fn update_work_item_as_actor(&self, id: &str, patch: WorkItemPatch, actor: &str) -> Result<WorkItem> {
        match classify_id(id)? {
            ItemKind::Product => self.update_product(id, patch),
            ItemKind::Project => self.update_project(id, patch, actor),
            ItemKind::Task => self.update_task(id, patch, actor),
        }
    }

    pub fn delete_work_item(&self, id: &str) -> Result<()> {
        match classify_id(id)? {
            ItemKind::Task => {
                let mut conn = self.connect()?;
                let tx = conn.transaction()?;
                let now = now_string();
                let rows = tx.execute(
                    "UPDATE tasks SET deleted_at = ?2, updated_at = ?2
                     WHERE id = ?1 AND deleted_at IS NULL",
                    params![id, now],
                )?;
                if rows == 0 {
                    bail!("unknown task: {id}");
                }
                // Cascade soft-delete to every revision in the chain
                // (BFS over parent_task_id links). A live revision is
                // meaningless once its parent is gone — it can only ever
                // amend the parent's PR, which no longer exists — and
                // leaves board noise / dispatch hazards.
                // Use the same `now` timestamp so restore can identify
                // which revisions were cascade-deleted alongside this parent.
                let revision_ids = collect_chain_revision_ids(&tx, id)?;
                for rev_id in &revision_ids {
                    tx.execute(
                        "UPDATE tasks SET deleted_at = ?2, updated_at = ?2
                         WHERE id = ?1 AND deleted_at IS NULL",
                        params![rev_id, now],
                    )?;
                }
                // Q10 (deleted prereq): drop every dependency edge that
                // names this task as either endpoint. A row with a
                // tombstoned prerequisite is the worst of both worlds —
                // dependents stuck on a row that is no longer a thing.
                tx.execute(
                    "DELETE FROM work_item_dependencies
                     WHERE dependent_id = ?1 OR prerequisite_id = ?1",
                    params![id],
                )?;
                tx.commit()?;
                Ok(())
            }
            ItemKind::Product => bail!("product deletion is not supported; archive it instead"),
            ItemKind::Project => bail!("project deletion is not supported; archive it instead"),
        }
    }

    /// Inverse of [`Self::delete_work_item`]: clear the `deleted_at`
    /// tombstone on a soft-deleted task so it becomes visible again.
    /// Accepts a canonical `task_…` id or a friendly short id (`T43`);
    /// the friendly resolution deliberately includes soft-deleted rows
    /// so a tombstoned task is still findable. Idempotent — restoring a
    /// row that is already live succeeds as a no-op. Returns the now-live
    /// work item.
    ///
    /// Cascade-restore: child revisions whose `deleted_at` matches the
    /// parent's tombstone timestamp were cascade-deleted by
    /// [`Self::delete_work_item`] and are restored together with the
    /// parent in the same transaction. Revisions that were independently
    /// deleted (different timestamp) are left tombstoned.
    ///
    /// Note: the dependency edges that `delete_work_item` dropped are
    /// NOT recreated — they were deleted outright, not tombstoned, so a
    /// restored task comes back with no dependency edges. The operator
    /// must re-add any that still matter.
    pub fn restore_work_item(&self, id: &str) -> Result<WorkItem> {
        let mut conn = self.connect()?;
        let canonical = resolve_friendly_work_item_id_inner(&conn, id, true)?.unwrap_or_else(|| id.to_owned());
        match classify_id(&canonical)? {
            ItemKind::Task => {
                // The row must exist (live or tombstoned). A WHERE that
                // matched only tombstoned rows would make an idempotent
                // re-restore indistinguishable from an unknown id, so we
                // check existence separately before the conditional
                // UPDATE.
                let exists = conn
                    .query_row("SELECT 1 FROM tasks WHERE id = ?1", params![canonical], |_| Ok(()))
                    .optional()?
                    .is_some();
                if !exists {
                    bail!("unknown task: {canonical}");
                }
                // Capture the parent's current deleted_at before restoring.
                // Revisions with the same timestamp were cascade-deleted by
                // delete_work_item and should be restored alongside the parent.
                let parent_deleted_at: Option<String> = conn
                    .query_row(
                        "SELECT deleted_at FROM tasks WHERE id = ?1",
                        params![canonical],
                        |row| row.get(0),
                    )
                    .optional()?
                    .flatten();
                let now = now_string();
                {
                    let tx = conn.transaction()?;
                    tx.execute(
                        "UPDATE tasks SET deleted_at = NULL, updated_at = ?2
                         WHERE id = ?1 AND deleted_at IS NOT NULL",
                        params![canonical, now],
                    )?;
                    // BFS to restore all cascade-deleted revisions that share
                    // the parent's tombstone timestamp. Revisions deleted
                    // independently (different deleted_at) are left tombstoned.
                    if let Some(ref deleted_at) = parent_deleted_at {
                        let mut frontier = vec![canonical.clone()];
                        for _ in 0..64 {
                            if frontier.is_empty() {
                                break;
                            }
                            let mut next = Vec::new();
                            for ancestor_id in &frontier {
                                let children: Vec<String> = tx
                                    .prepare_cached(
                                        "SELECT id FROM tasks
                                         WHERE parent_task_id = ?1
                                           AND kind = 'revision'
                                           AND deleted_at = ?2",
                                    )?
                                    .query_map(params![ancestor_id, deleted_at], |row| row.get(0))?
                                    .filter_map(|r| r.ok())
                                    .collect();
                                for child_id in children {
                                    tx.execute(
                                        "UPDATE tasks SET deleted_at = NULL, updated_at = ?2
                                         WHERE id = ?1",
                                        params![child_id, now],
                                    )?;
                                    next.push(child_id);
                                }
                            }
                            frontier = next;
                        }
                    }
                    tx.commit()?;
                }
                query_task(&conn, &canonical)?
                    .map(task_to_item)
                    .with_context(|| format!("unknown task: {canonical}"))
            }
            ItemKind::Product => {
                bail!("products are archived, not soft-deleted; nothing to restore")
            }
            ItemKind::Project => {
                bail!("projects are archived, not soft-deleted; nothing to restore")
            }
        }
    }

    pub fn get_work_tree(&self, product_id: &str) -> Result<WorkTree> {
        let conn = self.connect()?;
        let product = query_product(&conn, product_id).require("product", product_id)?;

        let projects = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                        design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
                 FROM projects
                 WHERE product_id = ?1
                 ORDER BY created_at ASC, name COLLATE NOCASE ASC",
            )?;
            let rows = stmt.query_map([product_id], map_project)?;
            collect_rows(rows)?
        };

        let tasks = {
            // `kind IN ('project_task', 'design')` — the design task
            // auto-created at project birth lives in the same lane as
            // every other project task. Sorting on `ordinal` ASC puts
            // the design task (ordinal = 0) at the head of the
            // project's task chain, which matches the kanban
            // expectation that design lands first.
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver, external_ref_kind, external_ref_canonical_id, external_ref_raw, external_ref_synced_at, external_ref_unbound_at, parent_task_id, source_automation_id
                 FROM tasks
                 WHERE product_id = ?1 AND kind IN ('project_task', 'design', 'investigation', 'revision') AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task_with_external_ref_parent_and_source_automation_id)?;
            collect_rows(rows)?
        };

        let mut chores = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver, external_ref_kind, external_ref_canonical_id, external_ref_raw, external_ref_synced_at, external_ref_unbound_at, parent_task_id, source_automation_id
                 FROM tasks
                 WHERE product_id = ?1 AND kind = 'chore' AND deleted_at IS NULL
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([product_id], map_task_with_external_ref_parent_and_source_automation_id)?;
            collect_rows(rows)?
        };

        let task_runtimes = collect_task_runtimes(&conn, &tasks, &chores)?;
        let dependencies = collect_product_dependencies(&conn, product_id)?;

        // Compute revision projections (revision_seq, revision_parent_pr_url)
        // for every `kind = 'revision'` task. These are derived fields —
        // not stored columns — so they are calculated fresh here.
        let mut tasks = attach_revision_projections(tasks, &chores);
        // Compute has_in_progress_revision for every chain-root task that
        // has at least one todo/active descendant revision.
        attach_in_progress_revision_flag(&mut tasks, &mut chores);
        // Compute ai_reviewing for tasks held in Doing while a pr_review
        // execution is in flight. Surfaces the "Reviewing (AI)" badge on
        // the kanban card. Errors are non-fatal — log and continue with
        // the field defaulting to false.
        if let Err(err) = attach_ai_reviewing_flag(&conn, &mut tasks, &mut chores) {
            tracing::warn!(?err, "get_work_tree: failed to attach ai_reviewing flag; ignoring");
        }

        // Resolve the per-task doc-link state for project-less docs-backed
        // items (investigations / project-less designs) so their card renders
        // the Review-lane doc-link icon — parity with design cards, whose
        // state is resolved from the parent project's `design_doc_*` columns.
        // Pass `|_| None` for the workspace lookup: cube is not consulted in
        // get_work_tree, and the app prefers the GitHub raw-content URL for
        // in-review docs on the PR head branch anyway. Errors per task are
        // non-fatal — log and leave the field None (affordance hidden).
        for task in &mut tasks {
            if !crate::design_detector::task_uses_per_task_doc(&task.kind, task.project_id.is_none()) {
                continue;
            }
            match resolve_task_doc_pointer(&conn, &task.id, |_| None) {
                Ok(state) => task.doc_link_state = state,
                Err(err) => tracing::warn!(
                    task_id = %task.id,
                    ?err,
                    "get_work_tree: failed to resolve task doc-link state; leaving affordance hidden"
                ),
            }
        }

        Ok(WorkTree {
            product,
            projects,
            tasks,
            chores,
            task_runtimes,
            dependencies,
        })
    }

    pub fn reorder_project_tasks(&self, project_id: &str, task_ids: &[String]) -> Result<()> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        ensure_project_exists(&tx, project_id)?;

        let mut existing = {
            let mut stmt = tx.prepare(
                "SELECT id
                 FROM tasks
                 WHERE project_id = ?1 AND kind = 'project_task' AND deleted_at IS NULL
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            )?;
            let rows = stmt.query_map([project_id], |row| row.get::<_, String>(0))?;
            collect_rows(rows)?
        };
        let mut requested = task_ids.to_vec();
        existing.sort();
        requested.sort();
        if existing != requested {
            bail!("reorder request must include the full active task set for the project");
        }

        for (index, task_id) in task_ids.iter().enumerate() {
            tx.execute(
                "UPDATE tasks SET ordinal = ?2, updated_at = ?3 WHERE id = ?1",
                params![task_id, (index as i64) + 1, now_string()],
            )?;
        }

        tx.commit()?;
        Ok(())
    }

    pub fn get_work_item(&self, id: &str) -> Result<WorkItem> {
        let conn = self.connect()?;
        match classify_id(id)? {
            ItemKind::Product => query_product(&conn, id)?
                .map(WorkItem::Product)
                .with_context(|| format!("unknown product: {id}")),
            ItemKind::Project => query_project(&conn, id)?
                .map(WorkItem::Project)
                .with_context(|| format!("unknown project: {id}")),
            ItemKind::Task => query_task(&conn, id)?
                .filter(|task| task.deleted_at.is_none())
                .map(task_to_item)
                .with_context(|| format!("unknown task: {id}")),
        }
    }

    /// Look up a work item by canonical id or short-form (`T42`,
    /// `t42`). Returns `Ok(None)` when no item matches, `Ok(Some(…))`
    /// on success. Canonical ids are passed straight to
    /// [`get_work_item`]; short-form ids are first resolved via
    /// [`resolve_friendly_work_item_id`] and then fetched by canonical
    /// id. Unlike `get_work_item`, this never calls `classify_id`, so
    /// it accepts both forms without the caller choosing.
    pub fn get_work_item_resolving_short_id(&self, id: &str) -> Result<Option<WorkItem>> {
        let conn = self.connect()?;
        let canonical = resolve_friendly_work_item_id(&conn, id)?.unwrap_or_else(|| id.to_owned());
        match classify_id(&canonical) {
            Ok(_) => match self.get_work_item(&canonical) {
                Ok(item) => Ok(Some(item)),
                Err(_) => Ok(None),
            },
            Err(_) => Ok(None),
        }
    }

    /// Single-item version of the per-task runtime data carried in
    /// `WorkTree::task_runtimes`. Backs the `GetTaskRuntime` RPC that
    /// `boss chore show` / `boss task show` use to surface the active
    /// execution + run on the rendered work item. The lookup never
    /// fails on missing executions: an untouched work item simply
    /// returns a `TaskRuntime` with every `Option` field set to
    /// `None`. Friendly ids (`T42`, `boss/42`) are resolved to primary
    /// ids before the query runs, matching `get_work_item`'s contract.
    pub fn get_task_runtime(&self, work_item_id: &str) -> Result<TaskRuntime> {
        let conn = self.connect()?;
        let resolved = resolve_friendly_work_item_id(&conn, work_item_id)?.unwrap_or_else(|| work_item_id.to_owned());
        query_task_runtime(&conn, &resolved)
    }

    /// Look up a work item by its per-product short_id. Searches both
    /// the `tasks` table (returning `Task` or `Chore`) and the
    /// `projects` table, returning the first match. Returns `None` if
    /// no row with `(product_id, short_id)` exists.
    ///
    /// The per-product sequence is shared across tasks and projects
    /// (design Q1), so each short_id belongs to at most one row across
    /// both tables for a given product.
    pub fn get_work_item_by_short_id(&self, product_id: &str, short_id: i64) -> Result<Option<WorkItem>> {
        let conn = self.connect()?;
        if let Some(task) = conn
            .query_row(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver, parent_task_id
                 FROM tasks
                 WHERE product_id = ?1 AND short_id = ?2 AND deleted_at IS NULL",
                params![product_id, short_id],
                map_task_with_parent,
            )
            .optional()?
        {
            return Ok(Some(task_to_item(task)));
        }
        if let Some(project) = conn
            .query_row(
                "SELECT id, product_id, name, slug, description, goal, status, priority, created_at, updated_at, last_status_actor,
                        design_doc_repo_remote_url, design_doc_branch, design_doc_path, short_id
                 FROM projects
                 WHERE product_id = ?1 AND short_id = ?2",
                params![product_id, short_id],
                map_project,
            )
            .optional()?
        {
            return Ok(Some(WorkItem::Project(project)));
        }
        Ok(None)
    }

    /// Find every work item bound to GitHub PR number `pr_number`,
    /// together with the revisions in each match's chain.
    ///
    /// Unlike [`Self::list_tasks`] / [`Self::list_chores`] this scans
    /// the entire `tasks` table — all kinds (`project_task`, `chore`,
    /// `design`, `investigation`, `revision`) across every product — so
    /// a chore- or revision-backed PR is just as findable as a project
    /// task. The PR number is parsed from each row's stored `pr_url`
    /// using the same parser the merge poller uses, so query strings and
    /// fragments are tolerated. Revision tasks normally carry no
    /// `pr_url` (they surface inside a matched owner's `revisions`
    /// list), but may have one in exceptional engine recovery paths
    /// (e.g. the double-spawn race recovery), in which case they appear
    /// as owners too.
    ///
    /// Soft-deleted rows are excluded from both owners and revisions.
    /// Returns an empty vec when no row is bound to the PR number. More
    /// than one element may mean the same PR number exists in multiple
    /// repos, or that same-repo same-PR multiplicity occurred (e.g. a
    /// chore owner and a revision that each carry the URL). The caller
    /// must display all matches; `--repo` is an optional filter, not a
    /// required disambiguator.
    pub fn find_work_items_by_pr(&self, pr_number: i64) -> Result<Vec<PrWorkItemMatch>> {
        let conn = self.connect()?;
        // Owners: any live row carrying a pr_url. We parse the number in
        // Rust (rather than a SQL LIKE) so the match is identical to the
        // merge poller's and robust to `?`/`#` suffixes.
        let owners: Vec<Task> = {
            let mut stmt = conn.prepare(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver, parent_task_id
                 FROM tasks
                 WHERE pr_url IS NOT NULL AND pr_url != '' AND deleted_at IS NULL
                 ORDER BY created_at ASC",
            )?;
            let rows = stmt.query_map([], map_task_with_parent)?;
            collect_rows(rows)?
                .into_iter()
                .filter(|task| task.pr_url.as_deref().and_then(crate::merge_poller::parse_pr_number) == Some(pr_number))
                .collect()
        };

        let mut matches = Vec::with_capacity(owners.len());
        for owner in owners {
            // Gather every revision in this owner's chain (BFS over
            // parent_task_id) and annotate them with revision_seq /
            // revision_parent_pr_url so the caller can render R1, R2, ….
            let revision_ids = collect_chain_revision_ids(&conn, &owner.id)?;
            let mut revisions = Vec::with_capacity(revision_ids.len());
            for rev_id in &revision_ids {
                if let Some(task) = query_task(&conn, rev_id)?
                    && task.deleted_at.is_none()
                {
                    revisions.push(task);
                }
            }
            let mut revisions = attach_revision_projections(revisions, std::slice::from_ref(&owner));
            revisions.sort_by_key(|rev| rev.revision_seq.unwrap_or(i64::MAX));
            matches.push(PrWorkItemMatch { owner, revisions });
        }
        Ok(matches)
    }

    pub fn list_tasks(
        &self,
        product_id: &str,
        project_id: Option<&str>,
        dep_filter: Option<&DependencyFilter>,
        include_deleted: bool,
    ) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        // The soft-delete predicate is dropped entirely (rather than
        // negated) when the caller asks to include deleted rows, so the
        // default `--deleted` view shows live and tombstoned tasks
        // together. Restore acts on the tombstoned ones.
        let deleted_clause = if include_deleted { "" } else { " AND deleted_at IS NULL" };

        let mut tasks = if let Some(project_id) = project_id {
            ensure_project_belongs_to_product(&conn, project_id, product_id)?;
            let mut stmt = conn.prepare(&format!(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver
                 FROM tasks
                 WHERE product_id = ?1 AND project_id = ?2 AND kind IN ('project_task', 'design', 'investigation'){deleted_clause}
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            ))?;
            let rows = stmt.query_map(params![product_id, project_id], map_task)?;
            collect_rows(rows)?
        } else {
            let mut stmt = conn.prepare(&format!(
                "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver
                 FROM tasks
                 WHERE product_id = ?1 AND kind IN ('project_task', 'design', 'investigation'){deleted_clause}
                 ORDER BY COALESCE(ordinal, 0) ASC, created_at ASC",
            ))?;
            let rows = stmt.query_map([product_id], map_task)?;
            collect_rows(rows)?
        };

        if let Some(filter) = dep_filter {
            apply_dep_filter(
                &conn,
                filter,
                |task: &Task| task.id.as_str(),
                |task: &Task| task.status.as_str(),
                &mut tasks,
            )?;
        }
        Ok(tasks)
    }

    /// Look up a cached pane-titlebar summary for a work item.
    /// Returns `(summary, basis_hash)` so callers can compare the
    /// stored basis against a freshly computed one to decide whether
    /// the cache is still valid.
    /// Read a value from the engine's metadata KV. Returns `None` if
    /// the key has never been written. Used by the engine for small
    /// persisted settings (live-status disabled slot list, schema
    /// version, etc.) that don't deserve their own table.
    pub fn get_metadata(&self, key: &str) -> Result<Option<String>> {
        let conn = self.connect()?;
        let row = conn
            .query_row("SELECT value FROM metadata WHERE key = ?1", params![key], |row| {
                row.get::<_, String>(0)
            })
            .optional()?;
        Ok(row)
    }

    /// Insert-or-replace a metadata value. The metadata table is the
    /// engine-side KV store — schema version, persisted live-status
    /// disabled slots, anything that needs to outlive the process
    /// without justifying a dedicated table.
    pub fn set_metadata(&self, key: &str, value: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO metadata (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }

    pub fn get_pane_summary(&self, work_item_id: &str) -> Result<Option<(String, String)>> {
        let conn = self.connect()?;
        let row = conn
            .query_row(
                "SELECT summary, basis_hash FROM pane_summaries WHERE work_item_id = ?1",
                params![work_item_id],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        Ok(row)
    }

    /// Insert or replace the cached pane summary for a work item.
    /// `basis_hash` should be derived from the inputs that, if
    /// changed, invalidate the cached summary (typically a hash of
    /// name + description).
    pub fn set_pane_summary(&self, work_item_id: &str, summary: &str, basis_hash: &str) -> Result<()> {
        let conn = self.connect()?;
        conn.execute(
            "INSERT INTO pane_summaries (work_item_id, summary, basis_hash, created_at)
             VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(work_item_id) DO UPDATE SET
                 summary = excluded.summary,
                 basis_hash = excluded.basis_hash,
                 created_at = excluded.created_at",
            params![work_item_id, summary, basis_hash, now_string()],
        )?;
        Ok(())
    }

    pub fn list_chores(
        &self,
        product_id: &str,
        dep_filter: Option<&DependencyFilter>,
        include_deleted: bool,
    ) -> Result<Vec<Task>> {
        let conn = self.connect()?;
        ensure_product_exists(&conn, product_id)?;

        // See `list_tasks` for the include-deleted contract.
        let deleted_clause = if include_deleted { "" } else { " AND deleted_at IS NULL" };
        let mut stmt = conn.prepare(&format!(
            "SELECT id, product_id, project_id, kind, name, description, status, ordinal, pr_url, deleted_at, created_at, updated_at, autostart, last_status_actor, priority, created_via, blocked_reason, blocked_attempt_id, repo_remote_url, effort_level, model_override, ci_attempt_budget, ci_attempts_used, short_id, ci_required_state, review_required_state, ci_required_detail, review_required_detail, pr_state_polled_at, merge_queue_state, driver
             FROM tasks
             WHERE product_id = ?1 AND kind = 'chore'{deleted_clause}
             ORDER BY created_at ASC",
        ))?;
        let rows = stmt.query_map([product_id], map_task)?;
        let mut chores: Vec<Task> = collect_rows(rows)?;
        if let Some(filter) = dep_filter {
            apply_dep_filter(
                &conn,
                filter,
                |chore: &Task| chore.id.as_str(),
                |chore: &Task| chore.status.as_str(),
                &mut chores,
            )?;
        }
        Ok(chores)
    }

    /// Read the unsatisfied prerequisites of `work_item_id` outside
    /// of any in-flight transaction. Used by the engine app to refuse
    /// `RequestExecution` against a gated work item (see
    /// `boss::engine::app`).
    pub fn gating_prereqs_for(&self, work_item_id: &str) -> Result<Vec<String>> {
        let conn = self.connect()?;
        deps::gating_prereqs_for(&conn, work_item_id)
    }

    /// Declare a `relation` edge from `dependent` to `prerequisite`.
    /// Validates both endpoints resolve to live work items in the
    /// same product, refuses self-edges and cycles, and is
    /// idempotent on a re-add of an existing edge.
    ///
    /// v1 ships only `relation = 'blocks'`. The CLI accepts an
    /// explicit `--relation` flag but rejects anything else; the
    /// column accepts any TEXT value at the schema level so future
    /// relation types can ship without a re-migration.
    pub fn add_dependency(&self, input: AddDependencyInput) -> Result<WorkItemDependency> {
        let relation = input
            .relation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(RELATION_BLOCKS);
        if relation != RELATION_BLOCKS {
            bail!("unsupported dependency relation `{relation}`; only `blocks` is implemented in v1");
        }
        let dependent_id = input.dependent.trim();
        let prerequisite_id = input.prerequisite.trim();
        if dependent_id.is_empty() || prerequisite_id.is_empty() {
            bail!("dependent and prerequisite ids are required");
        }
        if dependent_id == prerequisite_id {
            bail!("a work item cannot depend on itself: {dependent_id}");
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Both ids must resolve and live in the same product. Cross-
        // product edges are tracked separately (see proj_18a2bbe20fc03718_8).
        let dependent_product = product_id_for_work_item(&tx, dependent_id)?;
        let prerequisite_product = product_id_for_work_item(&tx, prerequisite_id)?;
        if dependent_product != prerequisite_product {
            bail!(
                "dependency edges must stay within a single product; cross-product edges are tracked in proj_18a2bbe20fc03718_8"
            );
        }
        if deps::would_create_cycle(&tx, dependent_id, prerequisite_id)? {
            bail!("creating this edge would form a cycle: {prerequisite_id} → … → {dependent_id}");
        }
        let now = now_string();
        let (edge, _outcome): (WorkItemDependency, EdgeInsertOutcome) =
            deps::insert_edge(&tx, dependent_id, prerequisite_id, relation, &now)?;
        // Auto-block (Q4): if the dependent isn't already `blocked`
        // and the new edge introduces a gating prereq, the engine
        // flips it to `blocked` and stamps `last_status_actor =
        // 'engine'` so the eventual auto-unblock knows the engine
        // owns this transition.
        maybe_engine_block_dependent(&tx, dependent_id, &now)?;
        tx.commit()?;
        Ok(edge)
    }

    /// Drop the named edge if it exists. No-op success when the edge
    /// is already absent (mirrors `boss <kind> delete` on an
    /// already-archived row).
    pub fn remove_dependency(&self, input: RemoveDependencyInput) -> Result<bool> {
        let relation = input
            .relation
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(RELATION_BLOCKS);
        let dependent_id = input.dependent.trim();
        let prerequisite_id = input.prerequisite.trim();
        if dependent_id.is_empty() || prerequisite_id.is_empty() {
            bail!("dependent and prerequisite ids are required");
        }
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let removed = deps::delete_edge(&tx, dependent_id, prerequisite_id, relation)?;
        // Auto-unblock (Q4): when the only remaining gating reason is
        // gone and the engine itself put this row in `blocked` (identified
        // by blocked_reason='dependency'), flip it back to `todo`.
        // Human-placed blocks (other blocked_reason / NULL + human actor)
        // stick — the user must clear them.
        let now = now_string();
        maybe_engine_unblock_dependent(&tx, dependent_id, &now)?;
        tx.commit()?;
        Ok(removed)
    }

    /// All task ids that are currently in `blocked` status because of
    /// a dependency edge the engine set — i.e. rows that the periodic
    /// dependency-unblock sweeper should evaluate. Returns
    /// `(task_id, updated_at_epoch_secs)` so the sweeper can compute
    /// how long each row has been stuck.
    ///
    /// The candidate set is:
    ///   - `blocked_reason = 'dependency'`  — set by `maybe_engine_block_dependent`
    ///   - `blocked_reason IS NULL AND last_status_actor = 'engine'`  — pre-backfill rows
    pub fn list_dependency_blocked_candidates(&self) -> Result<Vec<(String, i64)>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, CAST(updated_at AS INTEGER)
             FROM tasks
             WHERE status = 'blocked'
               AND deleted_at IS NULL
               AND (
                   blocked_reason = 'dependency'
                   OR (blocked_reason IS NULL AND last_status_actor = 'engine')
               )
             ORDER BY updated_at ASC, id ASC",
        )?;
        let rows = stmt.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)))?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        Ok(out)
    }

    /// Check whether `work_item_id` is still gated by unsatisfied
    /// prerequisites. If not — all prereqs are done and the block was
    /// engine-owned — flip the item to `todo` and return `true`.
    /// Returns `false` without modifying the DB when the item is not
    /// blocked, is human-blocked, or still has gating prereqs.
    ///
    /// Used by the periodic dependency-unblock sweeper as a per-item
    /// fallback for the case where the event-driven cascade
    /// ([`cascade_dependents_after_prereq_status_change`]) silently
    /// skipped this row (e.g. `last_status_actor` mismatch from a
    /// concurrent update, or engine was offline when the prereq landed).
    pub fn try_unblock_dependency_if_resolved(&self, work_item_id: &str) -> Result<bool> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let now = now_string();
        let unblocked = maybe_engine_unblock_dependent(&tx, work_item_id, &now)?;
        tx.commit()?;
        Ok(unblocked)
    }

    /// Recovery sweep: find `todo, autostart=true` tasks whose latest execution
    /// is `waiting_dependency` (or absent) and whose gating prereqs are all
    /// satisfied, then promote those executions to `ready`. Returns the ids of
    /// tasks that were recovered.
    ///
    /// This handles tasks that got stuck after an auto-unblock (Part B
    /// recovery): the auto-unblock transitions `blocked` → `todo` and creates a
    /// `ready` execution atomically, but tasks unblocked before that fix landed
    /// may still have a stale `waiting_dependency` execution with no one to
    /// promote it.
    pub fn promote_todo_autostart_stuck_executions(&self) -> Result<Vec<String>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let candidates: Vec<String> = {
            let mut stmt = tx.prepare(
                "SELECT id FROM tasks
                 WHERE status = 'todo' AND autostart = 1 AND deleted_at IS NULL
                 ORDER BY updated_at ASC, id ASC",
            )?;
            stmt.query_map([], |row| row.get::<_, String>(0))?
                .collect::<rusqlite::Result<Vec<_>>>()?
        };
        let mut promoted = Vec::new();
        for work_item_id in candidates {
            if !deps::gating_prereqs_for(&tx, &work_item_id)?.is_empty() {
                continue;
            }
            let needs_promotion = match query_latest_execution_for_work_item(&tx, &work_item_id)? {
                Some(exec) => exec.status == ExecutionStatus::WaitingDependency,
                None => true,
            };
            if !needs_promotion {
                continue;
            }
            let kind = execution_kind_for_work_item(&tx, &work_item_id)?;
            let mut result = ExecutionReconcileResult::default();
            reconcile_work_item_execution(&tx, &mut result, &work_item_id, kind, ExecutionStatus::Ready)?;
            if !result.created.is_empty() || !result.updated.is_empty() {
                tracing::info!(
                    work_item_id = %work_item_id,
                    "dep-unblock sweep: promoted stuck todo execution to ready",
                );
                promoted.push(work_item_id);
            }
        }
        tx.commit()?;
        Ok(promoted)
    }

    /// Return the prerequisites and/or dependents of a single work
    /// item. Empty lists when nothing matches; errors only when the
    /// work item id itself is unknown.
    pub fn list_dependencies(&self, input: ListDependenciesInput) -> Result<WorkItemDependencyView> {
        let work_item_id = input.work_item.trim();
        if work_item_id.is_empty() {
            bail!("work_item id is required");
        }
        let conn = self.connect()?;
        // Validate the work item exists by classifying its id and
        // looking it up. Surfaces a clear error rather than returning
        // an empty list for typos.
        let _ = product_id_for_work_item(&conn, work_item_id)?;

        let direction = input.direction.unwrap_or_default();
        let prerequisites = match direction {
            DependencyDirection::Dependents => Vec::new(),
            DependencyDirection::Prereqs | DependencyDirection::Both => {
                deps::prerequisites_of(&conn, work_item_id, None)?
            }
        };
        let dependents = match direction {
            DependencyDirection::Prereqs => Vec::new(),
            DependencyDirection::Dependents | DependencyDirection::Both => {
                deps::dependents_of(&conn, work_item_id, None)?
            }
        };
        Ok(WorkItemDependencyView {
            work_item_id: work_item_id.to_owned(),
            prerequisites,
            dependents,
        })
    }

    /// Resolved counterpart of [`Self::list_dependencies`]: each edge
    /// is collapsed into the peer's id + status + name + kind so the
    /// CLI / app shows the gate context without a second lookup.
    /// Drives the `boss <kind> show` Dependencies section (Q6).
    pub fn list_dependencies_detailed(&self, input: ListDependenciesInput) -> Result<WorkItemDependencyDetail> {
        let work_item_id = input.work_item.trim();
        if work_item_id.is_empty() {
            bail!("work_item id is required");
        }
        let conn = self.connect()?;
        let _ = product_id_for_work_item(&conn, work_item_id)?;

        let direction = input.direction.unwrap_or_default();
        let prerequisites = match direction {
            DependencyDirection::Dependents => Vec::new(),
            DependencyDirection::Prereqs | DependencyDirection::Both => {
                let edges = deps::prerequisites_of(&conn, work_item_id, None)?;
                edges
                    .into_iter()
                    .map(|edge| {
                        let peer_id = edge.prerequisite_id.clone();
                        resolve_dependency_edge(&conn, &peer_id, &edge.relation)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        let dependents = match direction {
            DependencyDirection::Prereqs => Vec::new(),
            DependencyDirection::Dependents | DependencyDirection::Both => {
                let edges = deps::dependents_of(&conn, work_item_id, None)?;
                edges
                    .into_iter()
                    .map(|edge| {
                        let peer_id = edge.dependent_id.clone();
                        resolve_dependency_edge(&conn, &peer_id, &edge.relation)
                    })
                    .collect::<Result<Vec<_>>>()?
            }
        };
        Ok(WorkItemDependencyDetail {
            work_item_id: work_item_id.to_owned(),
            prerequisites,
            dependents,
        })
    }
}
