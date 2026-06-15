//! Shared implementation of the `cube` CLI command surface.
//!
//! Both [`CommandCubeClient`](crate::coordinator::CommandCubeClient) (which
//! shells out to a local `cube` binary) and
//! [`SshHostAdapter`](crate::host_adapter::SshHostAdapter) (which runs `cube`
//! over SSH on a remote host) need to invoke the same handful of cube
//! subcommands — `repo ensure`, `workspace lease`, `change create`,
//! `workspace release/status/heartbeat/force-release/list`, and `repo list`.
//!
//! Each of those is identical apart from the transport used to actually run
//! the command and collect its JSON output. To avoid maintaining two copies
//! of the argument-building and JSON-decoding logic, the command bodies live
//! here as free functions generic over a [`CubeJsonTransport`], and the two
//! `impl` blocks become thin wrappers that delegate to these helpers.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use async_trait::async_trait;
use serde::Deserialize;

use crate::coordinator::{CubeChangeHandle, CubeRepoHandle, CubeRepoSummary, CubeWorkspaceLease, CubeWorkspaceStatus};

/// A transport capable of running a `cube` invocation that emits JSON on
/// stdout and returning the parsed value.
///
/// The local client runs `cube` as a child process; the SSH adapter runs it
/// on a remote host. Both decode stdout into a [`serde_json::Value`]; the
/// command helpers below own everything past that point.
#[async_trait]
pub trait CubeJsonTransport: Send + Sync {
    /// Run `cube <args>` and return its parsed JSON stdout.
    async fn run_cube_json(&self, args: &[&str]) -> Result<serde_json::Value>;
}

// --- Wire payload structs shared by both transports -------------------------
//
// These small wrappers mirror the envelope cube wraps each subcommand's
// JSON in. The leaf records deserialize straight into the public
// `Cube*`/`CubeWorkspaceStatus` handle types from `coordinator` (which
// derive `Deserialize` with field names/renames matching the wire shape),
// so there is no second copy of those field sets to keep in sync.

#[derive(Deserialize)]
struct RepoEnsurePayload {
    repo_id: String,
}

#[derive(Deserialize)]
struct LeasePayload {
    workspace: LeaseWorkspace,
}

#[derive(Deserialize)]
struct LeaseWorkspace {
    lease_id: Option<String>,
    workspace_id: String,
    workspace_path: PathBuf,
}

#[derive(Deserialize)]
struct ChangePayload {
    change: ChangeRecord,
}

#[derive(Deserialize)]
struct ChangeRecord {
    change_id: String,
}

#[derive(Deserialize)]
struct StatusPayload {
    workspace: CubeWorkspaceStatus,
}

#[derive(Deserialize)]
struct ListWorkspacesPayload {
    workspaces: Vec<CubeWorkspaceStatus>,
}

#[derive(Deserialize)]
struct ListReposPayload {
    repos: Vec<CubeRepoSummary>,
}

// --- Command helpers --------------------------------------------------------

pub async fn ensure_repo<T: CubeJsonTransport + ?Sized>(transport: &T, origin: &str) -> Result<CubeRepoHandle> {
    let payload: RepoEnsurePayload = serde_json::from_value(
        transport
            .run_cube_json(&crate::repo_slug::repo_ensure_args(origin))
            .await?,
    )
    .context("decoding `cube repo ensure` payload")?;
    Ok(CubeRepoHandle {
        repo_id: payload.repo_id,
    })
}

pub async fn lease_workspace<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    repo_id: &str,
    task: &str,
    prefer_workspace_id: Option<&str>,
    allow_dirty: bool,
    exclude_workspace_ids: &[&str],
) -> Result<CubeWorkspaceLease> {
    let mut args: Vec<&str> = vec!["--json", "workspace", "lease", repo_id, "--task", task];
    if let Some(prefer) = prefer_workspace_id {
        args.extend_from_slice(&["--prefer", prefer]);
    }
    if allow_dirty {
        args.push("--allow-dirty");
    }
    for excluded in exclude_workspace_ids {
        args.extend_from_slice(&["--exclude", excluded]);
    }
    let payload: LeasePayload = serde_json::from_value(transport.run_cube_json(&args).await?)
        .context("decoding `cube workspace lease` payload")?;
    let lease_id = payload
        .workspace
        .lease_id
        .context("cube workspace lease response missing lease_id")?;
    Ok(CubeWorkspaceLease {
        lease_id,
        workspace_id: payload.workspace.workspace_id,
        workspace_path: payload.workspace.workspace_path,
    })
}

pub async fn create_change<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    workspace_path: &Path,
    title: &str,
) -> Result<CubeChangeHandle> {
    let workspace_arg = workspace_path.display().to_string();
    let payload: ChangePayload = serde_json::from_value(
        transport
            .run_cube_json(&[
                "--json",
                "change",
                "create",
                "--workspace",
                workspace_arg.as_str(),
                "--title",
                title,
            ])
            .await?,
    )
    .context("decoding `cube change create` payload")?;
    Ok(CubeChangeHandle {
        change_id: payload.change.change_id,
    })
}

pub async fn release_workspace<T: CubeJsonTransport + ?Sized>(transport: &T, lease_id: &str) -> Result<()> {
    let _ = transport
        .run_cube_json(&["--json", "workspace", "release", "--lease", lease_id])
        .await?;
    Ok(())
}

pub async fn workspace_status<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    workspace_path: &Path,
) -> Result<CubeWorkspaceStatus> {
    let workspace_arg = workspace_path.display().to_string();
    let payload: StatusPayload = serde_json::from_value(
        transport
            .run_cube_json(&["--json", "workspace", "status", "--workspace", workspace_arg.as_str()])
            .await?,
    )
    .context("decoding `cube workspace status` payload")?;
    Ok(payload.workspace)
}

pub async fn heartbeat_lease<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    lease_id: &str,
    ttl_seconds: Option<u64>,
) -> Result<()> {
    let ttl_string = ttl_seconds.map(|ttl| ttl.to_string());
    let mut args: Vec<&str> = vec!["--json", "workspace", "heartbeat", "--lease", lease_id];
    if let Some(ttl) = ttl_string.as_deref() {
        args.extend_from_slice(&["--ttl-seconds", ttl]);
    }
    let _ = transport.run_cube_json(&args).await?;
    Ok(())
}

pub async fn force_release_lease<T: CubeJsonTransport + ?Sized>(
    transport: &T,
    lease_id: &str,
    reason: Option<&str>,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["--json", "workspace", "force-release", "--lease", lease_id];
    if let Some(reason) = reason {
        args.extend_from_slice(&["--reason", reason]);
    }
    let _ = transport.run_cube_json(&args).await?;
    Ok(())
}

pub async fn list_workspaces<T: CubeJsonTransport + ?Sized>(transport: &T) -> Result<Vec<CubeWorkspaceStatus>> {
    let payload: ListWorkspacesPayload =
        serde_json::from_value(transport.run_cube_json(&["--json", "workspace", "list"]).await?)
            .context("decoding `cube workspace list` payload")?;
    Ok(payload.workspaces)
}

pub async fn list_repos<T: CubeJsonTransport + ?Sized>(transport: &T) -> Result<Vec<CubeRepoSummary>> {
    let payload: ListReposPayload = serde_json::from_value(transport.run_cube_json(&["--json", "repo", "list"]).await?)
        .context("decoding `cube repo list` payload")?;
    Ok(payload.repos)
}
