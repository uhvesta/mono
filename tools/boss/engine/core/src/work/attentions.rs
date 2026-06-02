//! Attention store — the engine core for the Attentions feature
//! (design: `tools/boss/docs/designs/attentions.md`).
//!
//! An *attention* is an agent-authored, human-actionable notification
//! (a `question` or a `followup`). Attentions never stand alone: each is a
//! member of an [`AttentionGroup`], the unit the human reads and acts on.
//! This module owns creation + reconciliation, the answer/dismiss state
//! transitions, and — via [`WorkDb::action_attention_group`] — producing the
//! single downstream artifact when the human actions a group: a revision (or
//! a fresh design task) for a question group, or a batch task-create for a
//! followup group.
//!
//! Reconciliation is an upsert on the `(grouping_key, generation)` unique
//! index: re-running a source that emits the same questions/followups joins
//! the open group of the current generation rather than spawning a second
//! one. Once a group is `actioned`/`dismissed` (terminal), a re-run bumps
//! `generation` and starts a fresh group — this is what keeps "one group ⇒
//! one revision" true across iteration.

use super::*;
use std::collections::HashSet;

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

/// Stable content key used by [`WorkDb::reconcile_attentions`] for
/// member-level dedup. Two members with the same key are "the same
/// question / followup re-emitted" and the second is skipped, so a
/// re-detected PR or a re-emitted `FOLLOWUPS:` block never appends
/// duplicate members within a generation. The unit separator (`\u{1f}`)
/// keeps the joined fields unambiguous.
///
/// - **question** → `question_type` + `prompt_text` + `source_anchor`
///   (a worker may legitimately ask the same prompt about two different
///   doc sections, so the anchor is part of the identity).
/// - **followup** → `proposed_name` (the title is the human-meaningful
///   identity; re-phrased descriptions of the same proposal collapse).
fn content_key(
    kind: &str,
    question_type: Option<&str>,
    prompt_text: Option<&str>,
    source_anchor: Option<&str>,
    proposed_name: Option<&str>,
) -> String {
    match kind {
        "question" => format!(
            "q\u{1f}{}\u{1f}{}\u{1f}{}",
            question_type.unwrap_or_default(),
            prompt_text.unwrap_or_default(),
            source_anchor.unwrap_or_default(),
        ),
        "followup" => format!("f\u{1f}{}", proposed_name.unwrap_or_default()),
        other => format!("{other}\u{1f}{}", prompt_text.unwrap_or_default()),
    }
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

/// Next ordinal for a group: one past the current maximum (1-based).
fn next_member_ordinal(conn: &Connection, group_id: &str) -> Result<i64> {
    conn.query_row(
        "SELECT COALESCE(MAX(ordinal), 0) + 1 FROM attentions WHERE group_id = ?1",
        [group_id],
        |row| row.get(0),
    )
    .map_err(Into::into)
}

/// Insert one member row into `group_id` at `ordinal` and return it.
/// Shared by [`WorkDb::create_attention`] (single append) and
/// [`WorkDb::reconcile_attentions`] (idempotent batch upsert). Callers are
/// responsible for validating the member and for confirming the group is
/// non-terminal before calling.
fn insert_member(
    conn: &Connection,
    group_id: &str,
    ordinal: i64,
    input: &CreateAttentionInput,
) -> Result<Attention> {
    let id = next_id("atn");
    let now = now_string();
    let confidence_source = input
        .confidence_source
        .as_deref()
        .filter(|s| !s.is_empty())
        .unwrap_or("structured");

    conn.execute(
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
            group_id,
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

    query_attention(conn, &id)?.with_context(|| format!("missing attention after insert: {id}"))
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

        let ordinal = next_member_ordinal(&tx, &group.id)?;
        let attention = insert_member(&tx, &group.id, ordinal, &input)?;
        // A brand-new member is always `open`, so it cannot change the
        // group's `open`/`partially_answered` state; re-fetch only to return
        // a canonical group row.
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after insert: {}", group.id))?;
        tx.commit()?;
        Ok((attention, group))
    }

    /// Reconcile a batch of structured attentions — a design-doc question
    /// manifest or a transcript `FOLLOWUPS:` block — into a single group.
    ///
    /// This is the content-idempotent counterpart to [`Self::create_attention`]
    /// that the creation-pipeline detectors (design `<slug>.attentions.json`,
    /// the followups sentinel) call. Group reconciliation is identical: the
    /// batch joins the latest open / partially-answered group for its grouping
    /// key, or — if that group is already `actioned`/`dismissed` (terminal) —
    /// starts a fresh generation. On top of the group's
    /// `(grouping_key, generation)` idempotency, member-level dedup keys on
    /// [`content_key`] so re-running the same source (a re-detected PR, a
    /// re-emitted block) does **not** append duplicate members.
    ///
    /// All `inputs` must share the same grouping identity (kind + association
    /// + source); the group is resolved from the first input. Returns the
    /// group plus the members **newly inserted on this call** (an empty `Vec`
    /// when every member already existed), or `Ok(None)` for an empty batch so
    /// callers can skip event publishing without a special case.
    pub fn reconcile_attentions(
        &self,
        inputs: Vec<CreateAttentionInput>,
    ) -> Result<Option<(AttentionGroup, Vec<Attention>)>> {
        let Some(first) = inputs.first() else {
            return Ok(None);
        };

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;

        let group = resolve_or_create_group(&tx, first)?;
        // `resolve_or_create_group` always returns a non-terminal group (it
        // bumps past a closed one), so members can be appended safely.
        debug_assert!(!group_is_terminal(&group.state));

        // Seed the dedup set + ordinal counter from the group's existing
        // members so re-runs are no-ops and ordinals stay monotonic.
        let existing = {
            let mut stmt = tx.prepare(&format!(
                "SELECT {ATTN_COLS} FROM attentions WHERE group_id = ?1"
            ))?;
            collect_rows(stmt.query_map([group.id.as_str()], map_attention)?)?
        };
        let mut seen: HashSet<String> = existing
            .iter()
            .map(|a| {
                content_key(
                    &group.kind,
                    a.question_type.as_deref(),
                    a.prompt_text.as_deref(),
                    a.source_anchor.as_deref(),
                    a.proposed_name.as_deref(),
                )
            })
            .collect();
        let mut ordinal = existing.iter().map(|a| a.ordinal).max().unwrap_or(0);

        let mut created = Vec::new();
        for input in &inputs {
            if input.kind != group.kind {
                bail!(
                    "attention kind {:?} does not match group {} kind {:?}",
                    input.kind,
                    group.id,
                    group.kind
                );
            }
            validate_member_input(input)?;
            let key = content_key(
                &group.kind,
                input.question_type.as_deref(),
                input.prompt_text.as_deref(),
                input.source_anchor.as_deref(),
                input.proposed_name.as_deref(),
            );
            // Skips both members already in the group and intra-batch dupes.
            if !seen.insert(key) {
                continue;
            }
            ordinal += 1;
            created.push(insert_member(&tx, &group.id, ordinal, input)?);
        }

        recompute_group_state(&tx, &group.id)?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after reconcile: {}", group.id))?;
        tx.commit()?;
        Ok(Some((group, created)))
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

// ===========================================================================
// ActionAttentionGroup — the single terminal producer (design §"Engine
// behaviour and take action per kind"). One entry point so the Notifications
// window and the inline doc surface produce identical effects.
// ===========================================================================

/// Outcome of [`WorkDb::action_attention_group`]: the now-`actioned` group
/// plus the ids of the work items the action produced. The RPC handler emits
/// [`boss_protocol::FrontendEvent::AttentionGroupActioned`] with the group and
/// publishes a work-tree invalidation for the produced ids so the kanban
/// reflects the new revision / tasks without a manual reload.
#[derive(Debug, Clone)]
pub struct ActionedAttentionGroup {
    pub group: AttentionGroup,
    pub produced_work_item_ids: Vec<String>,
}

/// Load a group's members in display order within an open transaction.
fn members_in_tx(conn: &Connection, group_id: &str) -> Result<Vec<Attention>> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {ATTN_COLS} FROM attentions WHERE group_id = ?1 ORDER BY ordinal ASC, id ASC"
    ))?;
    collect_rows(stmt.query_map([group_id], map_attention)?)
}

/// Map a `proposed_effort` hint (`"trivial"`…`"max"`) to an [`EffortLevel`].
/// Unrecognised / empty values yield `None`, letting the dispatcher fall
/// through to the product / engine default.
fn parse_effort(raw: Option<&str>) -> Option<EffortLevel> {
    match raw.map(str::trim).filter(|s| !s.is_empty()) {
        Some("trivial") => Some(EffortLevel::Trivial),
        Some("small") => Some(EffortLevel::Small),
        Some("medium") => Some(EffortLevel::Medium),
        Some("large") => Some(EffortLevel::Large),
        Some("max") => Some(EffortLevel::Max),
        _ => None,
    }
}

/// A concise card title for the revision / design task produced from a
/// question group — derived from the source doc's basename when known.
fn question_artifact_name(group: &AttentionGroup) -> String {
    match group
        .source_doc_path
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        Some(path) => {
            let base = path.rsplit('/').next().unwrap_or(path);
            format!("Apply answered questions to {base}")
        }
        None => "Apply answered design questions".to_owned(),
    }
}

/// Render the `answered` question/answer pairs into a markdown brief handed
/// to the revision / design worker. Skipped and dismissed members contribute
/// nothing (the design: "produces one downstream artifact from the answered
/// set").
fn build_qa_brief(group: &AttentionGroup, answered: &[&Attention]) -> String {
    use std::fmt::Write as _;
    let mut out = String::new();
    match group.source_doc_path.as_deref().filter(|s| !s.is_empty()) {
        Some(path) => {
            let _ = writeln!(
                out,
                "The operator answered open questions about the design doc `{path}`. \
                 Incorporate every answer below into the doc."
            );
        }
        None => {
            let _ = writeln!(
                out,
                "The operator answered open questions about this design. \
                 Incorporate every answer below into the doc."
            );
        }
    }
    out.push_str("\n## Answered questions\n");
    for m in answered {
        let prompt = m.prompt_text.as_deref().unwrap_or("(question)");
        let _ = write!(out, "\n### {prompt}\n");
        if let Some(anchor) = m.source_anchor.as_deref().filter(|s| !s.is_empty()) {
            let _ = writeln!(out, "_Section: {anchor}_");
        }
        let _ = writeln!(out, "\n**Answer:** {}", m.answer.as_deref().unwrap_or(""));
    }
    out
}

/// Insert a fresh `kind = 'design'` task seeded with the answered-questions
/// brief. Used when a question group's source doc has already merged, so a
/// revision (which needs an open PR) is impossible: a new design task opens a
/// new PR instead. Mirrors [`insert_design_task_for_project_in_tx`] but
/// carries a real description, a normal ordinal (the project's original
/// design task occupies ordinal 0), and `created_via = attention`.
fn insert_seeded_design_task_in_tx(
    conn: &Connection,
    product_id: &str,
    project_id: &str,
    name: &str,
    description: &str,
) -> Result<Task> {
    let id = next_id("task");
    let now = now_string();
    let ordinal = next_task_ordinal(conn, project_id)?;
    let short_id = allocate_short_id(conn, product_id)?;
    conn.execute(
        "INSERT INTO tasks (id, product_id, project_id, kind, name, description, status, ordinal, \
         pr_url, deleted_at, created_at, updated_at, autostart, priority, created_via, short_id) \
         VALUES (?1, ?2, ?3, 'design', ?4, ?5, 'todo', ?6, NULL, NULL, ?7, ?7, 1, 'medium', ?8, ?9)",
        params![
            id,
            product_id,
            project_id,
            name,
            description,
            ordinal,
            now,
            CREATED_VIA_ATTENTION,
            short_id,
        ],
    )?;
    query_task(conn, &id)?.with_context(|| format!("missing seeded design task after insert: {id}"))
}

/// Produce the downstream artifact for a **question** group. Returns
/// `(produced_artifact_kind, produced_artifact_ref_json, produced_ids)`.
///
/// Prefer a revision against the source doc's still-open PR. The revision
/// gate (parent PR open and unmerged) is the exact condition the design forks
/// on, so we attempt the revision and fall back to a fresh design task
/// *precisely* when the gate refuses (no PR / merged / closed). Any other
/// failure (e.g. a `gh` probe error) is a real error and propagates.
fn action_question_group(
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
    pr_checker: &dyn PrStateChecker,
) -> Result<(String, String, Vec<String>)> {
    let answered: Vec<&Attention> = members
        .iter()
        .filter(|m| m.answer_state == "answered")
        .collect();
    if answered.is_empty() {
        bail!(
            "attention group {} has no answered questions to act on; \
             dismiss it instead of actioning",
            group.id
        );
    }
    let brief = build_qa_brief(group, &answered);
    let name = question_artifact_name(group);

    if let Some(parent_task_id) = group
        .source_task_id
        .as_deref()
        .filter(|s| !s.is_empty())
    {
        let input = CreateRevisionInput::builder()
            .parent_task_id(parent_task_id)
            .description(brief.clone())
            .name(name.clone())
            .created_via(CREATED_VIA_ATTENTION)
            .build();
        match assert_parent_revisable_and_insert(conn, input, pr_checker) {
            Ok(revision) => {
                let reference = serde_json::json!({
                    "task_id": revision.id,
                    "short_id": revision.short_id,
                })
                .to_string();
                return Ok(("revision".to_owned(), reference, vec![revision.id]));
            }
            Err(err) => {
                // Fall back to a fresh design task only when the gate refused
                // (the source doc has no open PR to revise). The gate's checks
                // run before it inserts anything, so the transaction is still
                // clean here.
                if err.downcast_ref::<RevisionGateError>().is_none() {
                    return Err(err);
                }
            }
        }
    }

    // Merged doc (or no source task / open PR): a fresh design task opens a
    // new PR seeded with the Q&A.
    let project_id = group
        .association_project_id
        .as_deref()
        .filter(|s| !s.is_empty())
        .with_context(|| {
            format!(
                "question group {} has no associated project; cannot create a design task",
                group.id
            )
        })?;
    let task = insert_seeded_design_task_in_tx(conn, &group.product_id, project_id, &name, &brief)?;
    let reference = serde_json::json!({
        "task_id": task.id,
        "short_id": task.short_id,
    })
    .to_string();
    Ok(("design_task".to_owned(), reference, vec![task.id]))
}

/// Produce the downstream artifact for a **followup** group: one task/chore
/// per accepted (answered) member, created in the originating task's
/// product/project. Skipped/dismissed members contribute nothing. Returns
/// `(produced_artifact_kind, produced_artifact_ref_json, produced_ids)`.
///
/// `proposed_work_kind` is honoured as `chore` vs (project-)`task`; a
/// `project` hint is materialised as a task in the originating project (v1
/// produces tasks/chores, per the Attn-3 scope). When the originating work
/// item has no project (it is itself a chore), the followup is created as a
/// product-level chore.
fn action_followup_group(
    conn: &Connection,
    group: &AttentionGroup,
    members: &[Attention],
) -> Result<(String, String, Vec<String>)> {
    let accepted: Vec<&Attention> = members
        .iter()
        .filter(|m| m.answer_state == "answered")
        .collect();
    if accepted.is_empty() {
        bail!(
            "attention group {} has no accepted followups to create; \
             dismiss it instead of actioning",
            group.id
        );
    }

    // New work items inherit the originating task's product + project.
    let origin_id = group
        .association_task_id
        .as_deref()
        .or(group.source_task_id.as_deref())
        .filter(|s| !s.is_empty())
        .with_context(|| format!("followup group {} has no originating task", group.id))?;
    let origin = query_task(conn, origin_id)?.with_context(|| {
        format!("followup group {} references a missing task {origin_id}", group.id)
    })?;
    let product_id = origin.product_id.clone();
    let project_id = origin.project_id.clone();

    let mut created_ids = Vec::with_capacity(accepted.len());
    let mut created_refs = Vec::with_capacity(accepted.len());
    for m in accepted {
        let name = m
            .proposed_name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .with_context(|| format!("accepted followup {} has no proposed_name", m.id))?;
        let effort = parse_effort(m.proposed_effort.as_deref());
        // A `chore` hint, or the absence of a project to file into, lands a
        // product-level chore; everything else becomes a project task. The
        // duplicate guard is bypassed: actioning is an explicit human gesture.
        let as_chore = m.proposed_work_kind.as_deref() == Some("chore") || project_id.is_none();

        let created = if as_chore {
            insert_chore_in_tx(
                conn,
                CreateChoreInput::builder()
                    .product_id(product_id.clone())
                    .name(name)
                    .maybe_description(m.proposed_description.clone())
                    .maybe_effort_level(effort)
                    .created_via(CREATED_VIA_ATTENTION)
                    .force_duplicate(true)
                    .build(),
            )?
        } else {
            let project_id = project_id
                .clone()
                .expect("project_id is present when as_chore is false");
            insert_task_in_tx(
                conn,
                CreateTaskInput::builder()
                    .product_id(product_id.clone())
                    .project_id(project_id)
                    .name(name)
                    .maybe_description(m.proposed_description.clone())
                    .maybe_effort_level(effort)
                    .created_via(CREATED_VIA_ATTENTION)
                    .force_duplicate(true)
                    .build(),
            )?
        };
        created_refs.push(serde_json::json!({
            "task_id": created.id,
            "short_id": created.short_id,
            "kind": created.kind,
        }));
        created_ids.push(created.id);
    }

    let reference = serde_json::json!({ "tasks": created_refs }).to_string();
    Ok(("tasks".to_owned(), reference, created_ids))
}

impl WorkDb {
    /// Action an open / partially-answered attention group: produce the single
    /// downstream artifact and transition the group to `actioned` (terminal),
    /// recording `produced_artifact_kind` + `produced_artifact_ref`. All of it
    /// — the artifact insert and the group flip — happens in one transaction
    /// so a re-action can never spawn a second artifact.
    ///
    /// `skip_unanswered` marks every still-`open` member `skipped` first, so
    /// the caller does not have to touch every row. After that, *every* member
    /// must be in a terminal answer-state (`answered` / `skipped` / `dismissed`)
    /// — otherwise the action is refused.
    ///
    /// - **question** group → a revision on the source doc's open PR, or a
    ///   fresh `design` task when the doc has already merged.
    /// - **followup** group → a batch of tasks/chores from the accepted members.
    ///
    /// `pr_checker` supplies the live PR state for the question→revision gate;
    /// pass `&GhPrStateChecker` in production, `&FakePrStateChecker` in tests.
    pub fn action_attention_group(
        &self,
        id: &str,
        skip_unanswered: bool,
        pr_checker: &dyn PrStateChecker,
    ) -> Result<ActionedAttentionGroup> {
        let mut conn = self.connect()?;
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;

        let group =
            resolve_group(&tx, id)?.with_context(|| format!("unknown attention group: {id}"))?;
        match group.state.as_str() {
            "actioned" => bail!(
                "attention group {} is already actioned; an actioned group is terminal",
                group.id
            ),
            "dismissed" => bail!(
                "attention group {} is dismissed; a dismissed group cannot be actioned",
                group.id
            ),
            _ => {}
        }

        if skip_unanswered {
            tx.execute(
                "UPDATE attentions SET answer_state = 'skipped' \
                 WHERE group_id = ?1 AND answer_state = 'open'",
                params![group.id],
            )?;
        }

        let members = members_in_tx(&tx, &group.id)?;
        if members.is_empty() {
            bail!("attention group {} has no members to action", group.id);
        }
        let unanswered = members.iter().filter(|m| m.answer_state == "open").count();
        if unanswered > 0 {
            bail!(
                "attention group {} has {unanswered} unanswered member(s); answer or skip them \
                 (or pass skip_unanswered) before actioning",
                group.id
            );
        }

        let (produced_kind, produced_ref, produced_work_item_ids) = match group.kind.as_str() {
            "question" => action_question_group(&tx, &group, &members, pr_checker)?,
            "followup" => action_followup_group(&tx, &group, &members)?,
            other => bail!("cannot action attention group {} of kind {other:?}", group.id),
        };

        let now = now_string();
        tx.execute(
            "UPDATE attention_groups \
                SET state = 'actioned', produced_artifact_kind = ?2, \
                    produced_artifact_ref = ?3, actioned_at = ?4 \
              WHERE id = ?1",
            params![group.id, produced_kind, produced_ref, now],
        )?;
        let group = query_attention_group(&tx, &group.id)?
            .with_context(|| format!("missing attention group after action: {}", group.id))?;
        tx.commit()?;
        Ok(ActionedAttentionGroup {
            group,
            produced_work_item_ids,
        })
    }
}
