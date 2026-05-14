use std::path::PathBuf;

use serde::Serialize;

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoRecord {
    pub repo: String,
    pub origin: String,
    pub main_branch: String,
    pub workspace_root: PathBuf,
    pub workspace_prefix: String,
    pub source: Option<PathBuf>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceState {
    Free,
    Leased,
}

impl WorkspaceState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Free => "free",
            Self::Leased => "leased",
        }
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "free" => Some(Self::Free),
            "leased" => Some(Self::Leased),
            _ => None,
        }
    }
}

/// Cached on-disk health of a free workspace. Written by the lease handler
/// when it health-checks candidates, and persisted so `cube workspace list`
/// can surface it without running `jj status` on every workspace.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkspaceHealth {
    /// Working copy is clean and no bookmarks are in conflict.
    Clean,
    /// Working copy has uncommitted changes from a prior worker session.
    Dirty,
    /// One or more bookmarks are in a conflicted state; working copy itself is empty.
    Conflicted,
}

impl WorkspaceHealth {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Clean => "clean",
            Self::Dirty => "dirty",
            Self::Conflicted => "conflicted",
        }
    }

    pub fn from_str(raw: &str) -> Option<Self> {
        match raw {
            "clean" => Some(Self::Clean),
            "dirty" => Some(Self::Dirty),
            "conflicted" => Some(Self::Conflicted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct WorkspaceRecord {
    pub repo: String,
    pub workspace_id: String,
    pub workspace_path: PathBuf,
    pub state: WorkspaceState,
    pub lease_id: Option<String>,
    pub holder: Option<String>,
    pub task: Option<String>,
    pub leased_at_epoch_s: Option<i64>,
    pub lease_expires_at_epoch_s: Option<i64>,
    pub head_commit: Option<String>,
    pub last_release_reason: Option<String>,
    /// Last-known health status, written by the lease health-check phase.
    /// `None` means health has not been checked since the row was created or
    /// last released.
    pub health_status: Option<WorkspaceHealth>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorkspaceCandidate {
    pub workspace_id: String,
    pub workspace_path: PathBuf,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeRecord {
    pub change_id: String,
    pub repo: String,
    pub workspace_path: PathBuf,
    pub parent_change_id: Option<String>,
    pub title: String,
    pub jj_change_id: String,
    pub head_commit: String,
    pub created_at_epoch_s: i64,
}
