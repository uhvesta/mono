//! GitHub Projects backend for [`ExternalTracker`].
//!
//! Shells out to `gh` for all network operations. `GhRunner` is an internal
//! trait so tests can inject a fake without spawning real processes.

use std::process::Stdio;

use async_trait::async_trait;
use serde_json::Value;
use tokio::process::Command;

use super::{
    CloseReason, ClosedReason, ExternalTracker, Result, TrackerConfigError, TrackerContext,
    TrackerError, UpstreamItem, UpstreamPrAssociation, UpstreamRef, UpstreamStatus,
};

// ── Config ────────────────────────────────────────────────────────────────────

/// Deserialized shape of `products.external_tracker_config` for GitHub.
#[derive(Debug, serde::Deserialize)]
pub struct GitHubConfig {
    pub org: String,
    pub repo: String,
    pub project_number: u64,
    /// If set, only items that carry at least one of these labels are returned.
    pub label_filter: Option<Vec<String>>,
    #[serde(default)]
    pub reverse_close: bool,
    /// Name of the GitHub Projects V2 "Status" single-select option to use
    /// when a linked Boss task moves to the active (Doing) state.
    /// Defaults to `"In Progress"` when absent.
    pub in_progress_column: Option<String>,
}

impl GitHubConfig {
    fn in_progress_column_name(&self) -> &str {
        self.in_progress_column.as_deref().unwrap_or("In Progress")
    }
}

impl GitHubConfig {
    fn from_ctx(ctx: &TrackerContext) -> Result<Self> {
        serde_json::from_value(ctx.config.clone()).map_err(|e| {
            TrackerError::ConfigInvalid(format!("invalid GitHub tracker config: {e}"))
        })
    }
}

// ── GhRunner abstraction ──────────────────────────────────────────────────────

/// Error from a `gh` invocation, carrying an optional HTTP status code for
/// classification by the caller.
#[derive(Debug)]
pub(crate) struct GhRunnerError {
    pub http_status: Option<u16>,
    pub message: String,
}

impl GhRunnerError {
    fn transient(message: impl Into<String>) -> Self {
        Self { http_status: None, message: message.into() }
    }

    fn with_status(status: u16, message: impl Into<String>) -> Self {
        Self { http_status: Some(status), message: message.into() }
    }
}

/// Response from a successful `gh` REST call.
#[derive(Debug)]
pub(crate) struct GhResponse {
    pub body: Value,
}

/// Internal abstraction over `gh` shellouts for testability.
#[async_trait]
pub(crate) trait GhRunner: Send + Sync {
    /// Run `gh api graphql -f query=<query> -F k=v ...` and return parsed JSON.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn graphql(
        &self,
        query: &str,
        vars: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<Value, GhRunnerError>;

    /// Run `gh api <path>` (GET) and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_get(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError>;

    /// Run `gh api -X PATCH <path> -f k=v ...` and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_patch(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError>;

    /// Run `gh api -X POST <path> --input -` with a JSON body and return parsed JSON body.
    /// When `token` is `Some`, sets `GH_TOKEN` on the process.
    async fn rest_post(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError>;
}

// ── CommandGhRunner (production) ──────────────────────────────────────────────

pub(crate) struct CommandGhRunner;

/// Scan `gh`'s stderr for an HTTP status code pattern like "(HTTP 404)" or "HTTP 404".
fn parse_http_status_from_stderr(stderr: &str) -> Option<u16> {
    let lower = stderr.to_lowercase();
    if let Some(pos) = lower.find("http ") {
        let after = &stderr[pos + 5..];
        let digits: String = after.chars().take_while(|c| c.is_ascii_digit()).collect();
        if let Ok(code) = digits.parse::<u16>() {
            return Some(code);
        }
    }
    None
}

#[async_trait]
impl GhRunner for CommandGhRunner {
    async fn graphql(
        &self,
        query: &str,
        vars: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<Value, GhRunnerError> {
        let mut cmd = Command::new("gh");
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        cmd.args(["api", "graphql", "-f", &format!("query={query}")]);
        for (k, v) in vars {
            cmd.args(["-F", &format!("{k}={v}")]);
        }
        let output = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = parse_http_status_from_stderr(&stderr);
            return Err(GhRunnerError::with_status(
                status.unwrap_or(0),
                stderr.trim().to_owned(),
            ));
        }

        serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse graphql response: {e}")))
    }

    async fn rest_get(
        &self,
        path: &str,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        let mut cmd = Command::new("gh");
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        let output = cmd
            .args(["api", path])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = parse_http_status_from_stderr(&stderr).unwrap_or(0);
            return Err(GhRunnerError::with_status(status, stderr.trim().to_owned()));
        }

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse REST response: {e}")))?;
        Ok(GhResponse { body })
    }

    async fn rest_patch(
        &self,
        path: &str,
        fields: &[(&str, &str)],
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        let mut cmd = Command::new("gh");
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        cmd.args(["api", "-X", "PATCH", path]);
        for (k, v) in fields {
            cmd.args(["-f", &format!("{k}={v}")]);
        }
        let output = cmd
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = parse_http_status_from_stderr(&stderr).unwrap_or(0);
            return Err(GhRunnerError::with_status(status, stderr.trim().to_owned()));
        }

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse PATCH response: {e}")))?;
        Ok(GhResponse { body })
    }

    async fn rest_post(
        &self,
        path: &str,
        body: &serde_json::Value,
        token: Option<&str>,
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        use tokio::io::AsyncWriteExt as _;
        let stdin_bytes = serde_json::to_vec(body)
            .map_err(|e| GhRunnerError::transient(format!("failed to serialize POST body: {e}")))?;
        let mut cmd = Command::new("gh");
        if let Some(t) = token {
            cmd.env("GH_TOKEN", t);
        }
        cmd.args(["api", "-X", "POST", "--input", "-", path])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        let mut child = cmd
            .spawn()
            .map_err(|e| GhRunnerError::transient(format!("failed to spawn gh: {e}")))?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(&stdin_bytes).await.map_err(|e| {
                GhRunnerError::transient(format!("failed to write POST body: {e}"))
            })?;
        }
        let output = child
            .wait_with_output()
            .await
            .map_err(|e| GhRunnerError::transient(format!("failed to wait for gh: {e}")))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let status = parse_http_status_from_stderr(&stderr).unwrap_or(0);
            return Err(GhRunnerError::with_status(status, stderr.trim().to_owned()));
        }

        let body = serde_json::from_slice(&output.stdout)
            .map_err(|e| GhRunnerError::transient(format!("failed to parse POST response: {e}")))?;
        Ok(GhResponse { body })
    }
}

// ── GraphQL query ─────────────────────────────────────────────────────────────

const GITHUB_GRAPHQL_QUERY: &str = "
query($org: String!, $number: Int!, $after: String) {
  organization(login: $org) {
    projectV2(number: $number) {
      items(first: 100, after: $after) {
        pageInfo {
          hasNextPage
          endCursor
        }
        nodes {
          id
          fieldValues(first: 20) {
            nodes {
              ... on ProjectV2ItemFieldSingleSelectValue {
                name
                field {
                  ... on ProjectV2SingleSelectField {
                    name
                  }
                }
              }
            }
          }
          content {
            __typename
            ... on Issue {
              number
              title
              body
              state
              stateReason
              url
              repository { nameWithOwner }
              labels(first: 20) { nodes { name } }
              assignees(first: 10) { nodes { login } }
              closedByPullRequestsReferences(first: 5) {
                nodes { url merged mergedAt }
              }
              updatedAt
            }
          }
        }
      }
    }
  }
}
";

/// Query to fetch project-level metadata needed for `updateProjectV2ItemFieldValue`:
/// the project node ID and all single-select field IDs + option IDs.
const GITHUB_PROJECT_METADATA_QUERY: &str = "
query($org: String!, $number: Int!) {
  organization(login: $org) {
    projectV2(number: $number) {
      id
      fields(first: 20) {
        nodes {
          ... on ProjectV2SingleSelectField {
            id
            name
            options {
              id
              name
            }
          }
        }
      }
    }
  }
}
";

/// Mutation that sets a single-select field value on a project item.
const GITHUB_SET_FIELD_MUTATION: &str = "
mutation($projectId: ID!, $itemId: ID!, $fieldId: ID!, $optionId: String!) {
  updateProjectV2ItemFieldValue(input: {
    projectId: $projectId
    itemId: $itemId
    fieldId: $fieldId
    value: { singleSelectOptionId: $optionId }
  }) {
    projectV2Item {
      id
    }
  }
}
";

// ── Parsing helpers ───────────────────────────────────────────────────────────

/// Parse an ISO 8601 datetime string (e.g. `"2026-05-17T10:00:00Z"`) to Unix
/// seconds. Avoids pulling in a datetime crate.
fn parse_iso8601(s: &str) -> Option<i64> {
    let s = s.trim_end_matches('Z');
    let (date_part, time_part) = s.split_once('T')?;

    let mut dp = date_part.split('-');
    let y: i64 = dp.next()?.parse().ok()?;
    let m: i64 = dp.next()?.parse().ok()?;
    let d: i64 = dp.next()?.parse().ok()?;

    // Time part may include fractional seconds; take only HH:MM:SS.
    let time_hms = time_part.split('.').next().unwrap_or(time_part);
    let mut tp = time_hms.split(':');
    let h: i64 = tp.next()?.parse().ok()?;
    let min: i64 = tp.next()?.parse().ok()?;
    let sec: i64 = tp.next()?.parse().ok()?;

    let days = days_since_epoch(y, m, d)?;
    Some(days * 86400 + h * 3600 + min * 60 + sec)
}

fn days_since_epoch(y: i64, m: i64, d: i64) -> Option<i64> {
    // Julian Day Number calculation; JD of 1970-01-01 = 2440588.
    let (y, m) = if m <= 2 { (y - 1, m + 12) } else { (y, m) };
    let a = y / 100;
    let b = 2 - a + a / 4;
    let jd = (365.25 * (y + 4716) as f64) as i64
        + (30.6001 * (m + 1) as f64) as i64
        + d
        + b
        - 1524;
    Some(jd - 2_440_588)
}

/// Parse one project item node from the GraphQL `items.nodes` array.
/// Returns `None` for non-Issue content (DraftIssue, PullRequest, etc.) and
/// for items excluded by `label_filter`.
fn parse_project_item(node: &Value, config: &GitHubConfig) -> Option<UpstreamItem> {
    let content = node.get("content")?;
    if content.get("__typename")?.as_str()? != "Issue" {
        return None;
    }

    let project_item_id = node.get("id")?.as_str()?.to_owned();
    let number = content.get("number")?.as_u64()?;
    let title = content.get("title")?.as_str()?.to_owned();
    let body = content.get("body").and_then(|b| b.as_str()).unwrap_or("").to_owned();
    let state = content.get("state")?.as_str()?;
    let state_reason = content.get("stateReason").and_then(|v| v.as_str()).unwrap_or("");
    let url = content.get("url")?.as_str()?.to_owned();
    let updated_at_str = content.get("updatedAt")?.as_str()?;
    let updated_at = parse_iso8601(updated_at_str).unwrap_or(0);

    // The repo the issue lives in, canonical `owner/repo` form.
    let repo_name_with_owner = content
        .get("repository")
        .and_then(|r| r.get("nameWithOwner"))
        .and_then(|v| v.as_str())
        .unwrap_or_else(|| &config.repo);

    let labels: Vec<String> = content
        .get("labels")
        .and_then(|l| l.get("nodes"))
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| n.get("name")?.as_str().map(|s| s.to_owned()))
        .collect();

    // Apply label filter: if configured, at least one label must match.
    if let Some(filter) = &config.label_filter {
        if !filter.iter().any(|f| labels.iter().any(|l| l == f)) {
            return None;
        }
    }

    let assignees: Vec<String> = content
        .get("assignees")
        .and_then(|a| a.get("nodes"))
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| n.get("login")?.as_str().map(|s| s.to_owned()))
        .collect();

    let pr_associations: Vec<UpstreamPrAssociation> = content
        .get("closedByPullRequestsReferences")
        .and_then(|r| r.get("nodes"))
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| {
            let pr_url = n.get("url")?.as_str()?.to_owned();
            let merged = n.get("merged")?.as_bool()?;
            let merged_at =
                n.get("mergedAt").and_then(|v| v.as_str()).and_then(parse_iso8601);
            Some(UpstreamPrAssociation { pr_url, merged, merged_at })
        })
        .collect();

    let status = match state {
        "OPEN" => UpstreamStatus::Open,
        "CLOSED" => UpstreamStatus::Closed {
            reason: match state_reason {
                "COMPLETED" => ClosedReason::Completed,
                "NOT_PLANNED" => ClosedReason::NotPlanned,
                _ => ClosedReason::Unknown,
            },
        },
        _ => UpstreamStatus::Open,
    };

    // Read the current GitHub Projects "Status" column name, if set.
    let project_status: Option<String> = node
        .get("fieldValues")
        .and_then(|fv| fv.get("nodes"))
        .and_then(|n| n.as_array())
        .into_iter()
        .flatten()
        .find_map(|field_node| {
            let field_name = field_node
                .get("field")
                .and_then(|f| f.get("name"))
                .and_then(|n| n.as_str())?;
            if field_name != "Status" {
                return None;
            }
            field_node.get("name")?.as_str().map(|s| s.to_owned())
        });

    let canonical_id = format!("{}#{}", repo_name_with_owner, number);
    let raw = serde_json::json!({
        "issue_number": number,
        "project_item_id": project_item_id,
    });

    Some(UpstreamItem {
        upstream_ref: UpstreamRef { kind: "github".to_owned(), canonical_id, raw },
        title,
        body,
        status,
        upstream_url: url,
        labels,
        assignees,
        pr_associations,
        updated_at,
        project_status,
    })
}

/// Parse an issue from the REST `GET /repos/{owner}/{repo}/issues/{n}` response.
fn parse_rest_issue(body: &Value, org: &str, repo: &str) -> Option<UpstreamItem> {
    let number = body.get("number")?.as_u64()?;
    let title = body.get("title")?.as_str()?.to_owned();
    let body_text =
        body.get("body").and_then(|b| b.as_str()).unwrap_or("").to_owned();
    let state = body.get("state")?.as_str()?;
    let state_reason =
        body.get("state_reason").and_then(|v| v.as_str()).unwrap_or("");
    let url = body.get("html_url")?.as_str()?.to_owned();
    let updated_at_str = body.get("updated_at")?.as_str()?;
    let updated_at = parse_iso8601(updated_at_str).unwrap_or(0);

    let labels: Vec<String> = body
        .get("labels")
        .and_then(|l| l.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| n.get("name")?.as_str().map(|s| s.to_owned()))
        .collect();

    let assignees: Vec<String> = body
        .get("assignees")
        .and_then(|a| a.as_array())
        .into_iter()
        .flatten()
        .filter_map(|n| n.get("login")?.as_str().map(|s| s.to_owned()))
        .collect();

    let status = match state {
        "open" => UpstreamStatus::Open,
        "closed" => UpstreamStatus::Closed {
            reason: match state_reason {
                "completed" => ClosedReason::Completed,
                "not_planned" => ClosedReason::NotPlanned,
                _ => ClosedReason::Unknown,
            },
        },
        _ => UpstreamStatus::Open,
    };

    let canonical_id = format!("{}/{}#{}", org, repo, number);
    let raw = serde_json::json!({ "issue_number": number });

    Some(UpstreamItem {
        upstream_ref: UpstreamRef { kind: "github".to_owned(), canonical_id, raw },
        title,
        body: body_text,
        status,
        upstream_url: url,
        labels,
        assignees,
        pr_associations: vec![],
        updated_at,
        project_status: None,
    })
}

/// Check a GraphQL response for errors. On error, classify as `ConfigInvalid`
/// for NOT_FOUND (project/org missing) and `Transient` for everything else.
fn check_graphql_errors(response: &Value) -> Result<()> {
    let Some(errors) = response.get("errors").and_then(|e| e.as_array()) else {
        return Ok(());
    };
    if errors.is_empty() {
        return Ok(());
    }
    let msg = errors
        .iter()
        .filter_map(|e| e.get("message")?.as_str())
        .collect::<Vec<_>>()
        .join("; ");
    let is_not_found = errors
        .iter()
        .any(|e| e.get("type").and_then(|t| t.as_str()) == Some("NOT_FOUND"));
    if is_not_found {
        Err(TrackerError::ConfigInvalid(msg))
    } else {
        Err(TrackerError::Transient(msg))
    }
}

/// Map a `GhRunnerError` from a GraphQL call to a `TrackerError`.
fn map_graphql_error(err: GhRunnerError) -> TrackerError {
    match err.http_status {
        Some(401) => TrackerError::TokenRevoked(err.message),
        Some(403) => TrackerError::Auth(err.message),
        Some(404) => TrackerError::ConfigInvalid(err.message),
        Some(s) if s >= 500 => TrackerError::Transient(err.message),
        _ => TrackerError::Transient(err.message),
    }
}

/// Map a `GhRunnerError` from a REST write call to a `TrackerError`.
fn map_write_error(err: GhRunnerError) -> TrackerError {
    match err.http_status {
        Some(403) => TrackerError::PermissionDenied(err.message),
        Some(404) => TrackerError::NotFound(err.message),
        Some(s) if s >= 500 => TrackerError::Transient(err.message),
        _ => TrackerError::Transient(err.message),
    }
}

/// Extract the OAuth token from a `TrackerContext` as an `Option<&str>`.
/// Returns `None` when the credential is ambient (empty token).
fn opt_token(ctx: &TrackerContext) -> Option<&str> {
    if ctx.credential.token.is_empty() { None } else { Some(&ctx.credential.token) }
}

// ── GitHubTracker ─────────────────────────────────────────────────────────────

/// `ExternalTracker` implementation for GitHub Projects + Issues.
pub struct GitHubTracker {
    runner: Box<dyn GhRunner>,
}

impl GitHubTracker {
    pub fn new() -> Self {
        Self { runner: Box::new(CommandGhRunner) }
    }

    #[cfg(test)]
    pub(crate) fn with_runner(runner: impl GhRunner + 'static) -> Self {
        Self { runner: Box::new(runner) }
    }
}

impl Default for GitHubTracker {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl ExternalTracker for GitHubTracker {
    fn kind(&self) -> &'static str {
        "github"
    }

    fn validate_config(
        &self,
        config: &serde_json::Value,
    ) -> std::result::Result<(), TrackerConfigError> {
        let obj = config.as_object().ok_or_else(|| {
            TrackerConfigError::new("config must be a JSON object")
        })?;
        for field in ["org", "repo", "project_number"] {
            if !obj.contains_key(field) {
                return Err(TrackerConfigError::new(format!("missing required field '{field}'")));
            }
        }
        serde_json::from_value::<GitHubConfig>(config.clone())
            .map_err(|e| TrackerConfigError::new(format!("invalid config shape: {e}")))?;
        Ok(())
    }

    async fn fetch_items(&self, ctx: &TrackerContext) -> Result<Vec<UpstreamItem>> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let project_number_str = config.project_number.to_string();
        let mut items: Vec<UpstreamItem> = Vec::new();
        let mut cursor: Option<String> = None;

        loop {
            let cursor_val = cursor.as_deref().unwrap_or("");
            let mut vars: Vec<(&str, &str)> =
                vec![("org", &config.org), ("number", &project_number_str)];
            if let Some(c) = cursor.as_deref() {
                vars.push(("after", c));
                let _ = cursor_val; // suppress unused warning
            }

            let response = self
                .runner
                .graphql(GITHUB_GRAPHQL_QUERY, &vars, opt_token(ctx))
                .await
                .map_err(map_graphql_error)?;

            check_graphql_errors(&response)?;

            // Null projectV2 means the project doesn't exist.
            if response.pointer("/data/organization/projectV2").map_or(false, |v| v.is_null()) {
                return Err(TrackerError::ConfigInvalid(format!(
                    "project #{} not found in org '{}'",
                    config.project_number, config.org
                )));
            }

            let nodes = response
                .pointer("/data/organization/projectV2/items/nodes")
                .and_then(|n| n.as_array())
                .ok_or_else(|| {
                    TrackerError::Transient(
                        "unexpected GraphQL response shape: missing items.nodes".to_owned(),
                    )
                })?;

            for node in nodes {
                if let Some(item) = parse_project_item(node, &config) {
                    items.push(item);
                }
            }

            let page_info = response.pointer("/data/organization/projectV2/items/pageInfo");
            let has_next = page_info
                .and_then(|p| p.get("hasNextPage"))
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

            if !has_next {
                break;
            }

            cursor = page_info
                .and_then(|p| p.get("endCursor"))
                .and_then(|v| v.as_str())
                .map(|s| s.to_owned());

            if cursor.is_none() {
                break;
            }
        }

        Ok(items)
    }

    async fn fetch_item(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> Result<Option<UpstreamItem>> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let issue_number = ref_
            .raw
            .get("issue_number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(
                    "upstream ref missing 'issue_number' in raw blob".to_owned(),
                )
            })?;

        let path = format!("repos/{}/{}/issues/{}", config.org, config.repo, issue_number);
        match self.runner.rest_get(&path, opt_token(ctx)).await {
            Ok(resp) => Ok(parse_rest_issue(&resp.body, &config.org, &config.repo)),
            Err(e) if e.http_status == Some(404) => Ok(None),
            Err(e) => Err(map_write_error(e)),
        }
    }

    async fn close_issue(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
        reason: CloseReason,
    ) -> Result<()> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let issue_number = ref_
            .raw
            .get("issue_number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(
                    "upstream ref missing 'issue_number' in raw blob".to_owned(),
                )
            })?;

        let state_reason = match reason {
            CloseReason::Completed => "completed",
            CloseReason::NotPlanned => "not_planned",
        };

        let path = format!("repos/{}/{}/issues/{}", config.org, config.repo, issue_number);
        let fields = [("state", "closed"), ("state_reason", state_reason)];

        match self.runner.rest_patch(&path, &fields, opt_token(ctx)).await {
            Ok(_) => Ok(()),
            // 404: issue deleted or never existed; treat as already-closed (success).
            Err(e) if e.http_status == Some(404) => Ok(()),
            Err(e) => Err(map_write_error(e)),
        }
    }

    async fn post_closing_pr_comment(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
        pr_url: &str,
    ) -> Result<()> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let issue_number = ref_
            .raw
            .get("issue_number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(
                    "upstream ref missing 'issue_number' in raw blob".to_owned(),
                )
            })?;

        let comments_path = format!(
            "repos/{}/{}/issues/{}/comments",
            config.org, config.repo, issue_number
        );

        // Idempotency: skip if any existing comment already mentions this PR URL.
        match self.runner.rest_get(&comments_path, opt_token(ctx)).await {
            Ok(resp) => {
                if let Some(comments) = resp.body.as_array() {
                    let already_present = comments.iter().any(|c| {
                        c.get("body")
                            .and_then(|b| b.as_str())
                            .map(|b| b.contains(pr_url))
                            .unwrap_or(false)
                    });
                    if already_present {
                        return Ok(());
                    }
                }
            }
            // Issue deleted or inaccessible — nothing to comment on.
            Err(e) if e.http_status == Some(404) => return Ok(()),
            Err(e) => return Err(map_write_error(e)),
        }

        let comment_text = format!("Closed by {pr_url}");
        let comment_body = serde_json::json!({ "body": comment_text });
        match self.runner.rest_post(&comments_path, &comment_body, opt_token(ctx)).await {
            Ok(_) => Ok(()),
            // Issue gone between the GET and POST — treat as success.
            Err(e) if e.http_status == Some(404) => Ok(()),
            Err(e) => Err(map_write_error(e)),
        }
    }

    async fn set_project_status(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
    ) -> Result<()> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let target_column = config.in_progress_column_name().to_owned();

        let project_item_id = ref_
            .raw
            .get("project_item_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(
                    "upstream ref missing 'project_item_id' in raw blob".to_owned(),
                )
            })?
            .to_owned();

        // Fetch project metadata: project node ID, Status field ID, option IDs.
        let project_number_str = config.project_number.to_string();
        let metadata = self
            .runner
            .graphql(
                GITHUB_PROJECT_METADATA_QUERY,
                &[("org", &config.org), ("number", &project_number_str)],
                opt_token(ctx),
            )
            .await
            .map_err(map_graphql_error)?;

        check_graphql_errors(&metadata)?;

        let project_id = metadata
            .pointer("/data/organization/projectV2/id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(format!(
                    "project #{} not found in org '{}'",
                    config.project_number, config.org
                ))
            })?
            .to_owned();

        let fields_nodes = metadata
            .pointer("/data/organization/projectV2/fields/nodes")
            .and_then(|n| n.as_array())
            .ok_or_else(|| {
                TrackerError::Transient(
                    "unexpected response shape: missing fields.nodes".to_owned(),
                )
            })?;

        // Find the Status single-select field and the option matching the target column.
        let (field_id, option_id) = fields_nodes
            .iter()
            .find_map(|field| {
                let field_name = field.get("name")?.as_str()?;
                if field_name != "Status" {
                    return None;
                }
                let fid = field.get("id")?.as_str()?.to_owned();
                let options = field.get("options")?.as_array()?;
                let oid = options.iter().find_map(|opt| {
                    if opt.get("name")?.as_str()? == target_column {
                        opt.get("id")?.as_str().map(|s| s.to_owned())
                    } else {
                        None
                    }
                })?;
                Some((fid, oid))
            })
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(format!(
                    "project #{} has no Status field with option '{target_column}'",
                    config.project_number
                ))
            })?;

        // Apply the mutation.
        let mutation_result = self
            .runner
            .graphql(
                GITHUB_SET_FIELD_MUTATION,
                &[
                    ("projectId", project_id.as_str()),
                    ("itemId", project_item_id.as_str()),
                    ("fieldId", field_id.as_str()),
                    ("optionId", option_id.as_str()),
                ],
                opt_token(ctx),
            )
            .await
            .map_err(map_graphql_error)?;

        check_graphql_errors(&mutation_result)?;
        Ok(())
    }

    async fn add_label(
        &self,
        ctx: &TrackerContext,
        ref_: &UpstreamRef,
        label: &str,
    ) -> Result<()> {
        let config = GitHubConfig::from_ctx(ctx)?;
        let issue_number = ref_
            .raw
            .get("issue_number")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| {
                TrackerError::ConfigInvalid(
                    "upstream ref missing 'issue_number' in raw blob".to_owned(),
                )
            })?;

        // The repo lives in the canonical_id ("owner/repo#number") rather
        // than the config, because GitHub Projects items can reference
        // issues across repos in the same org. Parse it back out.
        let repo_with_owner = ref_
            .canonical_id
            .split_once('#')
            .map(|(r, _)| r)
            .unwrap_or(&config.repo);

        let path = format!("repos/{}/issues/{}/labels", repo_with_owner, issue_number);
        let body = serde_json::json!({ "labels": [label] });

        match self.runner.rest_post(&path, &body, opt_token(ctx)).await {
            Ok(_) => Ok(()),
            // 404: issue deleted or never existed; treat as no-op success
            // so a label-add failure can't block reconciliation forever.
            Err(e) if e.http_status == Some(404) => Ok(()),
            Err(e) => Err(map_write_error(e)),
        }
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use serde_json::json;

    use super::*;

    // ── FakeGhRunner ──────────────────────────────────────────────────────────

    struct FakeGhRunner {
        graphql_q: Mutex<VecDeque<std::result::Result<Value, GhRunnerError>>>,
        rest_get_q: Mutex<VecDeque<std::result::Result<GhResponse, GhRunnerError>>>,
        rest_patch_q: Mutex<VecDeque<std::result::Result<GhResponse, GhRunnerError>>>,
        rest_post_q: Mutex<VecDeque<std::result::Result<GhResponse, GhRunnerError>>>,
        /// Captures the `token` argument from the most recent call (any method).
        /// `None` = no call made yet; `Some(None)` = called with ambient (no token);
        /// `Some(Some(t))` = called with OAuth token `t`.
        last_token: Mutex<Option<Option<String>>>,
    }

    impl FakeGhRunner {
        fn new() -> Self {
            Self {
                graphql_q: Mutex::new(VecDeque::new()),
                rest_get_q: Mutex::new(VecDeque::new()),
                rest_patch_q: Mutex::new(VecDeque::new()),
                rest_post_q: Mutex::new(VecDeque::new()),
                last_token: Mutex::new(None),
            }
        }

        #[allow(dead_code)]
        fn last_token(&self) -> Option<Option<String>> {
            self.last_token.lock().unwrap().clone()
        }

        fn push_graphql_ok(&mut self, v: Value) -> &mut Self {
            self.graphql_q.get_mut().unwrap().push_back(Ok(v));
            self
        }

        fn push_graphql_err(&mut self, status: u16, msg: &str) -> &mut Self {
            self.graphql_q
                .get_mut()
                .unwrap()
                .push_back(Err(GhRunnerError::with_status(status, msg)));
            self
        }

        fn push_rest_get_ok(&mut self, v: Value) -> &mut Self {
            self.rest_get_q.get_mut().unwrap().push_back(Ok(GhResponse { body: v }));
            self
        }

        fn push_rest_get_err(&mut self, status: u16, msg: &str) -> &mut Self {
            self.rest_get_q
                .get_mut()
                .unwrap()
                .push_back(Err(GhRunnerError::with_status(status, msg)));
            self
        }

        fn push_rest_patch_ok(&mut self, v: Value) -> &mut Self {
            self.rest_patch_q.get_mut().unwrap().push_back(Ok(GhResponse { body: v }));
            self
        }

        fn push_rest_patch_err(&mut self, status: u16, msg: &str) -> &mut Self {
            self.rest_patch_q
                .get_mut()
                .unwrap()
                .push_back(Err(GhRunnerError::with_status(status, msg)));
            self
        }

        fn push_rest_post_ok(&mut self, v: Value) -> &mut Self {
            self.rest_post_q.get_mut().unwrap().push_back(Ok(GhResponse { body: v }));
            self
        }

        fn push_rest_post_err(&mut self, status: u16, msg: &str) -> &mut Self {
            self.rest_post_q
                .get_mut()
                .unwrap()
                .push_back(Err(GhRunnerError::with_status(status, msg)));
            self
        }
    }

    #[async_trait]
    impl GhRunner for FakeGhRunner {
        async fn graphql(
            &self,
            _query: &str,
            _vars: &[(&str, &str)],
            token: Option<&str>,
        ) -> std::result::Result<Value, GhRunnerError> {
            *self.last_token.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.graphql_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no graphql response queued")
        }

        async fn rest_get(
            &self,
            _path: &str,
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.last_token.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.rest_get_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no rest_get response queued")
        }

        async fn rest_patch(
            &self,
            _path: &str,
            _fields: &[(&str, &str)],
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.last_token.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.rest_patch_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no rest_patch response queued")
        }

        async fn rest_post(
            &self,
            _path: &str,
            _body: &serde_json::Value,
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.last_token.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.rest_post_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no rest_post response queued")
        }
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn github_ctx() -> TrackerContext {
        TrackerContext {
            product_id: "prod1".to_owned(),
            config: json!({
                "org": "spinyfin",
                "repo": "mono",
                "project_number": 1
            }),
            credential: super::super::TrackerCredential::ambient(),
        }
    }

    fn github_ctx_with_label_filter(labels: &[&str]) -> TrackerContext {
        TrackerContext {
            product_id: "prod1".to_owned(),
            config: json!({
                "org": "spinyfin",
                "repo": "mono",
                "project_number": 1,
                "label_filter": labels
            }),
            credential: super::super::TrackerCredential::ambient(),
        }
    }

    fn open_issue_node(id: &str, number: u64, title: &str, labels: &[&str]) -> Value {
        json!({
            "id": id,
            "fieldValues": {
                "nodes": [
                    {
                        "name": "Todo",
                        "field": { "name": "Status" }
                    }
                ]
            },
            "content": {
                "__typename": "Issue",
                "number": number,
                "title": title,
                "body": "Body text.",
                "state": "OPEN",
                "stateReason": null,
                "url": format!("https://github.com/spinyfin/mono/issues/{number}"),
                "repository": { "nameWithOwner": "spinyfin/mono" },
                "labels": { "nodes": labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>() },
                "assignees": { "nodes": [] },
                "closedByPullRequestsReferences": { "nodes": [] },
                "updatedAt": "2026-05-17T10:00:00Z"
            }
        })
    }

    fn open_issue_node_with_status(
        id: &str,
        number: u64,
        title: &str,
        project_status: &str,
    ) -> Value {
        json!({
            "id": id,
            "fieldValues": {
                "nodes": [
                    {
                        "name": project_status,
                        "field": { "name": "Status" }
                    }
                ]
            },
            "content": {
                "__typename": "Issue",
                "number": number,
                "title": title,
                "body": "Body text.",
                "state": "OPEN",
                "stateReason": null,
                "url": format!("https://github.com/spinyfin/mono/issues/{number}"),
                "repository": { "nameWithOwner": "spinyfin/mono" },
                "labels": { "nodes": [] },
                "assignees": { "nodes": [] },
                "closedByPullRequestsReferences": { "nodes": [] },
                "updatedAt": "2026-05-17T10:00:00Z"
            }
        })
    }

    fn project_metadata_response(
        project_id: &str,
        field_id: &str,
        options: &[(&str, &str)],
    ) -> Value {
        json!({
            "data": {
                "organization": {
                    "projectV2": {
                        "id": project_id,
                        "fields": {
                            "nodes": [{
                                "id": field_id,
                                "name": "Status",
                                "options": options.iter().map(|(id, name)| json!({"id": id, "name": name})).collect::<Vec<_>>()
                            }]
                        }
                    }
                }
            }
        })
    }

    fn set_field_mutation_ok(item_id: &str) -> Value {
        json!({
            "data": {
                "updateProjectV2ItemFieldValue": {
                    "projectV2Item": { "id": item_id }
                }
            }
        })
    }

    fn graphql_page(nodes: Vec<Value>, has_next: bool, cursor: &str) -> Value {
        json!({
            "data": {
                "organization": {
                    "projectV2": {
                        "items": {
                            "pageInfo": {
                                "hasNextPage": has_next,
                                "endCursor": cursor
                            },
                            "nodes": nodes
                        }
                    }
                }
            }
        })
    }

    fn rest_issue(number: u64, state: &str, state_reason: Option<&str>) -> Value {
        json!({
            "number": number,
            "title": format!("Issue #{number}"),
            "body": "Some description.",
            "state": state,
            "state_reason": state_reason,
            "html_url": format!("https://github.com/spinyfin/mono/issues/{number}"),
            "labels": [],
            "assignees": [],
            "updated_at": "2026-05-17T10:00:00Z"
        })
    }

    fn issue_ref(number: u64) -> UpstreamRef {
        UpstreamRef {
            kind: "github".to_owned(),
            canonical_id: format!("spinyfin/mono#{number}"),
            raw: json!({ "issue_number": number, "project_item_id": "PVTI_xxx" }),
        }
    }

    // ── GH_TOKEN threading ───────────────────────────────────────────────────────

    fn github_ctx_with_token(token: &str) -> TrackerContext {
        TrackerContext {
            product_id: "prod1".to_owned(),
            config: json!({
                "org": "spinyfin",
                "repo": "mono",
                "project_number": 1
            }),
            credential: super::super::TrackerCredential { token: token.to_owned() },
        }
    }

    /// Thin wrapper around `FakeGhRunner` that records the `token` argument
    /// from each call into a shared `Arc<Mutex<...>>` so the test can read it
    /// back after the runner is consumed into a `Box<dyn GhRunner>`.
    struct TokenCapturingRunner {
        inner: FakeGhRunner,
        captured: std::sync::Arc<Mutex<Option<Option<String>>>>,
    }

    impl TokenCapturingRunner {
        fn new_with_capture(
            inner: FakeGhRunner,
        ) -> (Self, std::sync::Arc<Mutex<Option<Option<String>>>>) {
            let captured = std::sync::Arc::new(Mutex::new(None));
            let r = Self { inner, captured: captured.clone() };
            (r, captured)
        }
    }

    #[async_trait]
    impl GhRunner for TokenCapturingRunner {
        async fn graphql(
            &self,
            query: &str,
            vars: &[(&str, &str)],
            token: Option<&str>,
        ) -> std::result::Result<Value, GhRunnerError> {
            *self.captured.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.inner.graphql(query, vars, token).await
        }

        async fn rest_get(
            &self,
            path: &str,
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.captured.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.inner.rest_get(path, token).await
        }

        async fn rest_patch(
            &self,
            path: &str,
            fields: &[(&str, &str)],
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.captured.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.inner.rest_patch(path, fields, token).await
        }

        async fn rest_post(
            &self,
            path: &str,
            body: &serde_json::Value,
            token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            *self.captured.lock().unwrap() = Some(token.map(|t| t.to_owned()));
            self.inner.rest_post(path, body, token).await
        }
    }

    #[tokio::test]
    async fn fetch_items_passes_token_from_context() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(vec![], false, ""));
        let (runner, captured) = TokenCapturingRunner::new_with_capture(fake);
        let tracker = GitHubTracker::with_runner(runner);
        let _ = tracker.fetch_items(&github_ctx_with_token("ghp_oauth_token_abc")).await;
        assert_eq!(
            *captured.lock().unwrap(),
            Some(Some("ghp_oauth_token_abc".to_owned())),
            "fetch_items should pass the credential token to the runner"
        );
    }

    #[tokio::test]
    async fn fetch_items_passes_no_token_when_ambient() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(vec![], false, ""));
        let (runner, captured) = TokenCapturingRunner::new_with_capture(fake);
        let tracker = GitHubTracker::with_runner(runner);
        let _ = tracker.fetch_items(&github_ctx()).await;
        assert_eq!(
            *captured.lock().unwrap(),
            Some(None),
            "fetch_items should pass None when credential is ambient"
        );
    }

    // ── fetch_items: integration test against fixture ─────────────────────────

    #[tokio::test]
    async fn fetch_items_single_page_from_fixture() {
        let fixture: Value = serde_json::from_str(include_str!(
            "testdata/github_fetch_items_single_page.json"
        ))
        .expect("fixture must be valid JSON");

        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(fixture);

        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");

        // Fixture has 2 Issues + 1 DraftIssue (skipped).
        assert_eq!(items.len(), 2, "expected 2 issues, got {}", items.len());

        let open = &items[0];
        assert_eq!(open.upstream_ref.canonical_id, "spinyfin/mono#560");
        assert_eq!(open.title, "Implement external tracker sync");
        assert_eq!(open.status, UpstreamStatus::Open);
        assert_eq!(open.labels, ["enhancement", "boss"]);
        assert_eq!(open.assignees, ["brianduff"]);
        assert!(open.pr_associations.is_empty());

        let closed = &items[1];
        assert_eq!(closed.upstream_ref.canonical_id, "spinyfin/mono#561");
        assert!(matches!(
            closed.status,
            UpstreamStatus::Closed { reason: ClosedReason::Completed }
        ));
        assert_eq!(closed.pr_associations.len(), 1);
        assert!(closed.pr_associations[0].merged);
    }

    // ── fetch_items: pagination ───────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_items_follows_pagination_cursor() {
        let mut fake = FakeGhRunner::new();
        // Page 1: one item, more to come.
        fake.push_graphql_ok(graphql_page(
            vec![open_issue_node("id1", 1, "First", &[])],
            true,
            "cursor_abc",
        ));
        // Page 2: one item, done.
        fake.push_graphql_ok(graphql_page(
            vec![open_issue_node("id2", 2, "Second", &[])],
            false,
            "cursor_xyz",
        ));

        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0].upstream_ref.canonical_id, "spinyfin/mono#1");
        assert_eq!(items[1].upstream_ref.canonical_id, "spinyfin/mono#2");
    }

    // ── fetch_items: label filter ─────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_items_label_filter_excludes_non_matching() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(
            vec![
                open_issue_node("id1", 1, "Matching", &["boss", "enhancement"]),
                open_issue_node("id2", 2, "Non-matching", &["ui"]),
            ],
            false,
            "c",
        ));
        let tracker = GitHubTracker::with_runner(fake);
        let ctx = github_ctx_with_label_filter(&["boss"]);
        let items = tracker.fetch_items(&ctx).await.expect("fetch_items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].upstream_ref.canonical_id, "spinyfin/mono#1");
    }

    #[tokio::test]
    async fn fetch_items_label_filter_includes_matching() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(
            vec![
                open_issue_node("id1", 1, "Has label", &["bug"]),
                open_issue_node("id2", 2, "Also has label", &["bug", "critical"]),
            ],
            false,
            "c",
        ));
        let tracker = GitHubTracker::with_runner(fake);
        let ctx = github_ctx_with_label_filter(&["bug"]);
        let items = tracker.fetch_items(&ctx).await.expect("fetch_items");
        assert_eq!(items.len(), 2);
    }

    // ── fetch_items: empty project ─────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_items_empty_project_returns_empty_vec() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(vec![], false, ""));
        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");
        assert!(items.is_empty());
    }

    // ── fetch_items: non-Issue content is skipped ─────────────────────────────

    #[tokio::test]
    async fn fetch_items_skips_non_issue_content_types() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(
            vec![
                json!({ "id": "d1", "content": { "__typename": "DraftIssue" } }),
                json!({ "id": "p1", "content": { "__typename": "PullRequest" } }),
                open_issue_node("i1", 42, "Real issue", &[]),
            ],
            false,
            "c",
        ));
        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].upstream_ref.canonical_id, "spinyfin/mono#42");
    }

    // ── fetch_items: gh itself fails (network / auth) ─────────────────────────

    #[tokio::test]
    async fn fetch_items_returns_auth_error_on_403_from_gh() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_err(403, "Forbidden (HTTP 403)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker.fetch_items(&github_ctx()).await.expect_err("should fail");
        assert!(matches!(err, TrackerError::Auth(_)), "{err:?}");
    }

    #[tokio::test]
    async fn fetch_items_returns_transient_on_network_failure() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_err(503, "Service Unavailable (HTTP 503)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker.fetch_items(&github_ctx()).await.expect_err("should fail");
        assert!(matches!(err, TrackerError::Transient(_)), "{err:?}");
    }

    // ── fetch_items: GraphQL error surfaced ───────────────────────────────────

    #[tokio::test]
    async fn fetch_items_graphql_not_found_returns_config_invalid() {
        let mut fake = FakeGhRunner::new();
        // Successful HTTP but GraphQL NOT_FOUND error.
        fake.push_graphql_ok(json!({
            "data": null,
            "errors": [{"message": "Could not resolve to a ProjectV2", "type": "NOT_FOUND"}]
        }));
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker.fetch_items(&github_ctx()).await.expect_err("should fail");
        assert!(matches!(err, TrackerError::ConfigInvalid(_)), "{err:?}");
    }

    // ── fetch_item: 200 success ────────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_item_returns_item_on_200() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_get_ok(rest_issue(560, "open", None));
        let tracker = GitHubTracker::with_runner(fake);
        let result = tracker
            .fetch_item(&github_ctx(), &issue_ref(560))
            .await
            .expect("fetch_item");
        assert!(result.is_some());
        let item = result.unwrap();
        assert_eq!(item.upstream_ref.canonical_id, "spinyfin/mono#560");
        assert_eq!(item.status, UpstreamStatus::Open);
    }

    // ── fetch_item: 404 not found ─────────────────────────────────────────────

    #[tokio::test]
    async fn fetch_item_returns_none_on_404() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_get_err(404, "Not Found (HTTP 404)");
        let tracker = GitHubTracker::with_runner(fake);
        let result = tracker
            .fetch_item(&github_ctx(), &issue_ref(999))
            .await
            .expect("fetch_item should not error on 404");
        assert!(result.is_none());
    }

    // ── fetch_item: 500 server error ──────────────────────────────────────────

    #[tokio::test]
    async fn fetch_item_returns_transient_on_5xx() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_get_err(500, "Internal Server Error (HTTP 500)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .fetch_item(&github_ctx(), &issue_ref(1))
            .await
            .expect_err("should fail");
        assert!(matches!(err, TrackerError::Transient(_)), "{err:?}");
    }

    // ── close_issue: success ──────────────────────────────────────────────────

    #[tokio::test]
    async fn close_issue_succeeds_on_200() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_patch_ok(rest_issue(560, "closed", Some("completed")));
        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .close_issue(&github_ctx(), &issue_ref(560), CloseReason::Completed)
            .await
            .expect("close_issue");
    }

    // ── close_issue: already closed (idempotent 200) ──────────────────────────

    #[tokio::test]
    async fn close_issue_already_closed_is_idempotent_success() {
        // GitHub returns 200 even when an issue is already closed.
        let mut fake = FakeGhRunner::new();
        fake.push_rest_patch_ok(rest_issue(560, "closed", Some("completed")));
        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .close_issue(&github_ctx(), &issue_ref(560), CloseReason::Completed)
            .await
            .expect("close_issue on already-closed issue should succeed");
    }

    // ── close_issue: permission denied (403) ──────────────────────────────────

    #[tokio::test]
    async fn close_issue_returns_permission_denied_on_403() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_patch_err(403, "Forbidden (HTTP 403)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .close_issue(&github_ctx(), &issue_ref(560), CloseReason::Completed)
            .await
            .expect_err("should fail");
        assert!(matches!(err, TrackerError::PermissionDenied(_)), "{err:?}");
    }

    // ── close_issue: not found (404) is treated as already-closed ────────────

    #[tokio::test]
    async fn close_issue_returns_ok_on_404() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_patch_err(404, "Not Found (HTTP 404)");
        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .close_issue(&github_ctx(), &issue_ref(999), CloseReason::Completed)
            .await
            .expect("404 on close should be treated as already-closed (Ok)");
    }

    // ── close_issue: transient 5xx ────────────────────────────────────────────

    #[tokio::test]
    async fn close_issue_returns_transient_on_5xx() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_patch_err(503, "Service Unavailable (HTTP 503)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .close_issue(&github_ctx(), &issue_ref(560), CloseReason::Completed)
            .await
            .expect_err("should fail");
        assert!(matches!(err, TrackerError::Transient(_)), "{err:?}");
    }

    // ── validate_config ───────────────────────────────────────────────────────

    #[test]
    fn validate_config_accepts_valid_config() {
        let tracker = GitHubTracker::new();
        let config = json!({
            "org": "spinyfin",
            "repo": "mono",
            "project_number": 1
        });
        tracker.validate_config(&config).expect("valid config");
    }

    #[test]
    fn validate_config_rejects_missing_org() {
        let tracker = GitHubTracker::new();
        let config = json!({ "repo": "mono", "project_number": 1 });
        let err = tracker.validate_config(&config).expect_err("should fail");
        assert!(err.message.contains("org"), "{}", err.message);
    }

    #[test]
    fn validate_config_rejects_missing_repo() {
        let tracker = GitHubTracker::new();
        let config = json!({ "org": "spinyfin", "project_number": 1 });
        let err = tracker.validate_config(&config).expect_err("should fail");
        assert!(err.message.contains("repo"), "{}", err.message);
    }

    #[test]
    fn validate_config_rejects_missing_project_number() {
        let tracker = GitHubTracker::new();
        let config = json!({ "org": "spinyfin", "repo": "mono" });
        let err = tracker.validate_config(&config).expect_err("should fail");
        assert!(err.message.contains("project_number"), "{}", err.message);
    }

    #[test]
    fn validate_config_rejects_non_object() {
        let tracker = GitHubTracker::new();
        let err = tracker.validate_config(&json!("not an object")).expect_err("should fail");
        assert!(err.message.contains("object"), "{}", err.message);
    }

    // ── parse_project_item: project_status from fieldValues ──────────────────

    #[tokio::test]
    async fn fetch_items_parses_project_status_from_field_values() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(graphql_page(
            vec![open_issue_node_with_status("id1", 1, "Issue 1", "In Progress")],
            false,
            "c",
        ));
        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].project_status.as_deref(), Some("In Progress"));
    }

    #[tokio::test]
    async fn fetch_items_project_status_none_when_status_field_absent() {
        let mut fake = FakeGhRunner::new();
        // Node has no fieldValues — project_status should be None.
        let node = json!({
            "id": "id1",
            "content": {
                "__typename": "Issue",
                "number": 1,
                "title": "Issue without fieldValues",
                "body": "",
                "state": "OPEN",
                "stateReason": null,
                "url": "https://github.com/spinyfin/mono/issues/1",
                "repository": { "nameWithOwner": "spinyfin/mono" },
                "labels": { "nodes": [] },
                "assignees": { "nodes": [] },
                "closedByPullRequestsReferences": { "nodes": [] },
                "updatedAt": "2026-05-17T10:00:00Z"
            }
        });
        fake.push_graphql_ok(graphql_page(vec![node], false, "c"));
        let tracker = GitHubTracker::with_runner(fake);
        let items = tracker.fetch_items(&github_ctx()).await.expect("fetch_items");
        assert_eq!(items.len(), 1);
        assert!(items[0].project_status.is_none(), "project_status should be None when fieldValues absent");
    }

    // ── set_project_status ────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_project_status_succeeds_with_valid_project_and_field() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(project_metadata_response(
            "PVT_project1",
            "PVTSSF_field1",
            &[("opt_todo", "Todo"), ("opt_wip", "In Progress"), ("opt_done", "Done")],
        ));
        fake.push_graphql_ok(set_field_mutation_ok("PVTI_item1"));

        let tracker = GitHubTracker::with_runner(fake);
        let ref_ = issue_ref(560);
        tracker
            .set_project_status(&github_ctx(), &ref_)
            .await
            .expect("set_project_status should succeed");
    }

    #[tokio::test]
    async fn set_project_status_uses_custom_column_name() {
        let ctx = TrackerContext {
            product_id: "prod1".to_owned(),
            config: serde_json::json!({
                "org": "spinyfin",
                "repo": "mono",
                "project_number": 1,
                "in_progress_column": "Doing"
            }),
            credential: super::super::TrackerCredential::ambient(),
        };
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_ok(project_metadata_response(
            "PVT_project1",
            "PVTSSF_field1",
            &[("opt_todo", "Todo"), ("opt_doing", "Doing"), ("opt_done", "Done")],
        ));
        fake.push_graphql_ok(set_field_mutation_ok("PVTI_item1"));

        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .set_project_status(&ctx, &issue_ref(1))
            .await
            .expect("set_project_status with custom column should succeed");
    }

    #[tokio::test]
    async fn set_project_status_returns_config_invalid_when_option_not_found() {
        let mut fake = FakeGhRunner::new();
        // The Status field exists but has no "In Progress" option.
        fake.push_graphql_ok(project_metadata_response(
            "PVT_project1",
            "PVTSSF_field1",
            &[("opt_todo", "Todo"), ("opt_done", "Done")],
        ));

        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .set_project_status(&github_ctx(), &issue_ref(560))
            .await
            .expect_err("should fail when option not found");
        assert!(
            matches!(err, TrackerError::ConfigInvalid(_)),
            "expected ConfigInvalid, got {err:?}"
        );
    }

    #[tokio::test]
    async fn set_project_status_returns_config_invalid_on_missing_project_item_id() {
        let ref_without_item_id = UpstreamRef {
            kind: "github".to_owned(),
            canonical_id: "spinyfin/mono#1".to_owned(),
            raw: serde_json::json!({ "issue_number": 1 }), // no project_item_id
        };
        let fake = FakeGhRunner::new();
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .set_project_status(&github_ctx(), &ref_without_item_id)
            .await
            .expect_err("should fail when project_item_id missing");
        assert!(matches!(err, TrackerError::ConfigInvalid(_)));
    }

    #[tokio::test]
    async fn set_project_status_returns_transient_on_metadata_5xx() {
        let mut fake = FakeGhRunner::new();
        fake.push_graphql_err(503, "Service Unavailable (HTTP 503)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .set_project_status(&github_ctx(), &issue_ref(1))
            .await
            .expect_err("should fail on 5xx");
        assert!(matches!(err, TrackerError::Transient(_)), "{err:?}");
    }

    // ── add_label ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn add_label_succeeds_on_200() {
        let mut fake = FakeGhRunner::new();
        // GitHub returns the resulting label set on success.
        fake.push_rest_post_ok(json!([{"name": "tracked"}]));
        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .add_label(&github_ctx(), &issue_ref(560), "tracked")
            .await
            .expect("add_label should succeed");
    }

    #[tokio::test]
    async fn add_label_treats_404_as_success() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_post_err(404, "Not Found (HTTP 404)");
        let tracker = GitHubTracker::with_runner(fake);
        tracker
            .add_label(&github_ctx(), &issue_ref(999), "tracked")
            .await
            .expect("404 should be treated as success");
    }

    #[tokio::test]
    async fn add_label_returns_permission_denied_on_403() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_post_err(403, "Forbidden (HTTP 403)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .add_label(&github_ctx(), &issue_ref(560), "tracked")
            .await
            .expect_err("should fail on 403");
        assert!(matches!(err, TrackerError::PermissionDenied(_)), "{err:?}");
    }

    #[tokio::test]
    async fn add_label_returns_transient_on_5xx() {
        let mut fake = FakeGhRunner::new();
        fake.push_rest_post_err(503, "Service Unavailable (HTTP 503)");
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .add_label(&github_ctx(), &issue_ref(560), "tracked")
            .await
            .expect_err("should fail on 5xx");
        assert!(matches!(err, TrackerError::Transient(_)), "{err:?}");
    }

    #[tokio::test]
    async fn add_label_returns_config_invalid_on_missing_issue_number() {
        let ref_without_number = UpstreamRef {
            kind: "github".to_owned(),
            canonical_id: "spinyfin/mono#1".to_owned(),
            raw: json!({}), // no issue_number
        };
        let fake = FakeGhRunner::new();
        let tracker = GitHubTracker::with_runner(fake);
        let err = tracker
            .add_label(&github_ctx(), &ref_without_number, "tracked")
            .await
            .expect_err("should fail when issue_number missing");
        assert!(matches!(err, TrackerError::ConfigInvalid(_)), "{err:?}");
    }

    // ── add_label: request-body shape (regression guard for T630) ────────────
    //
    // This test asserts on the actual JSON body passed to rest_post, not just
    // the Rust-side data structure. A regression to `-f labels=tracked` (sending
    // a bare string) would make this test fail even if all higher-level tests pass.

    /// A GhRunner that records every body passed to rest_post and always returns Ok.
    struct CapturingGhRunner {
        post_bodies: std::sync::Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl CapturingGhRunner {
        fn new() -> (Self, std::sync::Arc<Mutex<Vec<serde_json::Value>>>) {
            let bodies = std::sync::Arc::new(Mutex::new(Vec::new()));
            (Self { post_bodies: bodies.clone() }, bodies)
        }
    }

    #[async_trait]
    impl GhRunner for CapturingGhRunner {
        async fn graphql(
            &self,
            _query: &str,
            _vars: &[(&str, &str)],
            _token: Option<&str>,
        ) -> std::result::Result<Value, GhRunnerError> {
            unimplemented!("CapturingGhRunner only supports rest_post")
        }

        async fn rest_get(
            &self,
            _path: &str,
            _token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            unimplemented!("CapturingGhRunner only supports rest_post")
        }

        async fn rest_patch(
            &self,
            _path: &str,
            _fields: &[(&str, &str)],
            _token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            unimplemented!("CapturingGhRunner only supports rest_post")
        }

        async fn rest_post(
            &self,
            _path: &str,
            body: &serde_json::Value,
            _token: Option<&str>,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            self.post_bodies.lock().unwrap().push(body.clone());
            Ok(GhResponse { body: json!([{"name": "tracked"}]) })
        }
    }

    #[tokio::test]
    async fn add_label_request_body_sends_labels_as_json_array_not_string() {
        // Regression guard for T630: the GitHub labels API requires `labels`
        // to be a JSON array. Using `-f labels=tracked` with `gh api` sends
        // the value as a bare string, causing HTTP 422. This test checks the
        // actual body the code sends, not just the Rust-level data structure.
        let (runner, bodies) = CapturingGhRunner::new();
        let tracker = GitHubTracker::with_runner(runner);
        tracker
            .add_label(&github_ctx(), &issue_ref(560), "tracked")
            .await
            .expect("add_label should succeed");

        let bodies = bodies.lock().unwrap();
        assert_eq!(bodies.len(), 1, "exactly one POST should be made");
        let body = &bodies[0];
        assert!(
            body["labels"].is_array(),
            "labels must be a JSON array, got: {}",
            body["labels"]
        );
        assert_eq!(
            body["labels"],
            json!(["tracked"]),
            "labels array must contain exactly 'tracked'"
        );
    }

    // ── parse_iso8601 ──────────────────────────────────────────────────────────

    #[test]
    fn parse_iso8601_known_epoch() {
        // 1970-01-01T00:00:00Z == 0
        assert_eq!(parse_iso8601("1970-01-01T00:00:00Z"), Some(0));
    }

    #[test]
    fn parse_iso8601_known_date() {
        // 2026-05-17T10:00:00Z == 1_779_012_000
        // (verified: 2026-01-01 starts at day 20454 since epoch; +136 days to May 17;
        //  × 86400 + 36000 seconds for 10:00:00 UTC = 1_779_012_000)
        let ts = parse_iso8601("2026-05-17T10:00:00Z").expect("valid timestamp");
        let expected: i64 = 1_779_012_000;
        assert_eq!(ts, expected, "ts={ts} expected={expected}");
    }

    #[test]
    fn parse_iso8601_rejects_malformed() {
        assert_eq!(parse_iso8601("not-a-date"), None);
        assert_eq!(parse_iso8601(""), None);
    }
}
