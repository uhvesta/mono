use super::*;

/// Actor literal recorded against `project_property_audit` rows
/// produced by CLI / app callers (`SetProjectDesignDoc` RPC). Boss
/// is single-user today (per design Q10), so this is currently the
/// only "human" actor; the field exists so a future multi-user
/// layer can swap in caller identity without a schema change.
pub const AUDIT_ACTOR_HUMAN: &str = "human";

/// Actor literal recorded when the engine's design-doc detector
/// auto-populates an empty project pointer (sync rule 1 of design
/// Q6, via `sync_project_design_doc_from_detector`).
pub const AUDIT_ACTOR_DESIGN_DETECTOR: &str = "engine_design_detector";

/// A single append-only row in the `project_property_audit` table.
/// Records that `actor` changed `property` on `project_id` from
/// `old_value` to `new_value` at `changed_at` (epoch seconds, the
/// same format as `projects.updated_at`).
#[derive(Debug, Clone, PartialEq, Eq, bon::Builder)]
#[builder(on(String, into))]
pub struct ProjectPropertyAuditEntry {
    pub id: String,
    pub project_id: String,
    pub property: String,
    pub old_value: Option<String>,
    pub new_value: Option<String>,
    pub actor: String,
    pub changed_at: String,
}

/// Emit one `project_property_audit` row for each of the three
/// `design_doc_*` columns whose value actually changed between
/// `before` and `after`. No-op when nothing changed (e.g. an
/// `unset = true` call on a project that was already unset, or a
/// branch-only edit that matched the existing branch). Runs inside
/// the caller's transaction so the audit row commits with the
/// underlying write.
pub(crate) fn record_design_doc_audit(
    conn: &Connection,
    project_id: &str,
    before: &Project,
    after: &Project,
    actor: &str,
    now: &str,
) -> Result<()> {
    let columns: [(&str, &Option<String>, &Option<String>); 3] = [
        (
            "design_doc_repo_remote_url",
            &before.design_doc_repo_remote_url,
            &after.design_doc_repo_remote_url,
        ),
        ("design_doc_branch", &before.design_doc_branch, &after.design_doc_branch),
        ("design_doc_path", &before.design_doc_path, &after.design_doc_path),
    ];
    for (property, old, new) in columns {
        if old == new {
            continue;
        }
        let id = next_id("paud");
        conn.execute(
            "INSERT INTO project_property_audit
                (id, project_id, property, old_value, new_value, actor, changed_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![id, project_id, property, old, new, actor, now],
        )?;
    }
    Ok(())
}

pub(crate) fn next_id(prefix: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let counter = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    format!("{prefix}_{nanos:x}_{counter:x}")
}

pub(crate) fn now_string() -> String {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .to_string()
}

pub(crate) fn normalize_optional_text(value: Option<String>) -> Option<String> {
    value.and_then(|text| {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            None
        } else {
            Some(trimmed.to_owned())
        }
    })
}

/// Canonicalise a product's `worker_branch_prefix`. Trims surrounding
/// whitespace; an empty result becomes `None` (→ engine default
/// `boss/`). A non-empty prefix is guaranteed a single trailing `/`
/// so the branch name `<prefix>exec_<id>` always has a path separator
/// between the configured prefix and the stable `exec_<id>` suffix —
/// callers may write `bduff` or `bduff/` and both land as `bduff/`.
/// This is the only transformation; the prefix is otherwise stored
/// verbatim and prepended literally at branch-name construction.
pub fn canonicalize_worker_branch_prefix(value: Option<String>) -> Option<String> {
    normalize_optional_text(value).map(|prefix| {
        if prefix.ends_with('/') {
            prefix
        } else {
            format!("{prefix}/")
        }
    })
}

/// Validate a caller-supplied `design_doc_path` per design Q8.
///
/// Rules: relative path (no leading `/`), no `..` segments, not
/// blank, must reference a markdown file (`.md` or `.markdown`).
/// Path is trimmed before storage so the column always reflects the
/// canonical form. Callers that want to *clear* the pointer should
/// use `unset = true` on `SetProjectDesignDocInput` instead of
/// passing an empty string here.
pub(crate) fn validate_design_doc_path(path: &str) -> Result<String> {
    let trimmed = path.trim();
    if trimmed.is_empty() {
        bail!("design_doc_path may not be empty (use `unset = true` to clear the pointer)");
    }
    if trimmed.starts_with('/') {
        bail!("design_doc_path must be repo-relative (no leading `/`): {trimmed}");
    }
    if trimmed.split('/').any(|seg| seg == "..") {
        bail!("design_doc_path may not contain `..` segments: {trimmed}");
    }
    // Cube workspace paths are ephemeral machine-local locations that
    // become invalid once the workspace is re-leased to a different task.
    // They must never be persisted as a design-doc pointer; GitHub is the
    // durable store. Reject any path that looks like a workspace-relative
    // path escaped into the repo-relative field.
    if trimmed.contains("cube/workspaces/") {
        bail!(
            "design_doc_path must not reference a cube workspace path \
             (contains 'cube/workspaces/'): {trimmed}"
        );
    }
    if !(trimmed.ends_with(".md") || trimmed.ends_with(".markdown")) {
        bail!("design_doc_path must reference a markdown file (.md or .markdown): {trimmed}");
    }
    Ok(trimmed.to_owned())
}

/// Canonicalise a caller-supplied repo remote URL into the same shape
/// stored on `products.repo_remote_url`. Shared between every column
/// that holds a repo URL: product default, task / chore override,
/// project design-doc pointer. Today the canonical form is just
/// `trim + blank→None`; lift to a richer `(scheme, owner, repo, .git)`
/// canonicaliser here when the column grows one — every write site
/// already routes through this function.
pub fn canonicalize_repo_remote_url(value: Option<String>) -> Option<String> {
    normalize_optional_text(value)
}

/// Enforce the repo-override invariant for task / chore inserts.
///
/// Rule: a task row carries `repo_remote_url` only when its parent
/// product has **no** repo of its own (multi-repo products). When the
/// product has a repo, the row must be `NULL`; the resolved repo is
/// always the product's.
///
/// Returns the canonicalised URL to write, or `None` when the product
/// owns the repo. Errors when the caller violates the invariant:
///   - product has a repo AND caller supplied a non-empty override
///   - product has no repo AND caller supplied no repo
pub(crate) fn enforce_task_repo_invariant(product: &Product, input_repo: Option<String>) -> Result<Option<String>> {
    let canonicalized = canonicalize_repo_remote_url(input_repo);
    if let Some(product_repo) = product.repo_remote_url.as_deref() {
        if canonicalized.is_some() {
            bail!(
                "cannot set per-task repo override on product `{}`: \
                 product has its own repo (`{}`). \
                 Clear the product's repo first, or omit --repo to inherit.",
                product.slug,
                product_repo,
            );
        }
        Ok(None)
    } else {
        match canonicalized {
            Some(url) => Ok(Some(url)),
            None => bail!(
                "work item under product `{}` has no repo; \
                 provide one via repo_remote_url (product has no default).",
                product.slug,
            ),
        }
    }
}

/// Thin wrapper kept for the design-doc call sites until they migrate
/// to [`canonicalize_repo_remote_url`] directly.
pub(crate) fn canonicalize_design_doc_repo_remote_url(value: Option<String>) -> Option<String> {
    canonicalize_repo_remote_url(value)
}

/// Build the GitHub web URL for a design doc per the design's Q5
/// recipe (`https://github.com/<owner>/<repo>/blob/<branch>/<path>`).
/// Falls back to a best-effort blob URL when the repo doesn't parse
/// as a `github.com` URL (e.g. an enterprise mirror) so the caller
/// always gets *something* to render — the resolver itself doesn't
/// fail the whole request just because the URL formatter can't pull
/// `owner/repo` out of the remote.
pub(crate) fn render_design_doc_web_url(repo_remote_url: &str, branch: &str, path: &str) -> String {
    match crate::completion::parse_repo_slug(repo_remote_url) {
        Ok(slug) => format!("https://github.com/{slug}/blob/{branch}/{path}"),
        Err(_) => format!("{repo_remote_url}/blob/{branch}/{path}"),
    }
}

/// Build the GitHub raw-content URL for a design doc.
///
/// Format: `https://raw.githubusercontent.com/<owner>/<repo>/<path>?ref=<branch>`
///
/// The branch is carried in `?ref=` rather than embedded as URL path
/// segments. Branch names like `boss/exec_*` contain `/`, which would be
/// split into separate path components when the Swift app parses the URL —
/// `segments[2]` would capture only `boss`, not `boss/exec_…`, causing
/// the GitHub Contents API call to fail with 404. Percent-encoding the
/// slash as `%2F` in the query parameter lets `URLComponents.queryItems`
/// recover the full branch name on the Swift side.
///
/// Returns `None` when the repo URL can't be parsed as a github.com URL
/// (e.g. an enterprise mirror or non-GitHub host) so callers know the
/// raw-content fast path is unavailable and should fall back to the
/// web URL.
pub(crate) fn render_design_doc_raw_content_url(repo_remote_url: &str, branch: &str, path: &str) -> Option<String> {
    // Percent-encode only `/` in branch names. Other characters legal in
    // Git branch names (alphanumeric, `-`, `_`, `.`) are safe in a query
    // string without encoding.
    let encoded_ref = branch.replace('/', "%2F");
    crate::completion::parse_repo_slug(repo_remote_url)
        .ok()
        .map(|slug| format!("https://raw.githubusercontent.com/{slug}/{path}?ref={encoded_ref}"))
}

/// Look up a product by `repo_remote_url`. Used by
/// `resolve_project_design_doc` to classify a resolved repo as
/// `OtherProduct` (Boss tracks it) vs `External` (we don't). Returns
/// `None` when no product matches. `NULL` `repo_remote_url` rows are
/// excluded so a freshly-created product without a URL doesn't
/// silently match the project's pointer.
pub(crate) fn find_product_by_repo_remote_url(conn: &Connection, repo_remote_url: &str) -> Result<Option<String>> {
    conn.query_row(
        "SELECT id FROM products
         WHERE repo_remote_url IS NOT NULL AND repo_remote_url = ?1
         ORDER BY created_at ASC, id ASC
         LIMIT 1",
        [repo_remote_url],
        |row| row.get::<_, String>(0),
    )
    .optional()
    .map_err(Into::into)
}

pub(crate) fn apply_text_patch(target: &mut String, patch: Option<String>) {
    if let Some(value) = patch {
        *target = value;
    }
}

/// Apply a `WorkItemPatch.repo_remote_url` update with the canonical
/// "empty-string clears" wire convention. `None` patch means "leave
/// the column alone." `Some("")` (or any whitespace-only string)
/// means "clear the override / inherit." Otherwise canonicalise and
/// store the value. Shared between product / task / chore update
/// paths so a single rule governs every repo URL column.
pub(crate) fn apply_repo_remote_url_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = canonicalize_repo_remote_url(Some(value));
    }
}

pub(crate) fn apply_optional_patch(target: &mut Option<String>, patch: Option<String>) {
    if let Some(value) = patch {
        *target = normalize_optional_text(Some(value));
    }
}

/// `WorkItemPatch.model_override` / `WorkItemPatch.default_model`
/// share the "empty string clears, otherwise store verbatim" wire
/// shape: `None` leaves the column alone, `Some("")` writes NULL,
/// and `Some(slug)` stores the slug after a trim. Slugs are
/// deliberately not validated — claude is the source of truth on
/// what `--model` accepts (design §Q3).
pub(crate) fn apply_optional_string_patch(target: &mut Option<String>, patch: Option<String>) {
    if patch.is_some() {
        *target = normalize_optional_text(patch);
    }
}

pub(crate) fn task_to_item(task: Task) -> WorkItem {
    if task.kind == TaskKind::Chore {
        WorkItem::Chore(task)
    } else {
        WorkItem::Task(task)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal `Product`, optionally carrying its own repo. The
    /// other fields are irrelevant to the helpers under test — only
    /// `repo_remote_url` and `slug` are inspected by
    /// `enforce_task_repo_invariant`.
    fn product_with_repo(repo: Option<&str>) -> Product {
        Product::builder()
            .id("prod_test")
            .name("Test product")
            .slug("test-product")
            .description("")
            .status("active")
            .created_at("2026-01-01T00:00:00Z")
            .updated_at("2026-01-01T00:00:00Z")
            .maybe_repo_remote_url(repo.map(str::to_owned))
            .build()
    }

    // ---- normalize_optional_text -------------------------------------

    #[test]
    fn normalize_optional_text_none_stays_none() {
        assert_eq!(normalize_optional_text(None), None);
    }

    #[test]
    fn normalize_optional_text_blank_becomes_none() {
        assert_eq!(normalize_optional_text(Some(String::new())), None);
        assert_eq!(normalize_optional_text(Some("   ".to_owned())), None);
        assert_eq!(normalize_optional_text(Some("\t \n".to_owned())), None);
    }

    #[test]
    fn normalize_optional_text_trims_non_empty() {
        assert_eq!(
            normalize_optional_text(Some("  hello  ".to_owned())),
            Some("hello".to_owned())
        );
        assert_eq!(
            normalize_optional_text(Some("hello".to_owned())),
            Some("hello".to_owned())
        );
    }

    // ---- canonicalize_worker_branch_prefix ---------------------------

    #[test]
    fn worker_branch_prefix_adds_trailing_slash() {
        assert_eq!(
            canonicalize_worker_branch_prefix(Some("bduff".to_owned())),
            Some("bduff/".to_owned())
        );
    }

    #[test]
    fn worker_branch_prefix_keeps_existing_trailing_slash() {
        assert_eq!(
            canonicalize_worker_branch_prefix(Some("bduff/".to_owned())),
            Some("bduff/".to_owned())
        );
    }

    #[test]
    fn worker_branch_prefix_trims_then_canonicalises() {
        assert_eq!(
            canonicalize_worker_branch_prefix(Some("  bduff/  ".to_owned())),
            Some("bduff/".to_owned())
        );
        assert_eq!(
            canonicalize_worker_branch_prefix(Some("  bduff  ".to_owned())),
            Some("bduff/".to_owned())
        );
    }

    #[test]
    fn worker_branch_prefix_blank_and_none_are_none() {
        assert_eq!(canonicalize_worker_branch_prefix(None), None);
        assert_eq!(canonicalize_worker_branch_prefix(Some("   ".to_owned())), None);
        assert_eq!(canonicalize_worker_branch_prefix(Some(String::new())), None);
    }

    #[test]
    fn worker_branch_prefix_is_idempotent() {
        for input in ["bduff", "bduff/", "  bduff  ", "team/sub"] {
            let once = canonicalize_worker_branch_prefix(Some(input.to_owned()));
            let twice = canonicalize_worker_branch_prefix(once.clone());
            assert_eq!(once, twice, "not idempotent for {input:?}");
        }
    }

    // ---- validate_design_doc_path ------------------------------------

    #[test]
    fn validate_design_doc_path_accepts_markdown() {
        assert_eq!(validate_design_doc_path("docs/design.md").unwrap(), "docs/design.md");
        assert_eq!(validate_design_doc_path("design.markdown").unwrap(), "design.markdown");
    }

    #[test]
    fn validate_design_doc_path_trims_to_canonical() {
        assert_eq!(
            validate_design_doc_path("  docs/design.md  ").unwrap(),
            "docs/design.md"
        );
    }

    #[test]
    fn validate_design_doc_path_rejects_empty() {
        assert!(validate_design_doc_path("").is_err());
        assert!(validate_design_doc_path("   ").is_err());
    }

    #[test]
    fn validate_design_doc_path_rejects_leading_slash() {
        assert!(validate_design_doc_path("/etc/design.md").is_err());
    }

    #[test]
    fn validate_design_doc_path_rejects_parent_segments() {
        assert!(validate_design_doc_path("../design.md").is_err());
        assert!(validate_design_doc_path("docs/../design.md").is_err());
        // A `..` only counts as a path segment; `..` embedded in a name is fine.
        assert!(validate_design_doc_path("docs/a..b.md").is_ok());
    }

    #[test]
    fn validate_design_doc_path_rejects_cube_workspace_path() {
        assert!(validate_design_doc_path("cube/workspaces/foo/design.md").is_err());
    }

    #[test]
    fn validate_design_doc_path_rejects_non_markdown() {
        assert!(validate_design_doc_path("docs/design.txt").is_err());
        assert!(validate_design_doc_path("docs/design").is_err());
        assert!(validate_design_doc_path("README.mdx").is_err());
    }

    // ---- enforce_task_repo_invariant ---------------------------------

    #[test]
    fn task_repo_invariant_product_repo_plus_override_errs() {
        let product = product_with_repo(Some("git@github.com:org/mono.git"));
        let result = enforce_task_repo_invariant(&product, Some("git@github.com:org/other.git".to_owned()));
        assert!(result.is_err());
    }

    #[test]
    fn task_repo_invariant_product_repo_no_override_inherits() {
        let product = product_with_repo(Some("git@github.com:org/mono.git"));
        assert_eq!(enforce_task_repo_invariant(&product, None).unwrap(), None);
        // A blank override is treated as "no override", not a violation.
        assert_eq!(
            enforce_task_repo_invariant(&product, Some("   ".to_owned())).unwrap(),
            None
        );
    }

    #[test]
    fn task_repo_invariant_no_product_repo_uses_override_canonicalised() {
        let product = product_with_repo(None);
        assert_eq!(
            enforce_task_repo_invariant(&product, Some("  git@github.com:org/mono.git  ".to_owned())).unwrap(),
            Some("git@github.com:org/mono.git".to_owned()),
        );
    }

    #[test]
    fn task_repo_invariant_no_product_repo_no_override_errs() {
        let product = product_with_repo(None);
        assert!(enforce_task_repo_invariant(&product, None).is_err());
        // A whitespace-only override canonicalises to None → still an error.
        assert!(enforce_task_repo_invariant(&product, Some("  ".to_owned())).is_err());
    }

    // ---- render_design_doc_raw_content_url ---------------------------

    #[test]
    fn raw_content_url_github_carries_branch_in_ref() {
        let url = render_design_doc_raw_content_url("git@github.com:org/mono.git", "main", "docs/design.md");
        assert_eq!(
            url,
            Some("https://raw.githubusercontent.com/org/mono/docs/design.md?ref=main".to_owned())
        );
    }

    #[test]
    fn raw_content_url_percent_encodes_slash_in_branch() {
        let url = render_design_doc_raw_content_url("https://github.com/org/mono", "boss/exec_123", "docs/design.md");
        assert_eq!(
            url,
            Some("https://raw.githubusercontent.com/org/mono/docs/design.md?ref=boss%2Fexec_123".to_owned()),
        );
    }

    #[test]
    fn raw_content_url_non_github_is_none() {
        assert_eq!(
            render_design_doc_raw_content_url("git@gitlab.com:org/mono.git", "main", "d.md"),
            None
        );
        assert_eq!(
            render_design_doc_raw_content_url("not a url at all", "main", "d.md"),
            None
        );
    }

    // ---- render_design_doc_web_url -----------------------------------

    #[test]
    fn web_url_github_uses_blob_form() {
        let url = render_design_doc_web_url("git@github.com:org/mono.git", "main", "docs/design.md");
        assert_eq!(url, "https://github.com/org/mono/blob/main/docs/design.md");
    }

    #[test]
    fn web_url_non_github_falls_back_to_remote() {
        let url = render_design_doc_web_url("https://example.com/org/mono", "main", "docs/design.md");
        assert_eq!(url, "https://example.com/org/mono/blob/main/docs/design.md");
    }
}
