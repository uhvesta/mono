use super::*;

impl WorkDb {
    /// Fetch a single project by id. Used by the runner when it
    /// composes the worker prompt for a `kind = 'design'` task —
    /// the design task itself is sparse, so the runner enriches the
    /// prompt with the parent project's name/goal/description.
    pub fn get_project(&self, id: &str) -> Result<Project> {
        let conn = self.connect()?;
        query_project(&conn, id).require("project", id)
    }

    /// Fetch a single product row by id. Returns `None` when no row
    /// matches so the dispatcher can fall through the design's Q3
    /// precedence (product default → engine default) without
    /// distinguishing "no product default set" from "the row's
    /// product id doesn't resolve" — both produce the same engine
    /// fall-through behaviour for the spawn config resolver.
    pub fn get_product(&self, id: &str) -> Result<Option<Product>> {
        let conn = self.connect()?;
        query_product(&conn, id)
    }

    /// Set (or clear) a product's `default_model` per the
    /// effort-and-model-estimation design (PR #370). `model = None`
    /// or `Some("")` clears the column; any other slug is stored
    /// verbatim after a trim. The engine deliberately does NOT
    /// validate the slug — `claude` is the source of truth on what
    /// `--model` accepts, and a new model must be adoptable without
    /// an engine release blocking it (design §Q3).
    pub fn set_product_default_model(&self, product_id: &str, model: Option<&str>) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = query_product(&tx, product_id).require("product", product_id)?;
        let now = now_string();
        let stored = model.map(|s| s.trim().to_owned()).filter(|s| !s.is_empty());
        tx.execute(
            "UPDATE products SET default_model = ?2, updated_at = ?3 WHERE id = ?1",
            params![product_id, stored, now],
        )?;
        let updated = query_product(&tx, product_id).require("product", product_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Bind (or unbind) a product's external tracker columns.
    ///
    /// When `unset = true`: clears both `external_tracker_kind` and
    /// `external_tracker_config` to NULL regardless of any other fields.
    /// When `unset = false`: both `kind` and `config` must be `Some`;
    /// the engine stores `config` as its JSON string representation.
    pub fn set_product_external_tracker(
        &self,
        product_id: &str,
        kind: Option<&str>,
        config: Option<&serde_json::Value>,
        unset: bool,
    ) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = query_product(&tx, product_id).require("product", product_id)?;
        let now = now_string();
        if unset {
            tx.execute(
                "UPDATE products SET external_tracker_kind = NULL, external_tracker_config = NULL, updated_at = ?2 WHERE id = ?1",
                params![product_id, now],
            )?;
        } else {
            let config_json = config.map(|c| c.to_string());
            tx.execute(
                "UPDATE products SET external_tracker_kind = ?2, external_tracker_config = ?3, updated_at = ?4 WHERE id = ?1",
                params![product_id, kind, config_json, now],
            )?;
        }
        let updated = query_product(&tx, product_id).require("product", product_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Set (or clear) a product's `editorial_rules` JSON blob.
    ///
    /// `rules = Some(r)` serialises the blob and stores it. `rules = None`
    /// clears the column to NULL so all-defaults behaviour resumes.
    pub fn set_product_editorial_rules(
        &self,
        product_id: &str,
        rules: Option<&boss_protocol::EditorialRules>,
    ) -> Result<Product> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let _ = query_product(&tx, product_id).require("product", product_id)?;
        let now = now_string();
        let rules_json = rules
            .map(serde_json::to_string)
            .transpose()
            .map_err(|e| anyhow::anyhow!("failed to serialize editorial_rules: {e}"))?;
        tx.execute(
            "UPDATE products SET editorial_rules = ?2, updated_at = ?3 WHERE id = ?1",
            params![product_id, rules_json, now],
        )?;
        let updated = query_product(&tx, product_id).require("product", product_id)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Write the project's design-doc pointer columns.
    ///
    /// Three input shapes (matching `SetProjectDesignDocInput`):
    /// - `unset = true` → all three columns are cleared to `NULL`
    ///   atomically. Any explicit field values supplied alongside are
    ///   ignored.
    /// - `design_doc_path = Some(p)` with non-empty `p` → set the
    ///   pointer. `p` is validated per the design's Q8 rules (no
    ///   leading `/`, no `..` segments, must end in `.md` /
    ///   `.markdown`). `design_doc_repo_remote_url` and
    ///   `design_doc_branch` are best-effort overrides; `None` /
    ///   blank clears that column so resolution falls back to the
    ///   product. The repo URL is canonicalised the same way
    ///   `products.repo_remote_url` is (trim-normalise).
    /// - `design_doc_path = None` (and `unset = false`) → update only
    ///   the non-path columns. The existing path stays put. Useful
    ///   when the user is correcting a typo in just the repo or
    ///   branch fields.
    ///
    /// Last-writer-wins: a fresh call overwrites whatever was there.
    /// `updated_at` is stamped on every write. `last_status_actor` is
    /// intentionally untouched — pointer edits are property edits,
    /// not status transitions (Q10).
    pub fn set_project_design_doc(&self, input: SetProjectDesignDocInput) -> Result<Project> {
        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let before = query_project(&tx, &input.project_id).require("project", &input.project_id)?;
        let now = now_string();

        if input.unset {
            tx.execute(
                "UPDATE projects
                 SET design_doc_repo_remote_url = NULL,
                     design_doc_branch = NULL,
                     design_doc_path = NULL,
                     updated_at = ?2
                 WHERE id = ?1",
                params![input.project_id, now],
            )?;
        } else {
            let repo = canonicalize_design_doc_repo_remote_url(input.design_doc_repo_remote_url);
            let branch = normalize_optional_text(input.design_doc_branch);

            match input.design_doc_path {
                Some(raw_path) => {
                    let path = validate_design_doc_path(&raw_path)?;
                    tx.execute(
                        "UPDATE projects
                         SET design_doc_repo_remote_url = ?2,
                             design_doc_branch = ?3,
                             design_doc_path = ?4,
                             updated_at = ?5
                         WHERE id = ?1",
                        params![input.project_id, repo, branch, path, now],
                    )?;
                }
                None => {
                    tx.execute(
                        "UPDATE projects
                         SET design_doc_repo_remote_url = ?2,
                             design_doc_branch = ?3,
                             updated_at = ?4
                         WHERE id = ?1",
                        params![input.project_id, repo, branch, now],
                    )?;
                }
            }
        }

        let updated = query_project(&tx, &input.project_id).require("project", &input.project_id)?;
        record_design_doc_audit(&tx, &input.project_id, &before, &updated, AUDIT_ACTOR_HUMAN, &now)?;
        tx.commit()?;
        Ok(updated)
    }

    /// Resolve a project's design-doc pointer into the structured
    /// `ProjectDesignDocState` the UI consumes.
    ///
    /// Resolution rules (per design Q2):
    /// - `design_doc_path` is `NULL` → `NotSet` (UI hides the
    ///   affordance entirely).
    /// - Otherwise fall back to the product for any missing
    ///   `repo_remote_url` / `branch` override. Branch defaults to
    ///   `"main"` when neither the project nor (a future)
    ///   `products.docs_branch` supplies one.
    /// - If no repo can be resolved (project override `NULL` and
    ///   product's `repo_remote_url` `NULL`) → `Broken` with a
    ///   human-readable reason.
    /// - Classify the resolved repo against the project's product:
    ///   `SameProduct` when it matches, `OtherProduct` when another
    ///   Boss-tracked product owns the repo, `External` otherwise.
    ///
    /// `lookup_repo_workspace_path` is consulted only on the resolved
    /// path — pass a closure that asks cube for the absolute path of
    /// a workspace currently leased for the resolved `repo_remote_url`
    /// (or `None` when no workspace is leased). The macOS open
    /// dispatcher uses the returned path to fast-path into `$EDITOR` /
    /// the in-app renderer; when `None`, the affordance falls back to
    /// the GitHub web URL. In test/CLI contexts where cube isn't
    /// reachable, `|_| None` is the safe default.
    pub fn resolve_project_design_doc<F>(
        &self,
        project_id: &str,
        lookup_repo_workspace_path: F,
    ) -> Result<ResolveProjectDesignDocOutput>
    where
        F: FnOnce(&str) -> Option<String>,
    {
        let conn = self.connect()?;
        let project = query_project(&conn, project_id).require("project", project_id)?;
        let product = query_product(&conn, &project.product_id).require("product", &project.product_id)?;

        let Some(path) = project.design_doc_path.clone() else {
            return Ok(ResolveProjectDesignDocOutput {
                project_id: project.id,
                state: ProjectDesignDocState::NotSet,
            });
        };

        let resolved_repo = project
            .design_doc_repo_remote_url
            .clone()
            .or_else(|| product.repo_remote_url.clone());
        let Some(repo) = resolved_repo else {
            return Ok(ResolveProjectDesignDocOutput {
                project_id: project.id,
                state: ProjectDesignDocState::Broken {
                    reason: "design_doc_path is set but neither the project's design_doc_repo_remote_url nor the product's repo_remote_url is populated".to_owned(),
                },
            });
        };

        let branch = project.design_doc_branch.clone().unwrap_or_else(|| "main".to_owned());

        let kind = if let Some(product_repo) = product.repo_remote_url.as_deref()
            && product_repo == repo.as_str()
        {
            ResolvedDesignDocKind::SameProduct {
                product_id: project.product_id.clone(),
            }
        } else if let Some(other_product) = find_product_by_repo_remote_url(&conn, &repo)? {
            ResolvedDesignDocKind::OtherProduct {
                product_id: other_product,
            }
        } else {
            ResolvedDesignDocKind::External
        };

        let web_url = render_design_doc_web_url(&repo, &branch, &path);
        let raw_content_url = render_design_doc_raw_content_url(&repo, &branch, &path);
        let workspace_path = lookup_repo_workspace_path(&repo);

        Ok(ResolveProjectDesignDocOutput {
            project_id: project.id,
            state: ProjectDesignDocState::Resolved {
                resolved: ResolvedDesignDoc {
                    repo_remote_url: repo,
                    branch,
                    path,
                    kind,
                },
                workspace_path,
                web_url,
                raw_content_url,
            },
        })
    }

    /// Sync a `(repo, branch, path)` triple discovered by
    /// `DesignDetector` into the parent project's pointer columns,
    /// **iff** the project's `design_doc_path` is currently `NULL`.
    ///
    /// This is the one-way auto-populate rule from design Q6: a
    /// project that already has a hand-set pointer wins; a project
    /// that has no pointer benefits from the detector's discovery.
    /// Repo URL is canonicalised on the way in; path is validated
    /// against the same Q8 rules `set_project_design_doc` enforces,
    /// so a detector that hands us a garbage path fails fast rather
    /// than corrupting the column.
    ///
    /// Returns `true` if the columns were written, `false` if the
    /// project already had a pointer set (no-op).
    ///
    /// TODO(design-producing-tasks): wire this method into
    /// `DesignDetector`'s `DOC_REF` stop handler once that detector
    /// exists. Until then, this is exercised only by integration
    /// tests with a hand-rolled caller.
    pub fn sync_project_design_doc_from_detector(
        &self,
        project_id: &str,
        repo_remote_url: Option<&str>,
        branch: Option<&str>,
        path: &str,
    ) -> Result<bool> {
        let validated_path = validate_design_doc_path(path)?;
        let repo = canonicalize_design_doc_repo_remote_url(repo_remote_url.map(str::to_owned));
        let branch = normalize_optional_text(branch.map(str::to_owned));

        let mut conn = self.connect()?;
        let tx = conn.transaction()?;
        let before = query_project(&tx, project_id).require("project", project_id)?;
        if before.design_doc_path.is_some() {
            return Ok(false);
        }
        let now = now_string();
        tx.execute(
            "UPDATE projects
             SET design_doc_repo_remote_url = ?2,
                 design_doc_branch = ?3,
                 design_doc_path = ?4,
                 updated_at = ?5
             WHERE id = ?1",
            params![project_id, repo, branch, validated_path, now],
        )?;
        let after = query_project(&tx, project_id).require("project", project_id)?;
        record_design_doc_audit(&tx, project_id, &before, &after, AUDIT_ACTOR_DESIGN_DETECTOR, &now)?;
        tx.commit()?;
        Ok(true)
    }

    /// Read the append-only audit trail of property edits on
    /// `project_id`. Returns rows in chronological order (oldest
    /// first), with a stable secondary sort on row id so two writes
    /// landing in the same `changed_at` second still serialise.
    ///
    /// v1 records design-doc pointer columns
    /// (`design_doc_repo_remote_url`, `design_doc_branch`,
    /// `design_doc_path`); the schema is general so future edits to
    /// other project properties can ride along without a re-migration.
    pub fn list_project_property_audit(&self, project_id: &str) -> Result<Vec<ProjectPropertyAuditEntry>> {
        let conn = self.connect()?;
        let mut stmt = conn.prepare(
            "SELECT id, project_id, property, old_value, new_value, actor, changed_at
             FROM project_property_audit
             WHERE project_id = ?1
             ORDER BY changed_at ASC, id ASC",
        )?;
        let rows = stmt.query_map(params![project_id], |row| {
            Ok(ProjectPropertyAuditEntry {
                id: row.get(0)?,
                project_id: row.get(1)?,
                property: row.get(2)?,
                old_value: row.get(3)?,
                new_value: row.get(4)?,
                actor: row.get(5)?,
                changed_at: row.get(6)?,
            })
        })?;
        collect_rows(rows)
    }

    /// Surface a `WorkAttentionItem` when an `ApproveDesign` event
    /// names a design doc whose location disagrees with the parent
    /// project's already-set pointer.
    ///
    /// Behaviour (per design Q6 sync rule 3):
    /// - Project pointer is `NULL` → no conflict, no item, returns
    ///   `Ok(None)`. The auto-populate path
    ///   (`sync_project_design_doc_from_detector`) handles that
    ///   case before approval.
    /// - Project pointer matches the approved triple (after
    ///   resolving `None` overrides against the product / default
    ///   branch) → no item, returns `Ok(None)`.
    /// - Project pointer differs → an attention item with kind
    ///   `design_doc_pointer_conflict` is inserted against
    ///   `execution_id` and returned.
    ///
    /// The helper does NOT overwrite the project's pointer — the
    /// user's manual value always wins; the attention item asks
    /// them to choose explicitly.
    ///
    /// TODO(design-producing-tasks): wire this method into
    /// `ApproveDesign`'s state-transition handler once that path
    /// exists. Until then, this is exercised only by integration
    /// tests with a hand-rolled caller.
    pub fn surface_design_doc_conflict_on_approve(
        &self,
        project_id: &str,
        execution_id: &str,
        approved_repo_remote_url: Option<&str>,
        approved_branch: Option<&str>,
        approved_path: &str,
    ) -> Result<Option<WorkAttentionItem>> {
        let approved_path = validate_design_doc_path(approved_path)?;
        let approved_repo = canonicalize_design_doc_repo_remote_url(approved_repo_remote_url.map(str::to_owned));
        let approved_branch = normalize_optional_text(approved_branch.map(str::to_owned));

        let conn = self.connect()?;
        let project = query_project(&conn, project_id).require("project", project_id)?;
        let Some(project_path) = project.design_doc_path.clone() else {
            return Ok(None);
        };
        let product = query_product(&conn, &project.product_id).require("product", &project.product_id)?;
        drop(conn);

        let project_repo_effective = project
            .design_doc_repo_remote_url
            .clone()
            .or_else(|| product.repo_remote_url.clone());
        let approved_repo_effective = approved_repo.clone().or_else(|| product.repo_remote_url.clone());

        let project_branch_effective = project.design_doc_branch.clone().unwrap_or_else(|| "main".to_owned());
        let approved_branch_effective = approved_branch.clone().unwrap_or_else(|| "main".to_owned());

        if project_repo_effective == approved_repo_effective
            && project_branch_effective == approved_branch_effective
            && project_path == approved_path
        {
            return Ok(None);
        }

        let title = "Design doc pointer disagrees with approved design".to_owned();
        let body_markdown = format!(
            "The project's design-doc pointer (`{project_repo}` `{project_branch}` `{project_path}`) differs from the location of the approved design doc (`{approved_repo}` `{approved_branch}` `{approved_path}`). Update the project pointer with `boss project set-design-doc` or revoke the approval.",
            project_repo = project_repo_effective.as_deref().unwrap_or("<no repo resolved>"),
            project_branch = project_branch_effective,
            project_path = project_path,
            approved_repo = approved_repo_effective.as_deref().unwrap_or("<no repo resolved>"),
            approved_branch = approved_branch_effective,
            approved_path = approved_path,
        );

        let item = self.create_attention_item(CreateAttentionItemInput {
            execution_id: Some(execution_id.to_owned()),
            work_item_id: None,
            kind: "design_doc_pointer_conflict".to_owned(),
            status: None,
            title,
            body_markdown,
            resolved_at: None,
        })?;
        Ok(Some(item))
    }
}
