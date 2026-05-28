use super::*;

/// Trait for checking the live state of a GitHub PR URL.
///
/// Injected into `create_revision` so the gate can distinguish "open"
/// from "closed without merging" without hardcoding a `gh` call, which
/// would make unit tests depend on GitHub access. Production wires in
/// [`GhPrStateChecker`]; tests pass [`FakePrStateChecker`].
pub trait PrStateChecker: Send + Sync {
    /// Return the live lifecycle state of the given PR URL.
    fn check(&self, pr_url: &str) -> Result<PrOpenState>;
}

/// Lifecycle state returned by [`PrStateChecker::check`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrOpenState {
    Open,
    Merged,
    ClosedUnmerged,
}

/// Production implementation: shells out to `gh pr view`.
pub struct GhPrStateChecker;

impl PrStateChecker for GhPrStateChecker {
    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        let output = std::process::Command::new("gh")
            .args(["pr", "view", pr_url, "--json", "state,mergedAt"])
            .output()
            .with_context(|| format!("failed to run `gh pr view` for {pr_url}"))?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            bail!("`gh pr view` failed for {pr_url}: {stderr}");
        }
        let body = String::from_utf8_lossy(&output.stdout);
        let v: serde_json::Value = serde_json::from_str(&body)
            .with_context(|| format!("failed to parse `gh pr view` JSON for {pr_url}"))?;
        let state = v["state"].as_str().unwrap_or("").to_ascii_uppercase();
        match state.as_str() {
            "MERGED" => Ok(PrOpenState::Merged),
            "CLOSED" => Ok(PrOpenState::ClosedUnmerged),
            _ => Ok(PrOpenState::Open),
        }
    }
}

/// Test double: returns a preset state for known PR URLs.
#[cfg(test)]
pub struct FakePrStateChecker {
    pub states: std::collections::HashMap<String, PrOpenState>,
    pub default: PrOpenState,
}

#[cfg(test)]
impl FakePrStateChecker {
    pub fn always(state: PrOpenState) -> Self {
        Self {
            states: Default::default(),
            default: state,
        }
    }
    pub fn with(mut self, url: &str, state: PrOpenState) -> Self {
        self.states.insert(url.to_owned(), state);
        self
    }
}

#[cfg(test)]
impl PrStateChecker for FakePrStateChecker {
    fn check(&self, pr_url: &str) -> Result<PrOpenState> {
        Ok(self
            .states
            .get(pr_url)
            .cloned()
            .unwrap_or(self.default.clone()))
    }
}

/// Errors produced by the create-time revision gate.
#[derive(Debug, thiserror::Error)]
pub enum RevisionGateError {
    #[error(
        "T{short_id} has no PR yet; a revision targets an existing open PR. \
         Wait for T{short_id} to reach review, or file a normal follow-up chore."
    )]
    NoPr { short_id: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is already merged; revisions only apply to \
         open, unmerged PRs. File a new chore against main instead."
    )]
    Merged { short_id: i64, pr_number: i64 },

    #[error(
        "T{short_id}'s PR (#{pr_number}) is closed without merging; \
         there is no open PR to revise."
    )]
    ClosedUnmerged { short_id: i64, pr_number: i64 },
}

impl RevisionGateError {
    pub(crate) fn no_pr(task: &Task) -> Self {
        Self::NoPr {
            short_id: task.short_id.unwrap_or(0),
        }
    }
    pub(crate) fn merged(task: &Task, pr_url: &str) -> Self {
        use crate::merge_poller::parse_pr_number;
        Self::Merged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: parse_pr_number(pr_url).unwrap_or(0),
        }
    }
    pub(crate) fn closed(task: &Task, pr_url: &str) -> Self {
        use crate::merge_poller::parse_pr_number;
        Self::ClosedUnmerged {
            short_id: task.short_id.unwrap_or(0),
            pr_number: parse_pr_number(pr_url).unwrap_or(0),
        }
    }
}
