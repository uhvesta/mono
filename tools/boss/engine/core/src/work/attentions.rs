//! Attention store — the engine core for the Attentions feature
//! (design: `tools/boss/docs/designs/attentions.md`).
//!
//! An *attention* is an agent-authored, human-actionable notification
//! (a `question` or a `followup`). Attentions never stand alone: each is a
//! member of an [`AttentionGroup`], the unit the human reads and acts on.
//! This module owns creation + reconciliation + the answer/dismiss state
//! transitions; producing the downstream artifact (`ActionAttentionGroup`)
//! is task 3 and lives elsewhere.
//!
//! Reconciliation is an upsert on the `(grouping_key, generation)` unique
//! index: re-running a source that emits the same questions/followups joins
//! the open group of the current generation rather than spawning a second
//! one. Once a group is `actioned`/`dismissed` (terminal), a re-run bumps
//! `generation` and starts a fresh group — this is what keeps "one group ⇒
//! one revision" true across iteration.

use super::*;

/// Canonical column order for `attention_groups` SELECTs. Must stay in
/// lockstep with [`map_attention_group`].
const GROUP_COLS: &str = "id, product_id, short_id, kind, \
     association_project_id, association_task_id, source_kind, source_task_id, \
     source_run_id, source_doc_path, source_doc_repo_remote_url, source_doc_branch, \
     grouping_key, generation, state, produced_artifact_kind, produced_artifact_ref, \
     created_at, actioned_at, dismissed_at";

/// Canonical column order for `attentions` SELECTs. Must stay in lockstep
/// with [`map_attention`].
const ATTN_COLS: &str = "id, group_id, ordinal, source_anchor, answer_state, \
     created_at, answered_at, question_type, prompt_text, choice_options, answer, \
     proposed_name, proposed_description, proposed_effort, proposed_work_kind, \
     rationale, confidence_source";

/// A group in `actioned`/`dismissed` is terminal: members can no longer be
/// changed and new attentions for the same key form a fresh generation.
fn group_is_terminal(state: &str) -> bool {
    matches!(state, "actioned" | "dismissed")
}

fn query_attention_group(conn: &Connection, id: &str) -> Result<Option<AttentionGroup>> {
    conn.query_row(
        &format!("SELECT {GROUP_COLS} FROM attention_groups WHERE id = ?1"),
        [id],
        map_attention_group,
    )
    .optional()
    .map_err(Into::into)
}

fn query_attention(conn: &Connection, id: &str) -> Result<Option<Attention>> {
    conn.query_row(
        &format!("SELECT {ATTN_COLS} FROM attentions WHERE id = ?1"),
        [id],
        map_attention,
    )
    .optional()
    .map_err(Into::into)
}

/// Resolve a group reference to its row. Accepts the canonical `atg_…` id
/// or an `A<n>` per-product short id. Because the lookup wire request
/// carries no product, an `A<n>` is resolved across all products and is an
/// error when it is ambiguous (the caller should use the `atg_…` id).
fn resolve_group(conn: &Connection, id: &str) -> Result<Option<AttentionGroup>> {
    if let Some(rest) = id.strip_prefix('A') {
        if let Ok(short_id) = rest.parse::<i64>() {
            let mut stmt = conn.prepare(&format!(
                "SELECT {GROUP_COLS} FROM attention_groups WHERE short_id = ?1"
            ))?;
            let mut groups = collect_rows(stmt.query_map([short_id], map_attention_group)?)?;
            return match groups.len() {
                0 => Ok(None),
                1 => Ok(Some(groups.remove(0))),
                _ => bail!(
                    "attention short id A{short_id} is ambiguous across products; \
                     use the atg_… id"
                ),
            };
        }
    }
    query_attention_group(conn, id)
}

/// Derive the stable grouping key per the design's two concrete shapes:
/// `question|{project_id}|doc:{path}` and `followup|{task_id}`. Used only
/// when the caller passes neither an explicit `group_id` nor a `group_key`.
fn derive_grouping_key(input: &CreateAttentionInput) -> Result<String> {
    match input.kind.as_str() {
        "question" => {
            let project_id = input
                .association_project_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .context(
                    "question attention needs association_project_id to derive a grouping key \
                     (or pass group_id / group_key)",
                )?;
            let doc_path = input
                .source_doc_path
                .as_deref()
                .filter(|s| !s.is_empty())
                .context(
                    "question attention needs source_doc_path to derive a grouping key \
                     (or pass group_id / group_key)",
                )?;
            Ok(format!("question|{project_id}|doc:{doc_path}"))
        }
        "followup" => {
            let task_id = input
                .source_task_id
                .as_deref()
                .filter(|s| !s.is_empty())
                .or_else(|| input.association_task_id.as_deref().filter(|s| !s.is_empty()))
                .context(
                    "followup attention needs source_task_id or association_task_id to derive \
                     a grouping key (or pass group_id / group_key)",
                )?;
            Ok(format!("followup|{task_id}"))
        }
        other => bail!("unknown attention kind {other:?}; expected \"question\" or \"followup\""),
    }
}

/// Per-kind sanity checks on the member content before it is inserted.
fn validate_member_input(input: &CreateAttentionInput) -> Result<()> {
    match input.kind.as_str() {
        "question" => {
            let question_type = input
                .question_type
                .as_deref()
                .filter(|s| !s.is_empty())
                .context("question attention needs question_type (yes_no|multiple_choice|prompt)")?;
            if !matches!(question_type, "yes_no" | "multiple_choice" | "prompt") {
                bail!(
                    "invalid question_type {question_type:?}; \
                     expected yes_no|multiple_choice|prompt"
                );
            }
            if input.prompt_text.as_deref().filter(|s| !s.is_empty()).is_none() {
                bail!("question attention needs a non-empty prompt_text");
            }
            if question_type == "multiple_choice"
                && input
                    .choice_options
                    .as_deref()
                    .filter(|s| !s.is_empty())
                    .is_none()
            {
                bail!("multiple_choice question needs choice_options (a JSON array of strings)");
            }
        }
        "followup" => {
            if input
                .proposed_name
                .as_deref()
                .filter(|s| !s.is_empty())
                .is_none()
            {
                bail!("followup attention needs a non-empty proposed_name");
            }
            if let Some(work_kind) = input
                .proposed_work_kind
                .as_deref()
                .filter(|s| !s.is_empty())
            {
                if !matches!(work_kind, "task" | "chore" | "project") {
                    bail!("invalid proposed_work_kind {work_kind:?}; expected task|chore|project");
                }
            }
        }
        other => bail!("unknown attention kind {other:?}; expected \"question\" or \"followup\""),
    }
    Ok(())
}

/// Resolve the group the new member belongs to: an explicit `group_id`
/// wins; otherwise reconcile on the grouping key, joining the latest-
/// generation open group or bumping `generation` past a terminal one.
fn resolve_or_create_group(
    conn: &Connection,
    input: &CreateAttentionInput,
) -> Result<AttentionGroup> {
    if let Some(group_id) = input.group_id.as_deref().filter(|s| !s.is_empty()) {
        return resolve_group(conn, group_id)?
            .with_context(|| format!("unknown attention group: {group_id}"));
    }

    let grouping_key = match input.group_key.as_deref().filter(|s| !s.is_empty()) {
        Some(key) => key.to_owned(),
        None => derive_grouping_key(input)?,
    };

    let latest = conn
        .query_row(
            &format!(
                "SELECT {GROUP_COLS} FROM attention_groups \
                 WHERE grouping_key = ?1 ORDER BY generation DESC LIMIT 1"
            ),
            [&grouping_key],
            map_attention_group,
        )
        .optional()?;

    match latest {
        // An open / partially-answered group of the current generation is
        // the reconciliation target.
        Some(group) if !group_is_terminal(&group.state) => Ok(group),
        // The prior group is closed — start the next generation so members
        // never reopen a closed group.
        Some(group) => create_group(conn, input, &grouping_key, group.generation + 1),
        None => create_group(conn, input, &grouping_key, 0),
    }
}

/// Insert a fresh `attention_groups` row (product + short id derived from
/// the association) at the requested generation and return it.
fn create_group(
    conn: &Connection,
    input: &CreateAttentionInput,
    grouping_key: &str,
    generation: i64,
) -> Result<AttentionGroup> {
    let assoc_project = input
        .association_project_id
        .as_deref()
        .filter(|s| !s.is_empty());
    let assoc_task = input
        .association_task_id
        .as_deref()
        .filter(|s| !s.is_empty());

    // The schema's XOR CHECK requires exactly one association; enforce it
    // here with a clear message rather than surfacing a raw SQLite error.
    let product_id = match (assoc_project, assoc_task) {
        (Some(project_id), None) => product_id_for_work_item(conn, project_id)?,
        (None, Some(task_id)) => product_id_for_work_item(conn, task_id)?,
        (Some(_), Some(_)) => bail!(
            "attention association is exclusive: set association_project_id OR \
             association_task_id, not both"
        ),
        (None, None) => bail!(
            "attention needs an association: set association_project_id or association_task_id"
        ),
    };

    let id = next_id("atg");
    let now = now_string();
    let short_id = allocate_attention_group_short_id(conn, &product_id)?;
    let source_kind = input
        .source_kind
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("manual");

    conn.execute(
        "INSERT INTO attention_groups (
             id, product_id, short_id, kind, association_project_id, association_task_id,
             source_kind, source_task_id, source_run_id, source_doc_path,
             source_doc_repo_remote_url, source_doc_branch, grouping_key, generation, state,
             produced_artifact_kind, produced_artifact_ref, created_at, actioned_at, dismissed_at
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14,
             'open', NULL, NULL, ?15, NULL, NULL
         )",
        params![
            id,
            product_id,
            short_id,
            input.kind,
            assoc_project,
            assoc_task,
            source_kind,
            input.source_task_id,
            input.source_run_id,
            input.source_doc_path,
            input.source_doc_repo_remote_url,
            input.source_doc_branch,
            grouping_key,
            generation,
            now,
        ],
    )?;

    query_attention_group(conn, &id)?
        .with_context(|| format!("missing attention group after insert: {id}"))
}

/// Recompute and persist a non-terminal group's `state` from its members:
/// `open` while every member is untouched, `partially_answered` once any
/// member has reached a terminal answer-state. Terminal groups
/// (`actioned`/`dismissed`) are left untouched — only an explicit action /
/// dismissal moves a group into or out of those.
fn recompute_group_state(conn: &Connection, group_id: &str) -> Result<()> {
    let state: String = conn.query_row(
        "SELECT state FROM attention_groups WHERE id = ?1",
        [group_id],
        |row| row.get(0),
    )?;
    if group_is_terminal(&state) {
        return Ok(());
    }
    let touched: bool = conn.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM attentions WHERE group_id = ?1 AND answer_state <> 'open'
         )",
        [group_id],
        |row| row.get(0),
    )?;
    let new_state = if touched { "partially_answered" } else { "open" };
    conn.execute(
        "UPDATE attention_groups SET state = ?2 WHERE id = ?1",
        params![group_id, new_state],
    )?;
    Ok(())
}

impl WorkDb {
    /// Create a new attention member, reconciling (or creating) its owning
    /// group. Returns the member plus its group so the caller can push an
    /// [`boss_protocol::FrontendEvent::AttentionCreated`].
    ///
    /// Each call appends exactly one member — a bare create is a one-shot,
    /// not content-idempotent. The `(grouping_key, generation)` unique index
    /// makes the *group* idempotent; the structured manifest/sentinel
    /// reconcilers (task 3) layer content-dedup on top of this.
    pub fn create_attention(
        &self,
        input: CreateAttentionInput,
    ) -> Result<(Attention, AttentionGroup)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let group = resolve_or_create_group(&tx, &input)?;
        if group.kind != input.kind {
            bail!(
                "attention kind {:?} does not match group {} kind {:?}",
                input.kind,
                group.id,
                group.kind
            );
        }
        if group_is_terminal(&group.state) {
            bail!(
                "attention group {} is {} (terminal); new attentions form a new generation, \
                 they cannot join a closed group",
                group.id,
                group.state
            );
        }
        validate_member_input(&input)?;

        let id = next_id("atn");
        let now = now_string();
        let ordinal: i64 = tx.query_row(
            "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM attentions WHERE group_id = ?1",
            [&group.id],
            |row| row.get(0),
        )?;
        let confidence_source = input
            .confidence_source
            .as_deref()
            .filter(|s| !s.is_empty())
            .unwrap_or("structured");

        tx.execute(
            "INSERT INTO attentions (
                 id, group_id, ordinal, source_anchor, answer_state, created_at, answered_at,
                 question_type, prompt_text, choice_options, answer,
                 proposed_name, proposed_description, proposed_effort, proposed_work_kind,
                 rationale, confidence_source
             ) VALUES (
                 ?1, ?2, ?3, ?4, 'open', ?5, NULL,
                 ?6, ?7, ?8, NULL,
                 ?9, ?10, ?11, ?12,
                 ?13, ?14
             )",
            params![
                id,
                group.id,
                ordinal,
                input.source_anchor,
                now,
                input.question_type,
                input.prompt_text,
                input.choice_options,
                input.proposed_name,
                input.proposed_description,
                input.proposed_effort,
                input.proposed_work_kind,
                input.rationale,
                confidence_source,
            ],
        )?;

        let attention = query_attention(&tx, &id)?
            .with_context(|| format!("missing attention after insert: {id}"))?;
        // A brand-new member is always `open`, so it cannot change the
        // group's `open`/`partially_answered` state; re-fetch only to return
        // a canonical group row.
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after insert: {}", group.id))?;
        tx.commit()?;
        Ok((attention, group))
    }

    /// List groups for `product_id`, newest first. Optional filters narrow
    /// by association (project/task), `kind`, and `state`. With no `state`
    /// filter the default is the actionable set: `open` + `partially_answered`.
    pub fn list_attention_groups(
        &self,
        product_id: &str,
        project_id: Option<&str>,
        task_id: Option<&str>,
        kind: Option<&str>,
        state: Option<&str>,
    ) -> Result<Vec<AttentionGroup>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {GROUP_COLS} FROM attention_groups
             WHERE product_id = ?1
               AND (?2 IS NULL OR association_project_id = ?2)
               AND (?3 IS NULL OR association_task_id = ?3)
               AND (?4 IS NULL OR kind = ?4)
               AND (
                    (?5 IS NULL AND state IN ('open', 'partially_answered'))
                    OR state = ?5
               )
             ORDER BY created_at DESC, id DESC"
        ))?;
        let rows = stmt.query_map(
            params![product_id, project_id, task_id, kind, state],
            map_attention_group,
        )?;
        collect_rows(rows)
    }

    /// Fetch one group by `atg_…` id or `A<n>` short id.
    pub fn get_attention_group(&self, id: &str) -> Result<AttentionGroup> {
        let conn = self.connect()?;
        resolve_group(&conn, id)?.with_context(|| format!("unknown attention group: {id}"))
    }

    /// List the members of a group in display order. Validates the group id
    /// (rejecting a typo with an error rather than an empty list).
    pub fn list_attentions_for_group(&self, group_id: &str) -> Result<Vec<Attention>> {
        let conn = self.connect()?;
        let group = resolve_group(&conn, group_id)?
            .with_context(|| format!("unknown attention group: {group_id}"))?;
        let mut stmt = conn.prepare(&format!(
            "SELECT {ATTN_COLS} FROM attentions \
             WHERE group_id = ?1 ORDER BY ordinal ASC, id ASC"
        ))?;
        let rows = stmt.query_map([group.id.as_str()], map_attention)?;
        collect_rows(rows)
    }

    /// Record the human's resolution of one member (`atn_…`) and return the
    /// owning group with its recomputed `state`.
    ///
    /// Precedence mirrors the wire: `dismiss` ⇒ `skip` ⇒ answer. A `dismiss`
    /// or `skip` clears any captured answer; answering a *question* requires
    /// a value, while answering a *followup* (an "accept") does not.
    pub fn answer_attention(
        &self,
        id: &str,
        answer: Option<String>,
        skip: bool,
        dismiss: bool,
    ) -> Result<AttentionGroup> {
        let new_state = if dismiss {
            "dismissed"
        } else if skip {
            "skipped"
        } else {
            "answered"
        };
        // Only an `answered` transition carries an answer value.
        let answer = if new_state == "answered" {
            answer.filter(|s| !s.is_empty())
        } else {
            None
        };
        self.set_member_answer_state(id, new_state, answer)
    }

    /// Dismiss without producing anything. `atg_…` / `A<n>` dismisses the
    /// whole group (terminal); `atn_…` dismisses a single member. `reason`
    /// has no column in the store and is accepted only for wire/CLI parity.
    pub fn dismiss_attention(
        &self,
        id: &str,
        _reason: Option<String>,
    ) -> Result<AttentionGroup> {
        if id.starts_with("atn_") {
            return self.set_member_answer_state(id, "dismissed", None);
        }

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let group = resolve_group(&tx, id)?
            .with_context(|| format!("unknown attention group: {id}"))?;
        match group.state.as_str() {
            // Idempotent: dismissing an already-dismissed group is a no-op.
            "dismissed" => {
                tx.commit()?;
                return Ok(group);
            }
            "actioned" => bail!(
                "attention group {} is already actioned; an actioned group cannot be dismissed",
                group.id
            ),
            _ => {}
        }
        let now = now_string();
        tx.execute(
            "UPDATE attention_groups SET state = 'dismissed', dismissed_at = ?2 WHERE id = ?1",
            params![group.id, now],
        )?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after dismiss: {}", group.id))?;
        tx.commit()?;
        Ok(group)
    }

    /// Shared member-state transition for `answer_attention` /
    /// `dismiss_attention`. Refuses to mutate a member whose group is
    /// terminal, then recomputes and returns the group.
    fn set_member_answer_state(
        &self,
        member_id: &str,
        new_state: &str,
        answer: Option<String>,
    ) -> Result<AttentionGroup> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let member = query_attention(&tx, member_id)?
            .with_context(|| format!("unknown attention: {member_id}"))?;
        let group = query_attention_group(&tx, &member.group_id)?
            .with_context(|| format!("attention {member_id} references a missing group"))?;
        if group_is_terminal(&group.state) {
            bail!(
                "attention group {} is {} (terminal); its members can no longer be changed",
                group.id,
                group.state
            );
        }
        if new_state == "answered" && group.kind == "question" && answer.is_none() {
            bail!("answering a question attention requires an answer value");
        }
        let answered_at = if new_state == "answered" {
            Some(now_string())
        } else {
            None
        };
        tx.execute(
            "UPDATE attentions
                SET answer_state = ?2, answer = ?3, answered_at = ?4
              WHERE id = ?1",
            params![member_id, new_state, answer, answered_at],
        )?;
        recompute_group_state(&tx, &group.id)?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after update: {}", group.id))?;
        tx.commit()?;
        Ok(group)
    }
}
