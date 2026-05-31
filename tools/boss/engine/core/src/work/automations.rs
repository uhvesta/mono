use super::*;

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
}
