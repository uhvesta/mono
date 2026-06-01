//! `work_comments` persistence + anchor-resolution side-effects for the
//! comments-in-markdown-viewer feature (Phase 2). Design:
//! `tools/boss/docs/designs/comments-in-markdown-viewer.md`.
//!
//! The pure `TextQuoteSelector` resolver lives in [`crate::comments_anchor`];
//! this module wires it to the database — persisting fuzzy re-anchors and
//! orphan flips on resolve, and re-keying work-item comments to a `pr_doc:*`
//! artifact when a design doc graduates to a PR.

use super::*;
use crate::comments_anchor::{AnchorResolution, CommentFuzzyConfig, resolve_anchor};

/// Default prefix/suffix length (chars) when (re-)extracting an anchor from
/// plain text. 64 each per design § "Anchoring model" (prefix/suffix length).
const ANCHOR_CONTEXT_CHARS: usize = 64;

/// Column list shared by every `work_comments` SELECT. Order must match
/// [`map_comment`].
const COMMENT_COLUMNS: &str = "id, artifact_kind, artifact_id, doc_version, anchor_json, body, \
     author, status, status_actor, last_resolved_with, plain_text_projection_version, \
     created_at, updated_at, dismissed_at";

const COMMENT_INSERT_SQL: &str = "INSERT INTO work_comments \
     (id, artifact_kind, artifact_id, doc_version, anchor_json, body, author, status, \
      status_actor, last_resolved_with, plain_text_projection_version, created_at, updated_at, \
      dismissed_at) \
     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)";

impl WorkDb {
    /// Create an `active` comment. Returns the inserted row.
    pub fn create_comment(&self, input: CreateCommentInput) -> Result<WorkComment> {
        if input.body.trim().is_empty() {
            bail!("comment body may not be empty");
        }
        if input.anchor.exact.is_empty() {
            bail!("comment anchor.exact may not be empty");
        }
        if input.artifact_id.trim().is_empty() {
            bail!("comment artifact_id may not be empty");
        }
        let conn = self.connect()?;
        let id = next_id("cmt");
        let now = now_string();
        let anchor_json = serde_json::to_string(&input.anchor)?;
        conn.execute(
            COMMENT_INSERT_SQL,
            params![
                id,
                input.artifact_kind,
                input.artifact_id,
                input.doc_version,
                anchor_json,
                input.body,
                input.author,
                COMMENT_STATUS_ACTIVE,
                Option::<String>::None,
                Option::<String>::None,
                input.plain_text_projection_version,
                now,
                now,
                Option::<String>::None,
            ],
        )?;
        query_comment(&conn, &id)?.with_context(|| format!("missing comment after insert: {id}"))
    }

    /// List comments for an artifact in document-creation order. Excludes
    /// `resolved` / `dismissed` unless `include_resolved`. `orphaned`
    /// comments are always included (the sidebar surfaces them).
    pub fn list_comments(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
        include_resolved: bool,
    ) -> Result<Vec<WorkComment>> {
        let conn = self.connect()?;
        query_comments(&conn, artifact_kind, artifact_id, include_resolved)
    }

    /// Fetch a single comment by id.
    pub fn get_comment(&self, comment_id: &str) -> Result<Option<WorkComment>> {
        let conn = self.connect()?;
        query_comment(&conn, comment_id)
    }

    /// Transition a comment's status. Accepts `active` / `resolved` /
    /// `orphaned` / `dismissed`; stamps `dismissed_at` when entering
    /// `resolved` / `dismissed` and clears it otherwise (re-activation).
    pub fn set_comment_status(
        &self,
        comment_id: &str,
        status: &str,
        actor: Option<&str>,
    ) -> Result<WorkComment> {
        match status {
            COMMENT_STATUS_ACTIVE
            | COMMENT_STATUS_RESOLVED
            | COMMENT_STATUS_ORPHANED
            | COMMENT_STATUS_DISMISSED
            | COMMENT_STATUS_DISPATCHED => {}
            other => bail!("invalid comment status: {other}"),
        }
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE work_comments
             SET status = ?2,
                 status_actor = ?3,
                 updated_at = ?4,
                 dismissed_at = CASE WHEN ?2 IN ('resolved', 'dismissed') THEN ?4 ELSE NULL END
             WHERE id = ?1",
            params![comment_id, status, actor, now],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after status update: {comment_id}"))
    }

    /// Soft-dismiss: transition a comment to `resolved`. Recoverable via
    /// `set_comment_status(.., "active", ..)`.
    pub fn dismiss_comment(&self, comment_id: &str, actor: Option<&str>) -> Result<WorkComment> {
        self.set_comment_status(comment_id, COMMENT_STATUS_RESOLVED, actor)
    }

    /// Persist a renderer-supplied re-anchor (the `comments_update_anchor`
    /// callback). Records the fuzzy outcome so the sidebar shows the ⚠ glyph
    /// and subsequent loads exact-match against the new shape.
    pub fn update_comment_anchor(
        &self,
        comment_id: &str,
        anchor: &CommentAnchor,
        new_doc_version: &str,
        plain_text_projection_version: i64,
    ) -> Result<WorkComment> {
        let conn = self.connect()?;
        let now = now_string();
        let anchor_json = serde_json::to_string(anchor)?;
        let n = conn.execute(
            "UPDATE work_comments
             SET anchor_json = ?2,
                 doc_version = ?3,
                 last_resolved_with = ?4,
                 plain_text_projection_version = ?5,
                 updated_at = ?6
             WHERE id = ?1",
            params![
                comment_id,
                anchor_json,
                new_doc_version,
                RESOLVED_WITH_FUZZY,
                plain_text_projection_version,
                now
            ],
        )?;
        if n == 0 {
            bail!("unknown comment: {comment_id}");
        }
        query_comment(&conn, comment_id)?
            .with_context(|| format!("missing comment after anchor update: {comment_id}"))
    }

    /// Resolve every active (or previously orphaned) comment on an artifact
    /// against `plain_text` — the renderer's current plain-text projection.
    ///
    /// Persists the resolution outcome: an `exact` hit marks the row
    /// `last_resolved_with = 'exact'`; a `fuzzy` hit re-extracts a fresh
    /// anchor around the match (so the next load exact-matches) and marks it
    /// `'fuzzy'`; an unresolvable comment flips to `status = 'orphaned'`. A
    /// previously orphaned comment that now resolves is revived to `active`.
    ///
    /// Resolution is a per-client read-with-side-effect (each client supplies
    /// its own projection), so it does **not** publish a topic event; the
    /// caller already receives the outcome in the reply.
    pub fn resolve_comments(
        &self,
        artifact_kind: &str,
        artifact_id: &str,
        plain_text: &str,
        plain_text_projection_version: i64,
        config: &CommentFuzzyConfig,
    ) -> Result<Vec<ResolvedComment>> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        // Resolve active + orphaned (orphans can recover); resolved/dismissed
        // are intentionally not re-resolved.
        let comments = query_comments(&tx, artifact_kind, artifact_id, false)?;
        let now = now_string();
        let mut out = Vec::with_capacity(comments.len());
        for mut comment in comments {
            if comment.status != COMMENT_STATUS_ACTIVE && comment.status != COMMENT_STATUS_ORPHANED {
                continue;
            }
            let resolution = resolve_anchor(plain_text, &comment.anchor, config);
            let wire = match resolution {
                AnchorResolution::Exact { start, length } => {
                    tx.execute(
                        "UPDATE work_comments
                         SET status = 'active', last_resolved_with = ?2, updated_at = ?3
                         WHERE id = ?1",
                        params![comment.id, RESOLVED_WITH_EXACT, now],
                    )?;
                    comment.status = COMMENT_STATUS_ACTIVE.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_EXACT.to_owned());
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_EXACT.to_owned(),
                        start: Some(start as i64),
                        length: Some(length as i64),
                        score: None,
                    }
                }
                AnchorResolution::Fuzzy {
                    start,
                    length,
                    score,
                } => {
                    let new_anchor = extract_anchor(plain_text, start, length);
                    let anchor_json = serde_json::to_string(&new_anchor)?;
                    tx.execute(
                        "UPDATE work_comments
                         SET anchor_json = ?2, status = 'active', last_resolved_with = ?3,
                             plain_text_projection_version = ?4, updated_at = ?5
                         WHERE id = ?1",
                        params![
                            comment.id,
                            anchor_json,
                            RESOLVED_WITH_FUZZY,
                            plain_text_projection_version,
                            now
                        ],
                    )?;
                    comment.anchor = new_anchor;
                    comment.status = COMMENT_STATUS_ACTIVE.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_FUZZY.to_owned());
                    comment.plain_text_projection_version = plain_text_projection_version;
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_FUZZY.to_owned(),
                        start: Some(start as i64),
                        length: Some(length as i64),
                        score: Some(score),
                    }
                }
                AnchorResolution::Orphan => {
                    tx.execute(
                        "UPDATE work_comments
                         SET status = 'orphaned', last_resolved_with = ?2, updated_at = ?3
                         WHERE id = ?1",
                        params![comment.id, RESOLVED_WITH_ORPHAN, now],
                    )?;
                    comment.status = COMMENT_STATUS_ORPHANED.to_owned();
                    comment.last_resolved_with = Some(RESOLVED_WITH_ORPHAN.to_owned());
                    comment.updated_at = now.clone();
                    CommentResolution {
                        kind: RESOLVED_WITH_ORPHAN.to_owned(),
                        start: None,
                        length: None,
                        score: None,
                    }
                }
            };
            out.push(ResolvedComment {
                comment,
                resolution: wire,
            });
        }
        tx.commit()?;
        Ok(out)
    }

    /// Re-key the active `work_item:<task_id>` comments onto a `pr_doc:*`
    /// artifact when a design doc graduates to a PR (DesignDetector
    /// `in_review` transition). Each original is copied to a new row keyed to
    /// `new_artifact_id`; the original is then soft-resolved so the trail is
    /// visible (design § "Comments on PR-backed docs").
    ///
    /// When `new_plain_text` is supplied, each migrated anchor is immediately
    /// re-resolved against it (fuzzy re-anchors are persisted; comments that
    /// can't re-anchor land as `orphaned` on the pr_doc side). When `None`,
    /// the anchors are copied verbatim and resolution is deferred to the
    /// renderer's next load — the engine cannot itself render markdown to
    /// plain text. Returns the number of comments migrated.
    pub fn migrate_work_item_comments_to_pr_doc(
        &self,
        task_id: &str,
        new_artifact_id: &str,
        new_plain_text: Option<&str>,
        plain_text_projection_version: i64,
        config: &CommentFuzzyConfig,
    ) -> Result<usize> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let originals: Vec<WorkComment> =
            query_comments(&tx, "work_item", task_id, false)?
                .into_iter()
                .filter(|c| c.status == COMMENT_STATUS_ACTIVE)
                .collect();
        let now = now_string();
        let actor = crate::work::AUDIT_ACTOR_DESIGN_DETECTOR;
        let mut migrated = 0usize;
        for original in &originals {
            let new_id = next_id("cmt");
            let mut anchor = original.anchor.clone();
            let mut status = COMMENT_STATUS_ACTIVE;
            let mut last_resolved: Option<&str> = None;
            let mut proj_ver = original.plain_text_projection_version;
            if let Some(text) = new_plain_text {
                match resolve_anchor(text, &original.anchor, config) {
                    AnchorResolution::Exact { .. } => last_resolved = Some(RESOLVED_WITH_EXACT),
                    AnchorResolution::Fuzzy { start, length, .. } => {
                        anchor = extract_anchor(text, start, length);
                        last_resolved = Some(RESOLVED_WITH_FUZZY);
                        proj_ver = plain_text_projection_version;
                    }
                    AnchorResolution::Orphan => {
                        status = COMMENT_STATUS_ORPHANED;
                        last_resolved = Some(RESOLVED_WITH_ORPHAN);
                    }
                }
            }
            let anchor_json = serde_json::to_string(&anchor)?;
            tx.execute(
                COMMENT_INSERT_SQL,
                params![
                    new_id,
                    "pr_doc",
                    new_artifact_id,
                    original.doc_version,
                    anchor_json,
                    original.body,
                    original.author,
                    status,
                    actor,
                    last_resolved,
                    proj_ver,
                    now,
                    now,
                    Option::<String>::None,
                ],
            )?;
            tx.execute(
                "UPDATE work_comments
                 SET status = 'resolved', status_actor = ?2, updated_at = ?3, dismissed_at = ?3
                 WHERE id = ?1",
                params![original.id, actor, now],
            )?;
            migrated += 1;
        }
        tx.commit()?;
        Ok(migrated)
    }

    // --- Magic-wand dispatch (Phase 3: engine-owned docs; Phase 4: PR-backed docs) ---

    /// Column list for every `magic_wand_dispatches` SELECT. Order must match
    /// [`map_magic_wand_dispatch`].
    fn magic_wand_columns() -> &'static str {
        "id, comment_id, artifact_kind, artifact_id, doc_version, status, \
         input_tokens, output_tokens, result_md, error_kind, anchor_warning, \
         created_at, resolved_at, chore_id"
    }

    /// Insert an `in_flight` dispatch row. Returns the inserted row.
    pub fn create_magic_wand_dispatch(
        &self,
        comment_id: &str,
        artifact_kind: &str,
        artifact_id: &str,
        doc_version: &str,
    ) -> Result<MagicWandDispatch> {
        let conn = self.connect()?;
        let id = next_id("mwd");
        let now = now_string();
        conn.execute(
            "INSERT INTO magic_wand_dispatches \
             (id, comment_id, artifact_kind, artifact_id, doc_version, status, \
              anchor_warning, created_at) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7)",
            params![
                id,
                comment_id,
                artifact_kind,
                artifact_id,
                doc_version,
                MAGIC_WAND_STATUS_IN_FLIGHT,
                now,
            ],
        )?;
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        conn.query_row(&sql, [&id], map_magic_wand_dispatch)
            .map_err(Into::into)
    }

    /// Insert a `chore_created` dispatch row for a Phase-4 PR-backed doc
    /// dispatch. Unlike the Phase-3 `in_flight` row, this is immediately
    /// terminal from the engine's perspective — no subsequent Claude call.
    /// The spawned chore's id is recorded in `chore_id` for audit linkage.
    ///
    /// The caller is also responsible for transitioning the comment to
    /// `dispatched` via `set_comment_status`.
    pub fn create_pr_backed_magic_wand_dispatch(
        &self,
        comment_id: &str,
        artifact_kind: &str,
        artifact_id: &str,
        doc_version: &str,
        chore_id: &str,
    ) -> Result<MagicWandDispatch> {
        let conn = self.connect()?;
        let id = next_id("mwd");
        let now = now_string();
        conn.execute(
            "INSERT INTO magic_wand_dispatches \
             (id, comment_id, artifact_kind, artifact_id, doc_version, status, \
              anchor_warning, created_at, chore_id) \
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 0, ?7, ?8)",
            params![
                id,
                comment_id,
                artifact_kind,
                artifact_id,
                doc_version,
                MAGIC_WAND_STATUS_CHORE_CREATED,
                now,
                chore_id,
            ],
        )?;
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        conn.query_row(&sql, [&id], map_magic_wand_dispatch)
            .map_err(Into::into)
    }

    /// Transition a dispatch from `in_flight` to `returned` (success) or
    /// `failed`. Records token counts, the result markdown, and an optional
    /// `error_kind` classification. Called from the async Claude-call task.
    pub fn complete_magic_wand_dispatch(
        &self,
        dispatch_id: &str,
        status: &str,
        result_md: Option<&str>,
        error_kind: Option<&str>,
        input_tokens: Option<i64>,
        output_tokens: Option<i64>,
        anchor_warning: bool,
    ) -> Result<MagicWandDispatch> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE magic_wand_dispatches
             SET status = ?2, result_md = ?3, error_kind = ?4,
                 input_tokens = ?5, output_tokens = ?6,
                 anchor_warning = ?7, resolved_at = ?8
             WHERE id = ?1 AND status = 'in_flight'",
            params![
                dispatch_id,
                status,
                result_md,
                error_kind,
                input_tokens,
                output_tokens,
                anchor_warning as i64,
                now,
            ],
        )?;
        if n == 0 {
            bail!(
                "dispatch {dispatch_id} not found or already in a terminal state \
                 (expected in_flight)"
            );
        }
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        conn.query_row(&sql, [dispatch_id], map_magic_wand_dispatch)
            .map_err(Into::into)
    }

    /// Fetch a dispatch row by id.
    pub fn get_magic_wand_dispatch(
        &self,
        dispatch_id: &str,
    ) -> Result<Option<MagicWandDispatch>> {
        let conn = self.connect()?;
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        conn.query_row(&sql, [dispatch_id], map_magic_wand_dispatch)
            .optional()
            .map_err(Into::into)
    }

    /// Apply the magic-wand result: overwrite the work-item description with
    /// `result_md` after a doc-version CAS check.
    ///
    /// `current_doc_version` is the SHA-256 of the doc's *current* plain-text
    /// projection, as computed by the macOS renderer. It is compared against the
    /// dispatch row's `doc_version` (captured at dispatch creation time).
    ///
    /// Returns `(updated_dispatch, conflict)`. On conflict the dispatch
    /// transitions to `conflict` and no description is overwritten. On
    /// match the description is overwritten, the dispatch transitions to
    /// `applied`, and the comment transitions to `resolved`.
    pub fn apply_magic_wand_dispatch(
        &self,
        dispatch_id: &str,
        current_doc_version: &str,
        actor: &str,
    ) -> Result<(MagicWandDispatch, bool)> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        let dispatch: MagicWandDispatch = tx
            .query_row(&sql, [dispatch_id], map_magic_wand_dispatch)
            .optional()?
            .with_context(|| format!("unknown magic_wand dispatch: {dispatch_id}"))?;

        if dispatch.status != MAGIC_WAND_STATUS_RETURNED {
            bail!(
                "dispatch {dispatch_id} cannot be applied: status is {}, expected returned",
                dispatch.status
            );
        }
        let result_md = dispatch
            .result_md
            .as_deref()
            .ok_or_else(|| anyhow::anyhow!("dispatch {dispatch_id} has no result_md"))?;

        let now = now_string();
        // CAS check: compare current doc version against the version the dispatch ran against.
        let conflict = dispatch.doc_version != current_doc_version;
        if conflict {
            tx.execute(
                "UPDATE magic_wand_dispatches SET status = ?2, resolved_at = ?3 WHERE id = ?1",
                params![dispatch_id, MAGIC_WAND_STATUS_CONFLICT, now],
            )?;
        } else {
            // Update the work item description.
            tx.execute(
                "UPDATE tasks SET description = ?2, updated_at = ?3 WHERE id = ?1",
                params![dispatch.artifact_id, result_md, now],
            )?;
            // Flip the dispatch to applied.
            tx.execute(
                "UPDATE magic_wand_dispatches SET status = ?2, resolved_at = ?3 WHERE id = ?1",
                params![dispatch_id, MAGIC_WAND_STATUS_APPLIED, now],
            )?;
            // Resolve the comment.
            tx.execute(
                "UPDATE work_comments
                 SET status = 'resolved', status_actor = ?2, updated_at = ?3, dismissed_at = ?3
                 WHERE id = ?1",
                params![dispatch.comment_id, actor, now],
            )?;
        }
        tx.commit()?;
        // Re-fetch the dispatch to pick up the new status.
        let updated: MagicWandDispatch = conn
            .query_row(&sql, [dispatch_id], map_magic_wand_dispatch)
            .map_err(anyhow::Error::from)?;
        Ok((updated, conflict))
    }

    /// Discard the magic-wand result without modifying the description.
    /// Transitions the dispatch to `discarded`; the comment stays `active`.
    pub fn discard_magic_wand_dispatch(
        &self,
        dispatch_id: &str,
    ) -> Result<MagicWandDispatch> {
        let conn = self.connect()?;
        let now = now_string();
        let n = conn.execute(
            "UPDATE magic_wand_dispatches SET status = ?2, resolved_at = ?3 WHERE id = ?1",
            params![dispatch_id, MAGIC_WAND_STATUS_DISCARDED, now],
        )?;
        if n == 0 {
            bail!("unknown magic_wand dispatch: {dispatch_id}");
        }
        let cols = Self::magic_wand_columns();
        let sql = format!("SELECT {cols} FROM magic_wand_dispatches WHERE id = ?1");
        conn.query_row(&sql, [dispatch_id], map_magic_wand_dispatch)
            .map_err(Into::into)
    }
}

/// Re-extract a 64/exact/64-char anchor around `[start, start+length)` in the
/// plain text, trimmed to text bounds. Used after a fuzzy resolve so the
/// stored anchor reflects the current doc and the next load exact-matches.
fn extract_anchor(plain_text: &str, start: usize, length: usize) -> CommentAnchor {
    let chars: Vec<char> = plain_text.chars().collect();
    let n = chars.len();
    let start = start.min(n);
    let end = (start + length).min(n);
    let prefix_start = start.saturating_sub(ANCHOR_CONTEXT_CHARS);
    let suffix_end = (end + ANCHOR_CONTEXT_CHARS).min(n);
    CommentAnchor {
        exact: chars[start..end].iter().collect(),
        prefix: chars[prefix_start..start].iter().collect(),
        suffix: chars[end..suffix_end].iter().collect(),
    }
}

pub(crate) fn query_comment(conn: &Connection, id: &str) -> Result<Option<WorkComment>> {
    let sql = format!("SELECT {COMMENT_COLUMNS} FROM work_comments WHERE id = ?1");
    conn.query_row(&sql, [id], map_comment)
        .optional()
        .map_err(Into::into)
}

pub(crate) fn query_comments(
    conn: &Connection,
    artifact_kind: &str,
    artifact_id: &str,
    include_resolved: bool,
) -> Result<Vec<WorkComment>> {
    let filter = if include_resolved {
        ""
    } else {
        " AND status NOT IN ('resolved', 'dismissed')"
    };
    let sql = format!(
        "SELECT {COMMENT_COLUMNS} FROM work_comments
         WHERE artifact_kind = ?1 AND artifact_id = ?2{filter}
         ORDER BY created_at ASC, id ASC"
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt.query_map(params![artifact_kind, artifact_id], map_comment)?;
    collect_rows(rows)
}

#[cfg(test)]
mod tests {
    use crate::comments_anchor::CommentFuzzyConfig;
    use crate::work::WorkDb;
    use boss_protocol::{CommentAnchor, CreateCommentInput};
    use std::path::PathBuf;

    /// Per-test named shared-cache in-memory db (see `work::tests`).
    fn mem_db() -> WorkDb {
        WorkDb::open(PathBuf::from(":memory:")).unwrap()
    }

    fn input(artifact_id: &str, exact: &str, prefix: &str, suffix: &str) -> CreateCommentInput {
        CreateCommentInput {
            artifact_kind: "work_item".to_owned(),
            artifact_id: artifact_id.to_owned(),
            doc_version: "v0".to_owned(),
            anchor: CommentAnchor {
                exact: exact.to_owned(),
                prefix: prefix.to_owned(),
                suffix: suffix.to_owned(),
            },
            body: "a comment body".to_owned(),
            author: "user:test@example.com".to_owned(),
            plain_text_projection_version: 1,
        }
    }

    fn cfg() -> CommentFuzzyConfig {
        CommentFuzzyConfig::default()
    }

    #[test]
    fn create_and_list_round_trip() {
        let db = mem_db();
        let c1 = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let _c2 = db.create_comment(input("t1", "beta", "", "")).unwrap();
        let list = db.list_comments("work_item", "t1", false).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(c1.status, "active");
        assert_eq!(c1.author, "user:test@example.com");
        assert_eq!(c1.plain_text_projection_version, 1);
        assert!(list.iter().any(|c| c.anchor.exact == "alpha"));
        assert!(list.iter().any(|c| c.anchor.exact == "beta"));
        // Other artifacts are isolated.
        assert!(db.list_comments("work_item", "other", false).unwrap().is_empty());
    }

    #[test]
    fn empty_body_and_empty_exact_are_rejected() {
        let db = mem_db();
        let mut bad = input("t1", "alpha", "", "");
        bad.body = "   ".to_owned();
        assert!(db.create_comment(bad).is_err());
        let mut bad2 = input("t1", "", "", "");
        bad2.body = "ok".to_owned();
        assert!(db.create_comment(bad2).is_err());
    }

    #[test]
    fn soft_dismiss_hides_then_show_resolved_reveals_then_reactivate() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "", "")).unwrap();

        let dismissed = db.dismiss_comment(&c.id, Some("user:me")).unwrap();
        assert_eq!(dismissed.status, "resolved");
        assert!(dismissed.dismissed_at.is_some());
        assert_eq!(dismissed.status_actor.as_deref(), Some("user:me"));

        // Hidden from the default list, revealed by include_resolved.
        assert!(db.list_comments("work_item", "t1", false).unwrap().is_empty());
        let revealed = db.list_comments("work_item", "t1", true).unwrap();
        assert_eq!(revealed.len(), 1);
        assert_eq!(revealed[0].status, "resolved");

        // Recoverable: re-activate clears dismissed_at.
        let reactivated = db.set_comment_status(&c.id, "active", Some("user:me")).unwrap();
        assert_eq!(reactivated.status, "active");
        assert!(reactivated.dismissed_at.is_none());
        assert_eq!(db.list_comments("work_item", "t1", false).unwrap().len(), 1);
    }

    #[test]
    fn resolve_exact_returns_span_and_persists_mode() {
        let db = mem_db();
        let doc = "Hello world, this is a sample document about anchoring.";
        let c = db
            .create_comment(input("t1", "sample document", "this is a ", " about anchoring"))
            .unwrap();
        let resolved = db.resolve_comments("work_item", "t1", doc, 2, &cfg()).unwrap();
        assert_eq!(resolved.len(), 1);
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "exact");
        let start = r.resolution.start.unwrap() as usize;
        let length = r.resolution.length.unwrap() as usize;
        let span: String = doc.chars().skip(start).take(length).collect();
        assert_eq!(span, "sample document");
        assert_eq!(r.comment.last_resolved_with.as_deref(), Some("exact"));
        // Persisted.
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("exact"));
    }

    #[test]
    fn resolve_fuzzy_reanchors_and_next_load_is_exact() {
        let db = mem_db();
        let prefix = "The renderer maintains a mapping so the ";
        let exact = "engine never has to know about layout";
        let suffix = ", and the macOS app never round-trips";
        let c = db.create_comment(input("t1", exact, prefix, suffix)).unwrap();

        // A word ("carefully") was inserted inside the prefix region, so the
        // verbatim context no longer matches — but the region is ~identical.
        let edited = "Intro. The renderer carefully maintains a mapping so the engine never \
                      has to know about layout, and the macOS app never round-trips. Outro.";
        let resolved = db.resolve_comments("work_item", "t1", edited, 3, &cfg()).unwrap();
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "fuzzy");
        assert!(r.resolution.score.unwrap() >= 0.8);
        assert_eq!(r.comment.last_resolved_with.as_deref(), Some("fuzzy"));

        // The engine re-extracted and persisted a fresh anchor against the
        // edited text + recorded the projection version → a second load now
        // exact-matches.
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("fuzzy"));
        assert_eq!(reloaded.plain_text_projection_version, 3);
        let again = db.resolve_comments("work_item", "t1", edited, 3, &cfg()).unwrap();
        assert_eq!(again[0].resolution.kind, "exact");
    }

    #[test]
    fn resolve_orphan_when_containing_element_deleted() {
        let db = mem_db();
        let c = db
            .create_comment(input(
                "t1",
                "The widget config lives here",
                "Section A. ",
                " Section B.",
            ))
            .unwrap();
        // The anchored span is removed entirely and replaced with unrelated text.
        let edited = "Section A. Section B. Totally different unrelated content now appears.";
        let resolved = db.resolve_comments("work_item", "t1", edited, 2, &cfg()).unwrap();
        let r = &resolved[0];
        assert_eq!(r.resolution.kind, "orphan");
        assert!(r.resolution.start.is_none());
        let reloaded = db.get_comment(&c.id).unwrap().unwrap();
        assert_eq!(reloaded.status, "orphaned");
        assert_eq!(reloaded.last_resolved_with.as_deref(), Some("orphan"));
        // Orphans still appear in the default list (the sidebar surfaces them).
        assert_eq!(db.list_comments("work_item", "t1", false).unwrap().len(), 1);
    }

    #[test]
    fn update_anchor_persists_new_shape_and_marks_fuzzy() {
        let db = mem_db();
        let c = db.create_comment(input("t1", "alpha", "p", "s")).unwrap();
        let new_anchor = CommentAnchor {
            exact: "alpha-v2".to_owned(),
            prefix: "new-prefix".to_owned(),
            suffix: "new-suffix".to_owned(),
        };
        let updated = db
            .update_comment_anchor(&c.id, &new_anchor, "v2", 5)
            .unwrap();
        assert_eq!(updated.anchor.exact, "alpha-v2");
        assert_eq!(updated.doc_version, "v2");
        assert_eq!(updated.last_resolved_with.as_deref(), Some("fuzzy"));
        assert_eq!(updated.plain_text_projection_version, 5);
        assert!(db.update_comment_anchor("nope", &new_anchor, "v2", 5).is_err());
    }

    #[test]
    fn cross_doc_migration_rekeys_and_resolves_originals() {
        let db = mem_db();
        db.create_comment(input("task1", "alpha", "", "")).unwrap();
        db.create_comment(input("task1", "beta", "", "")).unwrap();

        let pr_artifact = "pr_doc:git@github.com:o/r.git:boss/exec_x:doc.md";
        let migrated = db
            .migrate_work_item_comments_to_pr_doc("task1", pr_artifact, None, 0, &cfg())
            .unwrap();
        assert_eq!(migrated, 2);

        // Originals are soft-resolved (the trail) and gone from the default list.
        assert!(db.list_comments("work_item", "task1", false).unwrap().is_empty());
        let originals = db.list_comments("work_item", "task1", true).unwrap();
        assert_eq!(originals.len(), 2);
        assert!(originals.iter().all(|c| c.status == "resolved"));
        assert!(
            originals
                .iter()
                .all(|c| c.status_actor.as_deref() == Some("engine_design_detector"))
        );

        // The pr_doc artifact carries the migrated comments, active, with
        // anchors copied verbatim (resolution deferred to renderer load).
        let pr = db.list_comments("pr_doc", pr_artifact, false).unwrap();
        assert_eq!(pr.len(), 2);
        assert!(pr.iter().all(|c| c.status == "active"));
        assert!(pr.iter().any(|c| c.anchor.exact == "alpha"));
        assert!(pr.iter().any(|c| c.anchor.exact == "beta"));
    }

    #[test]
    fn cross_doc_migration_orphans_unanchorable_comments() {
        let db = mem_db();
        db.create_comment(input("task1", "present span", "", "")).unwrap();
        db.create_comment(input("task1", "absent span zzqq", "", "")).unwrap();

        let pr_artifact = "pr_doc:r:b:p.md";
        let pr_text = "This PR doc contains the present span among other unrelated words.";
        let migrated = db
            .migrate_work_item_comments_to_pr_doc("task1", pr_artifact, Some(pr_text), 9, &cfg())
            .unwrap();
        assert_eq!(migrated, 2);

        let pr = db.list_comments("pr_doc", pr_artifact, true).unwrap();
        let present = pr.iter().find(|c| c.anchor.exact == "present span").unwrap();
        assert_eq!(present.status, "active");
        assert_eq!(present.last_resolved_with.as_deref(), Some("exact"));

        let absent = pr
            .iter()
            .find(|c| c.anchor.exact == "absent span zzqq")
            .unwrap();
        assert_eq!(absent.status, "orphaned");
        assert_eq!(absent.last_resolved_with.as_deref(), Some("orphan"));
    }

    #[test]
    fn migration_is_noop_when_no_active_comments() {
        let db = mem_db();
        let n = db
            .migrate_work_item_comments_to_pr_doc("task-empty", "pr_doc:r:b:p.md", None, 0, &cfg())
            .unwrap();
        assert_eq!(n, 0);
    }

    // --- Magic-wand dispatch unit tests (Phase 3) ---

    #[test]
    fn magic_wand_create_and_fetch() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();
        assert_eq!(dispatch.comment_id, comment.id);
        assert_eq!(dispatch.artifact_kind, "work_item");
        assert_eq!(dispatch.artifact_id, "t1");
        assert_eq!(dispatch.doc_version, "v0");
        assert_eq!(dispatch.status, "in_flight");
        assert!(dispatch.result_md.is_none());
        assert!(dispatch.error_kind.is_none());
        assert!(!dispatch.anchor_warning);

        let fetched = db.get_magic_wand_dispatch(&dispatch.id).unwrap().unwrap();
        assert_eq!(fetched.id, dispatch.id);
        assert_eq!(fetched.status, "in_flight");
    }

    #[test]
    fn magic_wand_complete_returned() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();

        let updated = db
            .complete_magic_wand_dispatch(
                &dispatch.id,
                "returned",
                Some("# Updated\n\nNew content."),
                None,
                Some(500),
                Some(200),
                false,
            )
            .unwrap();
        assert_eq!(updated.status, "returned");
        assert_eq!(updated.result_md.as_deref(), Some("# Updated\n\nNew content."));
        assert_eq!(updated.input_tokens, Some(500));
        assert_eq!(updated.output_tokens, Some(200));
        assert!(updated.resolved_at.is_some());
    }

    #[test]
    fn magic_wand_complete_failed() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();
        let updated = db
            .complete_magic_wand_dispatch(
                &dispatch.id,
                "failed",
                None,
                Some("length_sanity"),
                None,
                None,
                false,
            )
            .unwrap();
        assert_eq!(updated.status, "failed");
        assert!(updated.result_md.is_none());
        assert_eq!(updated.error_kind.as_deref(), Some("length_sanity"));
    }

    #[test]
    fn magic_wand_complete_idempotency_guard() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();
        db.complete_magic_wand_dispatch(
            &dispatch.id,
            "returned",
            Some("result"),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        // A second complete call must fail because status is no longer 'in_flight'.
        assert!(db
            .complete_magic_wand_dispatch(&dispatch.id, "failed", None, None, None, None, false)
            .is_err());
    }

    #[test]
    fn magic_wand_discard() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();
        db.complete_magic_wand_dispatch(
            &dispatch.id,
            "returned",
            Some("result"),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        let discarded = db.discard_magic_wand_dispatch(&dispatch.id).unwrap();
        assert_eq!(discarded.status, "discarded");
        // Comment stays active.
        let comment = db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(comment.status, "active");
    }

    #[test]
    fn magic_wand_apply_cas_conflict() {
        let db = mem_db();
        let comment = db.create_comment(input("t1", "alpha", "", "")).unwrap();
        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t1", "v0")
            .unwrap();
        db.complete_magic_wand_dispatch(
            &dispatch.id,
            "returned",
            Some("# Updated"),
            None,
            None,
            None,
            false,
        )
        .unwrap();
        // Pass a different current_doc_version to trigger a conflict.
        let (updated, conflict) = db
            .apply_magic_wand_dispatch(&dispatch.id, "v_different", "user:me")
            .unwrap();
        assert!(conflict);
        assert_eq!(updated.status, "conflict");
        // Comment stays active on conflict.
        let comment = db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(comment.status, "active");
    }

    #[test]
    fn magic_wand_apply_cas_match() {
        let db = mem_db();
        // Create a tasks row so the UPDATE in apply_magic_wand_dispatch
        // hits an actual row. We use the raw connection for setup.
        let comment = db
            .create_comment(CreateCommentInput {
                artifact_kind: "work_item".to_owned(),
                artifact_id: "t_apply".to_owned(),
                doc_version: "v_current".to_owned(),
                anchor: CommentAnchor {
                    exact: "span".to_owned(),
                    prefix: String::new(),
                    suffix: String::new(),
                },
                body: "fix this".to_owned(),
                author: "user:test@example.com".to_owned(),
                plain_text_projection_version: 1,
            })
            .unwrap();

        let dispatch = db
            .create_magic_wand_dispatch(&comment.id, "work_item", "t_apply", "v_current")
            .unwrap();
        db.complete_magic_wand_dispatch(
            &dispatch.id,
            "returned",
            Some("# Updated description"),
            None,
            Some(100),
            Some(50),
            false,
        )
        .unwrap();

        // Match: doc_version of dispatch == current_doc_version passed in.
        let (updated, conflict) = db
            .apply_magic_wand_dispatch(&dispatch.id, "v_current", "user:me")
            .unwrap();
        assert!(!conflict, "should not conflict when doc_version matches");
        assert_eq!(updated.status, "applied");
        // Comment is resolved.
        let comment = db.get_comment(&comment.id).unwrap().unwrap();
        assert_eq!(comment.status, "resolved");
    }
}
