//! Design-doc auto-population from PR file scans.
//!
//! When a `kind=design` task's worker opens a PR, or that PR merges,
//! the engine scans the PR's changed files for a single markdown file
//! under any product's `docs/designs/` directory (e.g.
//! `tools/boss/docs/designs/` or `tools/checkleft/docs/designs/`). If
//! exactly one match is found, it becomes the project's design-doc pointer.
//! Zero matches or multiple matches skip auto-population with a logged warning.
//!
//! Two entry points, called from their respective trigger modules:
//!
//! - [`on_design_pr_detected`] — fired when `tasks.pr_url` is set for
//!   a `kind=design` task (the `in_review` transition). Populates the
//!   project's design-doc pointer when it is unset, or updates only
//!   `design_doc_branch` when it is already set. Uses the PR's **head**
//!   branch (e.g. `boss/exec_*`) so the viewer can fetch the doc while
//!   the PR is still open. The `raw_content_url` builder percent-encodes
//!   `/` as `%2F` in the `?ref=` query param so slashed branch names
//!   round-trip correctly through the Swift URL parser.
//! - [`on_design_pr_merged`] — fired when `mark_chore_pr_merged`
//!   transitions a `kind=design` task to `done`. If the project
//!   already has a path, only the branch is updated to the PR's base
//!   branch (typically `main`). If the project has no path yet, the
//!   full pointer is written.

use std::process::Stdio;

use anyhow::{Context, Result};
use tokio::process::Command;

use crate::work::WorkDb;
use boss_protocol::SetProjectDesignDocInput;

/// Metadata extracted from `gh pr view --json files,headRefName,baseRefName`.
pub(crate) struct PrScanResult {
    /// The single design-doc path found in the PR, or `None` if zero
    /// or multiple design docs were present.
    pub(crate) doc_path: Option<String>,
    /// Head branch name (e.g. `boss/exec_18b07a506d2518d0_1b`).
    pub(crate) head_ref_name: Option<String>,
    /// Base branch name (e.g. `main`).
    pub(crate) base_ref_name: Option<String>,
}

/// Fired by `completion::finalize_pr_transition` (target = `InReview`)
/// when the work item is `kind=design` with a `project_id`.
///
/// Scans the PR's changed files for a design-doc markdown file under any
/// product's `docs/designs/` directory. On a single match, populates (or updates)
/// the project's design-doc pointer using the PR's **head** branch so
/// the in-app viewer can fetch the doc from the PR branch while the PR
/// is still open. The `raw_content_url` builder percent-encodes `/` as
/// `%2F` in `?ref=` so slashed branch names like `boss/exec_*` round-trip
/// correctly through `parseRawContentURL` in the Swift app and reach
/// the GitHub Contents API as a proper query parameter.
///
/// [`WorkDb::sync_project_design_doc_from_detector`] is used for the
/// initial (pointer-is-NULL) case; it is a no-op when the path is already
/// set, at which point only `design_doc_branch` is updated.
pub async fn on_design_pr_detected(
    work_db: &WorkDb,
    task_id: &str,
    product_id: &str,
    project_id: &str,
    pr_url: &str,
) {
    let scan = match scan_pr(task_id, pr_url).await {
        Some(s) => s,
        None => return,
    };
    let Some(path) = scan.doc_path else {
        return;
    };
    let repo_remote_url = resolve_product_repo(work_db, task_id, product_id);
    // Use the head branch (e.g. `boss/exec_*`) so the in-app viewer can
    // fetch the doc from the PR branch while the PR is still open. The
    // raw_content_url builder encodes `/` as `%2F` in `?ref=` so slashed
    // branch names round-trip correctly through the Swift URL parser.
    let head_ref_name = scan.head_ref_name;
    let branch = head_ref_name.as_deref();
    // Captured for the cross-doc comment migration below; the Ok(false)
    // arm of the match moves `head_ref_name`.
    let migration_branch = head_ref_name.clone();
    let migration_repo = repo_remote_url.clone();
    let migration_path = path.clone();
    match work_db.sync_project_design_doc_from_detector(
        project_id,
        repo_remote_url.as_deref(),
        branch,
        &path,
    ) {
        Ok(true) => {
            tracing::info!(
                task_id,
                project_id,
                pr_url,
                path,
                branch,
                "design detector: populated project design-doc pointer (in_review)"
            );
        }
        Ok(false) => {
            // Path was already set — update design_doc_branch to the PR head
            // branch so the in-app viewer fetches from the live PR branch
            // while the PR is still open.
            if let Some(head_branch) = head_ref_name {
                let input = SetProjectDesignDocInput {
                    project_id: project_id.to_owned(),
                    design_doc_path: None,
                    design_doc_branch: Some(head_branch.clone()),
                    design_doc_repo_remote_url: None,
                    unset: false,
                };
                match work_db.set_project_design_doc(input) {
                    Ok(_) => {
                        tracing::info!(
                            task_id,
                            project_id,
                            pr_url,
                            branch = head_branch,
                            "design detector: updated design-doc branch to PR head branch (in_review)"
                        );
                    }
                    Err(err) => {
                        tracing::warn!(
                            task_id,
                            project_id,
                            pr_url,
                            ?err,
                            "design detector: failed to update design-doc branch to PR head branch"
                        );
                    }
                }
            } else {
                tracing::debug!(
                    task_id,
                    project_id,
                    pr_url,
                    "design detector: project already has a design-doc pointer, head branch unknown; skipping (in_review)"
                );
            }
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                project_id,
                pr_url,
                ?err,
                "design detector: failed to write design-doc pointer (in_review)"
            );
        }
    }

    // Cross-doc comment migration (design § "Comments on PR-backed docs").
    // Re-key the work item's active comments to the new
    // `pr_doc:<repo>:<branch>:<path>` artifact so they travel with the doc;
    // the originals are soft-resolved for the trail. Re-anchoring against the
    // PR's doc text is deferred to the renderer's next load (the engine can't
    // render markdown to plain text), so `new_plain_text` is `None`. No-op
    // when the work item has no active comments. Idempotent on repeated
    // `in_review` polls: once migrated the originals are `resolved`, so a
    // later call finds nothing active to migrate.
    if let (Some(repo), Some(branch)) = (migration_repo.as_deref(), migration_branch.as_deref()) {
        let artifact_id = format!("pr_doc:{repo}:{branch}:{migration_path}");
        let config = crate::comments_anchor::CommentFuzzyConfig::from_env();
        match work_db.migrate_work_item_comments_to_pr_doc(task_id, &artifact_id, None, 0, &config) {
            Ok(n) if n > 0 => tracing::info!(
                task_id,
                project_id,
                artifact_id,
                migrated = n,
                "design detector: migrated work-item comments to pr_doc artifact (in_review)"
            ),
            Ok(_) => {}
            Err(err) => tracing::warn!(
                task_id,
                project_id,
                ?err,
                "design detector: failed to migrate work-item comments to pr_doc artifact"
            ),
        }
    }
}

/// Fired by `merge_poller::mark_merged` when the work item is
/// `kind=design` with a `project_id`.
///
/// Behaviour depends on whether the project's `design_doc_path` is
/// already set:
///
/// - **Path already set** (from the in_review detector or a prior
///   manual edit): update only `design_doc_branch` to `base_ref_name`
///   (typically `"main"`), so consumers know the doc is now on the
///   default branch. The path and repo URL are left unchanged.
/// - **Path not yet set**: scan the PR files and write the full
///   pointer with `branch = base_ref_name`.
///
/// Uses [`WorkDb::set_project_design_doc`] (last-writer-wins) in both
/// branches because the doc is now on main — this is authoritative.
pub async fn on_design_pr_merged(
    work_db: &WorkDb,
    task_id: &str,
    product_id: &str,
    project_id: &str,
    pr_url: &str,
    base_ref_name: Option<&str>,
) {
    // Check whether the project already has a design-doc path set.
    let existing_path = match work_db.get_project(project_id) {
        Ok(project) => project.design_doc_path,
        Err(err) => {
            tracing::warn!(
                task_id,
                project_id,
                ?err,
                "design detector: failed to fetch project for merge update"
            );
            return;
        }
    };

    if let Some(path) = existing_path {
        // Path already set — update only the branch to main.
        let input = SetProjectDesignDocInput {
            project_id: project_id.to_owned(),
            design_doc_path: None, // keep existing path
            design_doc_branch: base_ref_name.map(str::to_owned),
            design_doc_repo_remote_url: None, // keep existing repo
            unset: false,
        };
        match work_db.set_project_design_doc(input) {
            Ok(_) => {
                tracing::info!(
                    task_id,
                    project_id,
                    pr_url,
                    path,
                    branch = base_ref_name,
                    "design detector: updated design-doc branch to main after merge"
                );
            }
            Err(err) => {
                tracing::warn!(
                    task_id,
                    project_id,
                    pr_url,
                    ?err,
                    "design detector: failed to update design-doc branch after merge"
                );
            }
        }
        return;
    }

    // Path not set — scan the PR files and write the full pointer.
    let scan = match scan_pr(task_id, pr_url).await {
        Some(s) => s,
        None => return,
    };
    let Some(path) = scan.doc_path else {
        return;
    };
    let repo_remote_url = resolve_product_repo(work_db, task_id, product_id);
    let effective_branch = base_ref_name
        .or(scan.base_ref_name.as_deref())
        .map(str::to_owned);

    let input = SetProjectDesignDocInput {
        project_id: project_id.to_owned(),
        design_doc_path: Some(path.clone()),
        design_doc_branch: effective_branch.clone(),
        design_doc_repo_remote_url: repo_remote_url,
        unset: false,
    };
    match work_db.set_project_design_doc(input) {
        Ok(_) => {
            tracing::info!(
                task_id,
                project_id,
                pr_url,
                path,
                branch = effective_branch.as_deref(),
                "design detector: populated project design-doc pointer after merge"
            );
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                project_id,
                pr_url,
                ?err,
                "design detector: failed to write design-doc pointer after merge"
            );
        }
    }
}

/// Resolve the repo_remote_url for a product, returning `None` if the
/// product is not found or has no repo (causes the design-doc pointer
/// to fall back to the product default on resolution).
fn resolve_product_repo(work_db: &WorkDb, task_id: &str, product_id: &str) -> Option<String> {
    match work_db.get_product(product_id) {
        Ok(Some(product)) => product.repo_remote_url,
        Ok(None) => {
            tracing::warn!(
                task_id,
                product_id,
                "design detector: product not found; design-doc repo_remote_url will be null"
            );
            None
        }
        Err(err) => {
            tracing::warn!(
                task_id,
                product_id,
                ?err,
                "design detector: failed to fetch product; design-doc repo_remote_url will be null"
            );
            None
        }
    }
}

/// Call `gh pr view <pr_url> --json files,headRefName,baseRefName` and
/// parse the result. `head_ref_name` carries the PR branch for open PRs;
/// `base_ref_name` carries the target branch used on merge. Returns `None`
/// on tool failures; warnings are logged internally.
pub(crate) async fn scan_pr(task_id: &str, pr_url: &str) -> Option<PrScanResult> {
    match do_scan_pr(pr_url).await {
        Ok(result) => Some(result),
        Err(err) => {
            tracing::warn!(
                task_id,
                pr_url,
                ?err,
                "design detector: failed to scan PR files"
            );
            None
        }
    }
}

async fn do_scan_pr(pr_url: &str) -> Result<PrScanResult> {
    let output = Command::new("gh")
        .args([
            "pr",
            "view",
            pr_url,
            "--json",
            "files,headRefName,baseRefName",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `gh pr view {pr_url}`"))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!(
            "`gh pr view {pr_url} --json files,headRefName,baseRefName` failed: {}",
            stderr.trim()
        );
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    let root: serde_json::Value = serde_json::from_str(&stdout)
        .with_context(|| format!("failed to parse `gh pr view {pr_url}` JSON"))?;

    Ok(parse_pr_scan(&root))
}

/// Pure parse of the `gh pr view --json files,headRefName,baseRefName`
/// JSON into a [`PrScanResult`]. Kept separate from [`do_scan_pr`] so the
/// gh shell-out stays in `do_scan_pr` and the selection/extraction logic
/// is unit-testable without invoking `gh`.
///
/// - `head_ref_name`/`base_ref_name`: the corresponding string fields, with
///   missing keys and empty strings both mapped to `None`.
/// - `doc_path`: the single design-doc path among `files[].path` (per
///   [`is_design_doc_path`]). Zero or multiple matches yield `None`. A
///   missing or non-array `files` key is treated as zero matches.
pub(crate) fn parse_pr_scan(root: &serde_json::Value) -> PrScanResult {
    let head_ref_name = root
        .get("headRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let base_ref_name = root
        .get("baseRefName")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);

    let matches: Vec<String> = root
        .get("files")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|f| f.get("path").and_then(|p| p.as_str()).map(str::to_owned))
                .filter(|p| is_design_doc_path(p))
                .collect()
        })
        .unwrap_or_default();

    let doc_path = match matches.len() {
        1 => Some(matches.into_iter().next().unwrap()),
        0 => {
            tracing::warn!(
                "design detector: no `docs/designs/*.md` file in PR changed files; \
                 design-doc pointer not updated — add the file and re-push, or set \
                 manually with `boss project set-design-doc`"
            );
            None
        }
        n => {
            tracing::warn!(
                count = n,
                "design detector: multiple `docs/designs/*.md` files in PR; \
                 skipping auto-populate — use `boss project set-design-doc` to resolve"
            );
            None
        }
    };

    PrScanResult {
        doc_path,
        head_ref_name,
        base_ref_name,
    }
}

/// Return `true` when `path` is a direct child of any `docs/designs/`
/// directory, regardless of the leading product prefix.  For example:
/// - `tools/boss/docs/designs/foo.md`        → true
/// - `tools/checkleft/docs/designs/foo.md`   → true
/// - `docs/designs/foo.md`                   → true
/// - `tools/boss/docs/designs/sub/foo.md`    → false (sub-directory)
/// - `tools/boss/docs/other/foo.md`          → false (wrong segment)
fn is_design_doc_path(path: &str) -> bool {
    // Locate `docs/designs/` preceded by `/` or at the very start.
    let rest = if let Some(rest) = path.strip_prefix("docs/designs/") {
        rest
    } else if let Some((_, rest)) = path.split_once("/docs/designs/") {
        rest
    } else {
        return false;
    };
    // Only direct children — no sub-directories.
    !rest.contains('/') && (rest.ends_with(".md") || rest.ends_with(".markdown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_doc_path_matches_direct_child() {
        // Boss product directory.
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/my-feature.md"
        ));
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md"
        ));
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/x.markdown"
        ));
        // Non-boss product directories are also accepted (regression for P844).
        assert!(is_design_doc_path(
            "tools/checkleft/docs/designs/robust-change-detection-in-checkleft.md"
        ));
        assert!(is_design_doc_path(
            "tools/flunge/docs/designs/flunge-auth.md"
        ));
        // Root-level docs/designs/ (no product prefix).
        assert!(is_design_doc_path("docs/designs/top-level.md"));
    }

    #[test]
    fn design_doc_path_rejects_subdirectory() {
        // Only direct children of designs/ are matched.
        assert!(!is_design_doc_path(
            "tools/boss/docs/designs/sub/doc.md"
        ));
        assert!(!is_design_doc_path(
            "tools/checkleft/docs/designs/sub/doc.md"
        ));
    }

    #[test]
    fn design_doc_path_rejects_wrong_segment() {
        assert!(!is_design_doc_path("tools/boss/docs/other/doc.md"));
        assert!(!is_design_doc_path("README.md"));
        // `prodocs/designs/` does NOT contain `/docs/designs/` as a
        // proper segment, so it must be rejected.
        assert!(!is_design_doc_path("prodocs/designs/doc.md"));
    }

    #[test]
    fn design_doc_path_rejects_non_markdown() {
        assert!(!is_design_doc_path("tools/boss/docs/designs/doc.txt"));
        assert!(!is_design_doc_path("tools/boss/docs/designs/doc.rs"));
        assert!(!is_design_doc_path(
            "tools/checkleft/docs/designs/doc.txt"
        ));
    }

    /// Build a `files` array value from a list of paths, shaped like
    /// `gh pr view --json files` output (`[{"path": "..."}, ...]`).
    fn files_json(paths: &[&str]) -> serde_json::Value {
        serde_json::Value::Array(
            paths
                .iter()
                .map(|p| serde_json::json!({ "path": p }))
                .collect(),
        )
    }

    #[test]
    fn parse_pr_scan_single_design_doc_is_adopted() {
        let root = serde_json::json!({
            "files": files_json(&[
                "tools/boss/src/main.rs",
                "tools/boss/docs/designs/my-feature.md",
                "README.md",
            ]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(
            scan.doc_path.as_deref(),
            Some("tools/boss/docs/designs/my-feature.md")
        );
    }

    #[test]
    fn parse_pr_scan_zero_design_docs_yields_none() {
        let root = serde_json::json!({
            "files": files_json(&[
                "tools/boss/src/main.rs",
                "README.md",
                "tools/boss/docs/other/notes.md",
            ]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.doc_path, None);
    }

    #[test]
    fn parse_pr_scan_multiple_design_docs_is_ambiguous_none() {
        let root = serde_json::json!({
            "files": files_json(&[
                "tools/boss/docs/designs/feature-a.md",
                "tools/checkleft/docs/designs/feature-b.md",
            ]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.doc_path, None);
    }

    #[test]
    fn parse_pr_scan_excludes_subdir_and_non_markdown_from_match_set() {
        // A subdirectory entry and a non-markdown entry under docs/designs/
        // must NOT count toward the match set, so the single direct-child
        // markdown file remains the unambiguous adoption.
        let root = serde_json::json!({
            "files": files_json(&[
                "tools/boss/docs/designs/sub/nested.md",
                "tools/boss/docs/designs/diagram.png",
                "tools/boss/docs/designs/real-doc.md",
            ]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(
            scan.doc_path.as_deref(),
            Some("tools/boss/docs/designs/real-doc.md")
        );
    }

    #[test]
    fn parse_pr_scan_excludes_count_make_match_unambiguous() {
        // With only excluded (subdir / non-markdown) entries present and no
        // direct-child markdown, the match set is empty -> None.
        let root = serde_json::json!({
            "files": files_json(&[
                "tools/boss/docs/designs/sub/nested.md",
                "tools/boss/docs/designs/diagram.png",
            ]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.doc_path, None);
    }

    #[test]
    fn parse_pr_scan_extracts_present_ref_names() {
        let root = serde_json::json!({
            "files": files_json(&[]),
            "headRefName": "boss/exec_18b07a506d2518d0_1b",
            "baseRefName": "main",
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(
            scan.head_ref_name.as_deref(),
            Some("boss/exec_18b07a506d2518d0_1b")
        );
        assert_eq!(scan.base_ref_name.as_deref(), Some("main"));
    }

    #[test]
    fn parse_pr_scan_missing_ref_keys_yield_none() {
        let root = serde_json::json!({
            "files": files_json(&[]),
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.head_ref_name, None);
        assert_eq!(scan.base_ref_name, None);
    }

    #[test]
    fn parse_pr_scan_empty_ref_strings_are_filtered_to_none() {
        let root = serde_json::json!({
            "files": files_json(&[]),
            "headRefName": "",
            "baseRefName": "",
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.head_ref_name, None);
        assert_eq!(scan.base_ref_name, None);
    }

    #[test]
    fn parse_pr_scan_missing_files_key_yields_none_without_panic() {
        let root = serde_json::json!({
            "headRefName": "feature",
            "baseRefName": "main",
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.doc_path, None);
        // Ref extraction still works with files absent.
        assert_eq!(scan.head_ref_name.as_deref(), Some("feature"));
        assert_eq!(scan.base_ref_name.as_deref(), Some("main"));
    }

    #[test]
    fn parse_pr_scan_non_array_files_key_yields_none_without_panic() {
        // `files` present but not an array -> treated as zero matches.
        let root = serde_json::json!({
            "files": "not-an-array",
        });
        let scan = parse_pr_scan(&root);
        assert_eq!(scan.doc_path, None);
    }
}
