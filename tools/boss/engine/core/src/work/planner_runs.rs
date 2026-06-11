use super::*;
use boss_protocol::{PLANNER_OUTCOME_APPLIED, PLANNER_OUTCOME_RUNNING, PLANNER_OUTCOME_STAGED, PlannerRun};

// ---- input types ----

/// Input for creating a new `planner_runs` row via
/// [`WorkDb::claim_planner_run`].
pub struct ClaimPlannerRunInput<'a> {
    pub project_id: &'a str,
    pub product_id: &'a str,
    /// Set when the trigger is a design-PR merge. `None` for
    /// operator-initiated runs.
    pub design_task_id: Option<&'a str>,
    /// `"merge_trigger"` | `"operator"` | `"replan"`.
    pub caller: &'a str,
}

/// Patch applied by [`WorkDb::update_planner_run`]. All fields are
/// optional: only `Some` values overwrite the stored column; `None`
/// leaves it unchanged. Every call also bumps `updated_at` to now.
#[derive(Debug, Default, bon::Builder)]
#[builder(on(String, into))]
pub struct PlannerRunPatch {
    pub outcome: Option<String>,
    pub doc_ref: Option<String>,
    pub model: Option<String>,
    pub input_summary: Option<String>,
    pub raw_output: Option<String>,
    pub effort_audit: Option<String>,
    pub notes: Option<String>,
    pub result_summary: Option<String>,
}

// ---- mapper ----

fn map_planner_run(row: &Row<'_>) -> rusqlite::Result<PlannerRun> {
    Ok(PlannerRun {
        id: row.get(0)?,
        project_id: row.get(1)?,
        product_id: row.get(2)?,
        design_task_id: row.get(3)?,
        caller: row.get(4)?,
        doc_ref: row.get(5)?,
        model: row.get(6)?,
        input_summary: row.get(7)?,
        raw_output: row.get(8)?,
        effort_audit: row.get(9)?,
        notes: row.get(10)?,
        outcome: row.get(11)?,
        result_summary: row.get(12)?,
        created_at: row.get(13)?,
        updated_at: row.get(14)?,
    })
}

const SELECT_PLANNER_RUN: &str = "SELECT id, project_id, product_id, design_task_id, caller,
            doc_ref, model, input_summary, raw_output, effort_audit,
            notes, outcome, result_summary, created_at, updated_at
     FROM planner_runs";

// ---- WorkDb accessors ----

impl WorkDb {
    /// Attempt to claim a planner run for `project_id` by inserting a row
    /// with `outcome = 'running'`.
    ///
    /// Returns `Ok(Some(run))` when the insert succeeds (this invocation owns
    /// the populate) or `Ok(None)` when the UNIQUE partial index fires —
    /// indicating a prior `running` / `staged` / `applied` row already exists
    /// for this project and the caller should skip.
    ///
    /// The UNIQUE index `planner_runs_one_per_project` covers only
    /// `outcome IN ('running','staged','applied')`, so terminal failure rows
    /// (`planner_failed`, `fetch_failed`, etc.) do **not** block a subsequent
    /// retry claim.
    pub fn claim_planner_run(&self, input: ClaimPlannerRunInput<'_>) -> Result<Option<PlannerRun>> {
        let conn = self.connect()?;
        let id = next_id("run");
        let now = now_string();
        let rows = conn.execute(
            "INSERT OR IGNORE INTO planner_runs
                 (id, project_id, product_id, design_task_id, caller,
                  outcome, created_at, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                id,
                input.project_id,
                input.product_id,
                input.design_task_id,
                input.caller,
                PLANNER_OUTCOME_RUNNING,
                now,
                now,
            ],
        )?;
        if rows == 0 {
            return Ok(None);
        }
        Ok(Some(PlannerRun {
            id,
            project_id: input.project_id.to_owned(),
            product_id: input.product_id.to_owned(),
            design_task_id: input.design_task_id.map(|s| s.to_owned()),
            caller: input.caller.to_owned(),
            doc_ref: None,
            model: None,
            input_summary: None,
            raw_output: None,
            effort_audit: None,
            notes: None,
            outcome: PLANNER_OUTCOME_RUNNING.to_owned(),
            result_summary: None,
            created_at: now.clone(),
            updated_at: now,
        }))
    }

    /// Apply a partial update to the `planner_runs` row identified by `id`.
    ///
    /// Only `Some` fields in `patch` are written; `None` fields are left
    /// unchanged. `updated_at` is always bumped to the current epoch second.
    ///
    /// Returns `Ok(Some(run))` with the post-update state, or `Ok(None)` if
    /// no row with `id` exists (stale call after a delete).
    pub fn update_planner_run(&self, id: &str, patch: PlannerRunPatch) -> Result<Option<PlannerRun>> {
        let conn = self.connect()?;
        let now = now_string();
        let mut sets: Vec<String> = vec!["updated_at = ?1".to_owned()];
        // We build the SET list dynamically. Param index 1 is `now`; the `id`
        // for the WHERE clause will be assigned the next index after all SET
        // params. We collect the boxed `ToSql` values separately.
        let mut extra_params: Vec<Box<dyn rusqlite::ToSql>> = vec![Box::new(now.clone())];

        macro_rules! push_field {
            ($field:expr, $col:literal) => {
                if let Some(val) = $field {
                    extra_params.push(Box::new(val));
                    sets.push(format!("{} = ?{}", $col, extra_params.len()));
                }
            };
        }

        push_field!(patch.outcome, "outcome");
        push_field!(patch.doc_ref, "doc_ref");
        push_field!(patch.model, "model");
        push_field!(patch.input_summary, "input_summary");
        push_field!(patch.raw_output, "raw_output");
        push_field!(patch.effort_audit, "effort_audit");
        push_field!(patch.notes, "notes");
        push_field!(patch.result_summary, "result_summary");

        let id_idx = extra_params.len() + 1;
        extra_params.push(Box::new(id.to_owned()));

        let sql = format!("UPDATE planner_runs SET {} WHERE id = ?{}", sets.join(", "), id_idx,);
        let params_refs: Vec<&dyn rusqlite::ToSql> = extra_params.iter().map(|p| p.as_ref()).collect();
        let changed = conn.execute(&sql, params_refs.as_slice())?;
        if changed == 0 {
            return Ok(None);
        }
        self.get_planner_run(id)
    }

    /// Fetch a single `planner_runs` row by primary key.
    ///
    /// Returns `Ok(None)` when no row with `id` exists.
    pub fn get_planner_run(&self, id: &str) -> Result<Option<PlannerRun>> {
        let conn = self.connect()?;
        let sql = format!("{SELECT_PLANNER_RUN} WHERE id = ?1");
        conn.query_row(&sql, params![id], map_planner_run)
            .optional()
            .map_err(Into::into)
    }

    /// Return all `planner_runs` rows for `project_id`, newest first.
    ///
    /// Includes all outcomes — callers that only want live rows should filter
    /// on `outcome IN ('running','staged','applied')`.
    pub fn list_planner_runs_for_project(&self, project_id: &str) -> Result<Vec<PlannerRun>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_PLANNER_RUN}
             WHERE project_id = ?1
             ORDER BY created_at DESC, id DESC"
        );
        let mut stmt = conn.prepare(&sql)?;
        let rows = stmt.query_map(params![project_id], map_planner_run)?;
        rows.map(|r| r.map_err(Into::into)).collect()
    }

    /// Return the live `planner_runs` row for `project_id` — i.e. the row
    /// whose `outcome` is one of `'running'`, `'staged'`, or `'applied'`.
    ///
    /// At most one such row can exist per the UNIQUE partial index
    /// `planner_runs_one_per_project`. Returns `Ok(None)` when the project
    /// has never been successfully populated (or the idempotency gate has
    /// been cleared for a re-plan).
    pub fn live_planner_run_for_project(&self, project_id: &str) -> Result<Option<PlannerRun>> {
        let conn = self.connect()?;
        let sql = format!(
            "{SELECT_PLANNER_RUN}
             WHERE project_id = ?1
               AND outcome IN (?2, ?3, ?4)
             LIMIT 1"
        );
        conn.query_row(
            &sql,
            params![
                project_id,
                PLANNER_OUTCOME_RUNNING,
                PLANNER_OUTCOME_STAGED,
                PLANNER_OUTCOME_APPLIED,
            ],
            map_planner_run,
        )
        .optional()
        .map_err(Into::into)
    }

    /// Delete the `planner_runs` row for `id`.
    ///
    /// Used by the undo path (`boss project unpopulate`): removing the row
    /// also releases the idempotency gate so a corrected re-plan can claim
    /// the project again. Returns `true` if a row was deleted, `false` if
    /// the id was unknown.
    pub fn delete_planner_run(&self, id: &str) -> Result<bool> {
        let conn = self.connect()?;
        let changed = conn.execute("DELETE FROM planner_runs WHERE id = ?1", params![id])?;
        Ok(changed > 0)
    }
}

// ---- tests ----

#[cfg(test)]
mod tests {
    use super::*;

    fn open() -> WorkDb {
        WorkDb::open(std::path::PathBuf::from(":memory:")).unwrap()
    }

    fn product_and_project(db: &WorkDb) -> (String, String) {
        let product = db
            .create_product(
                boss_protocol::CreateProductInput::builder()
                    .name("Test")
                    .repo_remote_url("git@github.com:test/test.git")
                    .build(),
            )
            .unwrap();
        let project = db
            .create_project(
                boss_protocol::CreateProjectInput::builder()
                    .product_id(product.id.clone())
                    .name("Alpha")
                    .goal("build it")
                    .build(),
            )
            .unwrap();
        (product.id, project.id)
    }

    #[test]
    fn claim_succeeds_for_fresh_project() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap();
        assert!(run.is_some());
        let run = run.unwrap();
        assert_eq!(run.outcome, PLANNER_OUTCOME_RUNNING);
        assert_eq!(run.project_id, project_id);
        assert_eq!(run.caller, "merge_trigger");
    }

    #[test]
    fn claim_returns_none_when_running_row_exists() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let first = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap();
        assert!(first.is_some());
        let second = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap();
        assert!(second.is_none(), "second claim must be blocked by idempotency index");
    }

    #[test]
    fn claim_returns_none_when_staged_row_exists() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        // Transition to staged.
        db.update_planner_run(
            &run.id,
            PlannerRunPatch {
                outcome: Some(PLANNER_OUTCOME_STAGED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        // A second claim must still be blocked.
        let second = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap();
        assert!(second.is_none());
    }

    #[test]
    fn claim_allowed_after_terminal_failure() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        // Transition to a terminal failure — no longer covered by the partial
        // index, so a subsequent claim must succeed.
        db.update_planner_run(
            &run.id,
            PlannerRunPatch {
                outcome: Some(boss_protocol::PLANNER_OUTCOME_PLANNER_FAILED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        let retry = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap();
        assert!(retry.is_some(), "retry claim must succeed after terminal failure");
    }

    #[test]
    fn update_planner_run_patches_fields() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: Some("task_abc"),
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        let updated = db
            .update_planner_run(
                &run.id,
                PlannerRunPatch {
                    outcome: Some(PLANNER_OUTCOME_STAGED.to_owned()),
                    model: Some("claude-opus-4-5".to_owned()),
                    result_summary: Some("created 5 tasks, 3 edges".to_owned()),
                    ..PlannerRunPatch::default()
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(updated.outcome, PLANNER_OUTCOME_STAGED);
        assert_eq!(updated.model.as_deref(), Some("claude-opus-4-5"));
        assert_eq!(updated.result_summary.as_deref(), Some("created 5 tasks, 3 edges"));
        assert_eq!(updated.design_task_id.as_deref(), Some("task_abc"));
    }

    #[test]
    fn list_planner_runs_for_project_returns_all() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        // First run: terminal failure.
        let r1 = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        db.update_planner_run(
            &r1.id,
            PlannerRunPatch {
                outcome: Some(boss_protocol::PLANNER_OUTCOME_FETCH_FAILED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        // Second run: staged.
        let r2 = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap()
            .unwrap();
        db.update_planner_run(
            &r2.id,
            PlannerRunPatch {
                outcome: Some(PLANNER_OUTCOME_STAGED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        let runs = db.list_planner_runs_for_project(&project_id).unwrap();
        assert_eq!(runs.len(), 2, "expected 2 runs for the project");
    }

    #[test]
    fn live_planner_run_for_project_returns_staged_row() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        assert!(
            db.live_planner_run_for_project(&project_id).unwrap().is_none(),
            "no live run before claim"
        );
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        db.update_planner_run(
            &run.id,
            PlannerRunPatch {
                outcome: Some(PLANNER_OUTCOME_STAGED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        let live = db.live_planner_run_for_project(&project_id).unwrap();
        assert!(live.is_some());
        assert_eq!(live.unwrap().outcome, PLANNER_OUTCOME_STAGED);
    }

    #[test]
    fn delete_planner_run_releases_idempotency_gate() {
        let db = open();
        let (product_id, project_id) = product_and_project(&db);
        let run = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "merge_trigger",
            })
            .unwrap()
            .unwrap();
        db.update_planner_run(
            &run.id,
            PlannerRunPatch {
                outcome: Some(PLANNER_OUTCOME_STAGED.to_owned()),
                ..PlannerRunPatch::default()
            },
        )
        .unwrap();
        // Before delete: a new claim is blocked.
        let blocked = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap();
        assert!(blocked.is_none());
        // Delete the row (undo path).
        assert!(db.delete_planner_run(&run.id).unwrap());
        // After delete: a new claim succeeds.
        let reclaimed = db
            .claim_planner_run(ClaimPlannerRunInput {
                project_id: &project_id,
                product_id: &product_id,
                design_task_id: None,
                caller: "operator",
            })
            .unwrap();
        assert!(reclaimed.is_some(), "claim must succeed after undo delete");
    }
}
