use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use globset::{Glob, GlobSet, GlobSetBuilder};
use regex::Regex;

use crate::model::{ChangelogEntry, ChangelogRange};

pub struct ExtractionConfig {
    /// Working directory of the git repo.
    pub repo_path: PathBuf,
    /// Start tag (exclusive lower bound for git log).
    pub from_tag: String,
    /// End tag or ref (inclusive upper bound; use "HEAD" if not specified).
    pub to_tag: String,
    /// Glob patterns; empty means include all commits.
    pub path_globs: Vec<String>,
    /// `owner/name` slug used to build PR and compare URLs.
    pub repo_slug: String,
    /// When true, call `gh` to fetch real PR titles and @logins.
    pub enrich: bool,
}

/// Raw parsed PR info from a single commit before enrichment.
struct RawPr {
    pr_number: u64,
    title: String,
    author: String,
}

pub fn extract_changelog(config: &ExtractionConfig) -> Result<ChangelogRange> {
    let globset = build_globset(&config.path_globs)?;
    let commits = get_commits(&config.repo_path, &config.from_tag, &config.to_tag)?;

    let squash_re = Regex::new(r"^(.*) \(#(\d+)\)$").unwrap();
    let merge_re = Regex::new(r"^Merge pull request #(\d+) from ").unwrap();

    let mut entries: Vec<ChangelogEntry> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();

    for commit in &commits {
        let raw = match parse_pr_from_commit(commit, &squash_re, &merge_re) {
            Some(r) => r,
            None => continue,
        };

        if seen.contains(&raw.pr_number) {
            continue;
        }

        if !globset_is_empty(&config.path_globs) {
            let files = get_changed_files(&config.repo_path, &commit.hash)?;
            let matches = files.iter().any(|f| globset.is_match(f));
            if !matches {
                continue;
            }
        }

        seen.insert(raw.pr_number);

        let pr_url = format!("https://github.com/{}/pull/{}", config.repo_slug, raw.pr_number);
        entries.push(ChangelogEntry {
            pr_number: raw.pr_number,
            title: raw.title,
            author_login: raw.author,
            pr_url,
        });
    }

    if config.enrich {
        enrich_entries(&mut entries, &config.repo_slug);
    }

    let compare_url = format!(
        "https://github.com/{}/compare/{}...{}",
        config.repo_slug, config.from_tag, config.to_tag
    );

    Ok(ChangelogRange {
        from_tag: config.from_tag.clone(),
        to_tag: config.to_tag.clone(),
        compare_url,
        entries,
    })
}

/// Derive `owner/repo` from the git remote URL of the given remote name.
pub fn repo_slug_from_remote(repo_path: &Path, remote: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["remote", "get-url", remote])
        .current_dir(repo_path)
        .output()
        .with_context(|| format!("failed to run git remote get-url {remote}"))?;

    if !output.status.success() {
        bail!(
            "git remote get-url {} failed: {}",
            remote,
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let url = String::from_utf8_lossy(&output.stdout).trim().to_string();
    parse_github_slug(&url).ok_or_else(|| anyhow!("could not parse a GitHub owner/repo slug from remote URL: {url}"))
}

fn parse_github_slug(url: &str) -> Option<String> {
    // https://github.com/owner/repo(.git)?
    let https = regex::Regex::new(r"https://github\.com/([^/]+/[^/]+?)(?:\.git)?$").unwrap();
    if let Some(cap) = https.captures(url) {
        return Some(cap[1].to_string());
    }
    // git@github.com:owner/repo(.git)?
    let ssh = regex::Regex::new(r"git@github\.com:([^/]+/[^/]+?)(?:\.git)?$").unwrap();
    if let Some(cap) = ssh.captures(url) {
        return Some(cap[1].to_string());
    }
    None
}

struct CommitInfo {
    hash: String,
    /// First line of the commit message.
    subject: String,
    /// Remaining lines of the commit message (body).
    body: String,
    /// Git author name.
    author_name: String,
}

fn get_commits(repo_path: &Path, from: &str, to: &str) -> Result<Vec<CommitInfo>> {
    // Separator that is unlikely to appear in commit messages.
    const REC_SEP: &str = "\x01\x02\x03";
    const FIELD_SEP: &str = "\x04\x05\x06";

    let range = format!("{from}..{to}");
    let format = format!("%H{FIELD_SEP}%s{FIELD_SEP}%an{FIELD_SEP}%b{REC_SEP}");

    let output = Command::new("git")
        .args(["log", &range, &format!("--format={format}")])
        .current_dir(repo_path)
        .output()
        .context("failed to run git log")?;

    if !output.status.success() {
        bail!("git log failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let mut commits = Vec::new();

    for record in raw.split(REC_SEP) {
        let record = record.trim();
        if record.is_empty() {
            continue;
        }
        let parts: Vec<&str> = record.splitn(4, FIELD_SEP).collect();
        if parts.len() < 3 {
            continue;
        }
        commits.push(CommitInfo {
            hash: parts[0].trim().to_string(),
            subject: parts[1].trim().to_string(),
            author_name: parts[2].trim().to_string(),
            body: parts.get(3).map(|s| s.trim().to_string()).unwrap_or_default(),
        });
    }

    Ok(commits)
}

fn parse_pr_from_commit(commit: &CommitInfo, squash_re: &Regex, merge_re: &Regex) -> Option<RawPr> {
    // Style 1: squash merge — subject ends with (##N)
    if let Some(cap) = squash_re.captures(&commit.subject) {
        let title = cap[1].to_string();
        let pr_number: u64 = cap[2].parse().ok()?;
        let author = normalize_author(&commit.author_name);
        return Some(RawPr {
            pr_number,
            title,
            author,
        });
    }

    // Style 2: merge commit — "Merge pull request #N from <branch>"
    if let Some(cap) = merge_re.captures(&commit.subject) {
        let pr_number: u64 = cap[1].parse().ok()?;
        // Title is the first non-empty line of the body.
        let title = commit
            .body
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or(&commit.subject)
            .to_string();
        let author = normalize_author(&commit.author_name);
        return Some(RawPr {
            pr_number,
            title,
            author,
        });
    }

    None
}

/// Convert a git author name to a plausible GitHub-style login for git-only mode.
fn normalize_author(name: &str) -> String {
    name.to_lowercase().split_whitespace().collect::<Vec<_>>().join("-")
}

fn get_changed_files(repo_path: &Path, commit_hash: &str) -> Result<Vec<String>> {
    // -m handles merge commits by diffing against all parents.
    let output = Command::new("git")
        .args(["diff-tree", "--no-commit-id", "-r", "--name-only", "-m", commit_hash])
        .current_dir(repo_path)
        .output()
        .context("failed to run git diff-tree")?;

    if !output.status.success() {
        bail!("git diff-tree failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let files = String::from_utf8_lossy(&output.stdout)
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .map(str::to_string)
        .collect();

    Ok(files)
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for pattern in patterns {
        let glob = Glob::new(pattern).with_context(|| format!("invalid glob pattern: {pattern}"))?;
        builder.add(glob);
    }
    builder.build().context("failed to build globset")
}

fn globset_is_empty(patterns: &[String]) -> bool {
    patterns.is_empty()
}

/// Overlay real PR titles and @logins using `gh api`.
/// Warns on stderr and leaves the entry unchanged if the call fails.
fn enrich_entries(entries: &mut Vec<ChangelogEntry>, repo_slug: &str) {
    for entry in entries.iter_mut() {
        match fetch_pr_metadata(repo_slug, entry.pr_number) {
            Ok((title, login)) => {
                entry.title = title;
                entry.author_login = login;
            }
            Err(e) => {
                eprintln!("warning: could not enrich PR #{}: {e}", entry.pr_number);
            }
        }
    }
}

fn fetch_pr_metadata(repo_slug: &str, pr_number: u64) -> Result<(String, String)> {
    let output = Command::new("gh")
        .args([
            "api",
            &format!("repos/{repo_slug}/pulls/{pr_number}"),
            "--jq",
            ".title + \"\\t\" + .user.login",
        ])
        .output()
        .context("failed to run gh api")?;

    if !output.status.success() {
        bail!("gh api failed: {}", String::from_utf8_lossy(&output.stderr));
    }

    let raw = String::from_utf8_lossy(&output.stdout);
    let raw = raw.trim();
    let tab = raw.find('\t').ok_or_else(|| anyhow!("unexpected gh output: {raw}"))?;
    let title = raw[..tab].to_string();
    let login = raw[tab + 1..].to_string();
    Ok((title, login))
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---- PR extraction unit tests ----

    fn squash_re() -> Regex {
        Regex::new(r"^(.*) \(#(\d+)\)$").unwrap()
    }

    fn merge_re() -> Regex {
        Regex::new(r"^Merge pull request #(\d+) from ").unwrap()
    }

    fn commit(subject: &str, author: &str, body: &str) -> CommitInfo {
        CommitInfo {
            hash: "abc123".to_string(),
            subject: subject.to_string(),
            author_name: author.to_string(),
            body: body.to_string(),
        }
    }

    #[test]
    fn squash_merge_extraction() {
        let c = commit("Add cool feature (#42)", "Alice Smith", "");
        let raw = parse_pr_from_commit(&c, &squash_re(), &merge_re()).unwrap();
        assert_eq!(raw.pr_number, 42);
        assert_eq!(raw.title, "Add cool feature");
        assert_eq!(raw.author, "alice-smith");
    }

    #[test]
    fn squash_merge_title_preserves_parens() {
        let c = commit("Fix something (old) (#99)", "Bob", "");
        let raw = parse_pr_from_commit(&c, &squash_re(), &merge_re()).unwrap();
        assert_eq!(raw.pr_number, 99);
        assert_eq!(raw.title, "Fix something (old)");
    }

    #[test]
    fn merge_commit_extraction() {
        let c = commit(
            "Merge pull request #7 from user/branch",
            "GitHub",
            "\nActual PR title here\n",
        );
        let raw = parse_pr_from_commit(&c, &squash_re(), &merge_re()).unwrap();
        assert_eq!(raw.pr_number, 7);
        assert_eq!(raw.title, "Actual PR title here");
    }

    #[test]
    fn merge_commit_no_body_falls_back_to_subject() {
        let c = commit("Merge pull request #3 from user/branch", "GitHub", "");
        let raw = parse_pr_from_commit(&c, &squash_re(), &merge_re()).unwrap();
        assert_eq!(raw.pr_number, 3);
        assert_eq!(raw.title, "Merge pull request #3 from user/branch");
    }

    #[test]
    fn non_pr_commit_returns_none() {
        let c = commit("chore: update deps", "Alice", "");
        assert!(parse_pr_from_commit(&c, &squash_re(), &merge_re()).is_none());
    }

    // ---- Path glob filtering unit tests ----

    #[test]
    fn globset_matches_single_pattern() {
        let gs = build_globset(&["tools/boss/**".to_string()]).unwrap();
        assert!(gs.is_match("tools/boss/cli/src/main.rs"));
        assert!(!gs.is_match("tools/cube/src/main.rs"));
    }

    #[test]
    fn globset_any_pattern_matches() {
        let gs = build_globset(&["tools/boss/**".to_string(), "tools/cube/**".to_string()]).unwrap();
        assert!(gs.is_match("tools/boss/cli/src/main.rs"));
        assert!(gs.is_match("tools/cube/src/app.rs"));
        assert!(!gs.is_match("tools/checkleft/src/lib.rs"));
    }

    #[test]
    fn globset_empty_patterns() {
        assert!(globset_is_empty(&[]));
        assert!(!globset_is_empty(&["tools/**".to_string()]));
    }

    // ---- Remote URL parsing unit tests ----

    #[test]
    fn parse_https_remote() {
        assert_eq!(
            parse_github_slug("https://github.com/spinyfin/mono"),
            Some("spinyfin/mono".to_string())
        );
        assert_eq!(
            parse_github_slug("https://github.com/spinyfin/mono.git"),
            Some("spinyfin/mono".to_string())
        );
    }

    #[test]
    fn parse_ssh_remote() {
        assert_eq!(
            parse_github_slug("git@github.com:spinyfin/mono.git"),
            Some("spinyfin/mono".to_string())
        );
        assert_eq!(
            parse_github_slug("git@github.com:spinyfin/mono"),
            Some("spinyfin/mono".to_string())
        );
    }

    #[test]
    fn parse_unknown_remote_returns_none() {
        assert_eq!(parse_github_slug("https://gitlab.com/spinyfin/mono.git"), None);
    }
}
