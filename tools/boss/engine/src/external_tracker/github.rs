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
    async fn graphql(
        &self,
        query: &str,
        vars: &[(&str, &str)],
    ) -> std::result::Result<Value, GhRunnerError>;

    /// Run `gh api <path>` (GET) and return parsed JSON body.
    async fn rest_get(&self, path: &str) -> std::result::Result<GhResponse, GhRunnerError>;

    /// Run `gh api -X PATCH <path> -f k=v ...` and return parsed JSON body.
    async fn rest_patch(
        &self,
        path: &str,
        fields: &[(&str, &str)],
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
    ) -> std::result::Result<Value, GhRunnerError> {
        let mut cmd = Command::new("gh");
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

    async fn rest_get(&self, path: &str) -> std::result::Result<GhResponse, GhRunnerError> {
        let output = Command::new("gh")
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
    ) -> std::result::Result<GhResponse, GhRunnerError> {
        let mut cmd = Command::new("gh");
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
        Some(401) | Some(403) => TrackerError::Auth(err.message),
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
                .graphql(GITHUB_GRAPHQL_QUERY, &vars)
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
        match self.runner.rest_get(&path).await {
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

        match self.runner.rest_patch(&path, &fields).await {
            Ok(_) => Ok(()),
            // 404: issue deleted or never existed; treat as already-closed (success).
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
    }

    impl FakeGhRunner {
        fn new() -> Self {
            Self {
                graphql_q: Mutex::new(VecDeque::new()),
                rest_get_q: Mutex::new(VecDeque::new()),
                rest_patch_q: Mutex::new(VecDeque::new()),
            }
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
    }

    #[async_trait]
    impl GhRunner for FakeGhRunner {
        async fn graphql(
            &self,
            _query: &str,
            _vars: &[(&str, &str)],
        ) -> std::result::Result<Value, GhRunnerError> {
            self.graphql_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no graphql response queued")
        }

        async fn rest_get(
            &self,
            _path: &str,
        ) -> std::result::Result<GhResponse, GhRunnerError> {
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
        ) -> std::result::Result<GhResponse, GhRunnerError> {
            self.rest_patch_q
                .lock()
                .unwrap()
                .pop_front()
                .expect("no rest_patch response queued")
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
