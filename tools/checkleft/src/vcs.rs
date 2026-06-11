use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

use anyhow::{Context, Result, bail};
use serde::Deserialize;

use crate::input::{ChangeKind, ChangeSet, ChangedFile};
use tracing::info;

mod patch_line_deltas;

use patch_line_deltas::parse_file_diffs_from_git_patch;

fn ensure_rustls_provider() {
    static INIT: OnceLock<()> = OnceLock::new();
    INIT.get_or_init(|| {
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BaseRevision {
    Jujutsu(String),
    Git(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VcsKind {
    Jujutsu,
    Git,
}

#[derive(Debug, Clone)]
pub struct Vcs {
    root: PathBuf,
    kind: VcsKind,
}

impl Vcs {
    pub fn detect(root: impl Into<PathBuf>) -> Result<Self> {
        let start = root.into();
        let start = start
            .canonicalize()
            .with_context(|| format!("failed to canonicalize start path {}", start.display()))?;

        if let Some(root) = detect_jj_root(&start)? {
            return Ok(Self {
                root,
                kind: VcsKind::Jujutsu,
            });
        }

        if let Some(root) = detect_git_root(&start)? {
            return Ok(Self {
                root,
                kind: VcsKind::Git,
            });
        }

        bail!("unable to detect vcs at {}", start.display());
    }

    pub fn kind(&self) -> VcsKind {
        self.kind
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn current_changeset(&self) -> Result<ChangeSet> {
        match self.kind {
            VcsKind::Jujutsu => {
                let summary = run_command(&self.root, "jj", &["diff", "--summary"])?;
                let mut changeset = parse_jj_diff_summary(&summary)?;
                let patch = run_command(&self.root, "jj", &["diff", "--git"])?;
                attach_line_deltas(&mut changeset, &patch);
                Ok(changeset)
            }
            VcsKind::Git => {
                let summary = run_command(&self.root, "git", &["diff", "--name-status", "HEAD"])?;
                let mut changeset = parse_git_name_status(&summary)?;
                let patch = run_command(&self.root, "git", &["diff", "--patch", "HEAD"])?;
                attach_line_deltas(&mut changeset, &patch);
                Ok(changeset)
            }
        }
    }

    pub fn changeset_since(&self, base_ref: &str) -> Result<ChangeSet> {
        match self.kind {
            VcsKind::Jujutsu => {
                let summary = run_command(
                    &self.root,
                    "jj",
                    &["diff", "--summary", "--from", base_ref, "--to", "@"],
                )?;
                let mut changeset = parse_jj_diff_summary(&summary)?;
                let patch = run_command(&self.root, "jj", &["diff", "--git", "--from", base_ref, "--to", "@"])?;
                attach_line_deltas(&mut changeset, &patch);
                Ok(changeset)
            }
            VcsKind::Git => {
                let merge_base = resolve_git_merge_base(&self.root, base_ref)?;
                info!(base_ref, merge_base, "resolved merge-base for changeset");
                let summary = run_command(&self.root, "git", &["diff", "--name-status", &merge_base, "HEAD"])?;
                let mut changeset = parse_git_name_status(&summary)?;
                let patch = run_command(&self.root, "git", &["diff", "--patch", &merge_base, "HEAD"])?;
                attach_line_deltas(&mut changeset, &patch);
                Ok(changeset)
            }
        }
    }

    pub fn all_files_changeset(&self) -> Result<ChangeSet> {
        let output = match self.kind {
            VcsKind::Jujutsu => run_command(&self.root, "jj", &["file", "list"]),
            VcsKind::Git => run_command(&self.root, "git", &["ls-files"]),
        }?;

        Ok(parse_tracked_file_list(&output))
    }

    pub fn base_revision(&self, all: bool, base_ref: Option<&str>) -> Result<Option<BaseRevision>> {
        if all {
            return Ok(None);
        }

        match self.kind {
            VcsKind::Jujutsu => Ok(Some(BaseRevision::Jujutsu(
                base_ref
                    .filter(|value| !value.trim().is_empty())
                    .unwrap_or("@-")
                    .to_owned(),
            ))),
            VcsKind::Git => {
                let revision = match base_ref.filter(|value| !value.trim().is_empty()) {
                    Some(base_ref) => resolve_git_merge_base(&self.root, base_ref)?,
                    None => "HEAD".to_owned(),
                };
                Ok(Some(BaseRevision::Git(revision)))
            }
        }
    }

    pub fn current_commit_description(&self) -> Result<String> {
        match self.kind {
            VcsKind::Jujutsu => run_command(&self.root, "jj", &["log", "-r", "@", "--no-graph", "-T", "description"]),
            VcsKind::Git => run_command(&self.root, "git", &["log", "-1", "--pretty=%B", "HEAD"]),
        }
    }

    pub fn remote_repo_slug(&self) -> Option<String> {
        if let Ok(output) = run_command(&self.root, "git", &["remote", "get-url", "origin"])
            && let Some(slug) = parse_repo_slug_from_remote_url(output.trim())
        {
            return Some(slug);
        }

        None
    }
}

#[derive(Deserialize)]
struct GithubPullRequestResponse {
    body: Option<String>,
}

pub async fn github_pull_request_description(
    repository: &str,
    change_id: &str,
    github_token: Option<&str>,
) -> Option<String> {
    let url = format!("https://api.github.com/repos/{repository}/pulls/{change_id}");
    ensure_rustls_provider();
    let client = reqwest::Client::new();
    let mut request = client
        .get(url)
        .header(reqwest::header::ACCEPT, "application/vnd.github+json")
        .header(reqwest::header::USER_AGENT, "checkleft-cli");

    if let Some(token) = github_token.filter(|token| !token.trim().is_empty()) {
        request = request.bearer_auth(token);
    }

    let response = request.send().await.ok()?;
    if !response.status().is_success() {
        return None;
    }

    let response_bytes = response.bytes().await.ok()?;
    let payload: GithubPullRequestResponse = serde_json::from_slice(&response_bytes).ok()?;
    normalize_non_empty(payload.body)
}

fn run_command(root: &Path, binary: &str, args: &[&str]) -> Result<String> {
    info!(
        root = %root.display(),
        binary,
        args = args.join(" "),
        "running command"
    );
    let output = Command::new(binary)
        .args(args)
        .current_dir(root)
        .output()
        .with_context(|| format!("failed to execute `{binary} {}`", args.join(" ")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("command `{binary} {}` failed: {}", args.join(" "), stderr.trim());
    }

    String::from_utf8(output.stdout)
        .with_context(|| format!("command `{binary} {}` returned invalid utf-8", args.join(" ")))
}

fn detect_jj_root(start: &Path) -> Result<Option<PathBuf>> {
    let output = match run_command(start, "jj", &["root"]) {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let root = parse_repo_root_output(&output, "jj root")?;
    Ok(Some(root))
}

fn detect_git_root(start: &Path) -> Result<Option<PathBuf>> {
    let output = match run_command(start, "git", &["rev-parse", "--show-toplevel"]) {
        Ok(output) => output,
        Err(_) => return Ok(None),
    };
    let root = parse_repo_root_output(&output, "git rev-parse --show-toplevel")?;
    Ok(Some(root))
}

fn parse_repo_root_output(output: &str, command_name: &str) -> Result<PathBuf> {
    let raw_root = output.trim();
    if raw_root.is_empty() {
        bail!("command `{command_name}` returned an empty repository root");
    }

    let root = PathBuf::from(raw_root);
    root.canonicalize()
        .with_context(|| format!("failed to canonicalize repository root {}", root.display()))
}

pub fn parse_jj_diff_summary(output: &str) -> Result<ChangeSet> {
    let mut changed_files = Vec::new();

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let mut parts = line.splitn(2, ' ');
        let status = parts.next().unwrap_or_default();
        let rest = parts.next().unwrap_or_default().trim();

        match status {
            "A" => changed_files.push(ChangedFile {
                path: PathBuf::from(rest),
                kind: ChangeKind::Added,
                old_path: None,
            }),
            "M" => changed_files.push(ChangedFile {
                path: PathBuf::from(rest),
                kind: ChangeKind::Modified,
                old_path: None,
            }),
            "D" => changed_files.push(ChangedFile {
                path: PathBuf::from(rest),
                kind: ChangeKind::Deleted,
                old_path: None,
            }),
            "R" => {
                let (old_path, new_path) = parse_arrow_rename(rest)?;
                changed_files.push(ChangedFile {
                    path: new_path,
                    kind: ChangeKind::Renamed,
                    old_path: Some(old_path),
                });
            }
            _ => bail!("unsupported jj diff summary line: {line}"),
        }
    }

    Ok(ChangeSet::new(changed_files))
}

pub fn parse_git_name_status(output: &str) -> Result<ChangeSet> {
    let mut changed_files = Vec::new();

    for raw_line in output.lines() {
        let line = raw_line.trim();
        if line.is_empty() {
            continue;
        }

        let fields: Vec<_> = line.split('\t').collect();
        let status = *fields
            .first()
            .ok_or_else(|| anyhow::anyhow!("missing status in git line: {line}"))?;

        if status.starts_with('R') {
            if fields.len() != 3 {
                bail!("invalid git rename line: {line}");
            }

            changed_files.push(ChangedFile {
                path: PathBuf::from(fields[2]),
                kind: ChangeKind::Renamed,
                old_path: Some(PathBuf::from(fields[1])),
            });
            continue;
        }

        if fields.len() < 2 {
            bail!("invalid git name-status line: {line}");
        }

        let kind = match status {
            "A" => ChangeKind::Added,
            "M" => ChangeKind::Modified,
            "D" => ChangeKind::Deleted,
            _ => bail!("unsupported git status: {status}"),
        };

        changed_files.push(ChangedFile {
            path: PathBuf::from(fields[1]),
            kind,
            old_path: None,
        });
    }

    Ok(ChangeSet::new(changed_files))
}

pub fn parse_tracked_file_list(output: &str) -> ChangeSet {
    let changed_files = output
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| ChangedFile {
            path: PathBuf::from(line),
            kind: ChangeKind::Modified,
            old_path: None,
        })
        .collect();

    ChangeSet::new(changed_files)
}

fn parse_arrow_rename(input: &str) -> Result<(PathBuf, PathBuf)> {
    let parts: Vec<_> = input.split("=>").collect();
    if parts.len() != 2 {
        bail!("invalid rename format: {input}");
    }

    let old_path = parts[0].trim();
    let new_path = parts[1].trim();

    if old_path.is_empty() || new_path.is_empty() {
        bail!("invalid rename format: {input}");
    }

    Ok((PathBuf::from(old_path), PathBuf::from(new_path)))
}

fn attach_line_deltas(changeset: &mut ChangeSet, patch: &str) {
    let file_diffs = parse_file_diffs_from_git_patch(patch);
    for changed_file in &changeset.changed_files {
        if let Some(diff) = file_diffs.get(&changed_file.path) {
            changeset
                .file_line_deltas
                .insert(changed_file.path.clone(), diff.line_delta);
            changeset
                .file_diffs
                .insert(changed_file.path.clone(), diff.file_diff.clone());
        }
    }
}

fn resolve_git_merge_base(root: &Path, base_ref: &str) -> Result<String> {
    let output = run_command(root, "git", &["merge-base", base_ref, "HEAD"])?;
    let revision = output.trim();
    if revision.is_empty() {
        bail!("git merge-base returned an empty revision for `{base_ref}`");
    }

    Ok(revision.to_owned())
}

fn parse_repo_slug_from_remote_url(remote_url: &str) -> Option<String> {
    let remote_url = remote_url.trim();
    let repo_path = if let Some(stripped) = remote_url.strip_prefix("git@github.com:") {
        stripped
    } else if let Some(stripped) = remote_url.strip_prefix("https://github.com/") {
        stripped
    } else {
        remote_url.strip_prefix("ssh://git@github.com/")?
    };

    let repo_path = repo_path.trim_end_matches(".git").trim_matches('/');
    let parts: Vec<_> = repo_path.split('/').collect();
    if parts.len() != 2 || parts.iter().any(|part| part.is_empty()) {
        return None;
    }

    Some(format!("{}/{}", parts[0], parts[1]))
}

fn normalize_non_empty(value: Option<String>) -> Option<String> {
    value.map(|text| text.trim().to_owned()).filter(|text| !text.is_empty())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use crate::input::ChangeKind;

    use super::{
        normalize_non_empty, parse_git_name_status, parse_jj_diff_summary, parse_repo_root_output,
        parse_repo_slug_from_remote_url, parse_tracked_file_list,
    };

    #[test]
    fn parses_jj_diff_summary() {
        let parsed = parse_jj_diff_summary(
            r#"
A checks/src/lib.rs
M checks/src/input.rs
D old/file.txt
R docs/old.md => docs/new.md
"#,
        )
        .expect("parse jj diff summary");

        assert_eq!(parsed.changed_files.len(), 4);
        assert_eq!(parsed.changed_files[0].kind, ChangeKind::Added);
        assert_eq!(parsed.changed_files[1].kind, ChangeKind::Modified);
        assert_eq!(parsed.changed_files[2].kind, ChangeKind::Deleted);
        assert_eq!(parsed.changed_files[3].kind, ChangeKind::Renamed);
        assert_eq!(parsed.changed_files[3].old_path, Some(PathBuf::from("docs/old.md")));
        assert_eq!(parsed.changed_files[3].path, PathBuf::from("docs/new.md"));
    }

    #[test]
    fn parses_git_name_status() {
        let parsed = parse_git_name_status(
            "A\tchecks/src/lib.rs\nM\tchecks/src/input.rs\nD\told/file.txt\nR100\tdocs/old.md\tdocs/new.md\n",
        )
        .expect("parse git name-status");

        assert_eq!(parsed.changed_files.len(), 4);
        assert_eq!(parsed.changed_files[0].kind, ChangeKind::Added);
        assert_eq!(parsed.changed_files[1].kind, ChangeKind::Modified);
        assert_eq!(parsed.changed_files[2].kind, ChangeKind::Deleted);
        assert_eq!(parsed.changed_files[3].kind, ChangeKind::Renamed);
        assert_eq!(parsed.changed_files[3].old_path, Some(PathBuf::from("docs/old.md")));
        assert_eq!(parsed.changed_files[3].path, PathBuf::from("docs/new.md"));
    }

    #[test]
    fn parses_all_files_list() {
        let parsed = parse_tracked_file_list("checks/src/lib.rs\ndocs/index.md\n");
        assert_eq!(parsed.changed_files.len(), 2);
        assert_eq!(parsed.changed_files[0].path, PathBuf::from("checks/src/lib.rs"));
        assert_eq!(parsed.changed_files[0].kind, ChangeKind::Modified);
    }

    #[test]
    fn parse_repo_root_output_rejects_empty_output() {
        let parsed = parse_repo_root_output(" \n ", "jj root");
        assert!(parsed.is_err());
    }

    #[test]
    fn parses_repo_slug_from_supported_remote_url_formats() {
        assert_eq!(
            parse_repo_slug_from_remote_url("git@github.com:example/flunge.git"),
            Some("example/flunge".to_owned())
        );
        assert_eq!(
            parse_repo_slug_from_remote_url("https://github.com/example/flunge"),
            Some("example/flunge".to_owned())
        );
        assert_eq!(
            parse_repo_slug_from_remote_url("ssh://git@github.com/example/flunge.git"),
            Some("example/flunge".to_owned())
        );
    }

    #[test]
    fn normalize_non_empty_trims_and_filters_empty_values() {
        assert_eq!(normalize_non_empty(None), None);
        assert_eq!(normalize_non_empty(Some("".to_owned())), None);
        assert_eq!(
            normalize_non_empty(Some("  example/flunge  ".to_owned())),
            Some("example/flunge".to_owned())
        );
    }

    /// Verifies that `changeset_since` scopes to the PR's own changes only, not
    /// drift that landed on the base branch after the PR was forked.
    ///
    /// Scenario: main gains a "drift" commit after the PR branch forked.
    /// `changeset_since("main")` must return only the PR file, not the drift file.
    #[test]
    fn git_changeset_since_excludes_base_branch_drift() {
        use std::fs;
        use std::process::Command;
        use tempfile::tempdir;

        fn run_git(root: &std::path::Path, args: &[&str]) {
            let output = Command::new("git")
                .args(args)
                .current_dir(root)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {:?} failed: {}",
                args,
                String::from_utf8_lossy(&output.stderr)
            );
        }

        let temp = tempdir().expect("create temp dir");
        // Pin the initial branch name so the test does not depend on the
        // machine's `init.defaultBranch` config (CI leaves it unset, which
        // defaults to `master`, breaking the `git merge-base main HEAD` below).
        run_git(temp.path(), &["init", "-b", "main"]);
        run_git(temp.path(), &["config", "user.email", "test@checkleft.example"]);
        run_git(temp.path(), &["config", "user.name", "Checkleft Test"]);

        // Base commit on main
        fs::write(temp.path().join("base.txt"), "base\n").expect("write base");
        run_git(temp.path(), &["add", "base.txt"]);
        run_git(temp.path(), &["commit", "-m", "initial"]);

        // Create PR branch from here and add pr_file.txt
        run_git(temp.path(), &["checkout", "-b", "pr-branch"]);
        fs::write(temp.path().join("pr_file.txt"), "pr change\n").expect("write pr file");
        run_git(temp.path(), &["add", "pr_file.txt"]);
        run_git(temp.path(), &["commit", "-m", "pr change"]);

        // Back to main: simulate another PR landing (drift)
        run_git(temp.path(), &["checkout", "-"]);
        fs::write(temp.path().join("drift.txt"), "main drift\n").expect("write drift");
        run_git(temp.path(), &["add", "drift.txt"]);
        run_git(temp.path(), &["commit", "-m", "main drift"]);

        // Return to PR branch (HEAD is now the PR tip, main has moved past fork)
        run_git(temp.path(), &["checkout", "pr-branch"]);

        let vcs = super::Vcs::detect(temp.path()).expect("detect vcs");
        let changeset = vcs.changeset_since("main").expect("changeset since main");

        let changed_paths: Vec<_> = changeset.changed_files.iter().map(|f| f.path.as_path()).collect();

        assert_eq!(
            changed_paths,
            vec![std::path::Path::new("pr_file.txt")],
            "expected only pr_file.txt; drift.txt must be excluded. Got: {changed_paths:?}"
        );
    }
}
