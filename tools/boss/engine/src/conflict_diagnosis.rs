//! Pre-collected conflict context for escalated rebase chores.
//!
//! Implements §Q11 of `tools/boss/docs/designs/auto-rebase-stacked-prs.md`
//! (chores 4a + 4b). When a rebase fails with conflicts the engine
//! gathers a structured diagnosis here — conflicted-file list with
//! per-file shape, dependent commits in the rebased range, and the
//! intersection of the merged PR's file footprint with the conflicted
//! set — then renders the worker chore description from it.
//!
//! Why this lives in its own module: the auto-rebase pipeline (the
//! parent design's chore #4) is not yet merged. The diagnosis
//! collector and template are useful even in v1's "manual conflict
//! chore" path: a `Fix merge conflicts on PR #N` chore creator can
//! call [`collect_conflict_diagnosis`] against an already-conflicted
//! workspace and embed the rendered description in the new chore.
//! When the auto-rebase pipeline lands it will plug in here directly.
//!
//! The data shape is intentionally `serde`-friendly so that, once the
//! `rebase_attempts.conflict_diagnosis` JSON column lands, the same
//! struct can be persisted verbatim.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use anyhow::{Context, Result, bail};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::process::Command;

/// Captured before-and-after state for a single conflicted file in
/// the dependent's rebased range.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictedFile {
    /// Repo-relative path.
    pub path: String,
    /// Number of `<<<<<<<` lines in the file (jj uses git-style
    /// markers in the working copy when conflicts surface).
    pub marker_count: u32,
    /// Total line count of the captured file.
    pub total_lines: u32,
    /// Whether this file was also touched by the base (merged) PR.
    /// True ↔ the file appears in the merged PR's `gh pr view --json
    /// files` set. This is the "upstream footprint intersection"
    /// from Q11.
    pub in_upstream_footprint: bool,
    /// Captured text — either the whole file (small) or a window
    /// around the first conflict marker (large). The
    /// [`ConflictBlock`] enum tells the renderer which it has.
    pub block: ConflictBlock,
}

/// Either the full file (when small) or a windowed excerpt around
/// the first conflict marker.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConflictBlock {
    /// File was within the inline budget — content is the whole file.
    Full { content: String },
    /// File was over the inline budget — content is a window.
    Excerpt {
        content: String,
        /// 1-based line of the first captured line in the original
        /// file (so the worker can correlate the excerpt back).
        window_start_line: u32,
        /// Number of lines we kept.
        captured_lines: u32,
    },
}

impl ConflictBlock {
    pub fn content(&self) -> &str {
        match self {
            ConflictBlock::Full { content } => content,
            ConflictBlock::Excerpt { content, .. } => content,
        }
    }
}

/// One commit on the dependent side that's in the rebased range
/// (`main..<dependent-bookmark>`).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct DependentCommit {
    pub change_id: String,
    pub description_first_line: String,
}

/// The complete diagnosis. Round-trips through serde so the engine
/// can persist it verbatim into `rebase_attempts.conflict_diagnosis`
/// (or for v1 just embed the rendered description from it directly).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ConflictDiagnosis {
    pub dependent_pr_number: u64,
    pub dependent_pr_url: String,
    pub dependent_branch: String,
    pub base_pr_number: u64,
    pub base_pr_url: String,
    pub base_branch: String,
    /// ISO8601, when known. `gh pr view --json mergedAt` is the
    /// source. Optional because the manual-chore path may not have
    /// it on hand.
    pub base_merged_at: Option<String>,
    /// `<owner>/<repo>` — used in the rendered playbook for
    /// `gh pr edit --repo`.
    pub repo: String,
    pub conflicted_files: Vec<ConflictedFile>,
    pub dependent_commits: Vec<DependentCommit>,
    /// Every file the merged base PR touched. Stored separately
    /// from `conflicted_files[].in_upstream_footprint` so that
    /// downstream consumers can show "upstream PR also touched X
    /// files (3 of them overlap with our conflicts)".
    pub base_pr_footprint: Vec<String>,
    /// Test command the worker is expected to run before pushing.
    /// `None` means the rendered playbook omits the test step.
    pub test_command: Option<String>,
}

impl ConflictDiagnosis {
    /// The `conflicted_files` paths that are also in
    /// `base_pr_footprint`. Convenience for renderers / tests.
    pub fn footprint_intersection(&self) -> Vec<&str> {
        self.conflicted_files
            .iter()
            .filter(|f| f.in_upstream_footprint)
            .map(|f| f.path.as_str())
            .collect()
    }
}

/// Inputs the collector needs that aren't discoverable from the
/// workspace alone. Filled by whoever triggers the diagnosis
/// (auto-rebase pipeline once it lands; the manual conflict chore
/// creator until then).
#[derive(Debug, Clone)]
pub struct DiagnosticInputs {
    pub workspace_path: PathBuf,
    pub dependent_pr_number: u64,
    pub dependent_pr_url: String,
    pub dependent_branch: String,
    pub base_pr_number: u64,
    pub base_pr_url: String,
    pub base_branch: String,
    pub base_merged_at: Option<String>,
    /// `<owner>/<repo>`, e.g. `brianduff/mono`.
    pub repo: String,
    pub test_command: Option<String>,
}

/// Inline budget: files at or under this line count get embedded in
/// full; bigger files are excerpted. Lines, not bytes — keeps the
/// embed predictable.
const FULL_FILE_LINE_BUDGET: u32 = 200;
/// Excerpt window for over-budget files (Q11: "first 60 lines around
/// the first marker"). We center the window on the first marker so
/// the worker sees ~30 lines of context on either side.
const EXCERPT_WINDOW_LINES: u32 = 60;

/// Conflict-marker prefix. jj's working-copy conflict markers use
/// the git-compatible form when surfaced via `jj st`; jj-native
/// `<<<<<<< Conflict` lines also start with the same seven `<`
/// characters, so a single prefix match catches both.
const CONFLICT_MARKER_PREFIX: &str = "<<<<<<<";

// ---------------------------------------------------------------------------
// Shell wrapper traits — collector talks to jj/gh through these so tests can
// stub them. Production impls below.

/// jj operations the collector needs. None mutate workspace state.
#[async_trait]
pub trait JjCli: Send + Sync {
    /// Read a conflicted file from the workspace. Default is just
    /// reading the on-disk text; we don't go through `jj resolve
    /// --list` because reading the file directly already shows the
    /// merged conflict markers and works on every jj version.
    async fn read_file(&self, cwd: &Path, path: &str) -> Result<String>;

    /// Return the conflicted files in the working copy. Equivalent
    /// to grepping `jj st --no-graph` for `Conflict in <path>`.
    async fn list_conflicted_files(&self, cwd: &Path) -> Result<Vec<String>>;

    /// Commits in the rebased range (`main..<dependent_branch>`),
    /// returning `(change_id, description first line)` per commit.
    async fn log_dependent_range(
        &self,
        cwd: &Path,
        dependent_branch: &str,
    ) -> Result<Vec<DependentCommit>>;
}

/// gh operations the collector needs.
#[async_trait]
pub trait GhCli: Send + Sync {
    /// File paths touched by `repo`'s PR `pr_number`. Wraps
    /// `gh pr view <num> --json files`.
    async fn pr_files(&self, repo: &str, pr_number: u64) -> Result<Vec<String>>;
}

// ---------------------------------------------------------------------------
// Production impls — shell out to jj / gh via tokio::process.

#[derive(Debug, Default, Clone)]
pub struct CommandJjCli;

impl CommandJjCli {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl JjCli for CommandJjCli {
    async fn read_file(&self, cwd: &Path, path: &str) -> Result<String> {
        let abs = cwd.join(path);
        tokio::fs::read_to_string(&abs)
            .await
            .with_context(|| format!("failed to read conflicted file {}", abs.display()))
    }

    async fn list_conflicted_files(&self, cwd: &Path) -> Result<Vec<String>> {
        // `jj st --no-graph` prints a `Conflict in <path>` line per
        // conflicted file in the working copy. We grep that out
        // ourselves — it's stable enough across jj versions for v1.
        let output = run_capture("jj", &["st", "--no-graph"], Some(cwd)).await?;
        let mut files: Vec<String> = output
            .lines()
            .filter_map(parse_jj_status_conflict_line)
            .map(str::to_owned)
            .collect();
        files.sort();
        files.dedup();
        Ok(files)
    }

    async fn log_dependent_range(
        &self,
        cwd: &Path,
        dependent_branch: &str,
    ) -> Result<Vec<DependentCommit>> {
        let revset = format!("main..{dependent_branch}");
        let template = r#"change_id.shortest(8) ++ "\t" ++ description.first_line() ++ "\n""#;
        let output = run_capture(
            "jj",
            &[
                "log",
                "-r",
                revset.as_str(),
                "--no-graph",
                "-T",
                template,
            ],
            Some(cwd),
        )
        .await?;
        Ok(output
            .lines()
            .filter(|l| !l.trim().is_empty())
            .filter_map(|l| {
                let mut parts = l.splitn(2, '\t');
                let change_id = parts.next()?.trim().to_owned();
                let desc = parts.next().unwrap_or("").trim().to_owned();
                if change_id.is_empty() {
                    None
                } else {
                    Some(DependentCommit {
                        change_id,
                        description_first_line: desc,
                    })
                }
            })
            .collect())
    }
}

#[derive(Debug, Default, Clone)]
pub struct CommandGhCli;

impl CommandGhCli {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl GhCli for CommandGhCli {
    async fn pr_files(&self, repo: &str, pr_number: u64) -> Result<Vec<String>> {
        let pr_arg = pr_number.to_string();
        let output = run_capture(
            "gh",
            &[
                "pr",
                "view",
                pr_arg.as_str(),
                "--repo",
                repo,
                "--json",
                "files",
                "--jq",
                ".files[].path",
            ],
            None,
        )
        .await?;
        Ok(output
            .lines()
            .map(str::trim)
            .filter(|l| !l.is_empty())
            .map(str::to_owned)
            .collect())
    }
}

async fn run_capture(program: &str, args: &[&str], cwd: Option<&Path>) -> Result<String> {
    let mut cmd = Command::new(program);
    cmd.args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(cwd) = cwd {
        cmd.current_dir(cwd);
    }
    let output = cmd
        .output()
        .await
        .with_context(|| format!("failed to spawn `{program} {}`", args.join(" ")))?;
    if !output.status.success() {
        bail!(
            "`{program} {}` failed: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// Parse one `jj st --no-graph` line; returns the path if the line
/// reports a conflict. We're conservative: only the explicit
/// `Conflict in <path>` form is matched.
fn parse_jj_status_conflict_line(line: &str) -> Option<&str> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("Conflict in ")?;
    Some(rest.trim())
}

// ---------------------------------------------------------------------------
// Collector

/// Run the diagnostic pass. Mutation-free: the workspace is read but
/// not modified. Errors from individual sub-steps are surfaced as
/// `Err` only when they make the diagnosis empty / useless; missing
/// optional bits (e.g. footprint fetch failure) are logged and
/// substituted with empty data so the rendered chore is still
/// usable.
pub async fn collect_conflict_diagnosis<J, G>(
    inputs: DiagnosticInputs,
    jj: &J,
    gh: &G,
) -> Result<ConflictDiagnosis>
where
    J: JjCli + ?Sized,
    G: GhCli + ?Sized,
{
    let conflicted_paths = jj
        .list_conflicted_files(&inputs.workspace_path)
        .await
        .context("listing conflicted files")?;

    // Footprint fetch is best-effort: if `gh` is offline or the PR
    // is private to us, the diagnosis still ships, just without the
    // intersection annotation. The rendered playbook then omits the
    // "also touched by base PR" notes — that's a minor downgrade,
    // not a failure.
    let base_pr_footprint = match gh.pr_files(&inputs.repo, inputs.base_pr_number).await {
        Ok(files) => files,
        Err(err) => {
            tracing::warn!(
                base_pr = inputs.base_pr_number,
                ?err,
                "conflict_diagnosis: failed to fetch base PR file footprint; continuing without intersection",
            );
            Vec::new()
        }
    };

    let dependent_commits = match jj
        .log_dependent_range(&inputs.workspace_path, &inputs.dependent_branch)
        .await
    {
        Ok(commits) => commits,
        Err(err) => {
            tracing::warn!(
                branch = %inputs.dependent_branch,
                ?err,
                "conflict_diagnosis: failed to list dependent commits; continuing without commit list",
            );
            Vec::new()
        }
    };

    let mut conflicted_files = Vec::with_capacity(conflicted_paths.len());
    for path in &conflicted_paths {
        let raw = match jj.read_file(&inputs.workspace_path, path).await {
            Ok(text) => text,
            Err(err) => {
                // A read failure on one file shouldn't sink the
                // whole diagnosis. Note it and keep going.
                tracing::warn!(
                    path = %path,
                    ?err,
                    "conflict_diagnosis: failed to read conflicted file; skipping",
                );
                continue;
            }
        };
        let in_upstream = base_pr_footprint.iter().any(|f| f == path);
        conflicted_files.push(build_conflicted_file(path, &raw, in_upstream));
    }

    Ok(ConflictDiagnosis {
        dependent_pr_number: inputs.dependent_pr_number,
        dependent_pr_url: inputs.dependent_pr_url,
        dependent_branch: inputs.dependent_branch,
        base_pr_number: inputs.base_pr_number,
        base_pr_url: inputs.base_pr_url,
        base_branch: inputs.base_branch,
        base_merged_at: inputs.base_merged_at,
        repo: inputs.repo,
        conflicted_files,
        dependent_commits,
        base_pr_footprint,
        test_command: inputs.test_command,
    })
}

fn build_conflicted_file(path: &str, raw: &str, in_upstream_footprint: bool) -> ConflictedFile {
    let lines: Vec<&str> = raw.split('\n').collect();
    let total_lines = lines.len() as u32;
    let marker_count = lines
        .iter()
        .filter(|l| l.starts_with(CONFLICT_MARKER_PREFIX))
        .count() as u32;

    let block = if total_lines <= FULL_FILE_LINE_BUDGET {
        ConflictBlock::Full {
            content: raw.to_owned(),
        }
    } else {
        let first_marker_idx = lines
            .iter()
            .position(|l| l.starts_with(CONFLICT_MARKER_PREFIX))
            .unwrap_or(0) as u32;
        let half = EXCERPT_WINDOW_LINES / 2;
        let start = first_marker_idx.saturating_sub(half);
        let end = (start + EXCERPT_WINDOW_LINES).min(total_lines);
        let captured = &lines[start as usize..end as usize];
        ConflictBlock::Excerpt {
            content: captured.join("\n"),
            window_start_line: start + 1,
            captured_lines: captured.len() as u32,
        }
    };

    ConflictedFile {
        path: path.to_owned(),
        marker_count,
        total_lines,
        in_upstream_footprint,
        block,
    }
}

// ---------------------------------------------------------------------------
// Renderer (chore #4b)

/// Produce the escalated-chore Markdown description from a
/// diagnosis. Output follows the Q11 template; readers / golden
/// tests pin the structure rather than every byte of whitespace.
pub fn render_chore_description(diag: &ConflictDiagnosis) -> String {
    let mut out = String::new();
    out.push_str("## Auto-rebase escalated to manual conflict resolution\n\n");

    out.push_str(&format!(
        "**Dependent PR**: [#{n}]({url}) — `{br}`\n",
        n = diag.dependent_pr_number,
        url = diag.dependent_pr_url,
        br = diag.dependent_branch,
    ));
    let merged_suffix = match &diag.base_merged_at {
        Some(t) => format!(" (merged {t})"),
        None => String::new(),
    };
    out.push_str(&format!(
        "**Base PR (merged)**: [#{n}]({url}) — `{br}`{suffix}\n",
        n = diag.base_pr_number,
        url = diag.base_pr_url,
        br = diag.base_branch,
        suffix = merged_suffix,
    ));
    out.push_str(
        "**Workspace**: pre-loaded with the failed `jj rebase -d main` state. \
         `jj st` shows the conflict markers; do not re-run `jj rebase`.\n\n",
    );

    out.push_str(&format!(
        "### Conflicted files ({})\n\n",
        diag.conflicted_files.len()
    ));
    if diag.conflicted_files.is_empty() {
        out.push_str("_No conflicted files reported. The workspace may have been resolved already._\n\n");
    } else {
        for f in &diag.conflicted_files {
            render_conflicted_file(&mut out, f, diag.base_pr_number);
        }
    }

    let intersection = diag.footprint_intersection();
    out.push_str(&format!(
        "### Upstream footprint\n\n\
         Base PR #{base} touched {total} file(s); {overlap} of them overlap \
         with this PR's conflicts.\n",
        base = diag.base_pr_number,
        total = diag.base_pr_footprint.len(),
        overlap = intersection.len(),
    ));
    if !intersection.is_empty() {
        out.push_str("\nOverlapping files:\n");
        for path in &intersection {
            out.push_str(&format!("- `{path}`\n"));
        }
    }
    out.push('\n');

    out.push_str(&format!(
        "### Dependent commits in flight ({})\n\n",
        diag.dependent_commits.len()
    ));
    if diag.dependent_commits.is_empty() {
        out.push_str(
            "_No commits found in `main..<branch>` — the diagnosis collector \
             could not enumerate the dependent's work; investigate manually._\n\n",
        );
    } else {
        for c in &diag.dependent_commits {
            out.push_str(&format!(
                "- `{id}` \"{desc}\"\n",
                id = c.change_id,
                desc = escape_md_quote(&c.description_first_line),
            ));
        }
        out.push('\n');
    }

    out.push_str("### Suggested approach\n\n");
    out.push_str("1. Run `jj st` to confirm the workspace state.\n");
    out.push_str(
        "2. For each conflicted file, run `jj resolve <file>`. The base PR's \
         intent (see the diff at the merged-PR URL above) and the dependent \
         PR's intent are both visible in the conflict block.\n",
    );
    let next_step = match &diag.test_command {
        Some(cmd) => {
            out.push_str(&format!(
                "3. Once all files resolve, run `{cmd}` to verify nothing \
                 broke. Iterate until green.\n"
            ));
            4
        }
        None => 3,
    };
    out.push_str(&format!(
        "{n}. `jj git push --bookmark {br}`\n",
        n = next_step,
        br = diag.dependent_branch,
    ));
    out.push_str(&format!(
        "{n}. `gh pr edit {pr} --base main --repo {repo}`\n",
        n = next_step + 1,
        pr = diag.dependent_pr_number,
        repo = diag.repo,
    ));
    out.push_str(&format!(
        "{n}. `gh pr comment {pr} --body \"$(...)\"` — see the post-resolution \
         comment template in `tools/boss/docs/designs/auto-rebase-stacked-prs.md` Q11.\n\n",
        n = next_step + 2,
        pr = diag.dependent_pr_number,
    ));

    out.push_str("### Stop conditions (do NOT push if any of these apply)\n\n");
    out.push_str(&format!(
        "- **Semantic obsolescence.** The merged PR (#{base}) appears to \
         have done the same work this PR was attempting. Close the dependent \
         PR with a comment instead of pushing a resolved rebase.\n",
        base = diag.base_pr_number,
    ));
    out.push_str(
        "- **Product decision required.** Resolving the conflict needs a \
         non-mechanical product decision (e.g. picking between two divergent \
         API shapes). Stop, comment on the PR with the question, and set \
         this chore's status to `blocked`.\n",
    );
    out.push_str(
        "- **Architectural mismatch.** The merged PR removed an abstraction \
         the dependent was extending and the dependent needs re-scoping or \
         splitting — beyond a rebase. Same as above: comment, block, stop.\n",
    );

    out
}

fn render_conflicted_file(out: &mut String, f: &ConflictedFile, base_pr_number: u64) {
    let upstream_note = if f.in_upstream_footprint {
        format!(" Also modified by base PR #{base_pr_number}.")
    } else {
        String::new()
    };
    let marker_word = if f.marker_count == 1 { "marker" } else { "markers" };
    out.push_str(&format!(
        "- `{path}` — {n} conflict {marker_word} ({total} lines).{upstream_note}\n",
        path = f.path,
        n = f.marker_count,
        marker_word = marker_word,
        total = f.total_lines,
        upstream_note = upstream_note,
    ));

    let (summary, body) = match &f.block {
        ConflictBlock::Full { content } => (
            format!("conflict block (full file, {} lines)", f.total_lines),
            content.as_str(),
        ),
        ConflictBlock::Excerpt {
            content,
            window_start_line,
            captured_lines,
        } => (
            format!(
                "conflict block (excerpt: lines {start}-{end} of {total}, {n} total markers)",
                start = window_start_line,
                end = window_start_line + captured_lines - 1,
                total = f.total_lines,
                n = f.marker_count,
            ),
            content.as_str(),
        ),
    };

    out.push_str("\n  <details><summary>");
    out.push_str(&summary);
    out.push_str("</summary>\n\n  ```\n");
    for line in body.split('\n') {
        out.push_str("  ");
        out.push_str(line);
        out.push('\n');
    }
    out.push_str("  ```\n  </details>\n\n");
}

fn escape_md_quote(s: &str) -> String {
    s.replace('"', "\\\"")
}

// ---------------------------------------------------------------------------
// Tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::sync::Mutex;

    struct StubJj {
        conflicted_files: Vec<String>,
        files: HashMap<String, String>,
        commits: Vec<DependentCommit>,
    }

    #[async_trait]
    impl JjCli for StubJj {
        async fn read_file(&self, _cwd: &Path, path: &str) -> Result<String> {
            self.files
                .get(path)
                .cloned()
                .ok_or_else(|| anyhow::anyhow!("stub jj missing file {path}"))
        }
        async fn list_conflicted_files(&self, _cwd: &Path) -> Result<Vec<String>> {
            Ok(self.conflicted_files.clone())
        }
        async fn log_dependent_range(
            &self,
            _cwd: &Path,
            _branch: &str,
        ) -> Result<Vec<DependentCommit>> {
            Ok(self.commits.clone())
        }
    }

    struct StubGh {
        files: Mutex<Result<Vec<String>, String>>,
    }

    #[async_trait]
    impl GhCli for StubGh {
        async fn pr_files(&self, _repo: &str, _pr_number: u64) -> Result<Vec<String>> {
            match &*self.files.lock().unwrap() {
                Ok(v) => Ok(v.clone()),
                Err(e) => Err(anyhow::anyhow!("{e}")),
            }
        }
    }

    fn small_conflict_text() -> String {
        // Three lines around a 1-marker conflict so the block fits the
        // "Full" branch and the marker count is exactly 1.
        "let x = 1;\n\
         <<<<<<< Conflict 1 of 1\n\
         %%%%%%% Changes from base to side #1\n\
         -let y = 2;\n\
         +let y = 3;\n\
         +++++++ Contents of side #2\n\
         let y = 4;\n\
         >>>>>>> Conflict 1 of 1 ends\n\
         let z = 5;\n"
            .to_owned()
    }

    fn large_conflict_text() -> String {
        // 220 lines, with a single conflict block sitting around line 150.
        let mut out = String::new();
        for i in 1..=140 {
            out.push_str(&format!("preamble line {i}\n"));
        }
        out.push_str("<<<<<<< Conflict in big file\n");
        for i in 1..=10 {
            out.push_str(&format!("ours line {i}\n"));
        }
        out.push_str("=======\n");
        for i in 1..=10 {
            out.push_str(&format!("theirs line {i}\n"));
        }
        out.push_str(">>>>>>> end\n");
        for i in 1..=58 {
            out.push_str(&format!("trailer line {i}\n"));
        }
        out
    }

    #[test]
    fn build_conflicted_file_full_branch_for_small_file() {
        let raw = small_conflict_text();
        let f = build_conflicted_file("src/foo.rs", &raw, false);
        assert_eq!(f.path, "src/foo.rs");
        assert_eq!(f.marker_count, 1);
        assert!(matches!(f.block, ConflictBlock::Full { .. }));
        assert_eq!(f.block.content(), raw);
    }

    #[test]
    fn build_conflicted_file_excerpt_branch_for_large_file() {
        let raw = large_conflict_text();
        let f = build_conflicted_file("src/big.rs", &raw, true);
        assert_eq!(f.marker_count, 1);
        assert_eq!(f.in_upstream_footprint, true);
        match &f.block {
            ConflictBlock::Excerpt {
                content,
                window_start_line,
                captured_lines,
            } => {
                assert!(*captured_lines <= EXCERPT_WINDOW_LINES);
                // Window should be centered on the marker (line 141).
                assert!(*window_start_line > 100 && *window_start_line < 141);
                assert!(content.contains("<<<<<<<"));
                assert!(content.contains("======="));
                assert!(content.contains(">>>>>>>"));
            }
            other => panic!("expected Excerpt, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn collect_diagnosis_intersects_upstream_footprint() {
        let mut files = HashMap::new();
        files.insert("a.rs".to_owned(), small_conflict_text());
        files.insert("b.rs".to_owned(), small_conflict_text());

        let jj = StubJj {
            conflicted_files: vec!["a.rs".to_owned(), "b.rs".to_owned()],
            files,
            commits: vec![
                DependentCommit {
                    change_id: "abcd1234".into(),
                    description_first_line: "engine: add stale-lease detection".into(),
                },
                DependentCommit {
                    change_id: "efgh5678".into(),
                    description_first_line: "engine: wire stale-lease into coordinator".into(),
                },
            ],
        };
        let gh = StubGh {
            // Base PR touched a.rs and c.rs — so a.rs is in the
            // intersection, b.rs is not.
            files: Mutex::new(Ok(vec!["a.rs".to_owned(), "c.rs".to_owned()])),
        };

        let inputs = DiagnosticInputs {
            workspace_path: PathBuf::from("/tmp/ws"),
            dependent_pr_number: 251,
            dependent_pr_url: "https://github.com/owner/repo/pull/251".into(),
            dependent_branch: "feat-B".into(),
            base_pr_number: 238,
            base_pr_url: "https://github.com/owner/repo/pull/238".into(),
            base_branch: "feat-A".into(),
            base_merged_at: Some("2026-05-07T14:22:01Z".into()),
            repo: "owner/repo".into(),
            test_command: Some("bazel test //tools/boss/...".into()),
        };

        let diag = collect_conflict_diagnosis(inputs, &jj, &gh).await.unwrap();
        assert_eq!(diag.conflicted_files.len(), 2);
        let a = diag
            .conflicted_files
            .iter()
            .find(|f| f.path == "a.rs")
            .unwrap();
        let b = diag
            .conflicted_files
            .iter()
            .find(|f| f.path == "b.rs")
            .unwrap();
        assert!(a.in_upstream_footprint, "a.rs should intersect");
        assert!(!b.in_upstream_footprint, "b.rs should not intersect");
        assert_eq!(diag.dependent_commits.len(), 2);
        assert_eq!(diag.base_pr_footprint, vec!["a.rs", "c.rs"]);
        assert_eq!(diag.footprint_intersection(), vec!["a.rs"]);
    }

    #[tokio::test]
    async fn collect_diagnosis_survives_gh_failure() {
        let mut files = HashMap::new();
        files.insert("a.rs".to_owned(), small_conflict_text());

        let jj = StubJj {
            conflicted_files: vec!["a.rs".to_owned()],
            files,
            commits: vec![],
        };
        let gh = StubGh {
            files: Mutex::new(Err("gh offline".into())),
        };
        let inputs = DiagnosticInputs {
            workspace_path: PathBuf::from("/tmp/ws"),
            dependent_pr_number: 251,
            dependent_pr_url: "https://example/251".into(),
            dependent_branch: "feat-B".into(),
            base_pr_number: 238,
            base_pr_url: "https://example/238".into(),
            base_branch: "feat-A".into(),
            base_merged_at: None,
            repo: "owner/repo".into(),
            test_command: None,
        };
        let diag = collect_conflict_diagnosis(inputs, &jj, &gh).await.unwrap();
        assert_eq!(diag.conflicted_files.len(), 1);
        assert!(diag.base_pr_footprint.is_empty());
        assert!(!diag.conflicted_files[0].in_upstream_footprint);
    }

    #[test]
    fn parse_jj_status_conflict_line_matches_expected_form() {
        assert_eq!(
            parse_jj_status_conflict_line("Conflict in tools/boss/engine/src/work.rs"),
            Some("tools/boss/engine/src/work.rs"),
        );
        assert_eq!(
            parse_jj_status_conflict_line("M  tools/boss/engine/src/work.rs"),
            None,
        );
        assert_eq!(parse_jj_status_conflict_line(""), None);
    }

    #[test]
    fn diagnosis_round_trips_through_serde() {
        let diag = sample_diagnosis_small();
        let json = serde_json::to_string(&diag).unwrap();
        let back: ConflictDiagnosis = serde_json::from_str(&json).unwrap();
        assert_eq!(diag, back);
    }

    fn sample_diagnosis_small() -> ConflictDiagnosis {
        ConflictDiagnosis {
            dependent_pr_number: 243,
            dependent_pr_url: "https://github.com/owner/repo/pull/243".into(),
            dependent_branch: "riker/feat-B".into(),
            base_pr_number: 238,
            base_pr_url: "https://github.com/owner/repo/pull/238".into(),
            base_branch: "feat-A".into(),
            base_merged_at: Some("2026-05-07T14:22:01Z".into()),
            repo: "owner/repo".into(),
            conflicted_files: vec![ConflictedFile {
                path: "tools/boss/engine/src/work.rs".into(),
                marker_count: 1,
                total_lines: 9,
                in_upstream_footprint: true,
                block: ConflictBlock::Full {
                    content: small_conflict_text(),
                },
            }],
            dependent_commits: vec![DependentCommit {
                change_id: "wmnpqxyl".into(),
                description_first_line: "engine: add new branch for stale-lease detection".into(),
            }],
            base_pr_footprint: vec![
                "tools/boss/engine/src/work.rs".into(),
                "tools/boss/engine/src/coordinator.rs".into(),
            ],
            test_command: Some("bazel test //tools/boss/...".into()),
        }
    }

    fn sample_diagnosis_multi_file() -> ConflictDiagnosis {
        ConflictDiagnosis {
            dependent_pr_number: 251,
            dependent_pr_url: "https://github.com/owner/repo/pull/251".into(),
            dependent_branch: "feat-B".into(),
            base_pr_number: 238,
            base_pr_url: "https://github.com/owner/repo/pull/238".into(),
            base_branch: "feat-A".into(),
            base_merged_at: None,
            repo: "owner/repo".into(),
            conflicted_files: vec![
                ConflictedFile {
                    path: "tools/boss/engine/src/work.rs".into(),
                    marker_count: 1,
                    total_lines: 9,
                    in_upstream_footprint: true,
                    block: ConflictBlock::Full {
                        content: small_conflict_text(),
                    },
                },
                ConflictedFile {
                    path: "tools/boss/engine/src/coordinator.rs".into(),
                    marker_count: 2,
                    total_lines: 220,
                    in_upstream_footprint: true,
                    block: ConflictBlock::Excerpt {
                        content: "<<<<<<< abridged\n=======\n>>>>>>>".into(),
                        window_start_line: 130,
                        captured_lines: 60,
                    },
                },
                ConflictedFile {
                    path: "tools/boss/engine/Cargo.toml".into(),
                    marker_count: 1,
                    total_lines: 30,
                    in_upstream_footprint: false,
                    block: ConflictBlock::Full {
                        content: "[package]\nname = \"x\"\n".into(),
                    },
                },
            ],
            dependent_commits: vec![
                DependentCommit {
                    change_id: "wmnpqxyl".into(),
                    description_first_line: "engine: add new branch for stale-lease detection"
                        .into(),
                },
                DependentCommit {
                    change_id: "kvqrtsuv".into(),
                    description_first_line: "engine: wire stale-lease detection into coordinator"
                        .into(),
                },
            ],
            base_pr_footprint: vec![
                "tools/boss/engine/src/work.rs".into(),
                "tools/boss/engine/src/coordinator.rs".into(),
                "tools/boss/engine/src/merge_poller.rs".into(),
            ],
            test_command: Some("bazel test //tools/boss/...".into()),
        }
    }

    #[test]
    fn render_includes_required_sections() {
        let rendered = render_chore_description(&sample_diagnosis_multi_file());

        // Header block.
        assert!(rendered.contains("Auto-rebase escalated to manual conflict resolution"));
        assert!(rendered.contains("[#251]"));
        assert!(rendered.contains("`feat-B`"));
        assert!(rendered.contains("[#238]"));
        assert!(rendered.contains("`feat-A`"));
        assert!(rendered.contains("Workspace"));

        // All conflicted files listed (acceptance: each file with shape).
        assert!(rendered.contains("Conflicted files (3)"));
        assert!(rendered.contains("`tools/boss/engine/src/work.rs`"));
        assert!(rendered.contains("`tools/boss/engine/src/coordinator.rs`"));
        assert!(rendered.contains("`tools/boss/engine/Cargo.toml`"));
        // marker counts surface as "1 conflict marker" / "2 conflict markers".
        assert!(rendered.contains("1 conflict marker"));
        assert!(rendered.contains("2 conflict markers"));
        // Excerpt files surface their window range.
        assert!(rendered.contains("excerpt"));
        assert!(rendered.contains("of 220"));

        // Upstream-footprint intersection (acceptance: footprint
        // intersection present).
        assert!(rendered.contains("Upstream footprint"));
        assert!(rendered.contains("touched 3 file(s)"));
        assert!(rendered.contains("2 of them overlap"));
        // Cargo.toml is *not* in the intersection so it should NOT
        // be listed under "Overlapping files".
        let intersection_section_idx = rendered.find("Overlapping files:").unwrap();
        let after_intersection = &rendered[intersection_section_idx..];
        // Find the next section header.
        let next_section = after_intersection.find("\n###").unwrap_or(after_intersection.len());
        let intersection_block = &after_intersection[..next_section];
        assert!(intersection_block.contains("`tools/boss/engine/src/work.rs`"));
        assert!(intersection_block.contains("`tools/boss/engine/src/coordinator.rs`"));
        assert!(!intersection_block.contains("Cargo.toml"));

        // Dependent commits (acceptance: commit list present).
        assert!(rendered.contains("Dependent commits in flight (2)"));
        assert!(rendered.contains("`wmnpqxyl`"));
        assert!(rendered.contains("`kvqrtsuv`"));
        assert!(rendered.contains("engine: add new branch for stale-lease detection"));

        // Test command surfaces (acceptance: test command present).
        assert!(rendered.contains("bazel test //tools/boss/..."));

        // Stop conditions (acceptance: stop conditions present).
        assert!(rendered.contains("Stop conditions"));
        assert!(rendered.contains("Semantic obsolescence"));
        assert!(rendered.contains("Product decision required"));
        assert!(rendered.contains("Architectural mismatch"));

        // Push / retarget steps reference correct PR number + branch.
        assert!(rendered.contains("jj git push --bookmark feat-B"));
        assert!(rendered.contains("gh pr edit 251 --base main --repo owner/repo"));
    }

    #[test]
    fn render_omits_test_step_when_no_test_command() {
        let mut diag = sample_diagnosis_small();
        diag.test_command = None;
        let rendered = render_chore_description(&diag);
        // Step 3 should jump straight to push, with no `bazel test` line.
        assert!(!rendered.contains("bazel test"));
        assert!(rendered.contains("3. `jj git push"));
    }

    #[test]
    fn render_handles_empty_footprint_and_commits() {
        let mut diag = sample_diagnosis_small();
        diag.base_pr_footprint.clear();
        diag.conflicted_files[0].in_upstream_footprint = false;
        diag.dependent_commits.clear();
        let rendered = render_chore_description(&diag);
        assert!(rendered.contains("touched 0 file(s)"));
        assert!(rendered.contains("0 of them overlap"));
        assert!(rendered.contains("could not enumerate the dependent's work"));
        assert!(!rendered.contains("Overlapping files:"));
    }

    /// Golden-output guard for the small/clean-diff diagnosis. We
    /// pin the rendered Markdown structure so accidental drift in
    /// the template is caught; the assertion is on the prefix of
    /// the rendered text up to the suggested-approach section to
    /// keep this test maintainable.
    #[test]
    fn render_golden_small_diagnosis_prefix() {
        let rendered = render_chore_description(&sample_diagnosis_small());
        let expected_prefix = "## Auto-rebase escalated to manual conflict resolution\n\
            \n\
            **Dependent PR**: [#243](https://github.com/owner/repo/pull/243) — `riker/feat-B`\n\
            **Base PR (merged)**: [#238](https://github.com/owner/repo/pull/238) — `feat-A` (merged 2026-05-07T14:22:01Z)\n\
            **Workspace**: pre-loaded with the failed `jj rebase -d main` state. `jj st` shows the conflict markers; do not re-run `jj rebase`.\n\
            \n\
            ### Conflicted files (1)\n";
        assert!(
            rendered.starts_with(expected_prefix),
            "rendered output did not start with the expected prefix.\n\
             Got:\n{rendered}",
        );
    }
}
