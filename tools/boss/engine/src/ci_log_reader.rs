//! CI provider abstraction for log-fetching and retrigger
//! (`tools/boss/docs/designs/merge-conflict-handling-in-review.md`
//! §"CI provider abstraction" / Phase 9 #25-#26).
//!
//! Mono builds on Buildkite. Flunge builds on Buildkite *and* GitHub
//! Actions. CI-failure *detection* is provider-agnostic (GitHub's
//! `statusCheckRollup` aggregates both); CI-failure *fixing* requires
//! reading the failing job's log, which is provider-specific. This
//! module owns that seam.
//!
//! The trait surface is intentionally thin: read the job log tail
//! (pre-spawn excerpt), read the full log (worker deeper dive), and
//! retry the failing unit. A fourth method returns a hint string the
//! worker prompt can embed so the worker knows which CLI to shell out
//! to.
//!
//! Provider inference from `targetUrl` host lives on
//! [`merge_poller::CiProvider`]; this module re-exports the type and
//! adds the URL → id parsing helpers each impl needs to translate
//! GitHub's `targetUrl` into the provider-CLI arguments.
//!
//! The engine pre-spawn code (Phase 9 #27) and the retrigger
//! pre-triage (Phase 9 #28) compose these readers; the worker prompt
//! template (Phase 9 #29) embeds the `worker_cli_invocation_hint`
//! result. None of those callers ship in this commit — this module
//! is the seam they will pivot on.

use std::process::Stdio;

use anyhow::{Context, Result, anyhow};
use async_trait::async_trait;
use tokio::process::Command;

pub use crate::merge_poller::CiProvider;

/// Reader + retrigger surface for one CI provider's failing-job
/// signals. The engine implements one reader per known provider; an
/// unknown / fallback reader returns errors so the worker can
/// classify the failure as `unfixable`.
#[async_trait]
pub trait CiLogReader: Send + Sync {
    /// Read the tail of the failing job's log. `n_lines` is the
    /// maximum number of trailing lines to return; impls fetch the
    /// full log and trim locally because neither `bk` nor `gh`
    /// exposes a server-side tail flag.
    async fn read_log_tail(&self, job_id: &str, n_lines: usize) -> Result<String>;

    /// Read the full job log (for the worker's deeper dive).
    async fn read_log_full(&self, job_id: &str) -> Result<String>;

    /// Re-trigger a failed job. The `id` argument is the
    /// provider-appropriate parent identifier — the **build id** for
    /// Buildkite (`bk build retry <build-id>`) and the **run id** for
    /// GitHub Actions (`gh run rerun <run-id> --failed`). Callers
    /// extract the correct id from the failing check's `target_url`
    /// using [`parse_buildkite_build_id`] / [`parse_gha_run_id`].
    /// Returns a provider-emitted identifier for the new run/build
    /// (or the same id, for GHA, which re-uses the run id).
    async fn retrigger(&self, id: &str) -> Result<String>;

    /// Identifier-bearing CLI invocation hint the worker prompt
    /// embeds so the worker knows exactly which CLI to shell out to
    /// for a deeper look at the job. The string is informational and
    /// human-readable; not parsed back.
    fn worker_cli_invocation_hint(&self, job_id: &str) -> String;
}

/// `CiLogReader` for Buildkite. Wraps `bk job log <job-uuid>` (for
/// reads) and `bk build retry <build-id>` (for the retrigger path).
/// The binary path defaults to `"bk"`; tests substitute a fake script
/// via [`Self::with_binary`].
#[derive(Debug, Clone)]
pub struct BuildkiteLogReader {
    binary: String,
}

impl Default for BuildkiteLogReader {
    fn default() -> Self {
        Self::new()
    }
}

impl BuildkiteLogReader {
    pub fn new() -> Self {
        Self {
            binary: "bk".to_owned(),
        }
    }

    /// Override the `bk` binary path. Used in tests to inject a fake
    /// script that returns canned responses.
    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

#[async_trait]
impl CiLogReader for BuildkiteLogReader {
    async fn read_log_tail(&self, job_id: &str, n_lines: usize) -> Result<String> {
        let full = self.read_log_full(job_id).await?;
        Ok(tail_lines(&full, n_lines))
    }

    async fn read_log_full(&self, job_id: &str) -> Result<String> {
        run_capture(&self.binary, &["job", "log", job_id]).await
    }

    async fn retrigger(&self, id: &str) -> Result<String> {
        let out = run_capture(&self.binary, &["build", "retry", id]).await?;
        Ok(out.trim().to_owned())
    }

    fn worker_cli_invocation_hint(&self, job_id: &str) -> String {
        format!("bk job log {job_id}")
    }
}

/// `CiLogReader` for GitHub Actions. Wraps `gh run view --log-failed
/// --job <job-id>` (for reads) and `gh run rerun <run-id> --failed`
/// (for the retrigger path). The binary path defaults to `"gh"`;
/// tests substitute a fake via [`Self::with_binary`].
#[derive(Debug, Clone)]
pub struct GithubActionsLogReader {
    binary: String,
}

impl Default for GithubActionsLogReader {
    fn default() -> Self {
        Self::new()
    }
}

impl GithubActionsLogReader {
    pub fn new() -> Self {
        Self {
            binary: "gh".to_owned(),
        }
    }

    pub fn with_binary(binary: impl Into<String>) -> Self {
        Self {
            binary: binary.into(),
        }
    }
}

#[async_trait]
impl CiLogReader for GithubActionsLogReader {
    async fn read_log_tail(&self, job_id: &str, n_lines: usize) -> Result<String> {
        let full = self.read_log_full(job_id).await?;
        Ok(tail_lines(&full, n_lines))
    }

    async fn read_log_full(&self, job_id: &str) -> Result<String> {
        run_capture(
            &self.binary,
            &["run", "view", "--log-failed", "--job", job_id],
        )
        .await
    }

    async fn retrigger(&self, id: &str) -> Result<String> {
        // `gh run rerun <run-id> --failed` re-runs failed jobs of an
        // existing workflow run; GHA reuses the same run id, so we
        // surface it back to the caller as the new id.
        run_capture(&self.binary, &["run", "rerun", id, "--failed"]).await?;
        Ok(id.to_owned())
    }

    fn worker_cli_invocation_hint(&self, job_id: &str) -> String {
        format!("gh run view --log-failed --job {job_id}")
    }
}

/// Fallback reader for `CiProvider::Other`. Every method returns an
/// error; the worker's pre-spawn triage uses that to classify the
/// failure as `unfixable` without consuming the per-PR budget. A real
/// third provider ships its own impl.
#[derive(Debug, Clone, Default)]
pub struct UnknownProviderReader;

#[async_trait]
impl CiLogReader for UnknownProviderReader {
    async fn read_log_tail(&self, _job_id: &str, _n_lines: usize) -> Result<String> {
        Err(anyhow!("unknown CI provider: no log reader available"))
    }

    async fn read_log_full(&self, _job_id: &str) -> Result<String> {
        Err(anyhow!("unknown CI provider: no log reader available"))
    }

    async fn retrigger(&self, _id: &str) -> Result<String> {
        Err(anyhow!("unknown CI provider: cannot retrigger"))
    }

    fn worker_cli_invocation_hint(&self, _job_id: &str) -> String {
        "(no CLI available for this CI provider)".to_owned()
    }
}

/// Build a boxed reader for `provider`. Convenience factory the
/// engine pre-spawn / pre-triage code uses to dispatch on the
/// provider inferred from `target_url`.
pub fn reader_for(provider: CiProvider) -> Box<dyn CiLogReader> {
    match provider {
        CiProvider::Buildkite => Box::new(BuildkiteLogReader::new()),
        CiProvider::GithubActions => Box::new(GithubActionsLogReader::new()),
        CiProvider::Other => Box::new(UnknownProviderReader),
    }
}

/// Extract the Buildkite build id from a `targetUrl`. Buildkite job
/// pages look like
/// `https://buildkite.com/<org>/<pipeline>/builds/<n>#<job-uuid>`;
/// the build id is the path segment after `/builds/`. Returns `None`
/// when the URL doesn't match the canonical shape.
pub fn parse_buildkite_build_id(url: &str) -> Option<String> {
    let after_builds = url.split_once("/builds/")?.1;
    // Strip query/fragment and any trailing path segments.
    let id = after_builds
        .split(['#', '?', '/'])
        .next()
        .unwrap_or(after_builds);
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Extract the Buildkite job UUID from a `targetUrl`. Job UUIDs ride
/// in the URL fragment (`…/builds/<n>#<job-uuid>`).
pub fn parse_buildkite_job_id(url: &str) -> Option<String> {
    let (_, frag) = url.split_once('#')?;
    if frag.is_empty() { None } else { Some(frag.to_owned()) }
}

/// Extract the GitHub Actions run id from a `targetUrl`. Run id is
/// the path segment after `/runs/`.
pub fn parse_gha_run_id(url: &str) -> Option<String> {
    let after_runs = url.split_once("/runs/")?.1;
    let id = after_runs
        .split(['/', '?', '#'])
        .next()
        .unwrap_or(after_runs);
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Extract the GitHub Actions job id from a `targetUrl`. Job id is
/// the path segment after `/job/`.
pub fn parse_gha_job_id(url: &str) -> Option<String> {
    let stripped = url.split('?').next().unwrap_or(url);
    let stripped = stripped.split('#').next().unwrap_or(stripped);
    let (_, tail) = stripped.rsplit_once("/job/")?;
    let id = tail.trim_end_matches('/');
    if id.is_empty() { None } else { Some(id.to_owned()) }
}

/// Trim `s` to its last `n` lines (preserving order). Used by the
/// `read_log_tail` impls — neither provider CLI exposes a server-side
/// tail, so we fetch all and trim locally.
fn tail_lines(s: &str, n: usize) -> String {
    if n == 0 {
        return String::new();
    }
    let lines: Vec<&str> = s.lines().collect();
    if lines.len() <= n {
        return s.trim_end_matches('\n').to_owned();
    }
    let start = lines.len() - n;
    lines[start..].join("\n")
}

/// Shell out to `binary` with `args`, capture stdout. Returns an
/// error annotated with the binary + args + stderr on non-zero exit
/// so the caller's logs make the failure mode obvious.
async fn run_capture(binary: &str, args: &[&str]) -> Result<String> {
    let output = Command::new(binary)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .output()
        .await
        .with_context(|| format!("failed to spawn `{binary} {}`", args.join(" ")))?;
    if !output.status.success() {
        return Err(anyhow!(
            "`{binary} {}` failed (exit {:?}): {}",
            args.join(" "),
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------- URL parsing -------------------------------------------------

    #[test]
    fn buildkite_build_id_parses_from_canonical_url() {
        let url = "https://buildkite.com/myorg/mypipe/builds/12345#abc-def-ghi";
        assert_eq!(
            parse_buildkite_build_id(url).as_deref(),
            Some("12345")
        );
    }

    #[test]
    fn buildkite_build_id_without_fragment_still_parses() {
        let url = "https://buildkite.com/myorg/mypipe/builds/777";
        assert_eq!(parse_buildkite_build_id(url).as_deref(), Some("777"));
    }

    #[test]
    fn buildkite_build_id_returns_none_for_non_buildkite_url() {
        assert!(parse_buildkite_build_id("https://example.com/foo").is_none());
        assert!(parse_buildkite_build_id("").is_none());
    }

    #[test]
    fn buildkite_job_id_parses_from_fragment() {
        let url = "https://buildkite.com/o/p/builds/42#018f-1234-uuid";
        assert_eq!(
            parse_buildkite_job_id(url).as_deref(),
            Some("018f-1234-uuid")
        );
    }

    #[test]
    fn buildkite_job_id_returns_none_when_fragment_missing() {
        assert!(parse_buildkite_job_id("https://buildkite.com/o/p/builds/42").is_none());
        assert!(parse_buildkite_job_id("https://buildkite.com/o/p/builds/42#").is_none());
    }

    #[test]
    fn gha_run_id_parses_from_canonical_url() {
        let url = "https://github.com/owner/repo/actions/runs/9988776655/job/12345";
        assert_eq!(parse_gha_run_id(url).as_deref(), Some("9988776655"));
    }

    #[test]
    fn gha_run_id_parses_without_job_segment() {
        let url = "https://github.com/owner/repo/actions/runs/9988776655";
        assert_eq!(parse_gha_run_id(url).as_deref(), Some("9988776655"));
    }

    #[test]
    fn gha_run_id_returns_none_for_unrelated_url() {
        assert!(parse_gha_run_id("https://buildkite.com/x/y/builds/1").is_none());
    }

    #[test]
    fn gha_job_id_parses_from_canonical_url() {
        let url = "https://github.com/owner/repo/actions/runs/1/job/424242";
        assert_eq!(parse_gha_job_id(url).as_deref(), Some("424242"));
    }

    #[test]
    fn gha_job_id_strips_query_and_fragment() {
        let url = "https://github.com/owner/repo/actions/runs/1/job/424242?check_suite_focus=true#step:3";
        assert_eq!(parse_gha_job_id(url).as_deref(), Some("424242"));
    }

    #[test]
    fn gha_job_id_returns_none_without_job_segment() {
        assert!(parse_gha_job_id("https://github.com/o/r/actions/runs/1").is_none());
    }

    // ---------- tail_lines --------------------------------------------------

    #[test]
    fn tail_lines_returns_all_when_under_limit() {
        assert_eq!(tail_lines("a\nb\nc", 10), "a\nb\nc");
    }

    #[test]
    fn tail_lines_returns_last_n() {
        assert_eq!(tail_lines("a\nb\nc\nd\ne", 2), "d\ne");
    }

    #[test]
    fn tail_lines_zero_returns_empty() {
        assert_eq!(tail_lines("a\nb\nc", 0), "");
    }

    #[test]
    fn tail_lines_handles_trailing_newline() {
        assert_eq!(tail_lines("a\nb\nc\n", 10), "a\nb\nc");
    }

    // ---------- worker_cli_invocation_hint ----------------------------------

    #[test]
    fn buildkite_invocation_hint_embeds_job_id() {
        let r = BuildkiteLogReader::new();
        assert_eq!(r.worker_cli_invocation_hint("abc-uuid"), "bk job log abc-uuid");
    }

    #[test]
    fn gha_invocation_hint_embeds_job_id() {
        let r = GithubActionsLogReader::new();
        assert_eq!(
            r.worker_cli_invocation_hint("99"),
            "gh run view --log-failed --job 99"
        );
    }

    #[test]
    fn unknown_invocation_hint_is_descriptive() {
        let r = UnknownProviderReader;
        assert!(r.worker_cli_invocation_hint("ignored").contains("no CLI"));
    }

    // ---------- reader_for dispatch -----------------------------------------

    #[tokio::test]
    async fn reader_for_unknown_provider_errors_on_every_method() {
        let r = reader_for(CiProvider::Other);
        assert!(r.read_log_tail("j", 10).await.is_err());
        assert!(r.read_log_full("j").await.is_err());
        assert!(r.retrigger("j").await.is_err());
    }

    // ---------- integration: fake bk / fake gh ------------------------------
    //
    // The Buildkite and GHA impls shell out, so they're covered by
    // integration tests that drop a fake script in a temp dir and
    // point the reader at it. The script switches on its argv and
    // emits canned responses, exactly matching what each CLI prints
    // on success / failure paths.

    #[cfg(unix)]
    mod fake_cli_integration {
        use super::*;
        use std::os::unix::fs::PermissionsExt;
        use std::path::{Path, PathBuf};

        fn write_script(dir: &Path, name: &str, body: &str) -> PathBuf {
            let path = dir.join(name);
            std::fs::write(&path, body).expect("write fake script");
            let mut perms = std::fs::metadata(&path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(&path, perms).expect("chmod fake script");
            path
        }

        // ---- Buildkite ----------------------------------------------------

        const FAKE_BK: &str = r#"#!/bin/sh
# Synthetic `bk` for tests. Matches a subset of the real CLI:
#   bk job log <job-id>
#   bk build retry <build-id>
if [ "$1" = "job" ] && [ "$2" = "log" ]; then
    cat <<'LOG'
preamble line 1
preamble line 2
TEST FAILED at frob_bar_test.rs:42
last meaningful line
LOG
    exit 0
fi
if [ "$1" = "build" ] && [ "$2" = "retry" ]; then
    echo "new-build-id-$3-retried"
    exit 0
fi
if [ "$1" = "fail-on-purpose" ]; then
    echo "synthetic stderr" 1>&2
    exit 7
fi
echo "unhandled args: $@" 1>&2
exit 2
"#;

        #[tokio::test]
        async fn buildkite_read_log_full_returns_stdout() {
            let dir = tempfile::tempdir().unwrap();
            let bk = write_script(dir.path(), "bk", FAKE_BK);
            let reader = BuildkiteLogReader::with_binary(bk.to_str().unwrap());
            let log = reader.read_log_full("any-job-uuid").await.unwrap();
            assert!(log.contains("TEST FAILED at frob_bar_test.rs:42"));
            assert!(log.contains("preamble line 1"));
        }

        #[tokio::test]
        async fn buildkite_read_log_tail_trims_to_n_lines() {
            let dir = tempfile::tempdir().unwrap();
            let bk = write_script(dir.path(), "bk", FAKE_BK);
            let reader = BuildkiteLogReader::with_binary(bk.to_str().unwrap());
            let tail = reader.read_log_tail("any-job-uuid", 2).await.unwrap();
            assert_eq!(
                tail,
                "TEST FAILED at frob_bar_test.rs:42\nlast meaningful line"
            );
        }

        #[tokio::test]
        async fn buildkite_retrigger_returns_new_build_id() {
            let dir = tempfile::tempdir().unwrap();
            let bk = write_script(dir.path(), "bk", FAKE_BK);
            let reader = BuildkiteLogReader::with_binary(bk.to_str().unwrap());
            let id = reader.retrigger("123").await.unwrap();
            assert_eq!(id, "new-build-id-123-retried");
        }

        #[tokio::test]
        async fn buildkite_propagates_nonzero_exit_with_stderr() {
            let dir = tempfile::tempdir().unwrap();
            // Use a script that always exits 7 with a stderr message,
            // regardless of args. Easier than threading a "fail" mode
            // through the multi-arg script.
            let body = "#!/bin/sh\necho 'unauthorized' 1>&2\nexit 7\n";
            let bk = write_script(dir.path(), "bk-fails", body);
            let reader = BuildkiteLogReader::with_binary(bk.to_str().unwrap());
            let err = reader.read_log_full("j").await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("exit"), "missing exit code in: {msg}");
            assert!(msg.contains("unauthorized"), "missing stderr in: {msg}");
        }

        #[tokio::test]
        async fn buildkite_missing_binary_errors() {
            let reader = BuildkiteLogReader::with_binary("/definitely/not/a/real/bk-xyz");
            let err = reader.read_log_full("j").await.unwrap_err();
            assert!(format!("{err:#}").contains("failed to spawn"));
        }

        // ---- GitHub Actions ---------------------------------------------

        const FAKE_GH: &str = r#"#!/bin/sh
# Synthetic `gh` for tests. Matches:
#   gh run view --log-failed --job <job-id>
#   gh run rerun <run-id> --failed
if [ "$1" = "run" ] && [ "$2" = "view" ]; then
    cat <<'LOG'
2026-05-15T10:00:00Z my-job: starting
2026-05-15T10:00:01Z my-job: assertion failed
2026-05-15T10:00:02Z my-job: exiting with code 1
LOG
    exit 0
fi
if [ "$1" = "run" ] && [ "$2" = "rerun" ]; then
    # Real `gh` prints a short confirmation; we mimic that.
    echo "Created workflow_dispatch event for failing jobs of run $3"
    exit 0
fi
echo "unhandled args: $@" 1>&2
exit 2
"#;

        #[tokio::test]
        async fn gha_read_log_full_returns_stdout() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(dir.path(), "gh", FAKE_GH);
            let reader = GithubActionsLogReader::with_binary(gh.to_str().unwrap());
            let log = reader.read_log_full("99").await.unwrap();
            assert!(log.contains("assertion failed"));
        }

        #[tokio::test]
        async fn gha_read_log_tail_trims_to_n_lines() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(dir.path(), "gh", FAKE_GH);
            let reader = GithubActionsLogReader::with_binary(gh.to_str().unwrap());
            let tail = reader.read_log_tail("99", 1).await.unwrap();
            assert_eq!(tail, "2026-05-15T10:00:02Z my-job: exiting with code 1");
        }

        #[tokio::test]
        async fn gha_retrigger_returns_same_run_id() {
            let dir = tempfile::tempdir().unwrap();
            let gh = write_script(dir.path(), "gh", FAKE_GH);
            let reader = GithubActionsLogReader::with_binary(gh.to_str().unwrap());
            let id = reader.retrigger("99887766").await.unwrap();
            assert_eq!(id, "99887766");
        }

        #[tokio::test]
        async fn gha_propagates_nonzero_exit() {
            let dir = tempfile::tempdir().unwrap();
            let body = "#!/bin/sh\necho 'auth required' 1>&2\nexit 4\n";
            let gh = write_script(dir.path(), "gh-fails", body);
            let reader = GithubActionsLogReader::with_binary(gh.to_str().unwrap());
            let err = reader.read_log_full("99").await.unwrap_err();
            let msg = format!("{err:#}");
            assert!(msg.contains("auth required"), "stderr missing: {msg}");
        }
    }
}
