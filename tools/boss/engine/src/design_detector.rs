//! Design-doc auto-population from PR file scans.
//!
//! When a `kind=design` task's worker opens a PR, or that PR merges,
//! the engine scans the PR's changed files for a single markdown file
//! under `tools/boss/docs/designs/`. If exactly one match is found,
//! it becomes the project's design-doc pointer. Zero matches or
//! multiple matches skip auto-population with a logged warning.
//!
//! Two entry points, called from their respective trigger modules:
//!
//! - [`on_design_pr_detected`] — fired when `tasks.pr_url` is set for
//!   a `kind=design` task (the `in_review` transition). Uses
//!   [`WorkDb::sync_project_design_doc_from_detector`], which is a
//!   no-op when the project already has a design-doc pointer.
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

const DESIGN_DOC_PREFIX: &str = "tools/boss/docs/designs/";

/// Metadata extracted from `gh pr view --json files,headRefName,baseRefName`.
struct PrScanResult {
    /// The single design-doc path found in the PR, or `None` if zero
    /// or multiple design docs were present.
    doc_path: Option<String>,
    /// Head branch name (e.g. `design-boss-ci-buildkite`).
    head_ref_name: Option<String>,
    /// Base branch name (e.g. `main`).
    base_ref_name: Option<String>,
}

/// Fired by `completion::finalize_pr_transition` (target = `InReview`)
/// when the work item is `kind=design` with a `project_id`.
///
/// Scans the PR's changed files for a design-doc markdown file under
/// `tools/boss/docs/designs/`. On a single match, calls
/// [`WorkDb::sync_project_design_doc_from_detector`] which populates
/// the project's pointer only when it was previously `NULL` — existing
/// human-set or previously-detected values are preserved.
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
    let branch = scan.head_ref_name.as_deref();
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
            tracing::debug!(
                task_id,
                project_id,
                pr_url,
                "design detector: project already has a design-doc pointer; skipping (in_review)"
            );
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
        .or_else(|| scan.base_ref_name.as_deref())
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
/// parse the result. Returns `None` on tool failures; warnings are
/// logged internally.
async fn scan_pr(task_id: &str, pr_url: &str) -> Option<PrScanResult> {
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
                pr_url,
                "design detector: no `tools/boss/docs/designs/*.md` file in PR changed files; \
                 design-doc pointer not updated — add the file and re-push, or set \
                 manually with `boss project set-design-doc`"
            );
            None
        }
        n => {
            tracing::warn!(
                pr_url,
                count = n,
                "design detector: multiple `tools/boss/docs/designs/*.md` files in PR; \
                 skipping auto-populate — use `boss project set-design-doc` to resolve"
            );
            None
        }
    };

    Ok(PrScanResult {
        doc_path,
        head_ref_name,
        base_ref_name,
    })
}

fn is_design_doc_path(path: &str) -> bool {
    let rest = match path.strip_prefix(DESIGN_DOC_PREFIX) {
        Some(r) => r,
        None => return false,
    };
    // Only match direct children (no sub-directories).
    !rest.contains('/') && (rest.ends_with(".md") || rest.ends_with(".markdown"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn design_doc_path_matches_direct_child() {
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/my-feature.md"
        ));
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/boss-ci-buildkite-pipeline-mirroring-flunge.md"
        ));
        assert!(is_design_doc_path(
            "tools/boss/docs/designs/x.markdown"
        ));
    }

    #[test]
    fn design_doc_path_rejects_subdirectory() {
        // Only direct children of designs/ are matched.
        assert!(!is_design_doc_path(
            "tools/boss/docs/designs/sub/doc.md"
        ));
    }

    #[test]
    fn design_doc_path_rejects_wrong_prefix() {
        assert!(!is_design_doc_path("tools/boss/docs/other/doc.md"));
        assert!(!is_design_doc_path("README.md"));
        assert!(!is_design_doc_path("docs/designs/doc.md"));
    }

    #[test]
    fn design_doc_path_rejects_non_markdown() {
        assert!(!is_design_doc_path("tools/boss/docs/designs/doc.txt"));
        assert!(!is_design_doc_path("tools/boss/docs/designs/doc.rs"));
    }
}
